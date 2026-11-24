struct ObservedAdapter {
    program: PathBuf,
    arguments: Vec<String>,
    executable: aq_session_protocol::ArtifactRef,
    renderer: aq_session_protocol::ArtifactRef,
    private_source: Option<PathBuf>,
    scoped_auth: Option<ScopedCodexAuthBinding>,
}
impl HarnessAdapter for ObservedAdapter {
    fn prepare(&self, m: &LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            manifest_digest: canonical_digest(m)?,
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            resource_qualification: None,
            command: LocalCommand {
                executable: self.program.clone(),
                arguments: {
                    let mut args = self.arguments.clone();
                    if let Some(scope) = &self.scoped_auth {
                        args.extend([
                            "--config".into(),
                            "model_provider=\"aq-runner-proxy\"".into(),
                            "--config".into(),
                            format!(
                                "model_providers.aq-runner-proxy.base_url=\"{}/v1\"",
                                scope.origin
                            ),
                            "--config".into(),
                            "model_providers.aq-runner-proxy.wire_api=\"responses\"".into(),
                        ]);
                    }
                    args
                },
                environment: self
                    .private_source
                    .as_ref()
                    .map(|_| {
                        std::collections::BTreeMap::from([(
                            "CODEX_HOME".into(),
                            self.program
                                .parent()
                                .unwrap()
                                .join(Uuid::from_u128(2).to_string())
                                .join("codex-home")
                                .to_str()
                                .unwrap()
                                .into(),
                        )])
                    })
                    .unwrap_or_default(),
            },
        })
    }
    fn private_input(&self, _: &LaunchManifest, _: &Path) -> Result<Option<PrivateLaunchInput>> {
        self.private_source
            .as_ref()
            .map(|source| {
                let key = std::fs::read(source.join("material.bin"))?;
                let record = std::fs::read(source.join("record.json"))?;
                if let Some(scope) = &self.scoped_auth {
                    PrivateLaunchInput::codex_scoped_capability(
                        std::fs::File::open(source)?,
                        OriginalPrivateMaterial {
                            digest: aq_session_protocol::Digest::of(&key),
                            bytes: key.len() as u64,
                            record_digest: aq_session_protocol::Digest::of(&record),
                        },
                        scope.clone(),
                        scoped_capability(),
                    )
                } else {
                    PrivateLaunchInput::codex_api_key(
                        std::fs::File::open(source)?,
                        aq_session_protocol::Digest::of(&key),
                        key.len() as u64,
                        aq_session_protocol::Digest::of(&record),
                    )
                }
            })
            .transpose()
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
}
impl observed::ObservedHarnessAdapter for ObservedAdapter {
    fn observed_qualification(
        &self,
        _: &LaunchManifest,
    ) -> Result<observed::ObservedAdapterQualification> {
        Ok(observed::ObservedAdapterQualification {
            executable: self.executable.clone(),
            renderer: self.renderer.clone(),
        })
    }
}
#[test]
#[ignore = "actual local tmux closed observed gate qualification"]
fn observed_helper_stays_starting_without_exec_or_started_until_original_expiry() {
    let mut c = Case::new("unused", false);
    let rendered = rendered_material(&mut c, b"literal startup remains sealed");
    let program = std::fs::canonicalize("/usr/bin/true").unwrap();
    let image = SmallProgram::seal(program.clone(), "synthetic-true", vec![]);
    let now = ownership::raw_boot_ms();
    let launch = image.launch(&c, rendered, now..now + 1500);
    let launch_result = launch.prepare(&mut c);
    let mut pending = match launch_result {
        Ok(v) => v,
        Err(error) => {
            let root = c.temp.keep();
            panic!(
                "observed launch error {error:#}; retained {}",
                root.display()
            );
        }
    };
    let state = pending.observe().unwrap();
    assert_eq!(state.phase, HostPhase::Starting);
    let pane = state.pane.unwrap();
    let image = std::fs::metadata(format!("/proc/{}/exe", pane.pane.pid)).unwrap();
    use std::os::unix::fs::MetadataExt;
    let helper = std::fs::metadata(&c.local.helper_executable).unwrap();
    assert_eq!((image.dev(), image.ino()), (helper.dev(), helper.ino()));
    assert!(!c.temp.path().join("caller-started.json").exists());
    assert!(!c.adapter.dir.join("pane-exec.claim").exists());
    assert_eq!(
        std::fs::read(c.adapter.dir.join("context-0.bin")).unwrap(),
        b"literal startup remains sealed"
    );
    let stopped = pending.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
    assert!(!c.temp.path().join("caller-started.json").exists());
}

