//! Private local read-client ownership; not an external relay or launch authority.
use super::*;
use std::mem::{size_of, size_of_val};
use std::os::{
    fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    unix::ffi::OsStrExt,
};
const SOCKET: &str = "terminal-reader.sock";
const MAX_PACKET: usize = 4096;
pub(crate) const LEASE_MS: u64 = 1000;
const DRAIN_CADENCE_MS: u64 = 250;
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Request {
    Register {
        binding: Box<terminal_io::TerminalBinding>,
        claim: Uuid,
    },
    Acknowledge {
        claim: Uuid,
    },
    Drained {
        claim: Uuid,
        sequence: u64,
        boot_ms: u64,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    binding: terminal_io::TerminalBinding,
    claim: Uuid,
    process: ProcessEvidence,
    deadline: u64,
}
struct Packet {
    bytes: Vec<u8>,
    fd: Option<OwnedFd>,
}
struct Socket(OwnedFd);
impl Socket {
    fn new() -> Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        ensure!(fd >= 0, "reader local socket creation");
        let socket = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        let capacity: libc::c_int = 8192;
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            ensure!(
                unsafe {
                    libc::setsockopt(
                        socket.raw(),
                        libc::SOL_SOCKET,
                        option,
                        (&capacity as *const libc::c_int).cast(),
                        size_of::<libc::c_int>() as _,
                    )
                } == 0,
                "reader socket capacity"
            );
        }
        Ok(socket)
    }
    fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }
    fn address(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
        let bytes = path.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        ensure!(
            !bytes.contains(&0) && bytes.len() < address.sun_path.len(),
            "reader socket path bound"
        );
        address.sun_family = libc::AF_UNIX as _;
        for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
            *out = *byte as _;
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as _;
        Ok((address, length))
    }
    fn connect(path: &Path) -> Result<Self> {
        let socket = Self::new()?;
        let (address, length) = Self::address(path)?;
        ensure!(
            unsafe {
                libc::connect(
                    socket.raw(),
                    (&address as *const libc::sockaddr_un).cast(),
                    length,
                )
            } == 0,
            "reader local connect refused"
        );
        Ok(socket)
    }
    fn credentials(&self) -> Result<libc::ucred> {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    self.raw(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut length,
                )
            } == 0
                && length as usize == size_of::<libc::ucred>()
                && credentials.uid == unsafe { libc::geteuid() },
            "reader local peer identity"
        );
        Ok(credentials)
    }
    fn accept(&self) -> Result<Option<Self>> {
        let fd = unsafe {
            libc::accept4(
                self.raw(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(e.into());
        }
        let socket = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        socket.credentials()?;
        Ok(Some(socket))
    }
    fn send<T: Serialize>(&self, value: &T, fd: Option<RawFd>) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(bytes.len() <= MAX_PACKET, "reader packet bound");
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(fd) = fd {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as _) } as _;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&msg);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as _) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
            }
        }
        ensure!(
            unsafe { libc::sendmsg(self.raw(), &msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) }
                == bytes.len() as isize,
            "reader packet send refused"
        );
        Ok(())
    }
    fn receive(&self) -> Result<Option<Packet>> {
        let mut bytes = vec![0u8; MAX_PACKET];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of_val(&control);
        let count = unsafe {
            libc::recvmsg(
                self.raw(),
                &mut msg,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        if count < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(e.into());
        }
        let mut fds = Vec::new();
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&msg);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
                {
                    let length = (*header)
                        .cmsg_len
                        .saturating_sub(libc::CMSG_LEN(0) as usize)
                        / size_of::<RawFd>();
                    for n in 0..length {
                        fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                            libc::CMSG_DATA(header).cast::<RawFd>().add(n),
                        )));
                    }
                }
                header = libc::CMSG_NXTHDR(&msg, header);
            }
        }
        ensure!(
            count > 0
                && msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0
                && fds.len() <= 1,
            "reader closed or malformed packet"
        );
        bytes.truncate(count as usize);
        Ok(Some(Packet {
            bytes,
            fd: fds.pop(),
        }))
    }
}
struct Reader {
    socket: Socket,
    _stdin: std::process::ChildStdin,
    process: ProcessEvidence,
    pidfd: OwnedFd,
    claim: Uuid,
    acknowledged: bool,
    deadline: u64,
    last_sequence: u64,
    last_drain: u64,
}
impl Drop for Reader {
    fn drop(&mut self) {
        let _ = ownership::signal(&self.pidfd, libc::SIGKILL);
    }
}
pub(crate) struct Owner {
    listener: Socket,
    path: PathBuf,
    inode: (u64, u64),
    pending: Option<(Socket, u64)>,
    reader: Option<Reader>,
    claimed: bool,
    failed: bool,
}
impl Owner {
    pub(crate) fn new(dir: &Path) -> Result<Self> {
        let path = dir.join(SOCKET);
        let listener = Socket::new()?;
        let (address, length) = Socket::address(&path)?;
        ensure!(
            unsafe {
                libc::bind(
                    listener.raw(),
                    (&address as *const libc::sockaddr_un).cast(),
                    length,
                )
            } == 0,
            "reader socket collision"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        ensure!(
            unsafe { libc::listen(listener.raw(), 1) } == 0,
            "reader local listen"
        );
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            listener,
            path,
            inode: (metadata.dev(), metadata.ino()),
            pending: None,
            reader: None,
            claimed: false,
            failed: false,
        })
    }
    pub(crate) fn tick(
        &mut self,
        dir: &Path,
        config: &SealedLaunch,
        host: &HostObservation,
        wall_deadline: u64,
        guard: &terminal_io::DirectoryGuard,
        highwater: &mut u64,
    ) {
        if self.failed {
            return;
        }
        if self
            .try_tick(dir, config, host, wall_deadline, guard, highwater)
            .is_err()
        {
            self.reader.take();
            self.pending.take();
            self.failed = true;
        }
    }
    fn try_tick(
        &mut self,
        dir: &Path,
        config: &SealedLaunch,
        host: &HostObservation,
        wall_deadline: u64,
        guard: &terminal_io::DirectoryGuard,
        highwater: &mut u64,
    ) -> Result<()> {
        guard.verify()?;
        let metadata = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && (metadata.dev(), metadata.ino()) == self.inode,
            "reader socket replaced"
        );
        let now = ownership::raw_boot_ms();
        if let Some(reader) = &mut self.reader {
            ensure!(
                host.phase == HostPhase::Running
                    && now < reader.deadline
                    && now < host.permit_deadline_boot_ms
                    && now < wall_deadline
                    && ownership::pidfd_is_live(&reader.pidfd)?,
                "reader lease ended"
            );
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", reader.process.pid))?;
            let state = stat
                .rsplit_once(") ")
                .context("reader process state")?
                .1
                .as_bytes()[0];
            ensure!(state != b'T' && state != b't', "reader process stopped");
            if let Some(packet) = reader.socket.receive()? {
                ensure!(packet.fd.is_none(), "unexpected reader descriptor");
                match serde_json::from_slice::<Request>(&packet.bytes)? {
                    Request::Acknowledge { claim } => {
                        ensure!(
                            claim == reader.claim && !reader.acknowledged,
                            "reader acknowledgement conflict"
                        );
                        reader.acknowledged = true;
                    }
                    Request::Drained {
                        claim,
                        sequence,
                        boot_ms,
                    } => {
                        ensure!(
                            reader.acknowledged && claim == reader.claim,
                            "reader acknowledgement/claim"
                        );
                        reader.deadline = drain_deadline(
                            now,
                            reader.deadline,
                            (reader.last_sequence, reader.last_drain),
                            (sequence, boot_ms),
                            host.permit_deadline_boot_ms.min(wall_deadline),
                        )?;
                        reader.last_sequence = sequence;
                        reader.last_drain = boot_ms;
                    }
                    _ => bail!("unexpected reader command"),
                }
            }
            return Ok(());
        }
        if self.claimed || host.phase != HostPhase::Running {
            return Ok(());
        }
        if self.pending.is_none() {
            self.pending = self.listener.accept()?.map(|s| (s, now + LEASE_MS));
        }
        if let Some((socket, deadline)) = &self.pending {
            ensure!(now < *deadline, "reader registration deadline");
            if let Some(packet) = socket.receive()? {
                ensure!(packet.fd.is_none(), "unexpected registration descriptor");
                let Request::Register { binding, claim } =
                    serde_json::from_slice::<Request>(&packet.bytes)?
                else {
                    bail!("reader registration required")
                };
                terminal_io::verify_output_claim(dir, &binding, claim, host, guard, highwater)?;
                self.claimed = true;
                let now = ownership::raw_boot_ms();
                ensure!(
                    now < host.permit_deadline_boot_ms && now < wall_deadline,
                    "execution expired before reader spawn"
                );
                let socket = self.pending.take().unwrap().0;
                let mut child = Command::new(&config.tmux)
                    .args(["-N", "-C", "-S"])
                    .arg(dir.join("tmux.sock"))
                    .args([
                        "attach-session",
                        "-f",
                        "read-only,ignore-size,pause-after=1",
                        "-t",
                        "aq",
                    ])
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("TERM", "xterm-256color")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                // This child belongs to the guardian before any capability is sent.
                // On setup failure it is killed directly and reaped by the guardian.
                let setup = (|| -> Result<_> {
                    let process = ownership::process_evidence(child.id())?;
                    let pidfd = ownership::verified_pidfd(&process)?;
                    let stdin = child.stdin.take().context("reader stdin")?;
                    let stdout = child.stdout.take().context("reader stdout")?;
                    Ok((process, pidfd, stdin, stdout))
                })();
                let (process, pidfd, stdin, stdout) = match setup {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = child.kill();
                        return Err(e);
                    }
                };
                let deadline = now
                    .saturating_add(LEASE_MS)
                    .min(host.permit_deadline_boot_ms)
                    .min(wall_deadline);
                self.reader = Some(Reader {
                    socket,
                    _stdin: stdin,
                    process: process.clone(),
                    pidfd,
                    claim,
                    acknowledged: false,
                    deadline,
                    last_sequence: 0,
                    last_drain: now,
                });
                #[cfg(feature = "test-support")]
                durable_new(
                    &dir.join("reader.process.json"),
                    &serde_json::to_vec(&process)?,
                )?;
                self.reader.as_ref().unwrap().socket.send(
                    &Ready {
                        binding: *binding,
                        claim,
                        process,
                        deadline,
                    },
                    Some(stdout.as_raw_fd()),
                )?;
            }
        }
        Ok(())
    }
}
pub(crate) struct Client {
    socket: Socket,
    claim: Uuid,
    sequence: u64,
    last_sent: u64,
}
impl Client {
    pub(crate) fn open(
        terminal: &terminal_io::TerminalHandle,
        claim: Uuid,
    ) -> Result<(Self, File, ProcessEvidence)> {
        let (dir, _, _) = terminal.output_settings()?;
        let socket = Socket::connect(&dir.join(SOCKET))?;
        let peer = socket.credentials()?;
        ensure!(
            peer.pid as u32 == terminal.binding().guardian.process.pid
                && ownership::process_evidence(peer.pid as u32)?
                    == terminal.binding().guardian.process,
            "reader guardian identity"
        );
        socket.send(
            &Request::Register {
                binding: Box::new(terminal.binding().clone()),
                claim,
            },
            None,
        )?;
        test_pause(&dir, "before-ready")?;
        let deadline = Instant::now() + Duration::from_millis(500);
        let packet = loop {
            if let Some(packet) = socket.receive()? {
                break packet;
            }
            ensure!(Instant::now() < deadline, "reader ready timeout");
            std::thread::sleep(Duration::from_millis(2));
        };
        let ready: Ready = serde_json::from_slice(&packet.bytes)?;
        ensure!(
            ready.binding == *terminal.binding()
                && ready.claim == claim
                && ownership::raw_boot_ms() < ready.deadline,
            "reader ready identity/deadline"
        );
        let fd = packet.fd.context("reader output descriptor missing")?;
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        ensure!(
            flags >= 0
                && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                    == 0,
            "reader nonblocking descriptor"
        );
        test_pause(&dir, "before-ack")?;
        socket.send(&Request::Acknowledge { claim }, None)?;
        Ok((
            Self {
                socket,
                claim,
                sequence: 0,
                last_sent: 0,
            },
            File::from(fd),
            ready.process,
        ))
    }
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            self.socket.receive()?.is_none(),
            "unexpected reader response"
        );
        Ok(())
    }
    pub(crate) fn drained(&mut self) -> Result<()> {
        let now = ownership::raw_boot_ms();
        if now.saturating_sub(self.last_sent) < DRAIN_CADENCE_MS {
            return Ok(());
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("reader drain sequence overflow")?;
        self.socket.send(
            &Request::Drained {
                claim: self.claim,
                sequence: self.sequence,
                boot_ms: now,
            },
            None,
        )?;
        self.last_sent = now;
        Ok(())
    }
}
use std::os::unix::fs::FileTypeExt;

