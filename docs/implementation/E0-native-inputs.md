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

## Owner method decision and E0 application blocker (claim epoch 5)

Owner decision 2026-11-02 (task comment `1793676397.3807435`), recorded as
owner-approved: model `gpt-6.1-sol` with `native_reasoning = high` (v1
standard-high Codex class) and the Codex default `max_output_tokens`. Up to
three short real calls on the owner's ChatGPT login may be used to measure the
counting multiplier, overhead and capacity, recorded as measured evidence.
`seccomp=unconfined` is approved only for the local I0/E0 qualification
container. Credential profile `owner-chatgpt-codex` revision 1 uses the
owner's ChatGPT account and the Codex default endpoint, and allows only the
desktop dev worker node. Catalog and project-policy revision 2 apply to
`ElectricJack/aq-scratch`. Default branch: `main`.

That decision does not authorize changing E0. The E0 stack also gives no
public route to apply the server configuration. I checked this in the E0
source snapshot `8f6c49f` under `~/.local/state/aq-platform-e0-final/source`:

- `scripts/dev-stack.py` `prepare()` (lines 32-69) always builds
  `vault/config.toml` from `vault.example/config.toml` plus two fixed blocks.
  These are `[agent_access]` and `[source_retention]`. If the retained vault
  differs, it raises "development vault differs from generated configuration".
  A fresh `AQ_DEV_STATE_DIR` builds the same content. There is no overlay or
  extra-section input for `[intelligence]` or `[profiled_execution]`.
- `scripts/dev-init-volumes.sh` copies only `config.toml` and `graph_token`
  into the server's private volumes. A retained copy that differs is refused
  ("use a fresh Compose project"). The `dev-state-init` service in
  `compose.dev.yaml` mounts only the `graph_token` secret.
- `server_material_v1` requires the credential `secret_file` to be owned by
  UID 10001 with mode 0600, inside the server container. No route exists to
  place the owner's auth file there.
- `allowed_node_ids` is a list of explicit UUIDs (`intelligence/config.rs:21`,
  `inventory.rs:205`). The only existing worker identity is the seeded,
  offline `e0-local-development` node `806c44f7-0e2a-4094-b976-687e4e54aedf`.
  Its private credential is in `secrets.dev/dev_worker.json`. A fresh stack
  enrolls a different node only after the server starts, which is after its
  vault is fixed.

Applying `[intelligence]`/`[profiled_execution]` therefore needs one of these:
an edit to E0 source, writing directly into the volume (`docker cp`/exec or
deleting `dev_vault`), or bypassing `prepare()`. The task forbids all three
("Preserve E0 source", no admission bypass). I made no such change. I spent
no real provider calls, because a context fact expires within 24 hours and
cannot be used until the configuration can be applied. I wrote no private
lease input and made no catalog activation or binding change.

These are decisions for the supervisor and owner:

1. Authorize a reviewed E0 dev-stack change, or a separate stack generator.
   It would accept an owner-supplied private vault overlay plus a private
   credential secret, installed as 10001:0600 at
   `/run/aq-dev/owner-chatgpt-codex.auth.json`. It would run with fresh
   state, fresh secrets and a new Compose project.
2. Choose the worker identity for `allowed_node_ids`. One option is to reuse
   the seeded node `806c44f7-…` with its retained credential on the desktop
   dev worker. Another is to enroll a new node first and generate the vault
   afterwards.
3. Confirm that the three measurement calls should run just before
   application, inside the I0 image with a clean private `CODEX_HOME`.
   Measured with the host `~/.codex/config.toml` (243 KB of MCP and other
   configuration), the overhead would not match the container.

## Measured native environment profile (claim epoch 5)

Authority: **measured** (a local Docker probe of the pinned image). This is
not owner attestation. `tools/qualification/probe-e0-native-environment.py`
ran I0 image `sha256:0b7defe6…ddc7d` once. It used the owner-approved run
flags: `--read-only --cap-drop ALL --security-opt no-new-privileges=true
--security-opt seccomp=unconfined --network host --tmpfs /tmp:mode=1777`.
`/config` and `/state` were fresh tmpfs owned by 10001. No host path was
mounted, no credential was forwarded and no provider was called. Inside the
container the probe mirrors the checks in `named_environment.rs` `inspect`.

The retained canonical evidence is
[`environment-probe.json`](evidence/e0-native-inputs/environment-probe.json),
2165 bytes, sha256
`ab62ce165f795e22b2c988aa225e062eb2d118bf123ce4bdd37f619936d0eb97`.
All eleven checks passed:

- UID/GID 10001;
- all five capability sets empty, and `NoNewPrivs=1`;
- the single image-device mount row is `ro`, and a write gives `EROFS`;
- all six program pins (sha256 and bytes) match the I0 receipt;
- `codex-cli 0.166.0` and `tmux 3.4`;
- an OS `bwrap` is present, and its pin is recorded (see below);
- `codex sandbox -- /bin/true` exits 0 with a private `CODEX_HOME`.

The measured seccomp result corrects an assumption. On this Docker Desktop
29.3.1 host, `seccomp=unconfined` does **not** produce `Seccomp: 0`:

| Profile | Filters (`Seccomp: 2` in all cases) |
|---|---|
| Docker default profile | 2 |
| `seccomp=unconfined` | 1 |
| `--privileged` (probe only, never used for a run) | 1 |

The one remaining filter is the host baseline. The probe therefore checks
that unconfined removes exactly Docker's profile filter, measured against a
default-profile baseline container of the same image. It does not check for
`Seccomp == 0`.

Two further measured findings affect the definition bodies:

- **Runtime ref.** The worker example in the schema notes put the aq-worker
  binary sha in `runtime.runtime`. That is wrong for E0. The server requires
  every runtime ref to be a definition body with `sha256(body) == digest`
  (`profiled_execution/producer/build.rs:249-292`,
  `presence/qualification.rs:14-60`). The E0 reference recipe builds it as a
  JSON body `aq.codex-finite-peer-runtime` that embeds the worker ProgramPin
  (`tests/support/codex_runtime_recipe.rs:267-273`). The worker checks only
  the id/version format and equality with the adapter, composite and
  presence definitions (`native/runner/config.rs:226-262`); it never hashes
  its own binary against the ref. The real runtime ref will therefore be the
  digest of a runtime body that carries the measured aq-worker pin above.
- **OS bubblewrap.** The I0 image provides `/usr/bin/bwrap`
  **bubblewrap 0.8.0**, sha256
  `85580dd52ed366ece8844e90fa75ac7c4de8802963071344e123221fb9f6f11e`,
  72080 bytes. The recipe's environment body hard-codes a different binary
  (`e3189039…`, 72160 bytes) and the premise "OS bwrap 0.9.0". The real
  environment body must use the measured pin and version. Copying the
  recipe constant would be fixture-as-real.

This document is the measured basis for the environment qualification
ArtifactRef. It is not yet admission: the server definition body, context
qualification, credential placement and catalog revision 2 remain blocked on
the decisions listed above.

`aq test tools/qualification/tests/test_e0_native_environment.py -q` checks
the retained evidence offline:

- it is canonical, its authority is measured and its verdicts recompute;
- every hardening flag is kept;
- each check refuses its own regression.
