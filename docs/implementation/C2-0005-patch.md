# C2 findings: corrective delta for architecture 0005

Revision task: `grand-lantern-12`; previous revision task: `eager-falcon-68`;
original handoff: `calm-delta-18.2.1`; review: `rev-fleet-zenith` (revision 3,
revises revision 2).

This handoff is published in `aq-worker` for a separately authorized aq-platform
applier. It does not edit or approve the canonical aq-server design, change Rust
or admission behavior, or qualify a runtime configuration. SBX7 remains un-waived.

## Target, current baseline and supersession

Target: `aq-server/docs/architecture/0005-sandboxed-execution-and-deployment-topology.md`.
Baseline: aq-server remote `main` at
`841e19ff6260ce7a97c600227e94c3c42c1abec5`, independently checked with
`git ls-remote origin refs/heads/main` on 2026-11-02. Document SHA-256:
`42d0fdb5ec10d5358cd08dce8022f42467f68eff3b1e2232945148142190ec07`.
This is newer than review feedback's `c13cf4bf1ba1aa382af147ac9e502e1017bc7473`
and has the same target-document bytes. For revision 3, `git fetch origin main`
on 2026-11-02 returned `2917c84ea1459cf7bff1d7dad07aad7b37d8194b`, whose target
document has the same SHA-256.

**The application gate is the document SHA-256 above, not the commit.** A newer
aq-server `main` whose target document has identical bytes is still the pinned
baseline: apply this handoff unchanged. Only a different document hash makes
this delta stale (see "Application and acceptance").

Commit `18d89f859953cdee08da17af4412965bbc1f11c8` already applied the original
handoff. **Do not replay any of revision 1's nine replacements.** Original
replacements 1–6, 8 and 9 are already applied. Original replacement 1 is unsafe
to replay: its Before text remains present and would duplicate the proposed C1
contract paragraph. Original replacement 7 is also applied, with an additional
evidence paragraph retained below its diagnostic paragraph. That evidence
paragraph is no longer frozen: correction G repoints its handoff citation from
the retracted `20791e35` snapshot to the corrected handoff and renames the run
it refers to. Its two `267e9bdf` evidence links stay byte-for-byte intact.

| Original replacement | Current state and action in this revision |
| --- | --- |
| 1: §5 C1 template-presence proposal | Applied once; preserve it and do not reapply. No API implementation claimed. |
| 2: §5.1 daemon, limits, material, WSL2 | Applied; preserve daemon, resource and material text. Correction A changes only the WSL2 bullet. |
| 3: §8.3 native prerequisite table | Applied; preserve the table, including container's SBX7 gate. |
| 4: §8.3 docker-local/Cloudflare bullets | Applied; correction B changes only docker-local. Cloudflare remains independent. |
| 5: SBX10 | Applied; correction C adds identity/admission conditions. |
| 6: §13 daemon choice | Applied; preserve unchanged. |
| 7: §14 introduction | Applied plus evidence paragraph; correction D replaces the diagnostic paragraph and correction G replaces the evidence paragraph's handoff citation and closing sentence. |
| 8: §14 follow-up | Applied; correction E scopes requalification. |
| 9: §15 decision 14 | Applied; correction F removes the unsupported policy-wide inference. |

This revision supersedes revision 1's application instructions and evidence
presentation. It retains the original daemon choice, cgroup prerequisites,
archive delivery, shared-kernel limitation and C1 recommendation. It corrects
the diagnostic scope and admission consequences; it drops no SBX7 requirement.
Corrections A–G below are the entire live delta against current main, rather
than reapplications of the original changes. Corrections outside §14 are needed
because the unsupported conclusions were already copied into §§5.1, 8.3, 10 and
15. Correction G is needed because §14 otherwise cites the retracted handoff,
which still asserts those conclusions, as the record of the verification limits. There is no remaining bulk application of original replacement 7.

## Evidence and verification limits

Evidence remains pinned to aq-worker `267e9bdf094a93fcf77752ca53e6a551fa00e6a9`:

- `docs/implementation/C2.md`, SHA-256
  `8c5bf8aeac94a64d56333f9c7cf310e3dad8f4ffc9c04c4b812dee6e56ed52b0`:
  the single-UID limitation and post-uidmap follow-up scope the probe.
