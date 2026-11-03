# Sandbox template builds (schema 1)

`aq-worker template build <recipe.json> --build-config <vault/builder.json>
--output <new-directory>` assembles a template with Docker BuildKit, runs the
fixed finalize check, scans its exported OCI archive, then atomically adds its
digest to the vault allowlist. Python 3.12+ and Docker with the Buildx plugin
are required on the calling build node. Both Python tools are embedded in the
installed worker; no repository checkout is needed.

Builds are an explicit operator action on a configured build node, outside
attempt capacity. The runner never invokes this command. Configure a named
Docker context and a named BuildKit builder with OCI export support. The
builder and its cache are trusted infrastructure. For example, provision a
named builder with `docker buildx create --name aq-templates --driver
docker-container --bootstrap default`; the ordinary Docker driver may not
support OCI export. Registry credentials used
by Docker stay in the build node's Docker configuration; they are not passed
as build args, secrets, environment variables, SSH mounts or build-context
files. Build stdout/stderr are suppressed because arbitrary installers could
print sensitive data. Failures return a generic refusal and no allowlist
entry. No template is pushed or run by this command.

## Vault inputs

Store recipes beside profiles, for example `vault/templates/recipes/*.json`.
Pass the builder configuration explicitly; no worker/attempt configuration is
read. This command does not modify aq-server's config.toml or environment
catalog (C4 owns that interface).

Example recipe (digests below are placeholders):

```json
{
  "schema_version": 1,
  "name": "example-tools",
  "version": "1",
  "parent": "aq-sandbox-os@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "install_steps": [["sh", "-c", "mkdir -p /opt/example-tools"]],
  "prefetch": []
}
```

Each install step is an argv array rendered as an exec-form Docker `RUN`.
Shell expressions require an explicit `sh -c` argv. Recipes are trusted,
non-secret build definitions, not an untrusted-code sandbox. They cannot add
Dockerfile instructions, host context files, build secrets or mount options.
Only a generated Dockerfile is sent as context. Toolchain installers should
pin their own package versions and verify downloaded bytes; the final OCI
digest identifies the observed build, not a promise of reproducible compiler
output or trustworthy third-party installers.

```json
{
  "schema_version": 1,
  "role": "template-build",
  "docker_context": "default",
  "buildkit_builder": "aq-templates",
  "builder_identity": "desktop-1",
  "os_image": "aq-sandbox-os@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "runtime_image": "aq-sandbox-runtime@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "target_repository": "registry.example/aq/template",
  "registry_visibility": "private",
  "allowlist": "templates/allowlist.json"
}
```

`allowlist` is relative to the builder configuration's directory (the vault
root in this example). `docker_binary` may select an absolute Docker
executable. Otherwise Docker is resolved on PATH. Context, builder and
builder identity are required; there is no fallback to an attempt node.
The Docker client inherits only PATH, HOME, XDG_CONFIG_HOME and DOCKER_CONFIG.
Use dedicated build-node Docker settings and protect this configuration and
allowlist from attempt writers. Explicit private/public registry visibility
is an operator assertion: public-hosted registries can contain private
repositories; the tool cannot infer repository ACLs without registry access.

Parents are either the exact configured `os_image`, a previously admitted
`name@version`, or `codex-universal`/the exact configured `universal_image`.
The optional universal image must also be digest-pinned. Its finalize
bootstrap installs OS prerequisites using apt, creates runner/agent when
absent, and copies the pinned sandbox OS's `/opt/git` before copying the
runtime bundle. It requires an apt-compatible Linux amd64 image and compatible
runtime libraries; unsupported bases fail their build. Existing users are
retained. This optional bootstrap is not a qualification of every upstream
universal image.

Optional prefetch entries have exactly `repository` (HTTPS without userinfo,
query or fragment) and `commit` (40 lowercase hex characters). Git initializes
bare object stores at `/opt/aq-repositories/<ordinal>.git` and fetches the exact
commit without credentials. They are refused unless registry visibility is
`private`. This is a deliberate disclosure gate for private repository
objects; operators must verify registry ACLs before choosing private.

