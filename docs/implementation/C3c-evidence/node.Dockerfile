FROM ghcr.io/electricjack/aq-sandbox-os@sha256:f6344f0456cbc11ef543b0060a808b4532c3a928b5b110d875065efbf9e066bd
USER root
RUN ["sh", "-ec", "apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev cmake curl xz-utils python3 && rm -rf /var/lib/apt/lists/*"]
RUN ["sh", "-ec", "curl -fLsS https://nodejs.org/dist/v24.11.0/node-v24.11.0-linux-x64.tar.xz -o /tmp/node.tar.xz && echo '46da9a098973ab7ba4fca76945581ecb2eaf468de347173897044382f10e0a0a  /tmp/node.tar.xz' | sha256sum -c - && mkdir /opt/node && tar -xJf /tmp/node.tar.xz -C /opt/node --strip-components=1 && for tool in node npm npx; do ln -s /opt/node/bin/$tool /usr/local/bin/$tool; done && rm /tmp/node.tar.xz"]
RUN ["sh", "-ec", "curl -fLsS https://registry.npmjs.org/pnpm/-/pnpm-10.34.5.tgz -o /tmp/pnpm.tgz && echo 'ccb5c479cab1b00621325bfe7d4c9a8a8031e7a525d7249e275ecbec81b08db2  /tmp/pnpm.tgz' | sha256sum -c - && mkdir /opt/pnpm && tar -xzf /tmp/pnpm.tgz -C /opt/pnpm --strip-components=1 && ln -s /opt/pnpm/bin/pnpm.cjs /usr/local/bin/pnpm && ln -s /opt/pnpm/bin/pnpx.cjs /usr/local/bin/pnpx && rm /tmp/pnpm.tgz"]
RUN ["sh", "-ec", "node --version && pnpm --version"]
WORKDIR /workspace
COPY --from=ghcr.io/electricjack/aq-sandbox-runtime@sha256:13a64d6dfbedd8cb2e31b26a18670c71f6b43efd800eff94d62fc3b1d2bc2176 /opt/aq /opt/aq
RUN ["/bin/sh", "-ec", "getent passwd runner >/dev/null && getent passwd agent >/dev/null && command -v tmux >/dev/null && command -v bwrap >/dev/null && command -v git >/dev/null && test -s /opt/aq/manifest.json"]
