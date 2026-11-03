# AQ sandbox artifacts (linux/amd64)

`aq-sandbox-os` contains Debian bookworm, Git 2.56.0, tmux, bubblewrap,
CA certificates and separate `runner` (10001) and `agent` (10002) users.
It starts as root; B6b owns the runner's privilege transitions. A dated,
signature-verified Debian snapshot fixes the OS dependency inputs. Scheduled
security updates advance the snapshot/base pins and rebuild the OS explicitly.

`aq-sandbox-runtime` is a scratch transport image containing only `/opt/aq`.
It has no entrypoint and cannot execute by itself. It carries `aq-worker`, its
`aq-session-host` dependency, published aq-git 0.2.0, the qualified Codex 0.166.0
native executable and a private Node 22.16.0 distribution. Both proxies remain
inside the runner. There are no separate proxy binaries or `aq-agent` here.

The single [pins.json](pins.json) records all external input versions, SHA-256
digests, immutable builder/base image references and the Debian snapshot.
The Dockerfiles are definitions with pin placeholders; use the generator to
render them rather than building the repository root. No floating image tags
are resolved, and both the generator and Docker build verify public inputs.
The runner and session host build together from the selected committed source
with `cargo build --locked`.

## Prepare and build

Use Python 3.11+ (`hashlib.file_digest` and `tomllib`), Git and
Docker BuildKit. Supply a directory containing the following four public files:

- `git.tar.xz`: the upstream Git archive at its pinned URL.
- `aq-git`: the published aq-git linux-x86_64 binary at its pinned URL.
- `codex`: extract the pinned `archive_member` from the pinned Codex URL.
  Verify the download against `archive_sha256` before extraction. The context
  generator independently checks the extracted executable against `sha256`.
- `node.tar.xz`: the official Node archive at its pinned URL.

Only those four names are read from the public inputs directory. Use no
credentials, login homes or private build configuration. Example:

```sh
python3 scripts/package-sandbox.py --revision HEAD \
  --inputs /path/to/public-inputs --output /path/to/fresh-contexts
docker build --platform linux/amd64 --progress=plain \
  -t aq-sandbox-os:local /path/to/fresh-contexts/os
docker build --platform linux/amd64 --progress=plain \
  -t aq-sandbox-runtime:local /path/to/fresh-contexts/runtime
```

The generator exports committed Cargo files, crates and vendored sources only.
It does not traverse the worktree, `.aq`, `.git`, home directories or arbitrary
input-directory files. Definitions and pins come from the same selected
revision. Output directories must be fresh; corrupt inputs leave no context.

## Installed interface

- `/opt/aq/bin/{aq-worker,aq-session-host,aq-git,codex}`
- `/opt/aq/harnesses/codex/codex` (the native executable; `bin/codex` selects it)
- `/opt/aq/node/bin/node` (absolute private runtime path, independent of the
  project's Node/PATH; future Node-based harness wrappers must select it)
- `/opt/aq/{manifest.json,pins.json,source.json}`

`manifest.json` schema version 1 contains `platform`, `source`, `runner`,
`session_host`, `aq_git`, `node`, `harnesses` and `proxies`. Each executable
entry has `name`, `version`, absolute `path`, lowercase hex `sha256` of the
installed bytes and `bytes`. `source` records the worker revision, source tar
digest and crate versions. `harnesses` is a list so later tasks can extend it;
today only Codex is qualified. Proxy values are `in-process:aq-worker`.
The manifest generator hashes the installed files after copying/extraction.
Downstream template advertising (SBX18) can read this list directly.

Templates copy the tree in their final filesystem layer:

```dockerfile
# Both external images must be supplied by immutable digest.
FROM <project-toolchain-on-aq-sandbox-os>@sha256:<digest>
COPY --from=<aq-sandbox-runtime>@sha256:<digest> /opt/aq /opt/aq
```

Recipe assembly, finalize checks, credential scanning, allowlists, production
templates and GHCR publication belong to C3b/C3c. No images are published here.
The OS context contains no runtime pins or runner source, so a harness bump
cannot invalidate OS layers. See `docs/implementation/C3a.md` for real layer
digests and build checks. Fixed inputs do not by themselves establish bitwise
reproducibility of Rust compiler output, OCI metadata or compressed layers;
record the final artifact digest for each publication. These artifacts also
do not establish hostile-code isolation or end-to-end sandbox qualification.
