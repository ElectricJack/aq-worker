# E0 native inputs: bounded MS0 owner attestation

Status: **blocked on the remaining owner qualification decisions**. This change
records the available inputs and a reproducible observer. It does not supply an
admissible native configuration or complete sharp-harbor-18/I0/MS0.

The owner decision on sharp-harbor-18, dated 2026-11-02 (comment timestamp
1793671179.038809), authorizes an agent to record observed local facts for the
owner: installed Codex CLI 0.166.0, the owner's ChatGPT file login, harness
`codex`, and E0's designated project/task/base as context and retention inputs.
Every record in [the report](evidence/e0-native-inputs/owner-attestation.json)
is labelled **owner-attested (recorded by agent for the owner)**. These records
are not measured qualification evidence.

## Observed inputs

- Host: `/home/jkern/.local/bin/codex --version` returns `codex-cli 0.166.0`.
- `/home/jkern/.codex/auth.json` is an owner-only regular file with mode 0600,
  `auth_mode` equal to `chatgpt`, and a nonempty token record. No token,
  account ID, auth fingerprint or private bytes are recorded. File presence
  does not prove that authentication currently works or that this file is the
  CLI's effective credential source. The host config does not explicitly set
  `cli_auth_credentials_store`.
- I0 local immutable runtime image:
  `sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d`.
  The observer rehashes all six installed programs and checks the image's
  Codex version, with networking disabled and no host mounts or credentials.
  Its hashes match the retained I0 `installation.json`. The image labels
  source revision `dc62cfc0faf2109485b1c9f17a024f6fcb37ebd5`; it has no
  `/usr/share/agent-q/build.json`. A source label is not build provenance, and
  the image predates the current worker branch.
- E0 saved receipts designate `https://github.com/ElectricJack/aq-scratch.git`,
  public branch `main`, Git credential reference `none`. Project/task IDs end
  in `000002`/`000004`; the retained source is SHA1
  `2e32645bab743729bfaeb0f2eec417c2ace7794b`, handle
  `e0000000-0000-4000-8000-000000000011`. The seed and retention exchanges
  agree. This change does not recheck live graph state or retention validity.
- E0's retained host vault still lacks `intelligence` and `profiled_execution`.
  Its saved development catalog contains `configuration-required` model/provider
  candidates. The observer reads the saved catalog; it does not query or
  activate a live catalog.

The source/retention receipt checksums in the report identify ordinary saved
files. They are not protocol `ArtifactRef` digests or independently qualified
runtime, harness, environment, scoped-presence or context evidence.

## Reproduce the authorized observation

Run from this worker checkout, as the owner, with the retained E0 stack files,
Docker and the already-present immutable I0 image. Use a fresh output path:

```sh
python3 scripts/observe-e0-native-inputs.py \
  --stack-root /home/jkern/.local/state/aq-platform-e0-final \
  --auth-file /home/jkern/.codex/auth.json \
  --codex-binary /home/jkern/.local/bin/codex \
  --runtime-image sha256:0b7defe69f3e4cfde180f1de332cdae169aa16ac71805875770c4537ce9ddc7d \
  --output /tmp/e0-owner-attestation-new.json
```

The output is a fresh 0600 report. The observer refuses symlink auth inputs,
unsafe ownership/mode, oversized or malformed inputs, duplicate JSON fields,
mutable image tags, inconsistent saved receipts and existing output files.
It does not copy the login or create a material lease. A zero exit code means
the observation succeeded; the report still says
`blocked_missing_owner_qualification`, with
`native_configuration_emitted: false`. Repeating it refreshes the observation
time, not a qualification's expiry. This authorization is specific to the
named owner's MS0 task; the script is not an approval mechanism for other users.

## Required inputs before configuration can be generated

The bounded owner authorization supplies none of the following decisions.
An owner may supply exact records or authorize a bounded qualification method;
the agent must not invent them from fixture artifacts.

