# syntax=docker/dockerfile:1
# E6a dev worker image: the B5b/C2 node controller with the docker-local backend,
# built from this checkout. Build from the repository root:
#   docker build -f packaging/worker/Dockerfile -t aq-worker:dev .
# It talks only to the dedicated rootless sandbox daemon socket mounted at
# /run/aq-sandbox; it never needs or accepts the host Docker socket.
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --locked --release -p aq-worker --bin aq-worker \
    && install -Dm755 target/release/aq-worker /out/aq-worker

FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /config /state /run/aq-sandbox
COPY --from=build /out/aq-worker /usr/local/bin/aq-worker
# Runs as the WSL user (--user 1000:1000) so the sandbox socket owner check passes.
ENV XDG_CONFIG_HOME=/config HOME=/state LANG=C.UTF-8
VOLUME ["/config", "/state"]
WORKDIR /state
ENTRYPOINT ["/usr/local/bin/aq-worker"]
CMD ["--help"]
