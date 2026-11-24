#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Keep this synthetic image above the small-ELF profile's size limit. */
const unsigned char image_padding[5 * 1024 * 1024] = {1};
static const char local_material[] = "{\"OPENAI_API_KEY\":\"sk-private-offline-only\"}";
static const char scoped_material[] =
    "{\"OPENAI_API_KEY\":\"aq-responses-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}";

static int verify_private_input(const char *expected) {
    char path[4096];
    char auth[128];
    const char *home = getenv("CODEX_HOME");
    if (!home || snprintf(path, sizeof(path), "%s/auth.json", home) < 0)
        return 83;
    int fd = open(path, O_RDONLY | O_NOFOLLOW);
    if (fd < 0)
        return 84;
    ssize_t got = read(fd, auth, sizeof(auth));
    close(fd);
    size_t expected_size = strlen(expected);
    return got == (ssize_t)expected_size && memcmp(auth, expected, expected_size) == 0 ? 0 : 85;
}

static int check_arguments(int argc, char **argv) {
    if (argc == 7 && getenv("AQ_SESSION_DIRECTORY"))
        return 86;
    for (int i = 1; i < argc; i++) {
        if (strncmp(argv[i], "openai_base_url=", 16) == 0)
            return 87;
    }
    return 0;
}

static int image_has_padding(int argc) {
    return (argc == 1 || argc == 7) && image_padding[argc] == 0;
}

static uint64_t hash_chunk(uint64_t hash, const unsigned char *buffer, ssize_t length) {
    for (ssize_t i = 0; i < length; i++) {
        hash ^= buffer[i];
        hash *= 1099511628211ULL;
    }
    return hash;
}

static int summarize_stdin(int argc) {
    const int required = F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL;
    int seals = fcntl(STDIN_FILENO, F_GET_SEALS);
    if (seals < 0 || (seals & required) != required)
        return 82;

    unsigned char buffer[8192];
    uint64_t hash = 14695981039346656037ULL;
    uint64_t bytes = 0;
    ssize_t count;
    while ((count = read(STDIN_FILENO, buffer, sizeof(buffer))) > 0) {
        hash = hash_chunk(hash, buffer, count);
        bytes += (uint64_t)count;
    }
    if (count < 0 || !image_has_padding(argc))
        return 80;
    printf("STDIN:%llu:%016llx\n", (unsigned long long)bytes, (unsigned long long)hash);
    fflush(stdout);
    return 0;
}

int main(int argc, char **argv) {
    int status = verify_private_input(argc == 7 ? scoped_material : local_material);
    if (status != 0)
        return status;
    status = check_arguments(argc, argv);
    if (status != 0)
        return status;
    puts("AUTH:exact-private-file");
    status = summarize_stdin(argc);
    if (status != 0)
        return status;
    for (;;)
        pause();
}
