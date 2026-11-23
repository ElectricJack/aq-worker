use super::*;

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|r| r.to_lowercase().contains("microsoft"))
}

pub async fn enroll(args: ServiceEnrollArgs) -> Result<()> {
    let layout = Layout::for_profile(&args.profile)?;
    let cf = args.cf_service_token;
    let secrets = move |code: bool| match code {
        true => crate::read_secrets(cf, false).map(|s| Enrollment {
            code: s.code,
            cf: s.cf,
        }),
        false => crate::read_access().map(|access| Enrollment {
            code: String::new(),
            cf: Some(access),
        }),
    };
    enroll_in(
        &args,
        &layout,
        Host {
            name: crate::hostname(),
            wsl: is_wsl(),
        },
        secrets,
    )
    .await
}
/// What the operator types (or pipes) at enrollment: the code, or (resuming with
/// `--reconfigure --cf-service-token`) only the Access client id and secret.
pub(super) struct Enrollment {
    pub(super) code: String,
    pub(super) cf: Option<(String, String)>,
}
/// Slot 1's credential after the code is redeemed and before the machine key claim succeeds.
/// A rerun resumes the claim from it instead of redeeming a new code.
pub(super) fn pending_enrollment(layout: &Layout) -> PathBuf {
    layout.config_dir.join("enroll-pending.toml")
}

pub(super) fn staged(path: &Path) -> PathBuf {
    path.with_extension("toml.new")
}
/// The slot file an earlier run of this enrollment saved (staged or installed), when it is the
/// server's node for that slot.
fn saved_slot(layout: &Layout, slot: u32, node_id: Uuid, server: &str) -> Option<EnrollmentFile> {
    let path = layout.slot_enrollment(slot);
    [staged(&path), path]
        .iter()
        .filter_map(|p| EnrollmentFile::load(p).ok())
        .find(|f| f.node_id == node_id && f.server_url == server)
}

pub(super) struct Host {
    pub(super) name: Option<String>,
    pub(super) wsl: bool,
}

struct Plan {
    name: String,
    config: ServiceConfig,
    template: Vec<u8>,
}

fn machine_key(args: &ServiceEnrollArgs, hostname: Option<&str>) -> Result<String> {
    match &args.machine_key {
        Some(key) => {
            ensure!(
                valid_machine_key(key),
                "machine key {key:?} is not valid: 1-63 characters of a-z, 0-9 and '-', not starting with '-'"
            );
            Ok(key.clone())
        }
        None => hostname
            .and_then(default_machine_key)
            .context("pass --machine-key: the host name does not make a valid machine key"),
    }
}

impl Plan {
    fn validate(
        args: &ServiceEnrollArgs,
        layout: &Layout,
        hostname: Option<String>,
    ) -> Result<Self> {
        let machine_key = machine_key(args, hostname.as_deref())?;
        ensure!(
            (1..=aq_protocol::MAX_MACHINE_SLOTS).contains(&args.nodes),
            "nodes must be 1..={}",
            aq_protocol::MAX_MACHINE_SLOTS
        );
        aq_node_service::checked_origin(&args.server)?;
        let name = args
            .name
            .clone()
            .or(hostname)
            .context("cannot determine hostname; pass --name")?;
        let repositories: Vec<RepositoryBinding> = match &args.repositories {
            None => vec![],
            Some(path) => serde_json::from_slice(
                &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
            )
            .context("repositories must be a JSON array of repository bindings")?,
        };
        let template = std::fs::read(&args.native_template)
            .with_context(|| format!("read {}", args.native_template.display()))?;

        let config = ServiceConfig {
            schema_version: 1,
            profile: args.profile.clone(),
            machine_key,
            nodes: args.nodes,
            native_template: layout.config_dir.join("native.json"),
            state_root: layout.state_root.clone(),
            runtime_root: None,
            aq_git: AqGitConfig {
                binary: std::fs::canonicalize(&args.aq_git)
                    .with_context(|| format!("aq-git {}", args.aq_git.display()))?,
                git_executable: args.git_executable.clone(),
            },
            repositories,
            manual: None,
            shutdown_grace_ms: 120_000,
            retry: Default::default(),
            observations: Default::default(),
        };
        config.validate()?;
        Ok(Self {
            name,
            config,
            template,
        })
    }
    fn finish(&mut self, args: &ServiceEnrollArgs, layout: &Layout, wsl: bool) -> Result<()> {
        // The machine's label on the Swarm page is its key unless --name gives one.
        let label = args.name.as_deref().unwrap_or(&self.config.machine_key);
        write_machine_json(&layout.machine_json(), &self.config.machine_key, label, wsl)?;
        write_private(&self.config.native_template, &self.template)?;
        self.config.native_template = std::fs::canonicalize(&self.config.native_template)?;
        self.config.save(&layout.service_toml())?;
        std::fs::remove_file(pending_enrollment(layout))
            .context("remove the pending enrollment")?;
        Ok(())
    }
}

fn resume(
    args: &ServiceEnrollArgs,
    pending: &Path,
    secrets: impl FnOnce(bool) -> Result<Enrollment>,
) -> Result<EnrollmentFile> {
    let mut first = EnrollmentFile::load(pending)?;
    ensure!(
        first.server_url == args.server,
        "an unfinished enrollment for {} is in {}; pass that --server, or delete the file to start over with a new code",
        first.server_url,
        pending.display()
    );
    println!("resuming the unfinished enrollment at the machine key claim; no code is needed");
    if args.reconfigure && args.cf_service_token {
        let (id, secret) = secrets(false)?.cf.context("Access credentials")?;
        (first.cf_client_id, first.cf_client_secret) = (Some(id), Some(secret));
        first.save(pending)?;
    }
    Ok(first)
}

