# C2 findings: exact replacements for architecture 0005

Task: `calm-delta-18.2.1` (documentation handoff).

The supervisor authorized this handoff in message
`msg-1dd65840ad4d42f084488cda7ed44bec`: publish the replacements in
`aq-worker`; a separate aq-platform task applies them in the aq-server workspace.
This artifact does not change the canonical design by itself.

## Target and application

Target: `aq-server/docs/architecture/0005-sandboxed-execution-and-deployment-topology.md`.
The baseline read was aq-server `origin/main` commit
`fcb387bef76afba0489c3b84a0ab8f7a170d760b`, with document SHA-256
`d4a15dcc67f467c28179a02053f619d1c8ced6236bed524b2d717d57975b0b64`.

Apply each **Before** → **After** replacement below once. Each Before block
matches exactly once in that baseline. If the target changed, reconcile against
the current sections rather than replacing newer unrelated edits. Preserve all
other text and the existing SBX7 requirement. Run `git diff --check` and review
the resulting design in the destination repository before publishing it.

## Coverage and decisions

| Requested item | Replacements |
| --- | --- |
| §13 daemon decision and rootless detection | 2, 5, 6 |
| §5.1 cgroup admission and private archive delivery | 2, 5 |
| §8.3/§14 confinement probe and shared-kernel limits | 3, 4, 7, 8, 9 |
| SBX10/WSL2 uidmap prerequisite and single-UID caveat | 2, 5, 7, 8 |
| C1 template-presence preflight | 1 (recommendation; no Rust/API change) |

The single-UID fallback is recorded as a diagnostic qualification limit. It
cannot satisfy the existing runner/agent separation required by SBX7 and §8.3;
this handoff does not waive that requirement. Likewise, the cgroup admission
rule is required behavior, not a claim that C2 already checks every prerequisite.

## Evidence and verification limits

Sources are the parent task `calm-delta-18.2`, branch `aq/calm-delta-18.2`,
now committed at `267e9bdf094a93fcf77752ca53e6a551fa00e6a9`:

- `docs/implementation/C2.md`, especially “Proposed design-doc changes”,
  “Decision: confinement”, “Limitation: single-UID mapping” and WSL2 notes.
  SHA-256 at that commit: `8c5bf8aeac94a64d56333f9c7cf310e3dad8f4ffc9c04c4b812dee6e56ed52b0`.
- `docs/implementation/C2-evidence/confinement-probe.log`.
  Observed SHA-256: `4ae2c11e18a462788f69683e6a2a2b7f46a462f9028adbed8318d2bb42c9adae`.
- C1 `crates/aq-worker/src/backend/mod.rs`, `PlacementBackend`: no
  template-presence operation at this task's starting commit
  `dc62cfc0faf2109485b1c9f17a024f6fcb37ebd5`.

The evidence was first read from the parent's working tree, then reread from
the commit above after the parent committed it. The report's final verification
table changed; the five proposed design changes and probe stayed consistent.
The hashes pin the committed source content. This task did not rerun the live
probe, limits tests or multi-UID qualification, and does not infer which specific
kernel/runtime control denied UID mapping. Parent qualification results remain
the parent's evidence, rather than tests run by this documentation task.

The observed probe records:

```text
# Podman Engine 5.8.8, API 1.44
# WSL2 6.18.33.2-microsoft-standard-WSL2
# cgroup2fs; user_delegated=cpu memory pids
CapEff:  0000000000000000
NoNewPrivs:  1
Seccomp:  2
unshare -U true rc=0
unshare: write error: Operation not permitted
unshare -Ur id rc=1
```

## Exact replacements

### 1. §5: template-presence preflight

Before:

```markdown
Backend capabilities are advertised to placement and are part of eligibility. Examples are `vm-isolation`, `nested-docker`, `suspend-snapshot` and `persistent-disk-during-attempt`.
```

After:

```markdown
Backend capabilities are advertised to placement and are part of eligibility. Examples are `vm-isolation`, `nested-docker`, `suspend-snapshot` and `persistent-disk-during-attempt`.

**Proposed C1 contract extension:** add a template-presence operation, `ensure_template`, to `PlacementBackend` so admission can preflight an exact allowlisted digest through the common interface. It must confirm that the backend's materialization is present, or return a typed refusal or uncertainty; a failed check is not template availability. It does not create a sandbox or grant execution authority. C1 currently has no such trait operation, and C2's `pull_template` is backend-specific. The C1/admission owners should settle the signature and conformance coverage before adoption; this is a recommendation, not a claim that the method is implemented.
```

### 2. §5.1: daemon, limits, material and WSL2 prerequisites

Before:

