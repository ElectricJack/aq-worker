#define _GNU_SOURCE
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/wait.h>

/* Explicit static physical fixture. No agent, provider, auth, shell or network. */
static int canonical_image_path(char *path, size_t capacity) {
    ssize_t n = readlink("/proc/self/exe", path, capacity - 1);
    if (n < 0) return 60;
    path[n] = 0;
    char *canonical = realpath(path, NULL);
    if (!canonical || strcmp(path, canonical) || strcmp(path, "/opt/aq-qualification/codex")) return 61;
    free(canonical);
    return 0;
}

static int inspect_image(char *path, size_t capacity, struct stat *image) {
    int result = canonical_image_path(path, capacity);
    if (result) return result;
    struct statvfs fs;
    int self = open(path, O_RDONLY | O_NOFOLLOW);
    if (self < 0 || fstat(self, image) || fstatvfs(self, &fs) || !(fs.f_flag & ST_RDONLY)) return 62;
    close(self);
    return 0;
}

static int check_helper(const struct stat *image, char **argv) {
    if ((unsigned long long)image->st_dev != strtoull(argv[2], NULL, 10) ||
        (unsigned long long)image->st_ino != strtoull(argv[3], NULL, 10)) return 63;
    puts("NAMED-HELPER:same-inode-canonical");
    fflush(stdout);
    return 0;
}

static int check_stdin(void) {
    const char payload[] = "named readonly sealed stdin\n";
    char input[128];
    size_t got = 0;
    ssize_t count;
    while ((count = read(0, input + got, sizeof(input) - got)) > 0) {
        got += count;
        if (got == sizeof(input)) return 65;
    }
    int seals = fcntl(0, F_GET_SEALS);
    int required = F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL;
    if (count < 0 || got != sizeof(payload) - 1 || memcmp(input, payload, got) ||
        (seals & required) != required) return 66;
    return 0;
}

static int prepare_helper(const char *path, char *helper, size_t capacity) {
    const char *home = getenv("CODEX_HOME");
    if (!home || strncmp(home, "/var/lib/aq-qualification/", strlen("/var/lib/aq-qualification/"))) return 67;
    if (mkdir(home, 0700)) return 68;
    if (snprintf(helper, capacity, "%s/peer-helper", home) < 0) return 69;
    if (symlink(path, helper)) return 70;
    return 0;
}

static int run_helper(const char *path, const struct stat *image) {
    char helper[4096], device[40], inode[40];
    int result = prepare_helper(path, helper, sizeof(helper));
    if (result) return result;
    snprintf(device, sizeof(device), "%llu", (unsigned long long)image->st_dev);
    snprintf(inode, sizeof(inode), "%llu", (unsigned long long)image->st_ino);
    pid_t child = fork();
    if (child < 0) return 71;
    if (child == 0) {
        execl(helper, helper, "--helper", device, inode, NULL);
        _exit(72);
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 73;
    return 0;
}

int main(int argc, char **argv) {
    char path[4096];
    struct stat image;
    int result = inspect_image(path, sizeof(path), &image);
    if (result) return result;
    if (argc == 4 && !strcmp(argv[1], "--helper")) return check_helper(&image, argv);
    if (argc != 1) return 64;
    result = check_stdin();
    if (result) return result;
    puts("NAMED-STDIN:exact-sealed");
    printf("NAMED-EXE:%s\n", path);
    fflush(stdout);
    result = run_helper(path, &image);
    if (result) return result;
    puts("NAMED-READY:physical-only");
    fflush(stdout);
    for (;;) pause();
}
