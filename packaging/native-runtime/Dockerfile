# syntax=docker/dockerfile:1
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS build
WORKDIR /inputs
COPY SHA256SUMS worker-source.tar build.json pins.json smoke.py aq-git-source.tar.gz git-2.56.0.tar.xz tmux-3.4.tar.gz codex ./
RUN sha256sum --check SHA256SUMS && test "$(uname -m)" = x86_64
RUN mkdir /worker /aq-git /out && tar -xf worker-source.tar -C /worker \
    && tar -xzf aq-git-source.tar.gz -C /aq-git
WORKDIR /worker
ENV CARGO_BUILD_JOBS=2
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo build --locked --release -p aq-worker --bin aq-worker -p aq-session-host --bin aq-session-host -p aq-agent-client --bin aq-agent \
    && cp target/release/aq-worker target/release/aq-session-host target/release/aq-agent /out/
WORKDIR /aq-git
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo build --locked --release -p aq-git-cli && cp target/release/aq-git /out/

FROM build AS tools
RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl-dev libcurl4-openssl-dev libexpat1-dev gettext zlib1g-dev \
    libevent-dev libncurses-dev pkg-config bison && rm -rf /var/lib/apt/lists/*
RUN mkdir /git /tmux && tar -xf /inputs/git-2.56.0.tar.xz -C /git --strip-components=1 \
    && tar -xzf /inputs/tmux-3.4.tar.gz -C /tmux --strip-components=1
WORKDIR /git
RUN make -j2 prefix=/opt/aq-git-runtime RUNTIME_PREFIX=YesPlease NO_TCLTK=YesPlease NO_GETTEXT=YesPlease all \
    && make prefix=/opt/aq-git-runtime RUNTIME_PREFIX=YesPlease NO_TCLTK=YesPlease NO_GETTEXT=YesPlease install
WORKDIR /tmux
RUN ./configure --prefix=/opt/tmux && make -j2 && make install

FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3 AS native-runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libcurl4 libexpat1 libssl3 zlib1g \
    libevent-2.1-7 libevent-core-2.1-7 libncurses6 libtinfo6 ncurses-term \
    bash bubblewrap python3 procps ripgrep tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 aq && useradd --uid 10001 --gid aq --create-home --home-dir /state/home aq \
    && mkdir -p /config /state /opt/aq-runtime /usr/share/agent-q \
    && chown -R 10001:10001 /state /config
COPY --from=build /out/ /opt/aq-runtime/
COPY --from=build /inputs/codex /opt/aq-runtime/codex
COPY --from=tools /opt/tmux/bin/tmux /opt/aq-runtime/tmux
COPY --from=tools /opt/aq-git-runtime /opt/aq-git-runtime
COPY --from=build /inputs/build.json /inputs/pins.json /usr/share/agent-q/
COPY --from=build /inputs/smoke.py /usr/local/lib/aq-native-runtime-smoke.py
RUN chmod 755 /opt/aq-runtime/* \
    && for name in aq-worker aq-session-host aq-agent aq-git codex tmux; do ln -s /opt/aq-runtime/$name /usr/local/bin/$name; done \
    && ln -s /opt/aq-git-runtime/bin/git /usr/bin/git \
    && dpkg-query -W -f='${Package}=${Version}\n' > /usr/share/agent-q/debian-packages.txt \
    && python3 /usr/local/lib/aq-native-runtime-smoke.py --record
ENV PATH="/opt/aq-runtime:/opt/aq-git-runtime/bin:/usr/local/bin:/usr/bin:/bin" \
    HOME=/state/home XDG_CONFIG_HOME=/config TERM=xterm-256color LANG=C.UTF-8
USER 10001:10001
# Exercise the installed closure as the final unprivileged user, without secrets.
RUN python3 /usr/local/lib/aq-native-runtime-smoke.py
WORKDIR /state
ENTRYPOINT ["/usr/bin/tini", "--", "/opt/aq-runtime/aq-worker"]
CMD ["--help"]
