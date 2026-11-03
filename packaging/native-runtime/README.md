# Standalone native interactive runtime

This is the supported packaging entrypoint for the current worker's native
interactive runner. It builds worker, session host and the result-capable agent
client from one committed aq-worker tree and the locked Cargo dependency closure.
It installs aq-git 0.1.0 at `f3660a89`, Git 2.56.0, the actual Codex 0.166.0 musl
ELF and its tool-execution companions, tmux 3.4, Git HTTPS helpers, CA certificates, ncurses terminfo, bash,
bubblewrap, Python, procps, ripgrep and tini. Only `linux/amd64` is supported.

The exact public input hashes and base image digests are in [pins.json](pins.json).
These reuse the independently observed I0 component pins; I0's image and evidence
remain separate. No aq-server candidate or source pin is modified here.

## Build from public inputs

Use Python 3.12+ and Git to create the context, and Docker with BuildKit to build.
Prepare a directory containing **only public artifacts** (the packager reads only
the names pinned in `pins.json`, including the nested Codex paths):

- `aq-git-source.tar.gz`: `git archive --format=tar.gz f3660a89cbeac0e068105af2926dfdcb06a1daad`
  from the aq-git repository. This is the pinned source archive also shipped by
  aq-server's Git candidate. Its archive hash must match pins.json.
- `git-2.56.0.tar.xz` and `tmux-3.4.tar.gz`: upstream source releases; URLs and
  SHA256 hashes are in pins.json.
- `codex`: the extracted x86_64 Linux musl executable from Codex 0.166.0, not an
  npm wrapper or a system shim. Its ELF hash and size must match pins.json.
- `codex-code-mode-host`, `codex-path/rg`, and `codex-resources/bwrap`: from the
  **same** Codex 0.166.0 package. All three have exact size and SHA256 pins.

The npm platform package `@openai/codex-linux-x64@0.166.0` contains
`vendor/x86_64-unknown-linux-musl/` with `codex-package.json` layoutVersion 1.
Copy `bin/codex` and `bin/codex-code-mode-host` to the input directory's top level,
and retain the relative paths of `codex-path/rg` and `codex-resources/bwrap`.
The image uses Codex's supported companion layout, with the host next to the main
executable and bubblewrap in its adjacent `codex-resources` directory. Its PATH
selects the pinned `codex-path/rg`. Voice resources are outside this execution closure.
The packager checks each extracted file; it does not trust a reported version alone.
Changing a component requires an explicit reviewed pins.json change and fresh
qualifications for the resulting installation.

From a **committed** aq-worker checkout containing this packaging:

```sh
python3 scripts/package-native-runtime.py --revision HEAD \
  --inputs /path/to/public-inputs --output /path/to/new-context
docker build --platform linux/amd64 --progress=plain \
  -t aq-worker-native:local /path/to/new-context
docker run --rm --network none --cap-drop ALL --security-opt no-new-privileges \
  --entrypoint python3 aq-worker-native:local /usr/local/lib/aq-native-runtime-smoke.py
```

The context is generated in a new directory using `git archive` of Cargo files,
crates, vendor sources, the required `protocol/aq-admission` workspace crate,
the two embedded sandbox template tools, and native packaging at the selected
revision. It never traverses
the working tree, `.aq`, `.git`, user homes, arbitrary input-directory files or
runtime configuration. Dirty and untracked changes are excluded. No credentials,
private config, SSH agent, login home or build secret are required. The Dockerfile
uses explicit COPY paths. Do not build with the repository root as context.

Source inputs, the Rust builder and the Debian base are pinned. Debian packages
are installed from the configured Bookworm repositories; their observed versions
are retained in `/usr/share/agent-q/debian-packages.txt`. This is **not a
bit-for-bit image reproducibility claim**. Record the resulting immutable image
ID/digest alongside any installation qualification.

## Installed contract and smoke

All six program files are separate, executable, single-link files under
`/opt/aq-runtime`: `aq-worker`, `aq-session-host`, `aq-agent`, `aq-git`, `codex`,
`tmux`. Convenience symlinks are in `/usr/local/bin`; the full Git installation
is under `/opt/aq-git-runtime` and `/usr/bin/git` selects its pinned Git.
The three Codex companions retain the paths above under `/opt/aq-runtime`.
They are checked against `pins.json` separately, preserving the six-program
`images.json` contract used by the interactive launcher.

`/usr/share/agent-q/images.json` uses the existing interactive launcher's
`schema_version: 1` program list (name, absolute path, SHA256, bytes).
`build.json` records the worker commit, source-archive hash and crate versions;
`pins.json` identifies external inputs. These are installation observations,
not admission manifests. Native configuration still needs qualified definitions,
artifact references, model/provider context and trusted runtime evidence.

The final image build runs the offline smoke as UID/GID 10001. It checks all six
ELF architectures, file integrity and permissions, dynamic dependencies (including
Git helpers), exact versions, native worker CLI subcommands and result submit/status
surfaces. The session host has no version CLI: its version comes from its committed
Cargo manifest, while the smoke checks its actual loader/entrypoint refusal. A
fresh credential-free Codex home is used. The smoke also starts and stops a private
tmux server, catching the missing `libevent_core-2.1.so.7` regression.
Before recording or accepting an installation, it refuses any missing, modified,
non-executable or wrong-architecture Codex companion and checks their loader and
CLI entrypoints. `codex --version` and `--help` alone cannot detect a missing
code-mode host.

After building, run the targeted tool-execution probe from the checkout:

```sh
docker run --rm -i --network none --cap-drop ALL --security-opt no-new-privileges \
  --entrypoint python3 aq-worker-native:local - < tools/packaging/probe_codex_tools.py
```

It starts Codex with an empty home and a local scripted Responses endpoint,
requests a code-mode shell tool call, and requires both the written proof file
and its returned tool output. The old image fails despite Codex exiting zero.
This verifies the installed tool path without credentials or a real model; it
does not qualify provider behavior or Codex sandbox isolation.

## Running the worker

The image uses tini to reap children, runs as 10001:10001, and defaults to worker
help. Supply private configuration at runtime, with state writable by that UID:

```sh
docker run --name aq-native-worker --read-only --cap-drop ALL \
  --security-opt no-new-privileges --tmpfs /tmp:mode=1777 \
  --mount type=bind,src=/path/to/private-config,dst=/config,readonly \
  --mount type=bind,src=/path/to/worker-state,dst=/state \
  aq-worker-native:local --config /config/worker.toml \
  check-native --native-config /config/native.json
```

For execution select `run-native` with the same paths after successful native
preflight. The separately configured aq-git service is available in the image;
the worker does not start it automatically. Preserve the container and state for
recovery; use a fresh boot/journal only through the ordinary ownership procedure.
The existing `scripts/interactive-worker-bootstrap.py` remains available as the
separate local-node orchestration example and is not injected into production
worker behavior by this image.

Physical read-only filesystem, namespace, ptrace, seccomp and sandbox requirements
still belong to the selected runtime profile. I0 found the default Docker seccomp
profile insufficient for its Codex sandbox probe. This packaging smoke neither
relaxes seccomp nor certifies sandbox execution, provider/model behavior, terminal
ownership, authenticated real runs or end-to-end server settlement. Run those
qualifications separately against the final image digest. Never substitute the
installation manifest for qualification evidence.
