FROM ghcr.io/electricjack/aq-sandbox-os@sha256:f6344f0456cbc11ef543b0060a808b4532c3a928b5b110d875065efbf9e066bd
USER root
RUN ["sh", "-ec", "apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev cmake curl xz-utils python3 && rm -rf /var/lib/apt/lists/*"]
RUN ["sh", "-ec", "curl -fLsS https://static.rust-lang.org/dist/rust-1.96.1-x86_64-unknown-linux-gnu.tar.xz -o /tmp/rust.tar.xz && echo 'd29ccb1559a177c4e72291f6e5f629de7fe8885e7521ca47802627544b121e95  /tmp/rust.tar.xz' | sha256sum -c - && mkdir /tmp/rust-install && tar -xJf /tmp/rust.tar.xz -C /tmp/rust-install --strip-components=1 && /tmp/rust-install/install.sh --prefix=/opt/rust --components=rustc,rust-std-x86_64-unknown-linux-gnu,cargo,rustfmt-preview,clippy-preview --disable-ldconfig && for tool in rustc rustdoc cargo rustfmt cargo-fmt cargo-clippy clippy-driver; do ln -s /opt/rust/bin/$tool /usr/local/bin/$tool; done && rm -rf /tmp/rust.tar.xz /tmp/rust-install"]
RUN ["sh", "-ec", "rustc --version && cargo --version && rustfmt --version && cargo clippy --version"]
WORKDIR /workspace
COPY --from=ghcr.io/electricjack/aq-sandbox-runtime@sha256:13a64d6dfbedd8cb2e31b26a18670c71f6b43efd800eff94d62fc3b1d2bc2176 /opt/aq /opt/aq
RUN ["/bin/sh", "-ec", "getent passwd runner >/dev/null && getent passwd agent >/dev/null && command -v tmux >/dev/null && command -v bwrap >/dev/null && command -v git >/dev/null && test -s /opt/aq/manifest.json"]