- `docs/implementation/C2-evidence/confinement-probe.log`, SHA-256
  `4ae2c11e18a462788f69683e6a2a2b7f46a462f9028adbed8318d2bb42c9adae`.
- `scripts/sandbox-daemon/aq-sandbox-podman.sh`, `single_uid`: hides subordinate
  ranges using empty subuid/subgid files; its fallback maps one host UID.
- `crates/aq-worker/src/backend/docker_local/config.rs`, `DockerLocalConfig` and
  `validate`: `material_uid` and `material_gid` default to zero; validation does
  not reject those defaults or establish distinct runner/agent identities.
- `crates/aq-worker/src/backend/docker_local/tar.rs`, `material_archive`: owns
  the 0700 directory and 0600 material file using the configured UID/GID.

The pinned report and probe hashes were rechecked for revisions 2 and 3. The code
facts were also checked in this task's starting checkout
`9a00f05142957accfdb66cc7a7f40fe9e563356e`.
No live probe, post-uidmap separation test, bubblewrap qualification or resource
limit test was rerun. `unshare` success is not itself full native qualification.
The denying control remains unknown; single-UID mapping is a candidate cause,
not proof of a fixed-policy restriction. The admission refusals below are
required design behavior; this document does not claim C2 implements them.

The complete pinned probe log follows verbatim, including its command and
single-UID warning (tabs in status lines are retained). Its SHA-256 applies to
the raw log bytes, including the final newline, rather than the Markdown fence:

```text
# Confinement probe, 2026-11-02T16:39:30-08:00, 6.18.33.2-microsoft-standard-WSL2
# host: max_user_namespaces=128331 cgroup_fs=cgroup2fs user_delegated=cpu memory pids
# daemon: Podman Engine 5.8.8 api 1.44
$ aq-sandbox-podman.sh podman run --rm --cap-drop=ALL --security-opt no-new-privileges --network bridge docker.io/library/busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec sh -c 'grep -E "^(CapEff|NoNewPrivs|Seccomp):" /proc/self/status; unshare -U true; echo "unshare -U true rc=$?"; unshare -Ur id; echo "unshare -Ur id rc=$?"'
time="2026-11-02T16:39:44-08:00" level=warning msg="Additional gid=10 is not present in the user namespace, skip setting it"
CapEff:	0000000000000000
NoNewPrivs:	1
Seccomp:	2
unshare -U true rc=0
unshare: write error: Operation not permitted
unshare -Ur id rc=1
```

## Exact corrective replacements

### A. §5.1: WSL2 admission and runner identity

Before:

```markdown
- **WSL2 UID mapping:** install the `uidmap` package providing setuid `newuidmap` and `newgidmap`, and configure the daemon user's subordinate ranges in `/etc/subuid` and `/etc/subgid`. The C2 host lacked those helpers. `AQ_SANDBOX_SINGLE_UID=1` allowed diagnostic qualification with a single mapped host UID, but loses in-sandbox runner/agent UID separation. Mode `0600` does not restore that boundary. This fallback does not satisfy SBX7 or qualify agent execution requiring that separation; install the helpers and restart without the fallback before qualifying it. It is not a silent admission downgrade.
```

After:

```markdown
- **WSL2 UID mapping and execution admission:** install the `uidmap` package providing setuid `newuidmap` and `newgidmap`, configure the daemon user's subordinate ranges in `/etc/subuid` and `/etc/subgid`, and restart without `AQ_SANDBOX_SINGLE_UID`. Configure distinct non-root runner and agent OS identities, with `material_uid` and `material_gid` set to the runner's nonzero UID and GID; verify that the agent cannot read the runner's private material. Installing `uidmap` alone does not establish SBX7 separation. C2's 2026-11-02 WSL2 diagnostic run used `AQ_SANDBOX_SINGLE_UID=1` because those helpers were missing. That fallback maps a single host UID and cannot provide the required separation; mode `0600` does not restore it. **A host using this fallback is not admissible for agent execution and has no permitted confinement mode:** Codex `native` is unqualified and `container` requires SBX7 under section 8.3. Admission must refuse agent execution until distinct mappings and runner/agent separation are qualified. These are required admission conditions, not a claim that C2 already enforces them: C2's `material_uid` and `material_gid` default to `0`, and its configuration validation does not reject those defaults.
```

