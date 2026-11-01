#define _GNU_SOURCE
/* Separate IPC policy feasibility canary. Not a complete harness capsule. */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/capability.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>
#define REQUIRE(x) do { if (!(x)) { fprintf(stderr, "line %d: %s: %s\n", __LINE__, #x, strerror(errno)); _exit(111); } } while (0)
#define DENY (SECCOMP_RET_ERRNO | EPERM)
#define REJECT_SYSCALL(n) BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (n), 0, 1), BPF_STMT(BPF_RET | BPF_K, DENY)
static void policy(void) {
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JGE | BPF_K, 0x40000000, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        REJECT_SYSCALL(__NR_io_uring_setup),
        REJECT_SYSCALL(__NR_io_uring_enter),
        REJECT_SYSCALL(__NR_io_uring_register),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_socket, 0, 5),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AF_INET, 2, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AF_INET6, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, DENY),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_socketpair, 0, 12),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AF_UNIX, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, DENY),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[2])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, DENY),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[1])),
        BPF_STMT(BPF_ALU | BPF_AND | BPF_K, ~(SOCK_CLOEXEC | SOCK_NONBLOCK)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SOCK_STREAM, 2, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SOCK_SEQPACKET, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, DENY),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog program = {sizeof(filter) / sizeof(filter[0]), filter};
    REQUIRE(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0);
    REQUIRE(syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program) == 0);
}
static struct sockaddr_un address(const char *base, const char *name) {
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    REQUIRE(snprintf(a.sun_path, sizeof(a.sun_path), "%s/%s", base, name) < (int)sizeof(a.sun_path)); return a;
}
static void write_text(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CLOEXEC); REQUIRE(fd >= 0); REQUIRE(write(fd, text, strlen(text)) == (ssize_t)strlen(text)); REQUIRE(close(fd) == 0);
}
static int unix_listener(const char *base, const char *name, int type) {
    int fd = socket(AF_UNIX, type | SOCK_CLOEXEC | SOCK_NONBLOCK, 0); REQUIRE(fd >= 0);
    struct sockaddr_un a = address(base, name); REQUIRE(bind(fd, (void *)&a, sizeof(a)) == 0);
    if (type != SOCK_DGRAM) { REQUIRE(listen(fd, 4) == 0); }
    return fd;
}
static void pair_cannot_retarget(const char *base, const char *name, int type, int keep[2]) {
    REQUIRE(socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, keep) == 0);
    struct sockaddr_un target = address(base, name), unspec = {.sun_family = AF_UNSPEC};
    REQUIRE(connect(keep[0], (void *)&target, sizeof(target)) < 0);
    (void)connect(keep[0], (void *)&unspec, sizeof(unspec));
    REQUIRE(connect(keep[0], (void *)&target, sizeof(target)) < 0);
    (void)connect(keep[0], (void *)&unspec, sizeof(unspec.sun_family));
    REQUIRE(connect(keep[0], (void *)&target, sizeof(target)) < 0);
    char marker = 'x', readback;
    ssize_t sent = sendto(keep[0], &marker, 1, MSG_NOSIGNAL | MSG_DONTWAIT, (void *)&target, sizeof(target));
    if (sent >= 0) REQUIRE(sent == 1 && read(keep[1], &readback, 1) == 1 && readback == marker);
    struct iovec io = {&marker, 1}; struct msghdr msg = {.msg_name = &target, .msg_namelen = sizeof(target), .msg_iov = &io, .msg_iovlen = 1};
    sent = sendmsg(keep[0], &msg, MSG_NOSIGNAL | MSG_DONTWAIT);
    if (sent >= 0) REQUIRE(sent == 1 && read(keep[1], &readback, 1) == 1 && readback == marker);
    struct sockaddr_un own = address(base, type == SOCK_STREAM ? "bound-stream.sock" : "bound-seqpacket.sock");
    if (bind(keep[0], (void *)&own, sizeof(own)) == 0) {
        /* listen can return0 on a connected pair without making it a listener. */
        (void)listen(keep[0], 1); int accepting = -1; socklen_t length = sizeof(accepting);
        REQUIRE(getsockopt(keep[0], SOL_SOCKET, SO_ACCEPTCONN, &accepting, &length) == 0 && !accepting);
        REQUIRE(accept4(keep[0], NULL, NULL, SOCK_CLOEXEC | SOCK_NONBLOCK) < 0 && errno == EINVAL);
    }
    int closed[2]; REQUIRE(socketpair(AF_UNIX, type, 0, closed) == 0); REQUIRE(close(closed[1]) == 0);
    (void)shutdown(closed[0], SHUT_RDWR);
    (void)connect(closed[0], (void *)&unspec, sizeof(unspec));
    REQUIRE(connect(closed[0], (void *)&target, sizeof(target)) < 0); REQUIRE(close(closed[0]) == 0);
}
static void trusted_setup(int control, const char *base, const char *self, int shm) {
    int uid = getuid(), gid = getgid(); REQUIRE(unshare(CLONE_NEWUSER) == 0); char map[80];
    write_text("/proc/self/setgroups", "deny\n"); snprintf(map, sizeof(map), "%d %d 1\n", uid, uid); write_text("/proc/self/uid_map", map);
    snprintf(map, sizeof(map), "%d %d 1\n", gid, gid); write_text("/proc/self/gid_map", map);
    REQUIRE(unshare(CLONE_NEWNET | CLONE_NEWIPC) == 0);
    REQUIRE(shmat(shm, NULL, SHM_RDONLY) == (void *)-1);
    /* Deliberately demonstrate the unfiltered pathname-socket escape. */
    struct sockaddr_un a = address(base, "outer-stream.sock"); int socketfd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0); REQUIRE(socketfd >= 0);
    REQUIRE(connect(socketfd, (void *)&a, sizeof(a)) == 0 && close(socketfd) == 0);
    int pair[2]; REQUIRE(socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair) == 0); a = address(base, "outer-dgram.sock");
    REQUIRE(connect(pair[0], (void *)&a, sizeof(a)) == 0 && write(pair[0], "retarget-gap", 12) == 12); close(pair[0]); close(pair[1]);
    int net = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0); REQUIRE(net >= 0); struct ifreq lo = {0}; strcpy(lo.ifr_name, "lo");
    REQUIRE(ioctl(net, SIOCGIFFLAGS, &lo) == 0); lo.ifr_flags |= IFF_UP; REQUIRE(ioctl(net, SIOCSIFFLAGS, &lo) == 0 && close(net) == 0);
    int listener = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0); REQUIRE(listener >= 0);
    struct sockaddr_in tcp = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    REQUIRE(bind(listener, (void *)&tcp, sizeof(tcp)) == 0 && listen(listener, 1) == 0); socklen_t length = sizeof(tcp); REQUIRE(getsockname(listener, (void *)&tcp, &length) == 0);
    char ancillary[CMSG_SPACE(sizeof(int))] = {0}; int port = ntohs(tcp.sin_port); struct iovec io = {&port, sizeof(port)};
    struct msghdr msg = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = ancillary, .msg_controllen = sizeof(ancillary)};
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int)); memcpy(CMSG_DATA(c), &listener, sizeof(listener));
    REQUIRE(sendmsg(control, &msg, MSG_NOSIGNAL) == sizeof(port)); REQUIRE(close(listener) == 0);
    REQUIRE(syscall(SYS_close_range, 3u, (unsigned)control - 1, 0) == 0);
    REQUIRE(syscall(SYS_close_range, (unsigned)control + 1, ~0u, 0) == 0);
    REQUIRE(close(control) == 0);
    for (int i = 0; i <= CAP_LAST_CAP; i++) REQUIRE(prctl(PR_CAPBSET_DROP, i, 0, 0, 0) == 0);
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0}; struct __user_cap_data_struct caps[2] = {{0}}; REQUIRE(syscall(SYS_capset, &header, caps) == 0);
    policy(); char p[20], fd[20]; snprintf(p, sizeof(p), "%d", port); snprintf(fd, sizeof(fd), "%d", control);
    char *argv[] = {(char *)self, "--untrusted-canary", (char *)base, p, fd, NULL}; char *env[] = {"PATH=/usr/bin:/bin", NULL}; execve(self, argv, env); REQUIRE(0);
}
int main(int argc, char **argv) {
    if (argc == 5 && !strcmp(argv[1], "--untrusted-canary")) {
        REQUIRE(fcntl(atoi(argv[4]), F_GETFD) < 0 && errno == EBADF);
        REQUIRE(socket(AF_UNIX, SOCK_STREAM, 0) < 0 && errno == EPERM);
        REQUIRE(socket(AF_NETLINK, SOCK_RAW, 0) < 0 && errno == EPERM);
        int forbidden[2]; REQUIRE(socketpair(AF_UNIX, SOCK_DGRAM, 0, forbidden) < 0 && errno == EPERM);
        REQUIRE(syscall(SYS_io_uring_setup, 1, NULL) < 0 && errno == EPERM);
        pid_t descendant = fork(); REQUIRE(descendant >= 0);
        if (!descendant) { REQUIRE(socket(AF_UNIX, SOCK_STREAM, 0) < 0 && errno == EPERM); REQUIRE(socketpair(AF_UNIX, SOCK_DGRAM, 0, forbidden) < 0 && errno == EPERM); _exit(0); }
        int inherited; REQUIRE(waitpid(descendant, &inherited, 0) == descendant && WIFEXITED(inherited) && WEXITSTATUS(inherited) == 0);
        int stream[2], seqpacket[2]; pair_cannot_retarget(argv[2], "outer-stream.sock", SOCK_STREAM, stream); pair_cannot_retarget(argv[2], "outer-seqpacket.sock", SOCK_SEQPACKET, seqpacket);
        int tcp = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0); REQUIRE(tcp >= 0);
        struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK), .sin_port = htons(atoi(argv[3]))};
        REQUIRE(connect(tcp, (void *)&a, sizeof(a)) == 0 && write(tcp, "ready", 5) == 5); char reply; REQUIRE(read(tcp, &reply, 1) == 1 && reply == 'x');
        puts("PASS inherited seccomp after clean exec denies new Unix/other socket families, datagram pairs and io_uring; anonymous stream/seqpacket pairs cannot retarget");
        puts("PASS setup control FD closed before canary exec; trusted setup's transferred child-netns listener still serves TCP"); return 0;
    }
    REQUIRE(argc == 2 && getuid() != 0); alarm(10); const char *base = argv[1];
    int stream = unix_listener(base, "outer-stream.sock", SOCK_STREAM), seqpacket = unix_listener(base, "outer-seqpacket.sock", SOCK_SEQPACKET), datagram = unix_listener(base, "outer-dgram.sock", SOCK_DGRAM);
    int shm = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600); REQUIRE(shm >= 0);
    /* Mark for deletion while retained by the parent; failure cannot leak a segment. */
    void *original_mapping = shmat(shm, NULL, 0); REQUIRE(original_mapping != (void *)-1);
    REQUIRE(shmctl(shm, IPC_RMID, NULL) == 0);
    int control[2]; REQUIRE(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, control) == 0); int on = 1; REQUIRE(setsockopt(control[0], SOL_SOCKET, SO_PASSCRED, &on, sizeof(on)) == 0);
    char self[4096]; ssize_t n = readlink("/proc/self/exe", self, sizeof(self)-1); REQUIRE(n > 0); self[n] = 0;
    int parent = getpid(); pid_t child = fork(); REQUIRE(child >= 0);
    if (!child) { REQUIRE(prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0) == 0 && getppid() == parent); close(control[0]); close(stream); close(seqpacket); close(datagram); trusted_setup(control[1], base, self, shm); _exit(112); }
    close(control[1]); char ancillary[CMSG_SPACE(sizeof(int)) + CMSG_SPACE(sizeof(struct ucred))] = {0}; int port;
    struct iovec io = {&port, sizeof(port)}; struct msghdr msg = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = ancillary, .msg_controllen = sizeof(ancillary)};
    REQUIRE(recvmsg(control[0], &msg, MSG_CMSG_CLOEXEC) == sizeof(port) && !(msg.msg_flags & (MSG_TRUNC | MSG_CTRUNC)));
    int listener = -1, credential = 0;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
        REQUIRE(c->cmsg_level == SOL_SOCKET);
        if (c->cmsg_type == SCM_RIGHTS) { REQUIRE(listener == -1 && c->cmsg_len == CMSG_LEN(sizeof(int))); memcpy(&listener, CMSG_DATA(c), sizeof(int)); }
        else { REQUIRE(c->cmsg_type == SCM_CREDENTIALS && !credential && c->cmsg_len == CMSG_LEN(sizeof(struct ucred))); struct ucred value; memcpy(&value, CMSG_DATA(c), sizeof(value)); REQUIRE(value.pid == child && value.uid == getuid() && value.gid == getgid()); credential = 1; }
    }
    REQUIRE(listener >= 0 && credential); int type, accepting; socklen_t length = sizeof(type);
    REQUIRE(getsockopt(listener, SOL_SOCKET, SO_TYPE, &type, &length) == 0 && type == SOCK_STREAM); length = sizeof(accepting); REQUIRE(getsockopt(listener, SOL_SOCKET, SO_ACCEPTCONN, &accepting, &length) == 0 && accepting);
    struct sockaddr_in local; length = sizeof(local); REQUIRE(getsockname(listener, (void *)&local, &length) == 0 && local.sin_family == AF_INET && local.sin_addr.s_addr == htonl(INADDR_LOOPBACK) && ntohs(local.sin_port) == port);
    int tcp = accept4(listener, NULL, NULL, SOCK_CLOEXEC); REQUIRE(tcp >= 0); char ready[5]; REQUIRE(read(tcp, ready, 5) == 5 && !memcmp(ready, "ready", 5));
    int baseline = accept4(stream, NULL, NULL, SOCK_CLOEXEC); REQUIRE(baseline >= 0); close(baseline);
    REQUIRE(accept4(stream, NULL, NULL, SOCK_CLOEXEC) < 0 && errno == EAGAIN);
    REQUIRE(accept4(seqpacket, NULL, NULL, SOCK_CLOEXEC) < 0 && errno == EAGAIN);
    char gap[32]; REQUIRE(recv(datagram, gap, sizeof(gap), 0) == 12 && !memcmp(gap, "retarget-gap", 12));
    const char *bound[] = {"bound-stream.sock", "bound-seqpacket.sock"};
    for (int i = 0; i < 2; i++) { int s = socket(AF_UNIX, i ? SOCK_SEQPACKET : SOCK_STREAM, 0); REQUIRE(s >= 0); struct sockaddr_un a = address(base, bound[i]); REQUIRE(connect(s, (void *)&a, sizeof(a)) < 0 && (errno == ECONNREFUSED || errno == ENOENT)); close(s); }
    REQUIRE(write(tcp, "x", 1) == 1); close(tcp); close(listener); close(control[0]);
    int status; REQUIRE(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0); REQUIRE(shmdt(original_mapping) == 0);
    puts("PASS negative control: netns alone reaches outer pathname Unix stream socket; datagram socketpair retarget reaches outer service");
    puts("PASS private IPC namespace denies original System V shared memory ID; receiver verifies trusted peer and transferred listener family/type/accepting/origin"); return 0;
}