Input JSON is bounded to 1 MiB and rejects duplicate and unknown fields.
Names are lowercase identifiers, at most 64 bytes; versions at most 64;
parents and repository URLs at most 512. There are at most 64 install steps,
32 arguments per step, 4096 bytes per argument, and 16 prefetch entries.
Newlines/control characters and recognized credential-shaped strings refuse
before building. Builder identifiers are bounded to 128 bytes. The config,
vault paths and Docker executable are trusted operator inputs.

## Assembly and admission

Install steps and optional prefetch run before the runtime copy:

```dockerfile
ENV CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
WORKDIR /workspace
COPY --from=aq-sandbox-runtime@sha256:<digest> /opt/aq /opt/aq
RUN <fixed finalize check>
```

Every template's image environment disables Cargo's incremental cache and
keeps only line-table debuginfo for the `dev` and `test` profiles. Sandboxes
are single-use, so the incremental cache only costs disk. These three
variables override the matching repository profile keys; no other Cargo or
profile setting is changed. The `ENV` follows the install steps so their
build cache is unaffected. A harness that clears its tool environment must
forward these values itself for them to reach agent commands.

The Rust recipe adds an install step that writes `/.cargo/config.toml` with
`[build] jobs = 1`, sized for 2 vCPU / 4 GB sandboxes. It is the
lowest-priority Cargo config and survives a cleared environment.
Repository configs, `CARGO_BUILD_JOBS` and `-j` still override it. Rust
1.96.1 already links with its bundled `rust-lld`, so no linker flag is set.

Finalize requires runner and agent passwd entries, executable `tmux`, `bwrap`
and `git` on PATH, and a nonempty `/opt/aq/manifest.json`. Missing requirements
fail BuildKit. Scan then validates the installed schema-1 bundle manifest
and advertises its harness versions. A malformed/missing/whiteouted manifest
refuses. All mutable install operations precede the final runtime copy.

BuildKit exports an OCI archive with gzip layers, one linux/amd64 manifest,
and attestations/SBOM disabled. The emitted digest is the image **manifest**
digest, not a local Docker config ID, an index digest or a tag. Output keeps
`image.oci.tar`, `recipe.json`, `Dockerfile`, `bundle-manifest.json` and
`provenance.json`. Publication is C3c's job and must preserve the scanned
manifest digest; the advertised image reference becomes pullable only after
that separate publication succeeds.

Provenance contains a SHA-256 of canonical recipe JSON (sorted keys, compact
separators, ASCII escapes), output image manifest digest, base image digest, runtime-bundle image digest,
installed harness version map, UTC build time, configured builder identity,
and fixed finalize/credential-scan policy versions. Digests and explicit
builder identity establish traceability; this is not signed build attestation.

Allowlist JSON is `{ "schema_version": 1, "entries": [...] }`. Every entry has:

- `name`, `version`, `image_digest`, `image_reference` (`repository@sha256:...`);
- `provenance` with the fields above;
- `harness_versions`, matching provenance;
- `materializations`, initially `{}`, reserved for backend records such as
  `boat: {snapshot_id: ...}` and `cloudflare: {image_reference: ...}` (C7/C8).

Consumers should treat digest as identity and names as lookup metadata.
The builder validates existing entries, refuses duplicate name/version pairs,
rechecks the parent under an advisory file lock, and writes a same-directory
replacement with fsync. Backend writers must use the same `<allowlist>.lock`
and atomic replacement discipline. No standalone "add arbitrary digest"
command exists. Only successfully finalized/scanned builds enter via this
writer. The allowlist is operator-controlled configuration, not an
adversary-proof or cryptographically signed database. Launch enforcement
belongs to C2/C7. The file is bounded to 1 MiB/1024 entries; backend metadata
is bounded to 64 KiB per entry. A version already admitted must be bumped.

## Reusable scanner

`aq-worker template scan <image.oci.tar>` emits image digest, installed bundle
manifest and layer count as JSON, without editing an allowlist. C7 can reuse
`credential_scan.scan_oci(path)` or the CLI for an exported OCI snapshot.
Non-OCI backend snapshots must first be exported to this supported format;
no silent partial scan is provided.

