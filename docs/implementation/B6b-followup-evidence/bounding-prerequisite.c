#include <errno.h>
#include <linux/capability.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

static void report(const char *stage) {
    printf("stage=%s uid=%ld euid=%ld\n", stage, (long)getuid(), (long)geteuid());
    FILE *status = fopen("/proc/self/status", "r");
    char line[256];
    while (fgets(line, sizeof line, status))
        if (!strncmp(line, "Cap", 3) || !strncmp(line, "NoNewPrivs:", 11))
            fputs(line, stdout);
    fclose(status);
    errno = 0;
    int result = prctl(PR_CAPBSET_DROP, CAP_KILL, 0L, 0L, 0L);
    int error = errno;
    printf("PR_CAPBSET_DROP(CAP_KILL): result=%d errno=%d (%s)\n", result, error, strerror(error));
}
int main(void) {
    report("root-entrypoint");
    if (prctl(PR_SET_NO_NEW_PRIVS, 1L, 0L, 0L, 0L) || setgid(21002) || setuid(21002))
        return 2;
    struct __user_cap_header_struct header = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = 0 };
    struct __user_cap_data_struct empty[2] = {0};
    if (syscall(SYS_capset, &header, empty))
        return 3;
    report("agent-cleared-current-sets");
    return 0;
}
