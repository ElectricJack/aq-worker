//! Receive verified process witnesses and supervise the physical capsule until cleanup.
use super::*;

pub(super) struct Session<'a> {
    pub(super) control: &'a wire::Socket,
    pub(super) worker: i32,
    pub(super) plan: &'a PlanRecord,
}

struct Witnesses {
    init: ProcessEvidence,
    initfd: OwnedFd,
    origin: String,
    listener: TcpListener,
    harness: ProcessEvidence,
    harnessfd: OwnedFd,
}

#[derive(Clone, Copy)]
enum ProcessStage {
    Init { bootstrap: i32 },
    Harness { init: u32 },
}
impl ProcessStage {
    fn inner_pid(self, packet: wire::Packet<Message>, plan: &PlanRecord) -> Result<(u32, u32)> {
        let inner_pid = match (self, packet.value) {
            (Self::Init { .. }, Message::Init { nonce, inner_pid }) => {
                ensure!(
                    nonce == plan.nonce && inner_pid == 1 && packet.fd.is_none(),
                    "capsule init stage"
                );
                inner_pid
            }
            (Self::Init { .. }, _) => bail!("capsule init witness missing"),
            (Self::Harness { .. }, Message::Harness { nonce, inner_pid }) => {
                ensure!(
                    nonce == plan.nonce && inner_pid > 1 && packet.fd.is_none(),
                    "capsule harness stage binding"
                );
                inner_pid
            }
            (Self::Harness { .. }, _) => bail!("capsule harness stage absent"),
        };
        Ok((packet.credentials.pid as u32, inner_pid))
    }

    fn namespace_init(self, process: &ProcessEvidence) -> Result<u32> {
        let actual_parent = parent(process.pid)?;
        match self {
            Self::Init { bootstrap } => {
                ensure!(actual_parent == bootstrap as u32, "capsule init ancestry");
                Ok(process.pid)
            }
            Self::Harness { init } => {
                ensure!(
                    actual_parent == init && process.pid != init,
                    "capsule actual harness ancestry"
                );
                Ok(init)
            }
        }
    }