### B. §8.3: scope the probe and state the confinement consequence

Before:

```markdown
  - On `docker-local`, C2's WSL2 probe under the fixed launch policy recorded `CapEff=0`, `NoNewPrivs=1` and `Seccomp=2`. `unshare -U true` succeeded, but `unshare -Ur id` failed when writing the UID map with `EPERM` ("Operation not permitted"). User-namespace creation alone is therefore insufficient for Codex's bubblewrap sandbox. **Codex `native` is unavailable in the qualified docker-local configuration; confinement stays `container`.** Profiles must explicitly permit `container`; a profile requiring `native` is refused. The probe does not establish which host or runtime control denied mapping. Reconsidering `native` requires a new qualification under the fixed policy, not an automatic fallback or relaxed launch policy. Shared-kernel containers are not hostile-code isolation. See section 14 for the evidence and the single-UID qualification limit.
```

After:

```markdown
  - On `docker-local`, C2's 2026-11-02 WSL2 diagnostic probe used `AQ_SANDBOX_SINGLE_UID=1` and recorded `CapEff=0`, `NoNewPrivs=1` and `Seccomp=2`. `unshare -U true` succeeded, while `unshare -Ur id` failed writing the UID map with `EPERM` ("Operation not permitted"). This proves that namespace creation alone did not establish the UID mapping needed by Codex's bubblewrap sandbox in that single-UID run. It does not identify the denying control or prove that the fixed launch policy prevents mapping after the UID helpers and ranges are installed. **Codex `native` remains unqualified on docker-local pending a fresh post-uidmap probe.** `container` remains the intended explicit mode only on a host qualified for SBX7 separation and permitted by its execution profile; a profile requiring unqualified `native` is refused. **The probed single-UID host has no admissible confinement mode and must refuse agent execution**, because `native` is unqualified and `container` requires SBX7. Install `uidmap`, disable the fallback, configure distinct non-root runner and agent identities with material owned by the runner, and qualify separation before admitting attempts. Reconsidering `native` requires fresh evidence of namespace creation and UID mapping under the fixed policy; no automatic fallback or relaxation is authorized. Shared-kernel containers are not hostile-code isolation. See sections 5.1 and 14 for prerequisites and evidence limits.
```

### C. §10: complete SBX10 prerequisites

Before:

```markdown
| SBX10 | `docker-local` uses a dedicated per-user rootless Podman ≥ 5 or rootless `dockerd` daemon through its Docker-compatible API; `/info` must report `name=rootless`. Its fixed, non-privileged launch policy enforces reservation CPU, memory and PID limits through cgroup v2 user delegation; hosts lacking `cpu`, `memory` or `pids` delegation are not admissible. Private material is archived before start into a `0700` directory and `0600` file outside the work volume and mount aliases. On WSL2, `uidmap` and subordinate UID/GID ranges are prerequisites for SBX7 separation; `AQ_SANDBOX_SINGLE_UID=1` is a diagnostic fallback that loses that separation and does not meet SBX7. Shared-kernel containers do not claim hostile-code isolation. |
```

After:

```markdown
| SBX10 | `docker-local` uses a dedicated per-user rootless Podman ≥ 5 or rootless `dockerd` daemon through its Docker-compatible API; `/info` must report `name=rootless`. Its fixed, non-privileged launch policy enforces reservation CPU, memory and PID limits through cgroup v2 user delegation; hosts lacking `cpu`, `memory` or `pids` delegation are not admissible. Private material is archived before start into a `0700` directory and `0600` file outside the work volume and mount aliases. SBX7 requires distinct non-root runner and agent OS identities, with `material_uid` and `material_gid` explicitly set to the runner's nonzero UID and GID, and qualified denial of agent access to that material. On WSL2, install `uidmap`, configure subordinate UID/GID ranges, and disable `AQ_SANDBOX_SINGLE_UID` before qualifying that separation. The single-UID fallback is diagnostic only, fails SBX7, and is not admissible for agent execution in any confinement mode (section 8.3). C2's zero-valued material-owner defaults and current validation do not enforce these admission conditions. Shared-kernel containers do not claim hostile-code isolation. |
```

