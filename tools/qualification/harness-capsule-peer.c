#define _GNU_SOURCE
/* Kernel-only canary. No harness, credentials, provider or runtime admission. */
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <net/if.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define REQUIRE(x) do { if (!(x)) { fprintf(stderr, "line %d: %s: %s\n", __LINE__, #x, strerror(errno)); _exit(111); } } while (0)
struct packet { char kind; int inner_pid; int port; };
static _Atomic pid_t other_tid;
static void *thread_canary(void *unused) { (void)unused; other_tid = syscall(SYS_gettid); for (;;) pause(); return NULL; }
static void write_text(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CLOEXEC); REQUIRE(fd >= 0);
    REQUIRE(write(fd, text, strlen(text)) == (ssize_t)strlen(text)); REQUIRE(close(fd) == 0);
}
static void send_packet(int sock, char kind, int port, int fd) {
    struct packet packet = {kind, getpid(), port};
    char ancillary[CMSG_SPACE(sizeof(int))] = {0};
    struct iovec io = {&packet, sizeof(packet)};
    struct msghdr msg = {.msg_iov = &io, .msg_iovlen = 1};
    if (fd >= 0) {
        msg.msg_control = ancillary; msg.msg_controllen = sizeof(ancillary);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &fd, sizeof(fd));
    }
    REQUIRE(sendmsg(sock, &msg, MSG_NOSIGNAL) == sizeof(packet));
}
static pid_t recv_packet(int sock, char expected, struct packet *packet, int *fd) {
    char ancillary[CMSG_SPACE(sizeof(struct ucred)) + CMSG_SPACE(sizeof(int))] = {0};
    struct iovec io = {packet, sizeof(*packet)};
    struct msghdr msg = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = ancillary, .msg_controllen = sizeof(ancillary)};
    REQUIRE(recvmsg(sock, &msg, MSG_CMSG_CLOEXEC) == sizeof(*packet));
    REQUIRE(!(msg.msg_flags & (MSG_TRUNC | MSG_CTRUNC)) && packet->kind == expected);
    pid_t pid = -1; *fd = -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
        REQUIRE(c->cmsg_level == SOL_SOCKET);
        if (c->cmsg_type == SCM_CREDENTIALS) {
            REQUIRE(c->cmsg_len == CMSG_LEN(sizeof(struct ucred)) && pid == -1);
            struct ucred cred; memcpy(&cred, CMSG_DATA(c), sizeof(cred));
            REQUIRE(cred.uid == getuid() && cred.gid == getgid()); pid = cred.pid;
        } else {
            REQUIRE(c->cmsg_type == SCM_RIGHTS && c->cmsg_len == CMSG_LEN(sizeof(int)) && *fd == -1);
            memcpy(fd, CMSG_DATA(c), sizeof(int));
        }
    }
    REQUIRE(pid > 0); return pid;
}
static void no_read(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    REQUIRE(fd < 0 && (errno == ENOENT || errno == EACCES || errno == EPERM));
}
static void exact_file(const char *path, const char *expected) {
    char bytes[128] = {0}; int fd = open(path, O_RDONLY | O_CLOEXEC); REQUIRE(fd >= 0);
    ssize_t n = read(fd, bytes, sizeof(bytes)); REQUIRE(n == (ssize_t)strlen(expected));
    REQUIRE(!memcmp(bytes, expected, n)); REQUIRE(close(fd) == 0);
}
static void drop_privileges(void) {
    REQUIRE(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0);
    for (int i = 0; i <= CAP_LAST_CAP; i++) REQUIRE(prctl(PR_CAPBSET_DROP, i, 0, 0, 0) == 0);
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct caps[2] = {{0}};
    REQUIRE(syscall(SYS_capset, &header, caps) == 0);
}
static void close_except(int keep) {
    if (keep > 3) REQUIRE(syscall(SYS_close_range, 3u, (unsigned)keep - 1, 0) == 0);
    REQUIRE(syscall(SYS_close_range, (unsigned)keep + 1, ~0u, 0) == 0);
}
static int harness(int argc, char **argv) {
    REQUIRE(argc == 10); int control = atoi(argv[2]);
    pid_t parent = atoi(argv[3]), tid = atoi(argv[4]); int sourcefd = atoi(argv[5]);
    const char *source = argv[6]; unsigned long long dev = strtoull(argv[7], NULL, 10), ino = strtoull(argv[8], NULL, 10);
    REQUIRE(getuid() == (uid_t)atoi(argv[9]) && getpid() != 1);
    REQUIRE(getenv("AQ_CAPSULE_PARENT_CANARY") == NULL && getenv("AQ_SESSION_DIRECTORY") == NULL);
    no_read(source); no_read("/var/lib/aq-node/private-material"); no_read("/home/operator/.profile");
    char path[4096];
    pid_t ids[] = {parent, tid};
    for (unsigned i = 0; i < sizeof(ids)/sizeof(ids[0]); i++) {
        snprintf(path, sizeof(path), "/proc/%d/fd/%d", ids[i], sourcefd); no_read(path);
        snprintf(path, sizeof(path), "/proc/%d/root%s", ids[i], source); no_read(path);
        snprintf(path, sizeof(path), "/proc/%d/task/%d/fd/%d", parent, ids[i], sourcefd); no_read(path);
    }
    snprintf(path, sizeof(path), "/proc/self/root%s", source); no_read(path);
    snprintf(path, sizeof(path), "/proc/thread-self/root%s", source); no_read(path);
    snprintf(path, sizeof(path), "/proc/1/root%s", source); no_read(path);
    DIR *processes = opendir("/proc"); REQUIRE(processes != NULL); struct dirent *process;
    while ((process = readdir(processes))) {
        char *end; long pid = strtol(process->d_name, &end, 10);
        if (end != process->d_name && !*end) REQUIRE(pid == 1 || pid == getpid());
    }
    REQUIRE(closedir(processes) == 0);
    DIR *fds = opendir("/proc/self/fd"); REQUIRE(fds != NULL); struct dirent *entry;
    while ((entry = readdir(fds))) {
        if (entry->d_name[0] == '.') continue;
        struct stat st; snprintf(path, sizeof(path), "/proc/self/fd/%s", entry->d_name);
        REQUIRE(stat(path, &st) == 0); REQUIRE((unsigned long long)st.st_dev != dev || (unsigned long long)st.st_ino != ino);
    }
    REQUIRE(closedir(fds) == 0);
    exact_file("/auth/auth.json", "derived-capability-canary\n");
    REQUIRE(open("/auth/auth.json", O_WRONLY | O_CLOEXEC) < 0 && errno == EROFS);
    exact_file("/allowed/item", "allowed-folder-canary\n");
    exact_file("/git-metadata/item", "internal-git-canary\n");
    int out = open("/workspace/result", O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0600); REQUIRE(out >= 0); REQUIRE(close(out) == 0);
    REQUIRE(mount(NULL, "/", NULL, MS_REMOUNT, NULL) < 0 && errno == EPERM);
    FILE *route = fopen("/proc/net/route", "r"); REQUIRE(route != NULL); char route_line[256];
    REQUIRE(fgets(route_line, sizeof(route_line), route) != NULL);
    /* A new network namespace has no default route, not merely a CLI endpoint flag. */
    while (fgets(route_line, sizeof(route_line), route)) {
        char iface[32]; unsigned destination; REQUIRE(sscanf(route_line, "%31s %x", iface, &destination) == 2 && destination != 0);
    }
    REQUIRE(fclose(route) == 0);
    /* The sending task is already in its final namespaces. No claimed PID field is trusted. */
    send_packet(control, 'H', 0, -1);
    int ready[2]; REQUIRE(pipe2(ready, O_CLOEXEC) == 0);
    pid_t descendant = fork(); REQUIRE(descendant >= 0);
    if (!descendant) { close(ready[0]); send_packet(control, 'D', 0, -1); REQUIRE(write(ready[1], "x", 1) == 1); close(ready[1]); for (;;) pause(); }
    REQUIRE(close(ready[1]) == 0); char ack; REQUIRE(read(ready[0], &ack, 1) == 1 && close(ready[0]) == 0);
    int listener = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0); REQUIRE(listener >= 0);
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    REQUIRE(bind(listener, (void *)&address, sizeof(address)) == 0 && listen(listener, 1) == 0);
    socklen_t length = sizeof(address); REQUIRE(getsockname(listener, (void *)&address, &length) == 0);
    send_packet(control, 'L', ntohs(address.sin_port), listener); REQUIRE(close(listener) == 0);
    int connection = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0); REQUIRE(connection >= 0);
    REQUIRE(connect(connection, (void *)&address, sizeof(address)) == 0);
    REQUIRE(write(connection, "capsule-request", 15) == 15);
    char reply[32]; REQUIRE(read(connection, reply, sizeof(reply)) == 18 && !memcmp(reply, "parent-proxy-reply", 18));
    REQUIRE(close(connection) == 0); send_packet(control, 'P', 0, -1);
    for (;;) pause();
}
static void directory(const char *path) { REQUIRE(mkdir(path, 0700) == 0); }
static void bind_path(const char *root, const char *name, const char *source, int readonly, int file) {
    char dest[4096]; REQUIRE(snprintf(dest, sizeof(dest), "%s/%s", root, name) < (int)sizeof(dest));
    if (file) { int fd = open(dest, O_CREAT | O_EXCL | O_RDONLY | O_CLOEXEC, 0500); REQUIRE(fd >= 0); REQUIRE(close(fd) == 0); }
    else directory(dest);
    REQUIRE(mount(source, dest, NULL, MS_BIND, NULL) == 0);
    if (readonly) REQUIRE(mount(NULL, dest, NULL, MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV, NULL) == 0);
}
static void capsule(int sock, const char *base, const char *self, int parent, int tid, int sourcefd, unsigned long long dev, unsigned long long ino, int uid, int gid) {
    REQUIRE(unshare(CLONE_NEWUSER) == 0); char map[100];
    write_text("/proc/self/setgroups", "deny\n");
    snprintf(map, sizeof(map), "%d %d 1\n", uid, uid); write_text("/proc/self/uid_map", map);
    snprintf(map, sizeof(map), "%d %d 1\n", gid, gid); write_text("/proc/self/gid_map", map);
    REQUIRE(unshare(CLONE_NEWNS | CLONE_NEWNET | CLONE_NEWPID | CLONE_NEWUTS) == 0);
    REQUIRE(mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) == 0);
    int bootstrapfd = syscall(SYS_pidfd_open, getpid(), 0); REQUIRE(bootstrapfd >= 0);
    pid_t init = fork(); REQUIRE(init >= 0);
    if (init) { close(bootstrapfd); close(sock); int status; REQUIRE(waitpid(init, &status, 0) == init); _exit(WIFSIGNALED(status) ? 0 : 112); }
    REQUIRE(prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0) == 0);
    struct pollfd bootstrap_live = {bootstrapfd, POLLIN, 0}; REQUIRE(poll(&bootstrap_live, 1, 0) == 0); REQUIRE(close(bootstrapfd) == 0);
    REQUIRE(getpid() == 1); send_packet(sock, 'I', 0, -1);
    char root[4096], source[4096], dest[4096];
    snprintf(root, sizeof(root), "%s/root", base); REQUIRE(mount("tmpfs", root, "tmpfs", MS_NOSUID | MS_NODEV, "size=8m,mode=0700") == 0);
    const char *dirs[] = {"proc", "images", "tmp"};
    for (unsigned i = 0; i < sizeof(dirs)/sizeof(dirs[0]); i++) { REQUIRE(snprintf(dest, sizeof(dest), "%s/%s", root, dirs[i]) < (int)sizeof(dest)); directory(dest); }
    bind_path(root, "images/peer", self, 1, 1);
    const char *names[] = {"workspace", "auth", "allowed", "git-metadata"};
    for (unsigned i = 0; i < sizeof(names)/sizeof(names[0]); i++) { snprintf(source, sizeof(source), "%s/%s", base, names[i]); bind_path(root, names[i], source, i != 0, 0); }
    REQUIRE(chdir(root) == 0 && chroot(".") == 0 && chdir("/") == 0);
    REQUIRE(mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) == 0);
    int net = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0); REQUIRE(net >= 0);
    struct ifreq interface = {0}; strcpy(interface.ifr_name, "lo");
    REQUIRE(ioctl(net, SIOCGIFFLAGS, &interface) == 0); interface.ifr_flags |= IFF_UP;
    REQUIRE(ioctl(net, SIOCSIFFLAGS, &interface) == 0 && close(net) == 0);
    close_except(sock); drop_privileges();
    pid_t child = fork(); REQUIRE(child >= 0);
    if (!child) {
        char fd[20], pp[20], tt[20], sf[20], dv[30], in[30], user[20], privatepath[4096];
        snprintf(fd, sizeof(fd), "%d", sock); snprintf(pp, sizeof(pp), "%d", parent); snprintf(tt, sizeof(tt), "%d", tid);
        snprintf(sf, sizeof(sf), "%d", sourcefd); snprintf(dv, sizeof(dv), "%llu", dev); snprintf(in, sizeof(in), "%llu", ino); snprintf(user, sizeof(user), "%d", uid);
        snprintf(privatepath, sizeof(privatepath), "%s/private-node/material", base);
        char *argv[] = {"/images/peer", "--harness", fd, pp, tt, sf, privatepath, dv, in, user, NULL};
        char *env[] = {"PATH=/images", NULL}; execve(argv[0], argv, env); REQUIRE(0);
    }
    close(sock); for (;;) { int status; pid_t p = waitpid(-1, &status, 0); if (p == child) _exit(113); REQUIRE(p > 0 || errno == EINTR); }
}
static int pidfd(pid_t pid) { int fd = syscall(SYS_pidfd_open, pid, 0); REQUIRE(fd >= 0); return fd; }
static void witness(pid_t outer, int inner, pid_t init) {
    char path[128], line[512]; snprintf(path, sizeof(path), "/proc/%d/status", outer);
    FILE *status = fopen(path, "r"); REQUIRE(status != NULL); int matched = 0;
    while (fgets(line, sizeof(line), status)) if (!strncmp(line, "NSpid:", 6)) {
        int actual_outer, actual_inner; char extra;
        REQUIRE(sscanf(line + 6, "%d %d %c", &actual_outer, &actual_inner, &extra) == 2 && actual_outer == outer && actual_inner == inner); matched++;
    }
    REQUIRE(matched == 1 && fclose(status) == 0);
    const char *kinds[] = {"user", "mnt", "pid", "net"};
    for (unsigned i = 0; i < sizeof(kinds)/sizeof(kinds[0]); i++) {
        struct stat actual, parent, initial;
        snprintf(path, sizeof(path), "/proc/%d/ns/%s", outer, kinds[i]); REQUIRE(stat(path, &actual) == 0);
        snprintf(path, sizeof(path), "/proc/self/ns/%s", kinds[i]); REQUIRE(stat(path, &parent) == 0 && actual.st_ino != parent.st_ino);
        snprintf(path, sizeof(path), "/proc/%d/ns/%s", init, kinds[i]); REQUIRE(stat(path, &initial) == 0 && actual.st_ino == initial.st_ino);
    }
}
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--harness")) return harness(argc, argv);
    if (argc > 1 && !strcmp(argv[1], "--setup")) {
        REQUIRE(argc == 11); char self[4096]; ssize_t n = readlink("/proc/self/exe", self, sizeof(self)-1); REQUIRE(n > 0); self[n] = 0;
        /* A clean exec discarded the original parent's address space and source FD. */
        capsule(atoi(argv[2]), argv[3], self, atoi(argv[4]), atoi(argv[5]), atoi(argv[6]), strtoull(argv[7], NULL, 10), strtoull(argv[8], NULL, 10), atoi(argv[9]), atoi(argv[10])); _exit(114);
    }
    REQUIRE(argc == 2); const char *base = argv[1]; int uid = getuid(), gid = getgid(); REQUIRE(uid != 0);
    alarm(10);
    REQUIRE(prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0);
    char source[4096], self[4096]; snprintf(source, sizeof(source), "%s/private-node/material", base);
    int sourcefd = open(source, O_RDONLY); REQUIRE(sourcefd >= 0); struct stat secret; REQUIRE(fstat(sourcefd, &secret) == 0);
    unsigned char canary[64]; REQUIRE(read(sourcefd, canary, sizeof(canary)) == 32);
    REQUIRE(setenv("AQ_CAPSULE_PARENT_CANARY", "operator-context-canary", 1) == 0);
    pthread_t thread; REQUIRE(pthread_create(&thread, NULL, thread_canary, NULL) == 0); while (!other_tid) sched_yield();
    int sockets[2]; REQUIRE(socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sockets) == 0); int on = 1;
    REQUIRE(setsockopt(sockets[0], SOL_SOCKET, SO_PASSCRED, &on, sizeof(on)) == 0);
    ssize_t n = readlink("/proc/self/exe", self, sizeof(self)-1); REQUIRE(n > 0); self[n] = 0;
    pid_t parent = getpid(), bootstrap = fork(); REQUIRE(bootstrap >= 0);
    if (!bootstrap) {
        REQUIRE(prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0) == 0 && getppid() == parent);
        close(sockets[0]); close_except(sockets[1]);
        char fd[20], pp[20], tt[20], sf[20], dv[30], in[30], user[20], group[20];
        snprintf(fd, sizeof(fd), "%d", sockets[1]); snprintf(pp, sizeof(pp), "%d", parent); snprintf(tt, sizeof(tt), "%d", (int)other_tid);
        snprintf(sf, sizeof(sf), "%d", sourcefd); snprintf(dv, sizeof(dv), "%llu", (unsigned long long)secret.st_dev); snprintf(in, sizeof(in), "%llu", (unsigned long long)secret.st_ino);
        snprintf(user, sizeof(user), "%d", uid); snprintf(group, sizeof(group), "%d", gid);
        char *args[] = {self, "--setup", fd, (char *)base, pp, tt, sf, dv, in, user, group, NULL};
        char *env[] = {"PATH=/images", NULL}; execve(self, args, env); _exit(114);
    }
    REQUIRE(close(sockets[1]) == 0);
    struct packet packet; int received; pid_t init = recv_packet(sockets[0], 'I', &packet, &received); REQUIRE(packet.inner_pid == 1 && received == -1 && init != bootstrap && init != parent); int initfd = pidfd(init);
    witness(init, packet.inner_pid, init);
    pid_t child = recv_packet(sockets[0], 'H', &packet, &received); REQUIRE(packet.inner_pid > 1 && received == -1 && child != parent && child != init); int childfd = pidfd(child); witness(child, packet.inner_pid, init);
    pid_t descendant = recv_packet(sockets[0], 'D', &packet, &received); REQUIRE(received == -1 && descendant != child); int descendantfd = pidfd(descendant); witness(descendant, packet.inner_pid, init);
    pid_t listener_owner = recv_packet(sockets[0], 'L', &packet, &received); REQUIRE(listener_owner == child && received >= 0 && packet.port > 0);
    int connection = accept4(received, NULL, NULL, SOCK_CLOEXEC); REQUIRE(connection >= 0);
    char request[32]; REQUIRE(read(connection, request, sizeof(request)) == 15 && !memcmp(request, "capsule-request", 15));
    REQUIRE(write(connection, "parent-proxy-reply", 18) == 18); REQUIRE(close(connection) == 0 && close(received) == 0);
    REQUIRE(recv_packet(sockets[0], 'P', &packet, &received) == child && received == -1);
    REQUIRE(syscall(SYS_pidfd_send_signal, initfd, SIGKILL, NULL, 0) == 0);
    int reaps = 0; for (;;) { int status; pid_t p = waitpid(-1, &status, __WALL); if (p > 0) { reaps++; continue; } REQUIRE(errno == ECHILD); break; }
    /* Namespace teardown may reap descendants before the outer subreaper sees them. */
    REQUIRE(reaps >= 1);
    struct pollfd stopped[] = {{initfd, POLLIN, 0}, {childfd, POLLIN, 0}, {descendantfd, POLLIN, 0}};
    REQUIRE(poll(stopped, 3, 0) == 3);
    puts("PASS same-host-uid; random original canary read before clean setup exec; parent PID/TID/root/fd/path denied; no inherited source FD/env");
    puts("PASS derived-auth/allowed-folder/internal-git visible; workspace writable; remount denied");
    puts("PASS post-namespace SCM_CREDENTIALS outer init/harness/descendant PIDs pinned with pidfds; no socketpair SO_PEERCRED identity claim");
    puts("PASS child-netns loopback listener SCM_RIGHTS to parent; parent accepts without joining network namespace");
    puts("PASS PID1 SIGKILL stops harness+descendant; all three pidfds exited; dedicated subreaper reaches kernel ECHILD");
    return 0;
}