| Input | Required decision/evidence | Consuming contract |
| --- | --- | --- |
| Model | Exact model ID, native reasoning, reasoning configuration reference and output limit; this worker supports provider `openai` | worker `native/runner/codex.rs`, E0 `aq-selection` model support |
| Context | Qualified counting multiplier, framing overhead, capacity kind/tokens, counting/capacity evidence and observation/expiry | E0 `profiled_execution/context_budget.rs` |
| Installation | Exact runtime, adapter, renderer, result-client/composite definition and manifest references, plus their matching installed bytes | worker `native/runner/config.rs` and `native/runner/codex.rs` |
| Qualification | Independently selected runtime, harness, environment and scoped-presence qualifications, including accepted limits | E0 `intelligence/config.rs`, `profiled_execution/config.rs`; worker preflight |
| Credential | Logical profile/revision, account/endpoint references, allowed node, owner-only secret-file placement and material-delivery policy | E0 `intelligence/config.rs` |
| Project/catalog | Project enabled/priority/source policy, validated retention handle, approved catalog and project-binding revision | E0 `profiled_execution/config.rs` and public profile/project APIs |

Context qualification matches the exact runtime, runtime qualification, CLI,
adapter, environment, harness qualification, model/reasoning and credential
profile/revision/account/endpoint tuple. Its expiry must be after observation
and at most 86,400,000 ms later. A small chosen number is not qualified capacity.
The type explicitly says its content-addressed artifact does not prove external
qualification.

I0's saved sandbox probes report that the real CLI sandbox needed Docker
`seccomp=unconfined`. The observer does not use that relaxation because it
only reads installed files and CLI version. Successful observation therefore
does not resolve the real sandbox/environment qualification decision.

## Configuration and private material continuation

After the missing inputs are supplied, the remaining work is to generate
`native.json`, inventory and installation/result-client manifests, definition
bodies, and matching E0 `intelligence`/`profiled_execution` configuration from
those approved records. Use the actual protocol digest/validation crates for
native artifact identities; do not treat this report's file checksums as those
identities. Validate the resulting closed documents with the pinned worker and
server implementations before changing the retained stack.

The private lease input must remain outside the repository/evidence. E0's
credential loader requires an absolute server-side `secret_file`, owned by the
server UID with no group/other permissions and 1–65,536 bytes. It derives
`provisionable` and, when `delivery_mode = "server_material_v1"`, the material
owner reference itself. Configuration cannot assert `provisionable = true` or
inject a material owner. A host path to the owner's auth file does not establish
server container access, ownership, authorized delivery or a lease. The final
choice needs an explicit private placement/rotation decision, matching native
ChatGPT-auth validation, and the ordinary server material issue/receipt flow.
No private copy has been made here.

Publish the approved catalog through the public activation API with the
observed expected revision, update the project binding through its public API,
and run the ordinary `check-native` and registration/poll path. Preserve E0's
source snapshot. Refusals remain refusals; no database edits, test-support
admission, fixture qualification substitutions or synthetic grants are part
of this path. None of these continuation steps has run in this change.

## Verification

`aq test tools/qualification/tests/test_e0_native_inputs.py -q` exercises
private-data exclusion, unsafe/duplicate auth refusal, fresh output preservation,
immutable networkless image probing, command-error redaction, attestation labels,
and saved-receipt disagreement. These tests use synthetic inputs and do not
qualify production behavior. The real observer command above separately records
the actual local observations. Both scopes remain explicit in the report.

Results on 2026-11-02: the focused file passed 7 tests and 4 subtests; the
related area command `aq test tools/qualification/tests -q` passed 12 tests
and 4 subtests after the final image-metadata handling change. Independent
comparison against I0's retained installation receipt matched all six program
hashes and byte sizes. `git diff --check` passed.

Remaining owner decisions were sent once to `session:supervisor-aq-worker` as
`msg-b1a772345fd8480295b6ad4c47624cd6`. The task remains held awaiting that reply.
Supervisor message `msg-af5cec572dc74892bfc88fcdf9d68ae9` confirms the
`origin/main` substitution because `origin/dev` is absent. Applied with
`aq git pull --branch main`: fetched `origin/main` is
`26edf5cf04550e1252dc69debcd5d20fdea68f9a`, already an ancestor of the
published checkpoint, so no merge commit or source change was needed. No push
to `main` occurred. The supervisor is escalating the remaining values to the
owner. This task cannot close successfully without them; the active claim is
retained pending the relayed decisions.