### D. §14: replace the diagnostic paragraph

Before:

```markdown
The C2 docker-local qualification on 2026-11-02 used rootless Podman 5.8.8 (Docker-compatible API 1.44) on WSL2 kernel `6.18.33.2-microsoft-standard-WSL2`. Its confinement probe is recorded in `aq-worker` task `calm-delta-18.2`, `docs/implementation/C2-evidence/confinement-probe.log`; the findings and setup are in `docs/implementation/C2.md`. Under `CapEff=0`, `NoNewPrivs=1`, `Seccomp=2`, `unshare -U true` returned 0, while `unshare -Ur id` failed writing the UID map with `EPERM`. This configuration does not support Codex `native`; `container` remains the explicit confinement mode. The run used `AQ_SANDBOX_SINGLE_UID=1` because setuid UID-mapping helpers were missing, so it did not qualify SBX7 runner/agent separation. Install `uidmap` and requalify with distinct mappings; any future native-mode claim requires a fresh probe under the fixed policy. Do not infer results for other hosts or backends from this run.
```

After:

```markdown
The C2 docker-local diagnostic run on 2026-11-02 used rootless Podman 5.8.8 (Docker-compatible API 1.44) on WSL2 kernel `6.18.33.2-microsoft-standard-WSL2`. Its confinement probe is recorded in `aq-worker` task `calm-delta-18.2`, `docs/implementation/C2-evidence/confinement-probe.log`; the findings and setup are in `docs/implementation/C2.md`. The run used `AQ_SANDBOX_SINGLE_UID=1` because setuid UID-mapping helpers were missing; the log's "Additional gid=10 is not present in the user namespace, skip setting it" warning records that limitation. With `CapEff=0`, `NoNewPrivs=1` and `Seccomp=2`, `unshare -U true` returned 0, while `unshare -Ur id` failed writing the UID map with `EPERM`. This observation applies to that single-UID configuration and does not establish which control denied mapping or what a post-uidmap run will do. Codex `native` remains unqualified pending that fresh probe. The run also did not qualify SBX7 runner/agent separation, so the probed host has no admissible confinement mode and must refuse agent execution. Install `uidmap`, configure subordinate ranges, disable the fallback, and configure distinct non-root runner and agent identities with `material_uid`/`material_gid` set to the runner before qualifying separation. `container` is intended only after those prerequisites and explicit profile permission are satisfied. Any future native-mode claim needs fresh namespace-creation and UID-mapping evidence under the fixed policy. Do not infer results for other hosts or backends from this run.
```

### E. §14: replace only the confinement follow-up bullet

Before:

```markdown
- **Confinement follow-up:** qualify runner/agent separation on WSL2 after installing `uidmap` and disabling the single-UID fallback. Codex `native` remains unavailable for the C2 docker-local configuration; any proposed change needs both user-namespace creation and UID-mapping evidence under the fixed policy. Qualify bubblewrap independently in Cloudflare Containers.
```

After:

```markdown
- **Confinement follow-up:** on WSL2, install `uidmap`, configure subordinate UID/GID ranges, disable `AQ_SANDBOX_SINGLE_UID`, and configure distinct non-root runner and agent identities with `material_uid`/`material_gid` set to the runner. Qualify that the agent cannot read runner material before admitting any agent execution; the single-UID diagnostic host has no admissible confinement mode. Codex `native` remains unqualified on docker-local pending a fresh post-uidmap namespace-creation and UID-mapping probe under the fixed policy. Qualify bubblewrap independently in Cloudflare Containers.
```

### F. §15: correct decision 14 without generalizing the probe

Before:

```markdown
14. **`docker-local` stays at `container` confinement.** C2 showed that the fixed policy permits user-namespace creation but refuses the UID-map write Codex's bubblewrap sandbox needs (sections 8.3 and 14). Codex `native` is unavailable in that configuration; shared-kernel containers are not hostile-code isolation.
```

After:

