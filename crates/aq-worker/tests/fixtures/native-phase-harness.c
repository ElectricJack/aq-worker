#define _GNU_SOURCE
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static int save(const char *path, const void *bytes, size_t length) {
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0600);
    if (fd < 0) return 1;

    size_t written = 0;
    while (written < length) {
        ssize_t count = write(fd, (const char *)bytes + written, length - written);
        if (count <= 0) return 2;
        written += (size_t)count;
    }
    if (fsync(fd)) return 3;
    return close(fd) != 0;
}

/* Keep the fixture's exit codes stable for callers diagnosing startup failures. */
static int copy_payload(const char *source, const char *destination) {
    int fd = open(source, O_RDONLY | O_NOFOLLOW);
    if (fd < 0) return 12;

    struct stat metadata;
    if (fstat(fd, &metadata) || !S_ISREG(metadata.st_mode) || metadata.st_nlink != 1 ||
        (metadata.st_mode & 0777) != 0600 || metadata.st_size < 1 || metadata.st_size > 4096)
        return 13;

    char payload[4097];
    ssize_t length = read(fd, payload, sizeof(payload));
    if (length != metadata.st_size) return 14;
    char extra;
    if (read(fd, &extra, 1) != 0) return 15;
    close(fd);
    if (save(destination, payload, (size_t)length)) return 16;
    return 0;
}

static int save_working_directory(const char *destination) {
    char cwd[PATH_MAX], name[PATH_MAX];
    if (!getcwd(cwd, sizeof(cwd))) return 17;
    if (snprintf(name, sizeof(name), "%s.cwd", destination) >= (int)sizeof(name)) return 18;
    if (save(name, cwd, strlen(cwd))) return 19;
    return 0;
}

static int wait_for_stop(void) {
    const char message[] = "synthetic native fixture running\r\n";
    if (write(1, message, sizeof(message) - 1) < 0) return 20;
    for (;;) {
        struct timespec delay = {0, 10000000};
        nanosleep(&delay, 0);
#ifdef AQ_TEST_PERIODIC_OUTPUT
        /* Explicit tail-probe build only; normal fixture behavior stays unchanged. */
        static unsigned ticks = 0;
        if (++ticks % 25 == 0) {
            const char tick[] = "synthetic native fixture output tick\r\n";
            if (write(1, tick, sizeof(tick) - 1) < 0) return 21;
        }
#endif
    }
}

int main(int argc, char **argv) {
    if (argc != 4) return 10;
    if (strcmp(argv[3], "exit") == 0) return 0;
    if (strcmp(argv[3], "wait") != 0) return 11;

    int result = copy_payload(argv[1], argv[2]);
    if (result) return result;
    result = save_working_directory(argv[2]);
    if (result) return result;
    return wait_for_stop();
}