    fn receive(
        self,
        inner: &wire::Socket,
        plan: &PlanRecord,
    ) -> Result<(ProcessEvidence, OwnedFd)> {
        let packet = inner.recv::<Message>(plan.deadline)?;
        let (host_pid, inner_pid) = self.inner_pid(packet, plan)?;
        let witness = ownership::process_evidence(host_pid)?;
        let init = self.namespace_init(&witness)?;
        let fd = ownership::verified_pidfd(&witness)?;
        namespaces(witness.pid, inner_pid, init)?;
        Ok((witness, fd))
    }
}
fn receive_listener(
    inner: &wire::Socket,
    plan: &PlanRecord,
    init: &ProcessEvidence,
) -> Result<(String, TcpListener)> {
    let packet = inner.recv::<Message>(plan.deadline)?;
    let Message::Listener { nonce, port } = packet.value else {
        bail!("capsule listener stage absent")
    };
    ensure!(
        nonce == plan.nonce && packet.credentials.pid as u32 == init.pid && port > 0,
        "capsule listener sender/stage"
    );
    let origin = format!("http://127.0.0.1:{port}");
    let listener = listener(
        packet.fd.context("capsule child-netns listener missing")?,
        &origin,
    )?;
    Ok((origin, listener))
}
impl Session<'_> {
    pub(super) fn supervise(self, inner: wire::Socket, bootstrap: i32) -> Result<()> {
        let (init, initfd) = ProcessStage::Init { bootstrap }.receive(&inner, self.plan)?;
        let (origin, listener) = receive_listener(&inner, self.plan, &init)?;
        let (harness, harnessfd) =
            ProcessStage::Harness { init: init.pid }.receive(&inner, self.plan)?;
        let witnesses = Witnesses {
            init,
            initfd,
            origin,
            listener,
            harness,
            harnessfd,
        };
        self.run(inner, witnesses)
    }

    fn run(&self, inner: wire::Socket, witnesses: Witnesses) -> Result<()> {
        caller_still_waiting(self.control)?;
        if self.plan.prepared.is_some() {
            let info = self.publish_prepared(&witnesses)?;
            let Some(commit) = self.receive_commit(&info) else {
                drop(inner);
                drop(witnesses.listener);
                let reaped = cleanup(&witnesses.initfd)?;
                return finish_report(
                    self.control,
                    self.worker,
                    self.plan.nonce,
                    Message::PreparedStopped { info, reaped },
                );
            };
            caller_still_waiting(self.control)?;
            inner.send(&Message::Commit(commit), None, self.plan.deadline)?;
        } else {
            inner.send(
                &Message::Release {
                    nonce: self.plan.nonce,
                },
                None,
                self.plan.deadline,
            )?;
        }
        drop(inner);
        self.await_exec(&witnesses)?;
        let Witnesses {
            init,
            initfd,
            origin,
            listener,
            harness,
            harnessfd,
        } = witnesses;
        let witness = CapsuleWitness {
            guardian: ownership::process_evidence(std::process::id())?,
            init,
            harness,
            origin,
            nonce: self.plan.nonce,
        };
        self.control.send(
            &Message::Ready(witness),
            if self.plan.prepared.is_some() {
                None
            } else {
                Some(listener.as_raw_fd())
            },
            self.plan.deadline,
        )?;
        drop(listener);
        self.wait_stop(&initfd, &harnessfd)?;
        let reaped = cleanup(&initfd)?;
        ensure!(reaped > 0, "capsule owned descendant reap absent");
        // A dropped caller has no receiver; physical cleanup still completed.
        finish_report(
            self.control,
            self.worker,
            self.plan.nonce,
            Message::Stopped {
                nonce: self.plan.nonce,
                reaped,
            },
        )
    }

    fn publish_prepared(&self, witnesses: &Witnesses) -> Result<CandidatePreparedInfo> {
        let helper = File::open("/proc/self/exe")?;
        let metadata = helper.metadata()?;
        verify_image(
            &witnesses.harness,
            &seal::Pin {
                digest: Digest::of(b"sealed helper identity only"),
                bytes: metadata.len(),
                device: metadata.dev(),
                inode: metadata.ino(),
                executable: true,
            },
        )?;
        let info = prepared::info(
            self.plan,
            prepared::PreparedProcesses {
                guardian: ownership::process_evidence(std::process::id())?,
                init: witnesses.init.clone(),
                stub: witnesses.harness.clone(),
            },
            witnesses.origin.clone(),
        )?;
        self.control.send(
            &Message::Prepared(info.clone()),
            Some(witnesses.listener.as_raw_fd()),
            self.plan.deadline,
        )?;
        Ok(info)
    }

    fn receive_commit(&self, info: &CandidatePreparedInfo) -> Option<prepared::CommitRecord> {
        let packet = self.control.recv::<Message>(self.plan.deadline).ok()?;
        if packet.credentials.pid != self.worker || packet.fd.is_some() {
            return None;
        }
        match packet.value {
            Message::Commit(record) if prepared::validate_record(&record, info).is_ok() => {
                Some(record)
            }
            _ => None,
        }
    }

    fn await_exec(&self, witnesses: &Witnesses) -> Result<()> {
        while verify_image(&witnesses.harness, &self.plan.program).is_err() {
            caller_still_waiting(self.control)?;
            ensure!(
                ownership::raw_boot_ms() < self.plan.deadline
                    && ownership::pidfd_is_live(&witnesses.harnessfd)?,
                "capsule actual exec not established"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    fn wait_stop(&self, initfd: &OwnedFd, harnessfd: &OwnedFd) -> Result<()> {
        while self.live(initfd, harnessfd)? {
            if self.stop_requested()? {
                break;
            }
        }
        Ok(())
    }

    fn live(&self, initfd: &OwnedFd, harnessfd: &OwnedFd) -> Result<bool> {
        Ok(ownership::raw_boot_ms() < self.plan.deadline
            && ownership::pidfd_is_live(initfd)?
            && ownership::pidfd_is_live(harnessfd)?)
    }

    fn stop_requested(&self) -> Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.control.raw(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 10) };
        ensure!(ready >= 0, "capsule control poll");
        if ready == 0 {
            return Ok(false);
        }
        // Control loss is a stop fence, never stopped proof.
        if let Ok(packet) = self.control.recv::<Message>(self.plan.deadline) {
            ensure!(
                packet.credentials.pid == self.worker
                    && packet.fd.is_none()
                    && matches!(packet.value,Message::Stop {nonce} if nonce == self.plan.nonce),
                "capsule stop sender/binding"
            );
        }
        Ok(true)
    }
}