```markdown
### 5.1 `docker-local` (desktop and any self-managed host)

- The worker node runs as a container with access to a **dedicated sandbox Docker daemon**, not the host's main Docker daemon. Rootless Docker or Podman is preferred.
- That daemon is used only for sandboxes. It holds only template images and sandbox containers.
- This deliberately revises the worker spec's "no host Docker socket" baseline for this node class. The worker spec already requires that projects needing extra capabilities be routed to an explicitly configured node class.
- The worker node's launch policy is fixed. Sandbox containers:
  - run only allowlisted template digests;
  - are never privileged;
  - have no host bind mounts except the worker-managed per-attempt volume and declared mount aliases (worker spec §5);
  - have CPU, memory and PID limits taken from the reservation;
  - get a default bridge network with outbound access only.
- Isolation claim: separate attempts cannot see each other's files, processes or credentials. **Local containers share the host kernel and are not claimed to be hostile-code isolation** (consistent with W6).
- Nested Docker for agents is not provided by default. A template or node can advertise `nested-docker` only through an explicitly configured runtime, for example Sysbox. Projects that require it route only to such nodes.
```

After:

```markdown
### 5.1 `docker-local` (desktop and any self-managed host)

- The worker node runs as a container with access to a **dedicated rootless sandbox daemon** through its Docker-compatible API on a dedicated per-user Unix socket. The selected implementation is rootless Podman ≥ 5; rootless `dockerd` is an equivalent alternative. The backend requires `/info` `SecurityOptions` to include `name=rootless`, and refuses a rootful daemon. The socket is never the host's main Docker socket.
- That daemon is used only for sandboxes. It holds only template images and sandbox containers, with its own storage and configuration separate from the host's main daemon.
- This deliberately revises the worker spec's "no host Docker socket" baseline for this node class. The worker spec already requires that projects needing extra capabilities be routed to an explicitly configured node class.
- The worker node's launch policy is fixed. Sandbox containers:
  - run only allowlisted template digests;
  - are never privileged;
  - drop all capabilities, set no-new-privileges, and retain the seccomp policy;
  - have no host bind mounts except the worker-managed per-attempt volume and declared mount aliases (worker spec §5);
  - have CPU, memory and PID limits taken from the reservation;
  - get a default bridge network with outbound access only.
- **Resource-limit prerequisite:** the host must use cgroup v2 and delegate the `cpu`, `memory` and `pids` controllers to the daemon's user. A host without that delegation is not admissible: Podman can otherwise silently drop the reservation limits. Qualification must read back effective limits (`cpu.max`, `memory.max`, `pids.max`) inside a sandbox; setting create-request fields alone is not proof of enforcement.
- **Private material:** upload an archive through the control API before the container starts (`PUT /containers/{id}/archive?path=/`). Its directory has mode `0700`, its file has mode `0600`, and both are owned by the intended runner identity. The material path is outside the work volume and every mount alias; aliases must not overlap or expose it. Material never goes in environment variables, command-line arguments or labels. These permissions provide runner/agent separation only with the distinct UID mappings required by SBX7.
- **WSL2 UID mapping:** install the `uidmap` package providing setuid `newuidmap` and `newgidmap`, and configure the daemon user's subordinate ranges in `/etc/subuid` and `/etc/subgid`. The C2 host lacked those helpers. `AQ_SANDBOX_SINGLE_UID=1` allowed diagnostic qualification with a single mapped host UID, but loses in-sandbox runner/agent UID separation. Mode `0600` does not restore that boundary. This fallback does not satisfy SBX7 or qualify agent execution requiring that separation; install the helpers and restart without the fallback before qualifying it. It is not a silent admission downgrade.
- Isolation claim: separate attempts cannot see each other's files, processes or credentials. **Local containers share the host kernel and are not claimed to be hostile-code isolation** (consistent with W6). The single-UID diagnostic run does not prove the SBX7 in-sandbox boundary.
- Nested Docker for agents is not provided by default. A template or node can advertise `nested-docker` only through an explicitly configured runtime, for example Sysbox. Projects that require it route only to such nodes.
```

### 3. §8.3: Codex native-mode prerequisites (table row)

Before:

```markdown
For Codex on Linux, that means bubblewrap plus unprivileged user-namespace creation (see below).
```

After:

```markdown
For Codex on Linux, that means bubblewrap plus unprivileged user-namespace creation and UID mapping (see below).
```

### 4. §8.3: replace the docker-local/Cloudflare native-mode bullet

Before:

```markdown
  - On `docker-local` and `cloudflare-containers`, Docker's default seccomp/AppArmor profiles block the user namespaces bubblewrap needs. **`docker-local` therefore defaults to `container` mode** (decided 2026-11-02). Its execution profiles permit `container` mode, and the sandbox is the boundary. `native` mode on `docker-local` may be enabled later only if a qualification shows that the sandbox daemon can safely permit unprivileged user namespaces.
```

After:

