//! Build the capsule IPC policy. Instruction order and relative jumps are part of the kernel ABI.
fn statement(code: u32, value: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k: value,
    }
}
fn jump(code: u32, value: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k: value,
    }
}

pub(super) fn program() -> Vec<libc::sock_filter> {
    let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
    let equal = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
    let ret = libc::BPF_RET | libc::BPF_K;
    let deny = 0x00050000 | libc::EPERM as u32;
    let mut code = vec![
        statement(load, 4),
        jump(equal, 0xc000003e, 1, 0),
        statement(ret, 0x80000000),
        statement(load, 0),
        jump(
            libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K,
            0x40000000,
            0,
            1,
        ),
        statement(ret, 0x80000000),
    ];
    for syscall in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        code.extend([jump(equal, syscall as u32, 0, 1), statement(ret, deny)]);
    }
    code.extend(socket_rules());
    code
}

fn socket_rules() -> Vec<libc::sock_filter> {
    let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
    let equal = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
    let ret = libc::BPF_RET | libc::BPF_K;
    let deny = 0x00050000 | libc::EPERM as u32;
    let allow = 0x7fff0000;
    let argument = |i: u32| 16 + i * 8;
    vec![
        jump(equal, libc::SYS_socket as u32, 0, 19),
        statement(load, argument(0)),
        jump(equal, libc::AF_INET as u32, 2, 0),
        jump(equal, libc::AF_INET6 as u32, 1, 0),
        statement(ret, deny),
        statement(load, argument(1)),
        statement(
            libc::BPF_ALU | libc::BPF_AND | libc::BPF_K,
            !(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u32,
        ),
        jump(equal, libc::SOCK_STREAM as u32, 0, 5),
        statement(load, argument(2)),
        jump(equal, 0, 2, 0),
        jump(equal, libc::IPPROTO_TCP as u32, 1, 0),
        statement(ret, deny),
        statement(ret, allow),
        jump(equal, libc::SOCK_DGRAM as u32, 1, 0),
        statement(ret, deny),
        statement(load, argument(2)),
        jump(equal, 0, 2, 0),
        jump(equal, libc::IPPROTO_UDP as u32, 1, 0),
        statement(ret, deny),
        statement(ret, allow),
        jump(equal, libc::SYS_socketpair as u32, 0, 12),
        statement(load, argument(0)),
        jump(equal, libc::AF_UNIX as u32, 1, 0),
        statement(ret, deny),
        statement(load, argument(2)),
        jump(equal, 0, 1, 0),
        statement(ret, deny),
        statement(load, argument(1)),
        statement(
            libc::BPF_ALU | libc::BPF_AND | libc::BPF_K,
            !(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u32,
        ),
        jump(equal, libc::SOCK_STREAM as u32, 2, 0),
        jump(equal, libc::SOCK_SEQPACKET as u32, 1, 0),
        statement(ret, deny),
        statement(ret, allow),
        statement(ret, allow),
    ]
}
