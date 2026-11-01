# Named read-only image / self-helper kernel feasibility

Explicit opt-in probe on Linux 6.18.33.2-microsoft-standard-WSL2, x86_64.
Run `python3 tools/qualification/probe-named-readonly.py` from the repository root.
The probe requires local cc/static libc, unprivileged user namespaces and bwrap;
none is added as a runtime dependency of AQ modules. No Codex/model/provider runs.

The probe compiles one small static program, creates a new user/mount/PID/network
namespace with capabilities dropped, maps the image at `/opt/aq-image/codex`
read-only, and provides only separate private state for its helper alias. The
original writable source path and operator homes are absent inside this root.
The ordinary host UID is retained inside the user namespace; forcing UID0 was
not used for the recorded descendant-remount case.

Assertions in the final successful run:

- Before execution, opening the named image for write fails with EROFS (30).
- fstatvfs on its retained descriptor reports ST_RDONLY, and statx exposes a
  mount ID. Current executable and canonical path are both the stable named path.
- While executing, named and /proc/self/fd write opens fail with ETXTBSY (26).
  This result alone is not the filesystem immutability proof; the earlier
  inactive-image EROFS assertion is necessary.
- Direct remount to writable fails with EPERM (1). A descendant successfully
  enters another user/mount namespace but its remount command is also denied;
  the test requires the mount-stage permission failure, not an earlier failure
  to create/map the descendant namespace.
- Inheritable/permitted/effective/bounding/ambient capability sets are all zero.
- A private helper alias re-execs the stable named path; the second invocation
  verifies that its device/inode exactly matches the first invocation.

[result.txt](result.txt) records the successful output. This proves the narrow
kernel/path mechanism in this exact test environment. It does not qualify actual
Codex tools, container provisioning, provider semantics or production isolation.
The original host-side source is outside the qualified child boundary and remains
trusted; read-only bind flags alone do not protect against a trusted external
provisioner mutating that source.

Separate exploratory probes found that a direct bind mount from a sealed memfd
FD failed EINVAL although an ordinary file control bind succeeded. A synthetic
root alias for the deleted memfd name permitted raw exec but failed realpath with
ELOOP. Neither mechanism is used as a production fallback or part of this probe.
