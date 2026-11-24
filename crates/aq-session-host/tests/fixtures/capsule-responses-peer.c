#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

/* Finite synthetic HTTP client, not a model harness or general HTTP parser. */
static void marker(const char *name, const char *value) {
    char path[128];
    snprintf(path, sizeof(path), "/workspace/%s", name);
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0 || write(fd, value, strlen(value)) != (ssize_t)strlen(value)) _exit(102);
    close(fd);
}

static void fail(int line) {
    char text[32];
    snprintf(text, sizeof(text), "%d\n", line);
    marker("guest-error", text);
    _exit(101);
}
#define CHECK(x) do { if (!(x)) fail(__LINE__); } while (0)

static void send_all(int fd, const char *p, size_t n) {
    while (n) {
        ssize_t sent = write(fd, p, n);
        CHECK(sent > 0);
        p += sent;
        n -= (size_t)sent;
    }
}

static void file(const char *path, char *body, size_t bytes) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    CHECK(fd >= 0);
    ssize_t n = read(fd, body, bytes - 1);
    CHECK(n > 0 && (size_t)n < bytes - 1);
    body[n] = 0;
    char extra;
    CHECK(read(fd, &extra, 1) == 0);
    close(fd);
}

static unsigned hex_digit(char c) {
    if (c >= '0' && c <= '9') return (unsigned)(c - '0');
    if (c >= 'a' && c <= 'f') return (unsigned)(c - 'a' + 10);
    if (c >= 'A' && c <= 'F') return (unsigned)(c - 'A' + 10);
    fail(__LINE__);
    return 0;
}

static size_t chunk_length(const char *start, const char *end, size_t limit) {
    size_t length = 0;
    for (const char *c = start; c < end; c++) {
        length = length * 16 + hex_digit(*c);
        CHECK(length < limit);
    }
    return length;
}

static size_t decode_chunks(char *p, char *end, char *decoded, size_t capacity) {
    size_t used = 0;
    for (;;) {
        char *line = strstr(p, "\r\n");
        CHECK(line && line - p > 0 && line - p <= 8);
        size_t length = chunk_length(p, line, capacity);
        p = line + 2;
        CHECK((size_t)(end - p) >= length + 2 && p[length] == '\r' && p[length + 1] == '\n');
        if (!length) {
            CHECK(p + 2 == end);
            return used;
        }
        CHECK(used + length < capacity);
        memcpy(decoded + used, p, length);
        used += length;
        p += length + 2;
    }
}

static void check_response(char *body, size_t bytes) {
    char *head = strstr(body, "\r\n\r\n");
    CHECK(head && !strncmp(body, "HTTP/1.1 200 OK\r\n", 17));
    CHECK(strstr(body, "Content-Type: text/event-stream\r\n") &&
          strstr(body, "Transfer-Encoding: chunked\r\n"));
    char decoded[4096] = {0};
    size_t used = decode_chunks(head + 4, body + bytes, decoded, sizeof(decoded));
    const char *expected = "event: response.completed\ndata: {\"type\":\"response.completed\",\"marker\":\"finite-capsule-sse\"}\n\n";
    CHECK(used == strlen(expected) && !memcmp(decoded, expected, used));
}

static const char *check_launch(int argc, char **argv) {
    CHECK(argc == 5 && getpid() > 1 && fcntl(3, F_GETFD) < 0 && errno == EBADF);
    CHECK(!getenv("HOME") && !getenv("AQ_SESSION_DIRECTORY") && !getenv("TMPDIR"));
    const char *origin = getenv("AQ_CAPSULE_ENDPOINT");
    CHECK(origin && !strncmp(argv[2], "--candidate-origin=", 19) && !strcmp(origin, argv[2] + 19));
    CHECK(!strncmp(argv[3], "--candidate-preparation=", 24) && !strncmp(argv[4], "--candidate-deadline=", 21));
    char input[64] = {0};
    CHECK(read(0, input, sizeof(input)) == 24 && !memcmp(input, "finite scoped SSE stdin\n", 24));
    CHECK(read(0, input, sizeof(input)) == 0);
    return origin;
}

