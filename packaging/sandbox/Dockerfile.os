# Rendered from the single pins.json by scripts/package-sandbox.py.
FROM @@BUILDER@@ AS git-build
COPY snapshot.sources /etc/apt/sources.list.d/debian.sources
RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl-dev libcurl4-openssl-dev libexpat1-dev gettext zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /inputs
COPY git.tar.xz SHA256SUMS ./
RUN sha256sum --check SHA256SUMS && mkdir /git \
    && tar -xf git.tar.xz -C /git --strip-components=1
WORKDIR /git
RUN make -j2 prefix=/opt/git RUNTIME_PREFIX=YesPlease NO_TCLTK=YesPlease NO_GETTEXT=YesPlease all \
    && make prefix=/opt/git RUNTIME_PREFIX=YesPlease NO_TCLTK=YesPlease NO_GETTEXT=YesPlease install

FROM @@BASE@@ AS aq-sandbox-os
COPY snapshot.sources /etc/apt/sources.list.d/debian.sources
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tmux bubblewrap bash perl libcurl4 libexpat1 libssl3 zlib1g \
    libstdc++6 libgcc-s1 libatomic1 ncurses-term \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 runner \
    && useradd --uid 10001 --gid runner --create-home --shell /bin/bash runner \
    && groupadd --gid 10002 agent \
    && useradd --uid 10002 --gid agent --create-home --shell /bin/bash agent \
    && mkdir -p /usr/share/aq-sandbox \
    && dpkg-query -W -f='${Package}=${Version}\n' > /usr/share/aq-sandbox/debian-packages.txt
COPY --from=git-build /opt/git /opt/git
RUN ln -s /opt/git/bin/git /usr/local/bin/git \
    && git --version && tmux -V && bwrap --version
ENV PATH="/opt/aq/bin:/opt/git/bin:/usr/local/bin:/usr/bin:/bin" LANG=C.UTF-8 TERM=xterm-256color
# The sandbox runner starts as root and owns its later privilege transitions (B6b).
WORKDIR /workspace