```markdown
14. **`docker-local` intends explicit `container` confinement only where SBX7 separation is qualified and the execution profile permits it.** C2's 2026-11-02 single-UID WSL2 diagnostic run allowed namespace creation but failed the UID-map write; it did not establish the cause or qualify a normal multi-UID setup. Codex `native` remains unqualified on docker-local pending a fresh post-uidmap probe under the fixed policy. The probed single-UID host has no admissible confinement mode and must refuse agent execution until the UID-mapping and distinct non-root runner/agent prerequisites in sections 5.1, 8.3 and 14 are qualified. Shared-kernel containers are not hostile-code isolation.
```

### G. §14: cite the corrected handoff and name the diagnostic run

This corrects the evidence paragraph that directly follows D's paragraph. Only
the handoff citation and the closing sentence change; both `267e9bdf` evidence
links are unchanged. The new link pins aq-worker
`09d74f72f1bfcf333279e881cdc09c0b05c08a41`, which is on aq-worker `main` and
holds revision 2 of this handoff (SHA-256
`66d232e8b1e7c0b45562a5beb46902d6d9f5551ecdc019932513c3dd5809da8c`). Its
"Evidence and verification limits" section, including both hashes, is the
record cited and is unchanged in revision 3. Revision 3's own commit cannot cite
itself, so the link deliberately pins revision 2. The retracted snapshot is
named only as superseded, not linked.

Before:

```markdown
Evidence is pinned to aq-worker commit `267e9bdf094a93fcf77752ca53e6a551fa00e6a9`: [C2 findings and setup](https://github.com/ElectricJack/aq-worker/blob/267e9bdf094a93fcf77752ca53e6a551fa00e6a9/docs/implementation/C2.md) and [confinement probe](https://github.com/ElectricJack/aq-worker/blob/267e9bdf094a93fcf77752ca53e6a551fa00e6a9/docs/implementation/C2-evidence/confinement-probe.log). The [design patch handoff](https://github.com/ElectricJack/aq-worker/blob/20791e356df9f5d5b45031ca14304672b071d500/docs/implementation/C2-0005-patch.md) records their SHA-256 hashes and the verification limits. This documentation update does not rerun or extend that qualification.
```

After:

```markdown
Evidence is pinned to aq-worker commit `267e9bdf094a93fcf77752ca53e6a551fa00e6a9`: [C2 findings and setup](https://github.com/ElectricJack/aq-worker/blob/267e9bdf094a93fcf77752ca53e6a551fa00e6a9/docs/implementation/C2.md) and [confinement probe](https://github.com/ElectricJack/aq-worker/blob/267e9bdf094a93fcf77752ca53e6a551fa00e6a9/docs/implementation/C2-evidence/confinement-probe.log). The [corrective design handoff](https://github.com/ElectricJack/aq-worker/blob/09d74f72f1bfcf333279e881cdc09c0b05c08a41/docs/implementation/C2-0005-patch.md) records their SHA-256 hashes and the verification limits of the single-UID diagnostic run; it supersedes the confinement conclusions of the earlier handoff at aq-worker `20791e356df9f5d5b45031ca14304672b071d500`, which must not be cited for them. This documentation update does not rerun or extend that diagnostic run and qualifies no configuration.
```

## Application and acceptance

Before editing, read the destination's applicable instructions. If all seven
After blocks already occur once and no Before block occurs, this revision is
already applied: make no edit. Otherwise check the current document's SHA-256;
the commit it came from does not matter. If the hash equals the pinned baseline,
apply as written. If it differs, inspect the current sections and regenerate
an exact corrective delta; do not replay old replacements or overwrite unrelated
changes. Apply only A–G, each with exactly one Before match and no After match.
A mixed state requires reconciliation before publication.

Preserve the §8.3 confinement table, SBX7 row, §13 daemon choice, the single
existing C1 proposal and both `267e9bdf` evidence links in the §14 evidence
paragraph. After application, each After block must appear exactly once and
all seven Before blocks must be absent. Review the seven-line diff and run
`git diff --check` in the destination. Verify these facts:

1. The dated single-UID probe is diagnostic evidence only. Codex `native` is
   unqualified pending a post-uidmap probe; no cause is attributed to fixed policy.