static void load_capability(const char *origin, char *auth, size_t auth_bytes, char *cap) {
    char config[4096] = {0};
    file("/auth/auth.json", auth, auth_bytes);
    file("/auth/candidate-config.json", config, sizeof(config));
    CHECK(strstr(config, origin));
    const char *prefix = "{\"OPENAI_API_KEY\":\"";
    size_t offset = strlen(prefix);
    CHECK(strlen(auth) == offset + 77 + 2 && !memcmp(auth, prefix, offset) && !strcmp(auth + offset + 77, "\"}"));
    memcpy(cap, auth + offset, 77);
    CHECK(!strncmp(cap, "aq-responses-", 13));
    CHECK(open("/auth/auth.json", O_WRONLY | O_CLOEXEC) < 0 && errno == EROFS);
}

static void await_send(void) {
    marker("guest-ready", "paused until send\n");
    for (;;) {
        int fd = open("/workspace/send", O_RDONLY | O_CLOEXEC);
        if (fd >= 0) { close(fd); return; }
        CHECK(errno == ENOENT);
        struct timespec nap = {.tv_nsec = 2000000};
        CHECK(nanosleep(&nap, NULL) == 0);
    }
}

static int connect_origin(const char *origin, int *port) {
    CHECK(sscanf(origin, "http://127.0.0.1:%d", port) == 1 && *port > 0 && *port <= 65535);
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    CHECK(fd >= 0);
    struct timeval timeout = {.tv_sec = 6};
    CHECK(setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) == 0 &&
          setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) == 0);
    struct sockaddr_in address = {
        .sin_family = AF_INET,
        .sin_port = htons((unsigned short)*port),
        .sin_addr.s_addr = htonl(INADDR_LOOPBACK)
    };
    CHECK(connect(fd, (void *)&address, sizeof(address)) == 0);
    return fd;
}

static void receive_response(int fd, int positive) {
    char response[16384] = {0};
    size_t used = 0;
    for (;;) {
        ssize_t n = read(fd, response + used, sizeof(response) - 1 - used);
        if (n == 0) break;
        if (n < 0) { CHECK(!positive && errno == ECONNRESET); break; }
        used += (size_t)n;
        CHECK(used < sizeof(response) - 1);
    }
    close(fd);
    if (positive) {
        check_response(response, used);
        marker("response-complete", "finite SSE verified\n");
    } else {
        CHECK(used == 0);
        marker("response-refused", "no response data\n");
    }
}

static size_t format_request(char *request, size_t capacity, int port, const char *cap, const char *model) {
    char body[1024];
    int count = snprintf(body, sizeof(body), "{\"model\":\"%s\",\"stream\":true,\"store\":false,\"input\":[{\"type\":\"message\",\"role\":\"user\",\"content\":\"finite capsule input\"}],\"reasoning\":{\"effort\":\"medium\"},\"tools\":[]}", model);
    CHECK(count > 0 && (size_t)count < sizeof(body));
    int bytes = snprintf(request, capacity, "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nAuthorization: Bearer %s\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", port, cap, count, body);
    CHECK(bytes > 0 && (size_t)bytes < capacity);
    return (size_t)bytes;
}

static const char *request_model(const char *mode, char *cap) {
    int wrong_key = !strcmp(mode, "wrong-key");
    int wrong_model = !strcmp(mode, "wrong-model");
    CHECK(wrong_key || wrong_model || !strcmp(mode, "positive") || !strcmp(mode, "cancel") || !strcmp(mode, "expiry"));
    if (wrong_key) cap[76] = cap[76] == '0' ? '1' : '0';
    return wrong_model ? "other-model" : "offline-model";
}

int main(int argc, char **argv) {
    const char *origin = check_launch(argc, argv);
    char auth[256] = {0}, cap[78] = {0};
    load_capability(origin, auth, sizeof(auth), cap);
    await_send();
    const char *model = request_model(argv[1], cap);
    int positive = !strcmp(argv[1], "positive");
    int port = 0;
    int fd = connect_origin(origin, &port);
    char request[2048];
    size_t bytes = format_request(request, sizeof(request), port, cap, model);
    marker("request-attempt", "one finite request\n");
    send_all(fd, request, bytes);
    memset(cap, 0, sizeof(cap));
    memset(auth, 0, sizeof(auth));
    memset(request, 0, sizeof(request));
    receive_response(fd, positive);
    return 0;
}
