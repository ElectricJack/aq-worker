//! Frozen project grants and result receipts, held only on the trusted node.
use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspaces {
    pub workspaces: Vec<Project>,
    #[serde(default)]
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub project_id: Uuid,
    pub project_revision: u64,
    pub binding_generation: u64,
    pub access: Access,
    pub checkout_head: String,
    #[serde(flatten)]
    pub git: Grant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "service", rename_all = "snake_case")]
pub enum Service {
    Postgres,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    project_id: Uuid,
    repository_id: Uuid,
    result: Checkpoint,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Checkpoint {
    Unchanged { head: String },
    Pushed { head: String },
    Conflict { head: Option<String> },
}

impl Workspaces {
    pub fn has_resources(&self) -> bool {
        !self.workspaces.is_empty() || !self.services.is_empty()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.workspaces.len() <= 16 && self.services.len() <= 1,
            "workspace grant bound"
        );
        let mut projects = BTreeSet::new();
        let mut repositories = BTreeSet::new();
        for project in &self.workspaces {
            let grant = &project.git;
            grant.validate()?;
            ensure!(
                !project.project_id.is_nil()
                    && !grant.repository_id.is_nil()
                    && project.project_revision > 0
                    && project.binding_generation > 0
                    && projects.insert(project.project_id)
                    && repositories.insert(grant.repository_id)
                    && grant.endpoint == project_endpoint(project.project_id, grant.repository_id)
                    && !grant.update_from_main
                    && (project.access == Access::Write || grant.lease.is_none())
                    && grant.lease.as_deref().unwrap_or(&grant.base) == project.checkout_head,
                "sibling workspace scope differs from exact grant",
            );
        }
        Ok(())
    }
}

pub(crate) fn project_endpoint(project: Uuid, repository: Uuid) -> String {
    format!("/v1/git/workspaces/{project}/{repository}")
}

pub(crate) fn scoped_endpoint(value: &str, repository: Uuid) -> bool {
    let Some(relative) = value.strip_prefix("/v1/git/workspaces/") else {
        return false;
    };
    let Some((project, _)) = relative.split_once('/') else {
        return false;
    };
    Uuid::parse_str(project)
        .is_ok_and(|id| !id.is_nil() && value == project_endpoint(id, repository))
}

fn write_record(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    std::fs::File::open(path.parent().context("workspace record parent")?)?.sync_all()?;
    Ok(())
}

fn plan_path(primary: &Path) -> PathBuf {
    primary.with_extension("workspaces.json")
}

pub fn checkout_path(primary: &Path, project: Uuid) -> PathBuf {
    primary.with_extension("projects").join(project.to_string())
}

pub fn readonly_path(primary: &Path, project: Uuid) -> PathBuf {
    primary
        .with_extension("snapshots")
        .join(project.to_string())
}

pub fn sandbox_path(project: Uuid) -> String {
    format!("/aq/projects/{project}")
}

pub fn load(primary: &Path) -> Result<Workspaces> {
    let bytes = match std::fs::read(plan_path(primary)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Workspaces::default());
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(bytes.len() <= 32_768, "sibling workspace plan too large");
    let plan: Workspaces = serde_json::from_slice(&bytes)?;
    plan.validate()?;
    Ok(plan)
}

impl Node {
    pub async fn start_siblings(
        &self,
        attempt: Uuid,
        primary: &Path,
        remaining: Duration,
        required: bool,
    ) -> Result<()> {
        let plan = self
            .remote
            .workspaces(attempt, remaining.min(self.git.limits.command_timeout))
            .await?;
        ensure!(
            required == plan.has_resources(),
            "workspace_required_mismatch: supplemental workspace response differs from sealed offer",
        );
        if !required {
            ensure!(load(primary)? == plan, "sealed workspace plan disappeared");
            return Ok(());
        }
        let (git, remote, primary) = (self.git.clone(), self.remote.clone(), primary.to_owned());
        blocking(move || {
            let path = plan_path(&primary);
            if path.exists() {
                ensure!(
                    load(&primary)? == plan,
                    "sealed sibling workspace plan changed"
                );
            } else {
                write_record(&path, &plan)?;
            }
            for entry in plan.workspaces {
                let path = checkout_path(&primary, entry.project_id);
                let tree = git.start(&remote, &entry.git, attempt, &path)?;
                ensure!(
                    tree.start == entry.checkout_head,
                    "sibling worktree revision differs"
                );
            }
            Ok(())
        })
        .await
    }

    pub async fn finish_siblings(
        &self,
        attempt: Uuid,
        primary: &Path,
        author: Option<GitAuthor>,
    ) -> Result<()> {
        let plan = load(primary)?;
        let outputs: Vec<_> = plan
            .workspaces
            .into_iter()
            .filter(|entry| entry.access == Access::Write)
            .collect();
        if outputs.is_empty() {
            return Ok(());
        }
        let (git, remote, primary) = (self.git.clone(), self.remote.clone(), primary.to_owned());
        let outputs = blocking(move || {
            let path = primary.with_extension("workspace-results.json");
            if path.is_file() {
                let saved: Vec<Output> = serde_json::from_slice(&std::fs::read(&path)?)?;
                ensure!(
                    saved.len() == outputs.len()
                        && saved.iter().zip(&outputs).all(|(result, entry)| {
                            result.project_id == entry.project_id
                                && result.repository_id == entry.git.repository_id
                        }),
                    "stored workspace results differ from approved projects"
                );
                return Ok(saved);
            }
            let mut results = Vec::with_capacity(outputs.len());
            let message = format!("AQ attempt {attempt}");
            for entry in outputs {
                let tree = NodeGit::open(&checkout_path(&primary, entry.project_id))?;
                ensure!(
                    tree.attempt == attempt
                        && tree.start == entry.checkout_head
                        && tree.grant == entry.git
                        && tree.repository == git.repository(entry.git.repository_id),
                    "sibling checkout authority differs"
                );
                let commit = git.commit(
                    &tree.path,
                    &message,
                    author.as_ref().map(GitAuthor::identity),
                )?;
                let result = if commit.trim() == tree.start {
                    Checkpoint::Unchanged { head: tree.start }
                } else {
                    let lease = format!(
                        "--force-with-lease={}:{}",
                        tree.grant.task_ref,
                        tree.grant.lease.as_deref().unwrap_or_default()
                    );
                    match git.push(
                        &remote,
                        &tree,
                        &lease,
                        &tree.grant.task_ref,
                        commit.trim().to_owned(),
                    )? {
                        Pushed::Pushed { commit, .. } => Checkpoint::Pushed { head: commit },
                        Pushed::Conflict => Checkpoint::Conflict { head: None },
                    }
                };
                results.push(Output {
                    project_id: entry.project_id,
                    repository_id: entry.git.repository_id,
                    result,
                });
            }
            write_record(&path, &results)?;
            Ok(results)
        })
        .await?;
        self.remote
            .workspace_result(attempt, &outputs, self.git.limits.command_timeout)
            .await
    }

    pub async fn end_siblings(&self, primary: &Path) -> Result<()> {
        let plan = load(primary)?;
        if !plan.has_resources() {
            return Ok(());
        }
        for entry in plan.workspaces {
            let path = checkout_path(primary, entry.project_id);
            if record(&path).is_file() {
                self.end(path).await?;
            }
        }
        for path in [
            primary.with_extension("projects"),
            primary.with_extension("snapshots"),
        ] {
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            }
        }
        for path in [
            plan_path(primary),
            primary.with_extension("workspace-results.json"),
        ] {
            if path.is_file() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}