The scanner verifies descriptor sizes, SHA-256 blob digests and uncompressed
layer DiffIDs, then scans every inherited layer, including files later
removed or whiteouted. It rejects Codex auth.json (conservatively at any
path, covering arbitrary CODEX_HOME), Claude/Gemini credential directories,
Git credentials, netrc, SSH private-key names, Docker credentials and cloud
CLI configs. It conservatively refuses symlink/hardlink aliases of credential directories
(including .docker, .ssh and .config) and checks provider-key/credential
environment variables, config/history, known provider token prefixes, private
key markers, credential assignments, HTTP Authorization headers, credential-bearing
URLs and JWT shapes. Complete quoted JSON credential values and shell assignments
are checked; ordinary credential-store metadata labels are allowed.
The exact JSON pair `"credentials":"private-codex-chatgpt-auth-json"` is a
public protocol format label embedded in the worker, so it is allowed;
arbitrary credential values, suffix variants and shell assignments still refuse.
The exact public Serde capability label `persistent-disk-during-attempt`
does not turn the `sk-` inside `disk-` into a provider key, whatever literal
codegen places after it in `.rodata`. This exception consumes no bytes and
applies only to that occurrence in the `sk-` branch. Bytes adjacent to the
label are scanned independently, so appended provider keys, credential
assignments, URLs, altered labels and nearby keys still refuse.
Layer file content also recognizes a fixed catalog of 74 independently verified
public distribution files containing self-test vectors, format strings, source
code or documentation examples. An exception requires the SHA-256 and byte count
of the **entire original file**, read to EOF. Names, installed locations, ELF
headers and claimed package versions grant no exception. Any edit, appended
credential, truncation or unreviewed upstream version refuses when its content
matches the strict scanner. Credential paths/links, image config/history,
environment checks and bundle metadata keep their independent strict checks;
every layer, including deleted files, is still examined. The catalog ships
inside the scanner and cannot be configured by an image or build input.
Public archive/member fingerprints and a reproduction command are recorded in
`docs/implementation/C3b-public-constants.md`. Package updates that introduce new
matches require fresh public-byte verification and a reviewed catalog change.
Values and suspect paths are never printed. This is pattern/path detection,
not proof that an arbitrary encrypted or unknown-format file is non-secret.
False positives refuse admission; secrets must never be inputs to a build.

No archive paths are extracted onto the host. Parsing rejects traversal,
duplicate members, unsupported compression/manifests, invalid hashes,
unreferenced blobs and missing final bundle manifests. Limits are 32 GiB
archive/blob bytes, 64 GiB expanded layer bytes, one million members, 256
layers and 1 MiB JSON documents. Streaming scans use temporary disk for
verified blobs and one expanded layer. Gzip and uncompressed OCI layers are
supported; zstd/multi-platform/nested-index/attestation archives explicitly
refuse. A rejected OCI archive can still contain the detected secret; output
is created with mode 0700 and must be handled as private forensic data.

## Advertising allowlisted templates (C4)

Sandbox node registration (schema 2) derives template advertisements from this
allowlist through the controller configuration's optional `template_allowlist`
block (path relative to the controller configuration file):

```json
"template_allowlist": {
  "path": "templates/allowlist.json",
  "advertise": [{"environment": "example-tools@1", "estimated_cold_start_ms": 1500,
                 "estimated_cold_start_cost": {"currency": "USD", "micros": 20}}]
}
```

Each `environment` resolves as `name@version` exactly, or a bare `name` that
must be unique in the allowlist, matching aq-server's reading of the file
(extra provenance fields are ignored). The advertised `digest` is the entry's
`image_digest` without its `sha256:` prefix (64 lowercase hex), which is the
`InstalledHarness.environment.digest` C4 pins, and `harnesses` lists the
entry's exact `harness_versions`. Each resolved digest must also be a
`docker_local.templates` entry or the configuration is refused.
