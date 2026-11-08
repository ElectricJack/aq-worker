//! Who may run a stopped result's source and report steps: the live original
//! owner, or after a node restart the salvage owner (W4). The salvage owner
//! writes only through `Journal::salvage_result`'s gate and checks the original
//! source directories against the pins the journal recorded at launch.
use super::*;
use crate::{source::GitClient, transfer_client::TransferClient};
use std::{future::Future, pin::Pin};

type Job<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub(super) trait ResultJournal: Sync {
    fn journal<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Journal) -> Result<T> + Send + 'static,
    ) -> Job<'_, T>;
}
pub(super) trait StoppedOwner: ResultJournal {
    fn attempt_id(&self) -> Uuid;
    fn verify(&self) -> Result<()>;
    fn roots(&self) -> Arc<roots::SourceRoots>;
    fn git(&self) -> GitClient;
}
impl ResultJournal for InputPipeline {
    fn journal<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Journal) -> Result<T> + Send + 'static,
    ) -> Job<'_, T> {
        Box::pin(self.storage(move |owner| f(&mut owner.journal)))
    }
}
pub(super) struct Live<'a>(
    pub(super) &'a InputPipeline,
    pub(super) &'a DurableStoppedResult,
);
impl ResultJournal for Live<'_> {
    fn journal<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Journal) -> Result<T> + Send + 'static,
    ) -> Job<'_, T> {
        self.0.journal(f)
    }
}
impl StoppedOwner for Live<'_> {
    fn attempt_id(&self) -> Uuid {
        self.1.attempt_id()
    }
    fn verify(&self) -> Result<()> {
        self.1.verify(self.0)
    }
    fn roots(&self) -> Arc<roots::SourceRoots> {
        self.1.custody.roots.clone()
    }
    fn git(&self) -> GitClient {
        self.1.custody.scope.inputs.workspace().git().clone()
    }
}

/// The original source directories of a result, opened after a restart and
/// checked against the journal's launch pins.
pub struct SalvageRoots(Arc<roots::SourceRoots>);
pub fn salvage_roots(h: &ResultHistory) -> Result<SalvageRoots> {
    let roots = roots::SourceRoots::new(&h.scope.managed_root, &h.scope.workspace.path)?;
    ensure!(
        roots.pins()? == (h.scope.git_pin.clone(), h.scope.workspace_pin.clone()),
        "salvage source directories differ from the launch"
    );
    Ok(SalvageRoots(Arc::new(roots)))
}
struct Salvage {
    journal: Arc<Mutex<Journal>>,
    id: Uuid,
    roots: Arc<roots::SourceRoots>,
    git: GitClient,
}
impl ResultJournal for Salvage {
    fn journal<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Journal) -> Result<T> + Send + 'static,
    ) -> Job<'_, T> {
        let journal = self.journal.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                f(&mut *journal
                    .lock()
                    .map_err(|_| anyhow::anyhow!("salvage journal poisoned"))?)
            })
            .await?
        })
    }
}
impl StoppedOwner for Salvage {
    fn attempt_id(&self) -> Uuid {
        self.id
    }
    fn verify(&self) -> Result<()> {
        self.roots.verify()
    }
    fn roots(&self) -> Arc<roots::SourceRoots> {
        self.roots.clone()
    }
    fn git(&self) -> GitClient {
        self.git.clone()
    }
}
/// Records the salvage (`claim` is None to keep a retained claim), then runs the live stop's source and report steps:
/// checkpoint, export, bounded transfer and the exact report. Repeated calls
/// resume from the journal.
pub async fn finish_salvage(
    mut journal: Journal,
    id: Uuid,
    claim: Option<aq_agent_result_local::Submission>,
    roots: SalvageRoots,
    git: GitClient,
    transfer: &TransferClient,
    client: &NativeClient,
) -> Result<aq_admission::result_record::Receipt> {
    let h = journal.salvage_result(id, claim)?;
    ensure!(
        git.root() == h.scope.managed_root,
        "salvage aq-git root differs from the launch"
    );
    let owner = Salvage {
        journal: Arc::new(Mutex::new(journal)),
        id,
        roots: roots.0,
        git,
    };
    super::source::capture(&owner).await?;
    let bound = super::source::transfer_bound(&super::source::snapshot(&owner).await?)?;
    for _ in 0..bound {
        if super::source::transfer_step(&owner, transfer).await? {
            return super::report::record_report(&owner, client).await;
        }
    }
    anyhow::bail!("salvage source transfer step bound exhausted")
}
