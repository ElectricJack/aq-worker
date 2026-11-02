# Composed worker source integration

The integration tree is byte-identical to the qualified final composed worker
source across crates, scripts, qualification tests, Cargo manifests/lock and
Cargo configuration. The pinned binary hash was independently checked.

This retains the native result producer, main CLI/shared helpers, ordinary
RunNative fixture entry point and bounded finite session polling. The native
producer evidence records 58 focused component tests; the composed source has
its own CLI, strict and build evidence. No actual full Codex/result/settlement
run or canonical rollout is claimed.