```markdown
  - On `docker-local`, C2's WSL2 probe under the fixed launch policy recorded `CapEff=0`, `NoNewPrivs=1` and `Seccomp=2`. `unshare -U true` succeeded, but `unshare -Ur id` failed when writing the UID map with `EPERM` ("Operation not permitted"). User-namespace creation alone is therefore insufficient for Codex's bubblewrap sandbox. **Codex `native` is unavailable in the qualified docker-local configuration; confinement stays `container`.** Profiles must explicitly permit `container`; a profile requiring `native` is refused. The probe does not establish which host or runtime control denied mapping. Reconsidering `native` requires a new qualification under the fixed policy, not an automatic fallback or relaxed launch policy. Shared-kernel containers are not hostile-code isolation. See section 14 for the evidence and the single-UID qualification limit.
  - On `cloudflare-containers`, user-namespace creation and UID mapping remain to be qualified independently. The docker-local probe establishes no Cloudflare result.
```

### 5. §10: replace the SBX10 requirement row

Before:

```markdown
| SBX10 | `docker-local` uses a dedicated sandbox daemon with a fixed, non-privileged launch policy and resource limits; it does not claim hostile-code isolation. |
```

After:

```markdown
| SBX10 | `docker-local` uses a dedicated per-user rootless Podman ≥ 5 or rootless `dockerd` daemon through its Docker-compatible API; `/info` must report `name=rootless`. Its fixed, non-privileged launch policy enforces reservation CPU, memory and PID limits through cgroup v2 user delegation; hosts lacking `cpu`, `memory` or `pids` delegation are not admissible. Private material is archived before start into a `0700` directory and `0600` file outside the work volume and mount aliases. On WSL2, `uidmap` and subordinate UID/GID ranges are prerequisites for SBX7 separation; `AQ_SANDBOX_SINGLE_UID=1` is a diagnostic fallback that loses that separation and does not meet SBX7. Shared-kernel containers do not claim hostile-code isolation. |
```

### 6. §13: replace the daemon-choice bullet

Before:

```markdown
- The choice between rootless Docker and Podman for the sandbox daemon on WSL2.
```

After:

```markdown
- The sandbox daemon choice is resolved by C2: rootless Podman ≥ 5 via its Docker-compatible API on a dedicated per-user socket; rootless `dockerd` is an equivalent alternative. The backend checks `/info` `SecurityOptions` for `name=rootless`. Socket layout and service installation must preserve the dedicated scope and the cgroup/UID-mapping prerequisites in section 5.1.
```

### 7. §14: replace the introductory sentence

Before:

```markdown
These are platform facts this design depends on but did not test:
```

After:

```markdown
The C2 docker-local qualification on 2026-11-02 used rootless Podman 5.8.8 (Docker-compatible API 1.44) on WSL2 kernel `6.18.33.2-microsoft-standard-WSL2`. Its confinement probe is recorded in `aq-worker` task `calm-delta-18.2`, `docs/implementation/C2-evidence/confinement-probe.log`; the findings and setup are in `docs/implementation/C2.md`. Under `CapEff=0`, `NoNewPrivs=1`, `Seccomp=2`, `unshare -U true` returned 0, while `unshare -Ur id` failed writing the UID map with `EPERM`. This configuration does not support Codex `native`; `container` remains the explicit confinement mode. The run used `AQ_SANDBOX_SINGLE_UID=1` because setuid UID-mapping helpers were missing, so it did not qualify SBX7 runner/agent separation. Install `uidmap` and requalify with distinct mappings; any future native-mode claim requires a fresh probe under the fixed policy. Do not infer results for other hosts or backends from this run.

These platform facts remain to be verified:
```

### 8. §14: replace the pending user-namespace bullet

Before:

```markdown
- **Unprivileged user namespaces** (bubblewrap) under the sandbox Docker daemon on WSL2, and in Cloudflare Containers.
```

After:

```markdown
- **Confinement follow-up:** qualify runner/agent separation on WSL2 after installing `uidmap` and disabling the single-UID fallback. Codex `native` remains unavailable for the C2 docker-local configuration; any proposed change needs both user-namespace creation and UID-mapping evidence under the fixed policy. Qualify bubblewrap independently in Cloudflare Containers.
```

### 9. §15: replace decision 14 to remove the contradicted rationale

Before:

```markdown
14. **`docker-local` defaults to `container` confinement,** because Docker blocks the user namespaces Codex's bubblewrap sandbox needs.
```

After:

```markdown
14. **`docker-local` stays at `container` confinement.** C2 showed that the fixed policy permits user-namespace creation but refuses the UID-map write Codex's bubblewrap sandbox needs (sections 8.3 and 14). Codex `native` is unavailable in that configuration; shared-kernel containers are not hostile-code isolation.
```