fn test_pause(_dir: &Path, _stage: &str) -> Result<()> {
    #[cfg(feature = "test-support")]
    if std::env::var("AQ_HOST_READER_PAUSE").ok().as_deref() == Some(_stage) {
        durable_new(&_dir.join("reader-owner-paused"), _stage.as_bytes())?;
        unsafe {
            libc::raise(libc::SIGSTOP);
        }
    }
    Ok(())
}

fn drain_deadline(
    now: u64,
    deadline: u64,
    last: (u64, u64),
    next: (u64, u64),
    ceiling: u64,
) -> Result<u64> {
    ensure!(
        now < deadline
            && next.0 > last.0
            && next.1 >= last.1
            && next.1 <= now
            && now < next.1.saturating_add(LEASE_MS),
        "reader drain receipt stale"
    );
    Ok(next.1.saturating_add(LEASE_MS).min(ceiling))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_old_drain_cannot_extend_from_dequeue_or_revive_expiry() {
        assert_eq!(
            drain_deadline(1100, 1500, (0, 500), (1, 500), 9000).unwrap(),
            1500
        );
        assert!(drain_deadline(1500, 1500, (0, 500), (1, 500), 9000).is_err());
        assert!(drain_deadline(1100, 1500, (1, 500), (1, 1000), 9000).is_err());
        assert!(drain_deadline(1100, 1500, (0, 500), (1, 1101), 9000).is_err());
        assert!(drain_deadline(1100, 1500, (0, 500), (1, 499), 9000).is_err());
        assert_eq!(
            drain_deadline(1100, 1500, (0, 500), (1, 1000), 1200).unwrap(),
            1200
        );
    }
    #[test]
    fn packet_fd_is_bounded_and_close_on_exec_and_extra_mutation_fields_refuse() {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let a = Socket(unsafe { OwnedFd::from_raw_fd(pair[0]) });
        let b = Socket(unsafe { OwnedFd::from_raw_fd(pair[1]) });
        let file = tempfile::tempfile().unwrap();
        a.send(&"bounded", Some(file.as_raw_fd())).unwrap();
        let packet = b.receive().unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<String>(&packet.bytes).unwrap(),
            "bounded"
        );
        let fd = packet.fd.unwrap();
        assert!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC != 0);
        assert_eq!(
            File::from(fd).metadata().unwrap().ino(),
            file.metadata().unwrap().ino()
        );
        assert!(
            serde_json::from_str::<Request>(
                r#"{"kind":"Acknowledge","claim":"00000000-0000-0000-0000-000000000001","pid":1}"#
            )
            .is_err()
        );
        assert!(a.send(&"x".repeat(MAX_PACKET), None).is_err());
    }
}