impl observed::ObservedLaunchAuthority for Authority {
    fn record_baseline(&mut self, baseline: &observed::VerifiedBlankBaseline) -> Result<()> {
        durable_new(
            &self.root.join("caller-baseline.json"),
            &serde_json::to_vec(baseline)?,
        )
    }
}
fn observed_program(path: &Path) {
    use std::io::Write;
    let mut child = Command::new("/usr/bin/cc")
        .args(["-O2", "-static", "-x", "c", "-o"])
        .arg(path)
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <string.h>
int main(int argc,char **argv) {
 char path[4096], out[4096], b[4096];
 const char *d=getenv("AQ_SESSION_DIRECTORY"); if(!d)return 81;
 if(snprintf(path,sizeof(path),"%s/context-0.bin",d)>=(int)sizeof(path))return 82;
 if(snprintf(out,sizeof(out),"%s/observed-received.bin",d)>=(int)sizeof(out))return 83;
 int in=open(path,O_RDONLY),to=open(out,O_WRONLY|O_CREAT|O_EXCL,0600);if(in<0||to<0)return 84;
 ssize_t n;while((n=read(in,b,sizeof(b)))>0)if(write(to,b,n)!=n)return 85;
 if(n<0||fsync(to))return 86;close(in);close(to);
 if(write(1,"observed-native-ready\n",22)!=22)return 87;
 if(argc>1 && strcmp(argv[1],"--exit")==0)return 0;
 while(read(0,b,sizeof(b))>0){} return 0;
}
"#,
        )
        .unwrap();
    assert!(child.wait().unwrap().success());
}
#[test]
#[ignore = "actual local tmux sealed FD baseline/exec qualification"]
fn observed_blank_gate_releases_only_after_durable_baseline_and_live_fd_exec() {
    let mut c = Case::new("unused", false);
    let program = c.temp.path().join("observed-native");
    observed_program(&program);
    let image = SmallProgram::seal(program.clone(), "synthetic-static-elf", vec![]);
    let rendered = rendered_material(&mut c, "literal\n雪\0startup".as_bytes());
    // Replace mutable source bytes. Successful exec must use the held sealed FD.
    std::fs::write(
        &program,
        b"invalid replacement; this path must never execute",
    )
    .unwrap();
    let now = ownership::raw_boot_ms();
    let launch = image.launch(&c, rendered, now..now + 2500);
    let pending = launch.prepare(&mut c).unwrap();
    let (mut observer, baseline) = pending.open_baseline_observer().unwrap();
    assert_eq!(baseline.terminal(), &launch.terminal);
    assert_eq!(baseline.dimensions(), (80, 24));
    assert_eq!(pending.observe().unwrap().phase, HostPhase::Starting);
    assert!(!c.adapter.dir.join("observed-received.bin").exists());
    assert!(!c.temp.path().join("caller-started.json").exists());
    let mut released = pending
        .release_after_baseline(&baseline, &mut c.authority)
        .unwrap();
    assert!(c.temp.path().join("caller-baseline.json").exists());
    let mut host = wait_named("positive exact live sealed exec", || {
        observer.poll().unwrap();
        released.poll_started(&mut c.authority).unwrap()
    });
    wait_named("literal payload reached sealed native image", || {
        observer.poll().unwrap();
        std::fs::read(c.adapter.dir.join("observed-received.bin")).ok()
    });
    assert_eq!(
        std::fs::read(c.adapter.dir.join("observed-received.bin")).unwrap(),
        "literal\n雪\0startup".as_bytes()
    );
    assert!(c.temp.path().join("caller-started.json").exists());
    let late = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: ownership::raw_boot_ms(),
        expires_boot_ms: now + 2501,
    };
    assert!(
        host.renew(&late).is_err(),
        "observed original hard deadline cannot renew"
    );
    host.stop().unwrap();
    assert!(
        host.wait_stopped(Duration::from_secs(3))
            .unwrap()
            .kernel_wait_echild
    );
}