2. Both §5.1 and §8.3 explicitly forbid agent execution on the single-UID host:
   it has no admissible confinement mode. Container's SBX7 gate is unchanged.
3. §5.1 and SBX10 require distinct non-root runner and agent identities and
   explicit runner material UID/GID, plus mapping and separation qualification.
   Defaults of zero and current validation are not evidence of enforcement.
4. The §14 evidence paragraph keeps both `267e9bdf` links byte-for-byte, links
   the corrected handoff at `09d74f72`, names `20791e35` only as superseded and
   links it nowhere, and calls the run a diagnostic run, not a qualification.
   The C1 proposal appears once. Cloudflare has no inferred qualification
   result. SBX7 is unchanged.

This is documentation validation, not runtime qualification. A downstream applier
must publish through its own authorized workflow; review submission approves
neither this handoff nor the canonical design.

### Verification performed for this revision

For revision 3, a Python check (`grand-lantern-12` scratchpad `verify_r3.py`)
loaded the target from aq-server `841e19ff` and from current `origin/main`
`2917c84e`, verified that both have the pinned SHA-256, parsed these fourteen
fenced Before/After blocks, and applied the seven replacements in memory. Each
Before matched exactly once and no After was present beforehand. The result
contained each After once and no Before, and the diff changed exactly seven
lines: 216 (A), 367 (B), 410 (C), 466 (D), 468 (G), 484 (E) and 510 (F). A
repeat application returned an already-applied no-op, and a partially applied
input was rejected. Every other line, including the C1 paragraph, the §8.3
table and the SBX7 row, was byte-identical. In the result, the `20791e35` URL
does not occur, `20791e35` appears only in G's supersession clause, the
`09d74f72` URL occurs once, and "that qualification" does not occur. The
quoted probe log matched the committed `267e9bdf` bytes, and both evidence
hashes and the `09d74f72` handoff hash matched. `git diff --check` passed for
this handoff and for the generated destination diff. The checks can be repeated
from the pinned commit using the matching and preservation conditions above.

## Review finding dispositions

| Finding | Disposition and completion evidence |
| --- | --- |
| cmt-276bf64009d7 | fixed (revision 2, kept): current remote-main pin and target hash above; all original replacements marked applied and forbidden to replay; only A–G are live. Exact application and already-applied checks prevent duplication. |
| cmt-98e74931882a | fixed (revision 2, completed in revision 3): B, D, E and F scope results to the dated single-UID run, leave the denying control unknown, and require fresh post-uidmap qualification. G removes the last path to the withdrawn conclusions: §14 no longer links the retracted handoff. |
| cmt-32b5090ba8bc | fixed (revision 2, kept): A, B, C, D, E and F explicitly state no admissible confinement mode on the single-UID host; A/B require refusal of agent execution. The container table and SBX7 remain unchanged. |
| cmt-7c6f86506418 | fixed (revision 2, kept): A and C require distinct non-root runner and agent identities and runner material UID/GID; the evidence/limits section records zero defaults and lack of enforcement. Runtime validation changes are outside this documentation handoff; no implementation is claimed. |
| cmt-32b617171fbb | fixed (revision 2, kept): the complete committed log is quoted verbatim above, including the command, gid warning, tabs and timestamped headers; its raw-byte hash is pinned and rechecked. |
| cmt-ce6f3513df74 | fixed: new correction G repoints the §14 evidence paragraph's handoff link from `20791e35` to the corrected handoff at `09d74f72`, names `20791e35` only as superseded, and replaces "that qualification" with "that diagnostic run". The `267e9bdf` links stay intact. The baseline, preservation list and acceptance check 4 no longer freeze that paragraph; they name its new expected bytes. |
| cmt-e9cd59a65833 | fixed: the baseline section now says the gate is the document SHA-256 and a newer `main` with identical target bytes is still the pinned baseline; the application section repeats it. Current `origin/main` `2917c84e` was checked and has those bytes. |
| overall_note | fixed: revision 3 adds correction G for the remaining blocking finding and states the hash gate for the nit. All revision 2 resolutions remain. SBX7 is preserved, and the delta awaits final approval through the assigned review; submission does not autoapprove it. |
