# Codex workspace tools in Docker

This is an **explicit opt-in configuration for trusted workloads** on the
qualified Linux/amd64 host described below. It enables Codex 0.166.0's nested
bubblewrap sandbox while keeping the container non-root, its root filesystem
read-only, all capabilities dropped and no-new-privileges enabled. It does not
establish hostile-code isolation. The worker's launch defaults, placement
capabilities and qualification gates are unchanged.

## Qualified configuration

Use the checked-in
[Codex seccomp profile](../../tools/qualification/seccomp/codex-bwrap-amd64.json)
only with all the restrictions in this example. The image must already be
installed. This credential-free example uses fresh disposable state; it is not
a production worker launch or a credential-storage recipe.

From the repository root:

```sh
codex_seccomp_profile=$(realpath tools/qualification/seccomp/codex-bwrap-amd64.json)
docker run --rm --init \
  --read-only --user 10001:10001 --cap-drop ALL \
  --security-opt no-new-privileges=true \
  --security-opt "seccomp=$codex_seccomp_profile" \
  --network none --pids-limit 128 --memory 512m --cpus 1 \
  --tmpfs /state:rw,nosuid,nodev,mode=1777,size=64m \
  --tmpfs /tmp:rw,nosuid,nodev,mode=1777,size=64m \
  --entrypoint python3 \
  sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d \
  -c 'import os,pathlib,subprocess,sys
home=pathlib.Path("/state/codex"); home.mkdir(mode=0o700)
work=pathlib.Path("/state/workspace"); work.mkdir(mode=0o700)
os.environ["CODEX_HOME"]=str(home)
sys.exit(subprocess.run(["/opt/aq-runtime/codex","sandbox","-c","sandbox_mode=\"workspace-write\"","--","/bin/sh","-ec","echo ok > canary; cat canary"],cwd=work).returncode)'
```

Expected: `ok`, exit 0. The actual CLI syntax is `codex sandbox -- COMMAND`;
`sandbox linux` is obsolete for this installed binary. `CODEX_HOME` must be a
private writable directory outside `/tmp`, where this version sets up aliases.
The fresh workspace is private to UID 10001. No host home, credentials, Docker
socket or arbitrary host path is mounted. Docker's normal private `/dev` mounts
remain; the root filesystem itself is read-only. The example's `/state` and
`/tmp` limits and CPU/memory/PID limits bound this small probe, not a real build.

The `seccomp` file is read by the Docker client and sent to the daemon, so it
does not need a container bind mount. Do not change the daemon-wide default
profile. Do not add `--privileged`, `CAP_SYS_ADMIN`, root execution, host PID or
network namespaces, an unconfined LSM policy, or a host Docker socket to fix a
refusal. If the explicit profile fails on another host, preserve the refusal
and qualify that host separately; there is no automatic security fallback.

## Narrower syscall profile

Docker's default profile refused the original `sandbox -- /bin/true` probe with
`bwrap: No permissions to create new namespace`. An otherwise identical
`seccomp=unconfined` control succeeded. The narrower profile also succeeds,
without disabling the container seccomp filter.

The baseline is a pinned upstream Moby profile, **not a claim to have extracted
this Docker daemon's exact built-in profile**. Its original bytes, Apache-2.0
license and immutable source/hash metadata are retained in
[vendor/docker-seccomp](../../vendor/docker-seccomp/source.json). Both that
baseline and the actual daemon default were tested independently.

Exactly three rules are appended; every baseline rule and the default-deny
action are retained:

| Added allowance | Bound and reason |
| --- | --- |
| `clone` | On amd64, flags must contain `CLONE_NEWUSER` (`0x10000000`). Existing ordinary-process clone rules remain. Bubblewrap needs to create its user namespace. Other flag bits are not individually constrained by this added rule. |
| `unshare` | Flags must equal `CLONE_NEWUSER`, without extra bits. Bubblewrap creates another user namespace during setup. |
| `mount`, `umount2`, `pivot_root` | Needed to construct and switch the nested filesystem view. These three rules do not filter arguments. Kernel namespace/capability checks still apply. |

`clone3` still returns `ENOSYS` under the baseline's no-`CAP_SYS_ADMIN` rule,
allowing the libc fallback to `clone`. There is no added `setns`, `bpf`, keyring,
`io_uring`, new mount API or blanket syscall allowance. A broad six-call
exploratory candidate included `setns`; removing it still passed, so it was
excluded from the delivered profile.