fn observed_closed_case() -> (Case, observed::PendingObservedSession) {
    observed_closed_case_mode(false, 5_000)
}
fn observed_closed_case_mode(
    short: bool,
    permit_ms: u64,
) -> (Case, observed::PendingObservedSession) {
    let mut c = Case::new("unused", false);
    let program = c.temp.path().join("observed-native");
    observed_program(&program);
    let image = SmallProgram::seal(
        program.clone(),
        "synthetic-static-elf",
        if short { vec!["--exit".into()] } else { vec![] },
    );
    let rendered = rendered_material(&mut c, b"fixed literal startup");
    let now = ownership::raw_boot_ms();
    let launch = image.launch(&c, rendered, now..now + permit_ms);
    let result = launch.prepare(&mut c);
    let pending = match result {
        Ok(v) => v,
        Err(error) => {
            let root = c.temp.keep();
            panic!(
                "observed preparation {error:#}; retained {}",
                root.display()
            );
        }
    };
    (c, pending)
}
struct BaselineAuthority<'a> {
    base: &'a mut Authority,
    refuse: bool,
    delay_until: Option<u64>,
}
impl LaunchAuthority for BaselineAuthority<'_> {
    fn record_launch_intent(&mut self, v: &LaunchIntent) -> Result<()> {
        self.base.record_launch_intent(v)
    }
    fn record_guardian(&mut self, v: &GuardianIdentity) -> Result<()> {
        self.base.record_guardian(v)
    }
    fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
        self.base.record_start_intent(i, g)
    }
    fn record_started(&mut self, v: &HostObservation) -> Result<()> {
        self.base.record_started(v)
    }
}
impl observed::ObservedLaunchAuthority for BaselineAuthority<'_> {
    fn record_baseline(&mut self, v: &observed::VerifiedBlankBaseline) -> Result<()> {
        if self.refuse {
            anyhow::bail!("synthetic durability refusal");
        }
        if let Some(until) = self.delay_until {
            while ownership::raw_boot_ms() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.base.record_baseline(v)
    }
}
#[test]
#[ignore = "actual local tmux baseline refusal/reader loss/expiry qualification"]
fn observed_baseline_durability_reader_and_expiry_fail_closed_without_exec() {
    for mode in [
        "durability",
        "reader-drop",
        "reader-stop",
        "helper-death",
        "server-death",
        "expiry",
    ] {
        let (mut c, pending) = observed_closed_case();
        let initial = pending.observe().unwrap();
        let (observer, baseline) = pending.open_baseline_observer().unwrap();
        if mode == "reader-drop" {
            drop(observer);
        } else {
            if mode == "reader-stop" {
                drop(stop_observed_reader(observer.process()));
            }
            if mode == "server-death" {
                terminate(&initial.pane.as_ref().unwrap().server);
            }
            if mode == "helper-death" {
                terminate(&initial.pane.as_ref().unwrap().pane);
            }
            // Keep the observer alive through release for the other failure modes.
            let authority = BaselineAuthority {
                base: &mut c.authority,
                refuse: mode == "durability",
                delay_until: (mode == "expiry").then_some(initial.permit_deadline_boot_ms),
            };
            let mut authority = authority;
            assert!(
                pending
                    .release_after_baseline(&baseline, &mut authority)
                    .is_err(),
                "{mode}"
            );
            assert_stopped_without_exec(&c, &initial, mode);
            continue;
        }
        assert!(
            pending
                .release_after_baseline(&baseline, &mut c.authority)
                .is_err(),
            "{mode}"
        );
        assert_stopped_without_exec(&c, &initial, mode);
    }
}

fn stop_observed_reader(process: &ProcessEvidence) -> std::os::fd::OwnedFd {
    let pidfd = ownership::verified_pidfd(process).unwrap();
    ownership::signal(&pidfd, libc::SIGSTOP).unwrap();
    wait_named("reader stopped or already reaped by its guardian", || {
        // The guardian kills a stopped reader. Its pidfd can signal exit before
        // this sampling thread sees T in /proc.
        if !ownership::pidfd_is_live(&pidfd).unwrap() {
            return Some(());
        }
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", process.pid)).ok()?;
        let (_, fields) = stat.rsplit_once(") ")?;
        matches!(fields.as_bytes().first(), Some(b'T' | b't')).then_some(())
    });
    pidfd
}

fn assert_stopped_without_exec(c: &Case, initial: &HostObservation, mode: &str) {
    let mut recovered = SessionHost::recover(c.temp.path(), &initial.guardian.intent).unwrap();
    assert!(
        recovered
            .wait_stopped(Duration::from_millis(
                initial
                    .permit_deadline_boot_ms
                    .saturating_sub(ownership::raw_boot_ms())
                    + 3_000
            ))
            .unwrap()
            .kernel_wait_echild,
        "{mode}"
    );
    assert!(!c.adapter.dir.join("pane-exec.claim").exists(), "{mode}");
    assert!(
        !c.temp.path().join("caller-started.json").exists(),
        "{mode}"
    );
}

