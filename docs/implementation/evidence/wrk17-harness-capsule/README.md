# Kernel-only harness capsule canary

`result.txt` records an explicit run of
`python3 tools/qualification/probe-harness-capsule.py` on 2026-11-01 and the
exact source hashes. The helper is statically compiled and uses Linux
syscalls directly. It does not call bwrap, tmux, Codex or a provider, and uses
only a child-network-namespace loopback connection. It consumes no Cargo
build slot. Run as a non-root host user with unprivileged namespaces enabled.

The original material is a fresh random 32-byte canary, not a real credential.
The parent holds its FD and another parent thread open. A clean helper exec
occurs before private namespace/PID1 construction so original parent memory
is not copied into a process visible inside the capsule. The final canary
tests denied known raw/proc/root/TID/FD paths and inherited FD/environment
absence, selected filesystem access, listener FD transfer, kernel credential
identity, pidfd exit and actual outer ECHILD.

This proves a bounded kernel mechanism in this environment. It does not
qualify a production mount allowlist, actual SourcePin handoff, tmux terminal
ownership, readonly named-image transition, active namespace escape attacks,
recovery, actual harness behavior or upstream provider semantics. All runtime
profiles and factory gates remain unchanged. See
[the candidate design](../../WRK-17-harness-capsule-candidate.md) for those
requirements and the proposed distinct profile.