Removing **any one** of the five added syscall allowances from the delivered
profile makes the original probe fail. This is necessity evidence for this
image and command; it does not prove a globally minimal profile for all Codex
tools. The exact requests, stdout, stderr, exit codes and profile hashes are in
the [qualification evidence](../implementation/evidence/codex-docker-seccomp/qualification.json).

## Qualification and limits

The retained qualification used:

- Image ID `sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d`,
  worker source `dc62cfc0faf2109485b1c9f17a024f6fcb37ebd5`, UID/GID 10001.
- Codex CLI 0.166.0; bubblewrap and executable hashes recorded by the probe.
- Docker Engine/Desktop 29.3.1, runc 1.3.4, Linux
  `6.18.33.2-microsoft-standard-WSL2`, amd64.
- Read-only root, empty capability sets, no-new-privileges, private tmpfs state,
  no network and the resource bounds shown above, with Docker `--init`.

| Check | Observed result |
| --- | --- |
| Actual Docker default + `sandbox -- /bin/true` | Exit 1, namespace creation refused |
| `seccomp=unconfined` control, other bounds retained | Exit 0 |
| Pinned upstream default profile | Exit 1, namespace creation refused |
| Narrow profile + original command | Exit 0 |
| Explicit `workspace-write` canary | Workspace write/read succeeds; writes outside the workspace and under `/etc` fail |
| UID, capability sets, NNP, seccomp inside Codex command | UID/GID 10001, all capability sets zero, NNP 1, seccomp filter mode 2 |
| Docker-layer negative syscall probes, outside Codex | `unshare(0)`, `unshare(CLONE_NEWNET)`, `keyctl`, `bpf`, `io_uring_setup`: `EPERM`; `clone3`: `ENOSYS` |
| Remove clone / unshare / mount / umount2 / pivot_root allowance | Each variant exits 1 at the corresponding bubblewrap setup step |

Negative syscall errno checks are compatibility observations. A refusal may
also be enforced by kernel capabilities or other host policy; the byte-for-byte
baseline comparison establishes which seccomp restrictions were retained.

User namespaces and mounting expand the reachable kernel surface. Seccomp
cannot constrain mount path strings through these rules. All processes in this
container receive the outer profile, not just bubblewrap. Container rootfs and
capability restrictions plus the Codex canary are bounded defense evidence,
not a security audit or a hostile-code boundary. The existing worker trust
model and stronger per-attempt isolation requirements still apply.

This qualification used no credentials, provider/model calls, real assignments,
credential refreshes or source-retention/settlement flows. It does not establish
provider connectivity, full worker admission, every workspace tool, rootless
Docker, ARM, another kernel/LSM/user-namespace policy, or end-to-end deployment
qualification. The local I0 image was not registry-published; its floating build
inputs do not establish a bit-for-bit rebuild. Requalify an updated image,
Codex, kernel, Docker/LSM policy or profile before adopting it.

## Repeat the checks

No Docker daemon or image is needed for the offline security-delta guards:

```sh
aq test tools/qualification/tests/test_codex_docker_seccomp.py -q
```

The opt-in Docker probe uses only the installed image, never pulls/builds an
image, forwards no host credentials, creates no host bind mounts, and makes no
provider calls. Choose a new output directory:

```sh
python3 tools/qualification/probe-codex-docker.py --output /tmp/codex-docker-qualification
```

It checks Linux/amd64, image user and Codex version, records the environment,
runs the controls/canary/negative probes and generates each removal variant.
Every container has a unique name, disposable state and a 30-second command
timeout; on timeout it attempts to remove only its own container. The script
returns nonzero on a mismatch and preserves partial evidence with `passed:
false`. This timeout/cleanup behavior is for these credential-free probes.
Production attempts require their existing lifecycle and retained-state rules.

Upstream references:

- [Docker seccomp documentation](https://docs.docker.com/engine/security/seccomp/)
  describes the default allowlist and explicit per-container profile option.
- [Pinned Moby baseline](https://github.com/moby/profiles/blob/6fe7deb1b9fb7c0397a4593480d7d22b9ee8caef/seccomp/default.json)
  defines the retained syscall rules.
- [Original I0 corrected requests](https://github.com/ElectricJack/aq-worker/blob/428aa27f3adbc0f330da563c70b33e3fdaaf7ce6/docs/implementation/evidence/i0-first-real-run/sandbox-probes-corrected.json)
  and [I0 image/build limits](https://github.com/ElectricJack/aq-worker/blob/428aa27f3adbc0f330da563c70b33e3fdaaf7ce6/docs/implementation/I0-first-real-run.md)
  establish the initial finding and retained image provenance.