async fn redeem(
    args: &ServiceEnrollArgs,
    layout: &Layout,
    name: String,
    secrets: impl FnOnce(bool) -> Result<Enrollment>,
) -> Result<EnrollmentFile> {
    if layout.service_toml().exists() && !args.reconfigure {
        bail!(
            "profile {} is already enrolled; nothing was changed and no code was used. To enroll it again (for example after a revoke), pass --reconfigure",
            args.profile
        );
    }
    let typed = secrets(true)?;
    let (cf_id, cf_secret) = typed.cf.map_or((None, None), |(i, s)| (Some(i), Some(s)));
    let client = Client::new(&args.server, None, cf_id.clone(), cf_secret.clone())?;
    let enrolled = client
        .enroll(&EnrollRequest {
            code: typed.code,
            name,
            worker_version: env!("AQ_WORKER_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
        })
        .await?;
    let first = EnrollmentFile {
        server_url: args.server.clone(),
        node_id: enrolled.node_id,
        credential: enrolled.credential,
        cf_client_id: cf_id,
        cf_client_secret: cf_secret,
    };
    first.save(&pending_enrollment(layout))?;
    Ok(first)
}
async fn mint(
    first: &EnrollmentFile,
    machine_key: &str,
    nodes: u32,
    pending: &Path,
) -> Result<aq_protocol::SlotsResponse> {
    ServerLink::from_enrollment(first)?
        .slots(&SlotsRequest { schema_version: 1, machine_key: machine_key.to_owned(), slots: nodes })
        .await
        .map_err(|e| match e.downcast_ref::<LinkError>() {
            Some(l) if l.status == 409 => anyhow::anyhow!("machine key {machine_key} is in use by another machine; revoke it from the Swarm page or pass --machine-key (rerun this command: the code is not needed again)"),
            Some(l) if l.revoked() => anyhow::anyhow!("the server refused the unfinished enrollment's credential; delete {} and enroll with a new code", pending.display()),
            Some(l) if l.edge_refused() => anyhow::anyhow!("Cloudflare Access refused the stored token at the machine key claim; run enroll with --reconfigure --cf-service-token and new Access credentials (the code is not needed again)"),
            _ => e.context("the machine key claim failed; rerun this command to resume it (the code is not needed again)"),
        })
}
fn slot_files(
    layout: &Layout,
    nodes: u32,
    first: EnrollmentFile,
    minted: aq_protocol::SlotsResponse,
) -> Result<Vec<EnrollmentFile>> {
    let mut files = vec![first.clone()];
    for slot in 2..=nodes {
        let entry = minted
            .slots
            .iter()
            .find(|s| s.slot == slot)
            .with_context(|| format!("the server did not return slot {slot}"))?;
        let file = match &entry.credential {
            Some(credential) => EnrollmentFile {
                node_id: entry.node_id,
                credential: credential.clone(),
                ..first.clone()
            },
            // Minted by an earlier run of this enrollment: the server returns a credential
            // only once, so it must be the one that run saved here.
            None => saved_slot(layout, slot, entry.node_id, &first.server_url).with_context(|| {
                format!(
                    "slot {slot} was minted by an earlier run of this enrollment, which stopped before it saved the slot's credential; it cannot be recovered. Revoke the machine from the Swarm page, delete {}, then run enroll with --reconfigure and a new code",
                    pending_enrollment(layout).display()
                )
            })?,
        };
        files.push(file);
    }
    Ok(files)
}
fn install_slots(layout: &Layout, nodes: u32, files: &[EnrollmentFile]) -> Result<()> {
    // Every slot file is staged beside its final path first, then all are renamed. A cut
    // during the renames leaves some staged and some installed files; the rerun (pending is
    // still there) finds each one, staged or installed, and finishes the renames.
    for (slot, file) in (1..).zip(files) {
        file.save(&staged(&layout.slot_enrollment(slot)))?;
    }
    for slot in 1..=nodes {
        let path = layout.slot_enrollment(slot);
        std::fs::rename(staged(&path), &path)
            .with_context(|| format!("install {}", path.display()))?;
    }
    Ok(())
}
fn print_enrollment(args: &ServiceEnrollArgs, machine_key: &str, files: &[EnrollmentFile]) {
    println!(
        "machine {machine_key}: nodes {}",
        files
            .iter()
            .map(|f| f.node_id.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    if args.reconfigure {
        println!(
            "run `aq-worker service install --profile {}` to restart the service with this enrollment",
            args.profile
        );
    }
}

/// Validate all local inputs before reading secrets or contacting the server.
/// Resume a pending claim before installing credentials and the final configuration.
pub(super) async fn enroll_in(
    args: &ServiceEnrollArgs,
    layout: &Layout,
    host: Host,
    secrets: impl FnOnce(bool) -> Result<Enrollment>,
) -> Result<()> {
    let mut plan = Plan::validate(args, layout, host.name)?;
    let pending = pending_enrollment(layout);
    let first = if pending.exists() {
        resume(args, &pending, secrets)?
    } else {
        redeem(args, layout, plan.name.clone(), secrets).await?
    };
    let minted = mint(&first, &plan.config.machine_key, args.nodes, &pending).await?;
    let files = slot_files(layout, args.nodes, first, minted)?;
    install_slots(layout, args.nodes, &files)?;
    plan.finish(args, layout, host.wsl)?;
    print_enrollment(args, &plan.config.machine_key, &files);
    Ok(())
}
