use super::*;
use serde::de::DeserializeOwned;
use std::mem::{size_of, size_of_val};

const MAX_PACKET: usize = 32768;
pub(super) struct Socket(pub OwnedFd);
pub(super) struct Packet<T> {
    pub value: T,
    pub fd: Option<OwnedFd>,
    pub credentials: libc::ucred,
}
impl Socket {
    pub fn pair() -> Result<(Self, Self)> {
        let mut fds = [-1; 2];
        ensure!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    fds.as_mut_ptr(),
                )
            } == 0,
            "capsule socketpair"
        );
        let a = Self(unsafe { OwnedFd::from_raw_fd(fds[0]) });
        let b = Self(unsafe { OwnedFd::from_raw_fd(fds[1]) });
        for socket in [&a, &b] {
            let on = 1i32;
            ensure!(
                unsafe {
                    libc::setsockopt(
                        socket.raw(),
                        libc::SOL_SOCKET,
                        libc::SO_PASSCRED,
                        (&on as *const i32).cast(),
                        size_of::<i32>() as _,
                    )
                } == 0,
                "capsule passcred"
            );
        }
        Ok((a, b))
    }
    pub fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }
    pub fn poll(&self, events: i16, until: u64) -> Result<()> {
        let now = ownership::raw_boot_ms();
        ensure!(now < until, "capsule original deadline elapsed");
        let mut poll = libc::pollfd {
            fd: self.raw(),
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, (until - now).min(10000) as i32) };
        ensure!(result > 0, "capsule packet timeout or poll error");
        Ok(())
    }
    pub fn send<T: Serialize>(&self, value: &T, fd: Option<RawFd>, until: u64) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_PACKET,
            "capsule packet size"
        );
        self.poll(libc::POLLOUT, until)?;
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 16];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        if let Some(fd) = fd {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = unsafe { libc::CMSG_SPACE(size_of_val(&fd) as _) } as _;
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&message);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(size_of_val(&fd) as _) as _;
                std::ptr::copy_nonoverlapping(
                    (&fd as *const RawFd).cast::<u8>(),
                    libc::CMSG_DATA(c),
                    size_of_val(&fd),
                );
            }
        }
        ensure!(
            unsafe {
                libc::sendmsg(
                    self.raw(),
                    &message,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            } == bytes.len() as isize,
            "capsule packet send"
        );
        Ok(())
    }
    pub fn recv<T: DeserializeOwned>(&self, until: u64) -> Result<Packet<T>> {
        self.poll(libc::POLLIN, until)?;
        let mut bytes = vec![0u8; MAX_PACKET];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 32];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = size_of_val(&control);
        let count = unsafe {
            libc::recvmsg(
                self.raw(),
                &mut message,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        // SAFETY: recvmsg populated the aligned control buffer above. This call
        // takes ownership of every received descriptor before validating the packet.
        let mut ancillary = unsafe { Ancillary::receive(&message) };
        ensure!(
            count > 0
                && message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0
                && !ancillary.invalid
                && ancillary.fds.len() <= 1,
            "capsule closed/malformed packet"
        );
        let credentials = ancillary
            .credentials
            .context("capsule packet lacks post-send credentials")?;
        // An ancestor sender is deliberately invisible as PID0 inside a private
        // PID namespace. Only the trusted pre-exec Release stage permits that.
        ensure!(
            credentials.pid >= 0
                && credentials.uid == unsafe { libc::geteuid() }
                && credentials.gid == unsafe { libc::getegid() },
            "capsule sender credentials"
        );
        bytes.truncate(count as usize);
        Ok(Packet {
            value: serde_json::from_slice(&bytes)?,
            fd: ancillary.fds.pop(),
            credentials,
        })
    }
}

#[derive(Default)]
struct Ancillary {
    fds: Vec<OwnedFd>,
    credentials: Option<libc::ucred>,
    invalid: bool,
}

impl Ancillary {
    /// The message must own an aligned, accessible recvmsg control buffer.
    /// Each SCM_RIGHTS descriptor must be consumed exactly once.
    unsafe fn receive(message: &libc::msghdr) -> Self {
        let mut data = Self::default();
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(message);
            while let Some(control) = header.as_ref() {
                if control.cmsg_level == libc::SOL_SOCKET {
                    data.read_socket_control(control);
                } else {
                    data.invalid = true;
                }
                header = libc::CMSG_NXTHDR(message, header);
            }
        }
        data
    }

    unsafe fn read_socket_control(&mut self, header: &libc::cmsghdr) {
        unsafe {
            match header.cmsg_type {
                libc::SCM_RIGHTS if header.cmsg_len >= libc::CMSG_LEN(0) as usize => {
                    self.take_rights(header);
                }
                libc::SCM_CREDENTIALS
                    if header.cmsg_len
                        == libc::CMSG_LEN(size_of::<libc::ucred>() as _) as usize
                        && self.credentials.is_none() =>
                {
                    self.credentials = Some(std::ptr::read_unaligned(
                        libc::CMSG_DATA(header).cast::<libc::ucred>(),
                    ));
                }
                _ => self.invalid = true,
            }
        }
    }

    unsafe fn take_rights(&mut self, header: &libc::cmsghdr) {
        unsafe {
            let bytes = header.cmsg_len - libc::CMSG_LEN(0) as usize;
            self.invalid |= bytes != size_of::<RawFd>();
            for index in 0..bytes / size_of::<RawFd>() {
                let value = libc::CMSG_DATA(header)
                    .add(index * size_of::<RawFd>())
                    .cast::<RawFd>();
                let fd = std::ptr::read_unaligned(value);
                if fd >= 0 {
                    self.fds.push(OwnedFd::from_raw_fd(fd));
                } else {
                    self.invalid = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfers_optional_file_and_reports_the_sending_process() {
        let (tx, rx) = Socket::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        let until = ownership::raw_boot_ms() + 2_000;
        for with_file in [false, true] {
            tx.send(&43u64, with_file.then_some(file.as_raw_fd()), until)
                .unwrap();
            let packet: Packet<u64> = rx.recv(until).unwrap();
            assert_eq!(
                (packet.value, packet.credentials.pid),
                (43, std::process::id() as i32)
            );
            assert_eq!(packet.fd.is_some(), with_file);
            if let Some(fd) = packet.fd {
                let received = File::from(fd).metadata().unwrap();
                let expected = file.metadata().unwrap();
                assert_eq!(
                    (received.dev(), received.ino()),
                    (expected.dev(), expected.ino())
                );
            }
        }
    }
}