#[test]
#[ignore = "actual local tmux missed exec evidence fault qualification"]
fn observed_short_exec_without_live_kernel_evidence_never_records_started() {
    if std::env::var_os("AQ_OBS_SHORT_FIXTURE").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "observed_short_exec_without_live_kernel_evidence_never_records_started",
                "--ignored",
                "--nocapture",
            ])
            .env("AQ_OBS_SHORT_FIXTURE", "1")
            .env("AQ_SESSION_HOST_FAULT_STAGE", "observed-exec-permit")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let (mut c, pending) = observed_closed_case_mode(true, 1_500);
    let initial = pending.observe().unwrap();
    let (mut observer, baseline) = pending.open_baseline_observer().unwrap();
    let mut released = pending
        .release_after_baseline(&baseline, &mut c.authority)
        .unwrap();
    wait_named("guardian test fault after fixed exec permit", || {
        c.adapter
            .dir
            .join("guardian-fault.json")
            .exists()
            .then_some(())
    });
    wait_named(
        "short native exits before positive guardian observation",
        || {
            let _ = observer.poll();
            (c.adapter.dir.join("observed-received.bin").exists()
                && ownership::process_evidence(initial.pane.as_ref().unwrap().pane.pid).is_err())
            .then_some(())
        },
    );
    assert_eq!(released.observe().unwrap().phase, HostPhase::Starting);
    assert!(!c.temp.path().join("caller-started.json").exists());
    ownership::signal(
        &ownership::verified_pidfd(&initial.guardian.process).unwrap(),
        libc::SIGCONT,
    )
    .unwrap();
    wait_named(
        "missed exec evidence remains uncertain without Started",
        || {
            let _ = observer.poll();
            released
                .poll_started(&mut c.authority)
                .is_err()
                .then_some(())
        },
    );
    let mut recovered = SessionHost::recover(c.temp.path(), &initial.guardian.intent).unwrap();
    assert!(
        recovered
            .wait_stopped(Duration::from_secs(3))
            .unwrap()
            .kernel_wait_echild
    );
    assert!(c.adapter.dir.join("pane-exec.claim").exists());
    assert!(!c.temp.path().join("caller-started.json").exists());
}

struct SmallProgram {
    image: observed::QualifiedElf,
    adapter: ObservedAdapter,
}
impl SmallProgram {
    fn seal(program: PathBuf, name: &str, arguments: Vec<String>) -> Self {
        use aq_session_protocol::{ArtifactRef, Digest, Text};
        let executable = ArtifactRef {
            id: Text::new(name).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(&std::fs::read(&program).unwrap()),
        };
        let renderer = ArtifactRef {
            id: Text::new("synthetic-renderer").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"test-only installed qualification"),
        };
        let pool = observed::SealedElfPool::new(4 * 1024 * 1024, 1).unwrap();
        let image = pool.load_verified(&program, &executable).unwrap();
        let adapter = ObservedAdapter {
            program,
            arguments,
            executable,
            renderer,
            private_source: None,
            scoped_auth: None,
        };
        Self { image, adapter }
    }

    fn launch(
        self,
        case: &Case,
        rendered: RenderedStartup,
        window: std::ops::Range<u64>,
    ) -> SmallLaunch {
        let terminal = aq_session_protocol::TerminalIdentity {
            session: case.manifest.bindings.session.clone(),
            terminal_id: Uuid::new_v4(),
            epoch: 1,
        };
        let profile =
            observed::ObservationProfile::small_elf(self.adapter.renderer.clone(), 80, 24).unwrap();
        let startup = observed::ObservedStartup::new(
            rendered,
            terminal.clone(),
            self.image,
            profile,
            window.end,
        )
        .unwrap();
        let permit = ExecutionPermit {
            session: case.manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&case.manifest).unwrap(),
            sequence: 1,
            issued_boot_ms: window.start,
            expires_boot_ms: window.end,
        };
        SmallLaunch {
            startup,
            permit,
            adapter: self.adapter,
            terminal,
        }
    }
}
struct SmallLaunch {
    startup: observed::ObservedStartup,
    permit: ExecutionPermit,
    adapter: ObservedAdapter,
    terminal: aq_session_protocol::TerminalIdentity,
}
impl SmallLaunch {
    fn prepare(&self, case: &mut Case) -> Result<observed::PendingObservedSession> {
        SessionHost::prepare_observed(
            case.temp.path(),
            case.id,
            &self.startup,
            self.permit.clone(),
            &case.local,
            &self.adapter,
            &mut case.authority,
        )
    }
}
