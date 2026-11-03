"""Real native qualification inputs for the separate aq-e0-qual stack.

Loaded by scripts/e0-qual-stack.py for `configure` and `apply`. Every value is
either a pinned observed artifact (image program digests, exact public bodies
from the I0 worker source), a measured value from the bounded measurement
evidence, or an owner decision recorded in pins.json. Nothing here admits,
grants or observes anything: the server and the worker re-check all of it.

configure: build definitions, server [intelligence]/[profiled_execution],
           catalog revision 2 and the worker bundle under the qualification root.
apply:     install the vault into this project's own volume, restart the server,
           activate catalog rev 2 / binding rev 2 through the public API, install
           the worker bundle into a fresh volume and run `aq-worker check-native`.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tomllib

RUNTIME_DIR = "/opt/aq-runtime"
STATE = "/state"
ENVIRONMENT = "aq-e0-qual-desktop-i0"
AGENT = "e0-qual-implementer@1"
MODULE = "e0-qual-bounded-edit@1"
INTELLIGENCE = "e0-qual-codex-sol@1"
EXECUTION = "e0-qual-desktop-15m@1"
CREDENTIAL = "owner-chatgpt-codex"
PROFILE = "codex-0.166-chatgpt-interactive-result-local-v1"
REASONING_REFERENCE = "codex-high@1"
CONTEXT_BYTES = 65536
# Planning reservation only. Codex 0.166 runs with its own default output limit;
# this number bounds AQ selection/context planning and is not a Codex setting.
MAX_OUTPUT_TOKENS = 128000
EXECUTION_MS = 900000
MEMORY_BYTES = 2147483648
CLIENT_RESERVATION = 64 * 1024 * 1024
# Must equal aq-git's native reservation (aq-worker source.rs native_reservation_bytes):
# 2 * max_expanded (pool + workspace) + max_bundle + 1 MiB artifact headroom.
SOURCE_DISK_BYTES = 2 * 10737418240 + 1073741824 + 1024 * 1024
# Server refuses changed profiled content at an unchanged revision; bump on every content change.
# 2: SOURCE_DISK_BYTES corrected to the real aq-git reservation (I0).
PROFILED_REVISION = 2
TERMINAL_KEY_ID = "e0-qual-relay-1"
TERMINAL_KEY_FILE = "terminal-relay-key"
DISK_BYTES = 24 * 1024 * 1024 * 1024
FIXED_OVERHEAD_TOKENS = 16384
TOKENS_PER_BYTE = 1
QUALIFICATION_TTL_MS = 86400000
WORKER_VOLUME = "aq-e0-qual-worker-state"
LONG_CONTRACT = {"id": "aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1", "version": "1",
                 "digest": "bd9abe1d9ef02970beb7946c007418de90a6066a146698ac0c075da1c1f2a28f"}
TERMINAL_CONTRACT = {"id": "aq.phase-terminal.registry-refresh-v1.contract", "version": "1",
                     "digest": "f9af55c49127a8cb93f03ef5bbb28313d13f94a221affa3e26d91d45bffd6c2d"}
# Phase features the refresh-v1 long contract requires (aq-admission 0.1.11 preparation_shared.rs
# required_features() plus preparation_refresh_v1.rs extra_features). Omitting the extra one makes the
# I0 worker refuse native.json during decoding.
NEGOTIATION_FEATURES = ["accepted_preparation_v2", "fresh_initial_confirmation_v2", "independent_execution_delivery_v2",
                        "registry_observation_continuity_v1"]
OWNER_KINDS = ["Project", "Registry", "Principal", "Inventory", "Availability", "Budget", "Context", "Git",
               "PrivateMaterialBacking"]
# Exact public bodies compiled into the pinned I0 worker (source revision in pins.json).
RAW_BODIES = [
    ("aq.agent-result-local", "crates/aq-agent-result-local/artifacts/contract.json",
     "4fe711c405ead85b55a54de2d7446b2c106c4b8809b16bc57497ba2af1d35ef9"),
    ("aq.agent-result-startup", "crates/aq-agent-result-local/artifacts/startup.md",
     "667d48a537baa35335fb3da1e589846e9d294a2bc24a8df58cf682181b15229a"),
    ("aq.codex-interactive-result-local-recipe", "crates/aq-codex-mapper/artifacts/interactive-result-local-v1.json",
     "563a2303cd64d6f481b662327ab5dcb3ccbe34554443c446850574a0f14fe254"),
]
CAPABILITIES = [{"id": c, "version": 1} for c in (
    "aq.profiled-launch", "aq.terminal", "aq.harness.startup-context", "aq.harness.confined-worktree",
    "aq.harness.stop")]
FEATURES = ["artifact_transfer_v1", "server_material_v1", "durable_host_journal_v1"]
OWNER_ATTESTED = "owner-attested (recorded by agent for the owner)"


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def sha(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode()).hexdigest()


def ref(identifier, body):
    return {"id": identifier, "version": "1", "digest": sha(body)}


def snake(kind):
    return "".join("_" + c.lower() if c.isupper() and i else c.lower() for i, c in enumerate(kind))


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        if "\x7f" in value:
            raise ValueError("DEL is not representable in a TOML basic string")
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    if isinstance(value, dict):
        return "{" + ", ".join(f"{json.dumps(k)} = {toml_value(v)}" for k, v in value.items() if v is not None) + "}"
    raise ValueError(f"unsupported TOML value {type(value).__name__}")


def without_none(value):
    if isinstance(value, dict):
        return {k: without_none(v) for k, v in value.items() if v is not None}
    if isinstance(value, list):
        return [without_none(v) for v in value]
    return value


class Builder:
    def __init__(self):
        self.definitions = []

    def raw(self, identifier, body):
        artifact = ref(identifier, body)
        if any((d["artifact"]["id"], d["artifact"]["version"]) == (identifier, "1") for d in self.definitions):
            raise ValueError(f"duplicate definition {identifier}")
        self.definitions.append({"artifact": artifact, "kind": "definition", "body": body})
        return artifact

    def add(self, identifier, value):
        return self.raw(identifier, canonical(value))


def measured(stack):
    """Counting and capacity facts from the three bounded owner-approved calls."""
    evidence = stack.layout()["evidence"]
    raw = json.loads((evidence / "measure-raw.json").read_text())
    calls = raw["calls"]
    stack.require(len(calls) == 3 and [c["call"] for c in calls] == ["minimal", "mixed", "adversarial"],
                  "expected exactly the three bounded measurement calls")
    model = stack.PINS["model"]
    stack.require(raw["model"] == model["model"] and raw["native_reasoning"] == model["native_reasoning"]
                  and raw["codex_version"] == "codex-cli 0.166.0", "measurement is not for the selected model")
    rows = []
    windows = set()
    for c in calls:
        rollout = c["rollout"]
        stack.require(c["exit_code"] == 0 and not c["errors"] and rollout["models"] == [model["model"]]
                      and rollout["efforts"] == [model["native_reasoning"]], f"measurement {c['call']} incomplete")
        windows.update(rollout["model_context_windows"])
        bound = c["prompt_utf8_bytes"] * TOKENS_PER_BYTE + FIXED_OVERHEAD_TOKENS
        stack.require(c["usage"]["input_tokens"] <= bound, f"measured {c['call']} exceeds the counting bound")
        rows.append({"call": c["call"], "prompt_utf8_bytes": c["prompt_utf8_bytes"], "prompt_sha256": c["prompt_sha256"],
                     "payload_utf8_bytes": c["payload_utf8_bytes"], "input_tokens": c["usage"]["input_tokens"],
                     "cached_input_tokens": c["usage"]["cached_input_tokens"],
                     "output_tokens": c["usage"]["output_tokens"],
                     "reasoning_output_tokens": c["usage"]["reasoning_output_tokens"],
                     "rollout_last_input_tokens": rollout["last_input_tokens"],
                     "rollout_model_context_windows": rollout["model_context_windows"],
                     "bound_tokens": bound, "started_at_ms": c["started_at_ms"], "finished_at_ms": c["finished_at_ms"]})
    stack.require(len(windows) == 1, "measured context window is not a single value")
    capacity = windows.pop()
    stack.require(FIXED_OVERHEAD_TOKENS < capacity <= 16777216, "measured capacity out of range")
    return rows, capacity, max(c["finished_at_ms"] for c in calls)


def build(stack):
    pins = stack.PINS
    i0 = pins["worker_i0"]
    programs = i0["programs"]
    server_source = pins["e0_source"]["revision"]
    project, repository = stack.PROJECT, stack.REPOSITORY
    model = pins["model"]
    credential_pin = pins["credential_profile"]
    node = stack.receipt("node.json")
    seed = stack.receipt("seed.json")
    stack.require(node and seed and seed["node_id"] == node["node_id"], "enroll and seed first")
    stack.require(node["node_id"] != "806c44f7-0e2a-4094-b976-687e4e54aedf", "must not reuse the E0 seed node")
    scope = (f"aq-e0-qual desktop dev worker {node['node_id']} only; published I0 image {pins['images']['worker_i0']}; "
             f"owner ChatGPT login profile {CREDENTIAL}@1; ElectricJack/aq-scratch main; not a general production, "
             "hostile-UID secrecy or multi-node qualification")

    def pin(name):
        p = programs[name]
        stack.require(p["path"] == ("/usr/bin/bwrap" if name == "bwrap" else f"{RUNTIME_DIR}/{name}"),
                      f"unexpected {name} path")
        return {"path": p["path"], "artifact": {"id": f"aq.e0-qual-image/{name}", "version": "1",
                                                "digest": p["sha256"]}, "bytes": p["bytes"]}
    worker, codex, host, tmux, client = (pin(n) for n in ("aq-worker", "codex", "aq-session-host", "tmux", "aq-agent"))
    stack.require(codex["bytes"] == 319346888 and programs["codex"]["version"] == "0.166.0", "Codex 0.166.0 pin")

    permissions = [{"operation": "task.read", "scope_reference": f"project:{project}"},
                   {"operation": "knowledge.read", "scope_reference": f"project:{project}"},
                   {"operation": "source.write", "scope_reference": f"repository:{repository}"}]
    scopes = [{"operation": p["operation"], "resource": p["scope_reference"]} for p in permissions]
    # Assumption (not compiled here): the catalog/binding grant exactly these three
    # with no omissions, so requested == effective == required in this order.
    permission_plan = {"requested": scopes, "effective": scopes, "required": scopes, "omissions": []}
    permission_digest = sha(canonical(permission_plan))

    b = Builder()
    policy = b.add(f"aq.e0-qual-project-policy:{project}",
                   {"schema_version": 1, "kind": "source_change_reviewed_human_owner_no_checks_v1"})
    runtime = b.add("aq.e0-qual-runtime", {"schema_version": 1, "worker": worker, "worker_source": i0["source_revision"],
                                           "runtime_version": i0["runtime_version"], "scope": scope})
    bwrap = programs["bwrap"]
    environment = b.add(ENVIRONMENT, {
        "schema_version": 1, "scope": scope, "images": {"codex": codex, "host": host, "tmux": tmux, "client": client},
        "premises": [
            f"programs are read from the read-only root filesystem of image {pins['images']['worker_i0']} under {RUNTIME_DIR}",
            "worker container runs as uid 10001 with --read-only, --cap-drop=ALL and no-new-privileges",
            "worker container uses the Docker default seccomp profile; seccomp=unconfined was used only for the measurement container",
            f"OS bwrap {bwrap['version']} provisioned read-only at {bwrap['path']}",
            f"private state is the fresh volume {WORKER_VOLUME} mounted at {STATE} (0700, uid 10001)"],
        "primary_image_reservation_bytes": 536870912, "additional_client_image_reservation_bytes": CLIENT_RESERVATION,
        "bwrap": {"sha256": bwrap["sha256"], "bytes": bwrap["bytes"], "path": bwrap["path"],
                  "provisioning": "readonly OS environment; not a NamedExecManifest image pin"}})
    raw = {}
    for identifier, path, digest in RAW_BODIES:
        body = subprocess.run(["git", "-C", str(stack.HERE.parent), "show", f"{i0['source_revision']}:{path}"],
                              check=True, capture_output=True).stdout
        stack.require(sha(body) == digest, f"{path} differs from the pinned I0 source")
        raw[identifier] = b.raw(identifier, body.decode())
    stack.require(len(b.definitions[-2]["body"].encode()) == 1929, "startup byte count")
    contract, recipe = raw["aq.agent-result-local"], raw["aq.codex-interactive-result-local-recipe"]
    client_manifest = {"schema_version": 1, "profile": "aq-agent-result-client-named-readonly-v1", "contract": contract,
                       "environment": environment,
                       "executable": {"path": client["path"], "artifact": client["artifact"]},
                       "executable_bytes": client["bytes"]}
    client_ref = b.add("aq.agent-result-client-installation", client_manifest)
    composite = {"schema_version": 1, "profile": PROFILE, "runtime": runtime, "environment": environment,
                 "client_manifest": client_ref, "client_executable": client["artifact"], "contract": contract,
                 "recipe": recipe, "permission_digest": permission_digest, "reserved_client_bytes": CLIENT_RESERVATION}
    adapter = b.add("aq.codex-result-local-adapter", composite)
    qualification = b.add("aq.e0-qual-runtime-qualification", {
        "schema_version": 1, "runtime": runtime, "adapter": adapter, "environment": environment,
        "server_source": server_source, "scope": scope,
        "qualification": "pinned published I0 image programs, exact I0 public result contract/recipe/startup bodies, "
                         "owner-approved desktop ChatGPT interactive profile",
        "not_evidence": ["no current node health or inventory", "no successful execution claimed by configuration",
                         "worker preflight result is recorded separately"]})
    harness_q = b.add("aq.e0-qual-harness-qualification", {
        "schema_version": 1, "runtime": runtime, "qualification": qualification, "adapter": adapter,
        "environment": environment, "codex": codex, "host": host, "tmux": tmux, "scope": scope,
        "max_output_tokens": MAX_OUTPUT_TOKENS,
        "max_output_tokens_basis": "AQ planning reservation only; Codex 0.166.0 keeps its default output limit "
                                   "(owner decision), so this is not a Codex or provider setting"})
    credential = b.add(CREDENTIAL, {
        "schema_version": 1, "provider": credential_pin["provider"], "account_reference": "owner-chatgpt-account",
        "endpoint_reference": "codex-default-chatgpt-endpoint", "scope": "desktop dev worker only; " + scope,
        "attestation": OWNER_ATTESTED + ": the owner's ChatGPT account and the Codex default endpoint",
        "secret": "owner ChatGPT login file delivered by the server material owner; not included here"})
    rows, capacity, observed = measured(stack)
    rendering = "utf8_double_newline_v1"
    counting = b.add("aq.e0-qual-counting", {
        "schema_version": 1, "adapter": adapter, "runtime": runtime, "qualification": qualification,
        "harness_qualification": harness_q, "scope": scope, "rendering": rendering,
        "maximum_tokens_per_utf8_byte": TOKENS_PER_BYTE, "fixed_overhead_tokens": FIXED_OVERHEAD_TOKENS,
        "basis": "measured: three bounded Codex 0.166.0 exec calls in the I0 image (gpt-6.1-sol, high); "
                 "provider-reported input_tokens include Codex's own prompt and tools; every call satisfies "
                 "input_tokens <= prompt_utf8_bytes * 1 + 16384",
        "measurements": rows})
    capacity_ref = b.add("aq.e0-qual-capacity", {
        "schema_version": 1, "adapter": adapter, "scope": scope, "capacity_kind": "qualified_model_window",
        "capacity_tokens": capacity,
        "basis": "measured: model_context_windows reported in the Codex rollout of each of the three calls",
        "observed_windows": [r["rollout_model_context_windows"] for r in rows]})
    fact = {"schema_version": 1,
            "subject": {"runtime": runtime, "runtime_qualification": qualification, "harness": "codex",
                        "cli_version": "0.166.0", "adapter": adapter, "environment": environment,
                        "harness_qualification": harness_q, "provider": model["provider"], "model": model["model"],
                        "native_reasoning": model["native_reasoning"],
                        "reasoning_configuration_reference": REASONING_REFERENCE, "credential_profile": credential,
                        "credential_revision": credential_pin["revision"],
                        "account_reference": "owner-chatgpt-account",
                        "endpoint_reference": "codex-default-chatgpt-endpoint"},
            "rendering": rendering, "maximum_tokens_per_utf8_byte": TOKENS_PER_BYTE,
            "fixed_overhead_tokens": FIXED_OVERHEAD_TOKENS, "counting_evidence": counting,
            "capacity_kind": "qualified_model_window", "capacity_tokens": capacity, "capacity_evidence": capacity_ref,
            "observed_at_ms": observed, "expires_at_ms": observed + QUALIFICATION_TTL_MS}
    fact_digest = sha(canonical(fact))
    renderer = {"id": f"aq.qualified-context/{fact_digest}", "version": "1", "digest": fact_digest}
    primary = {"schema_version": 1, "profile": "codex-0.166-named-readonly-interactive-pty-v1",
               "executable_bytes": codex["bytes"], "environment": environment,
               "installation": {"schema_version": 1, "adapter": adapter, "runtime": runtime,
                                "qualification": qualification, "renderer": renderer,
                                "handoff": "opaque_private_file_v1",
                                "executable": {"path": codex["path"], "artifact": codex["artifact"]},
                                "helper": {"path": host["path"], "artifact": host["artifact"]},
                                "tmux": {"path": tmux["path"], "artifact": tmux["artifact"]}}}
    stack.require(len(canonical(primary).encode()) <= 16384, "primary manifest bound")
    source_profile = b.add("aq.e0-qual-default-git-profile", {
        "schema_version": 1, "checkout_profile": "no_attributes_v1", "max_expanded_bytes": 10737418240,
        "max_bundle_bytes": 1073741824, "source_disk_bytes": SOURCE_DISK_BYTES,
        "scope": "unchanged default aq-git bounds; the actual service capabilities must match before admission"})
    owners = []
    for kind in OWNER_KINDS:
        authority = b.add(f"aq.e0-qual-owner/{kind}", {
            "schema_version": 1, "kind": snake(kind), "server_source": server_source,
            "scope": "actual authenticated aq-e0-qual server owner; read and fenced again at execution"})
        owners.append({"kind": snake(kind), "authority": authority})
    phase_policy = b.add("aq.accepted-preparation-refresh-policy/e0-qual", {
        "schema_version": 1, "contract": LONG_CONTRACT, "runtime": runtime, "qualification": qualification,
        "phase": {"schema_version": 3, "maximum_preparation_ms": 30000, "maximum_execution_lease_ms": EXECUTION_MS,
                  "maximum_observation_ms": 2000},
        "source_profile": source_profile, "source_disk_bytes": SOURCE_DISK_BYTES, "owners": owners})
    b.add("aq.phase-refresh-execution-qualification/e0-qual", {
        "schema_version": 1, "contract": LONG_CONTRACT, "runtime": runtime, "runtime_qualification": qualification,
        "proposal_policy": phase_policy,
        "adapters": "registry_observation_continuity_current_project_principal_inventory_health_unissued_graph_retention_private_v1"})
    proof = b.add("aq.e0-qual-physical-proof-producer", {
        "schema_version": 1, "worker": worker, "helper": host, "tmux": tmux,
        "scope": "retained session-host guardian/incarnation plus kernel ECHILD; a definition is not a stopped event"})
    b.add("aq.phase-terminal-refresh-qualification/e0-qual", {
        "schema_version": 1, "contract": TERMINAL_CONTRACT, "runtime": runtime, "runtime_qualification": qualification,
        "proof_producer": proof})
    inventory_adapter = b.add("aq.e0-qual-scoped-presence-adapter", {
        "schema_version": 1, "worker": worker,
        "scope": "grant-bounded scoped material presence; public inventory lists provisionable profiles only"})
    inventory_evidence = b.add("aq.e0-qual-scoped-presence-evidence", {
        "schema_version": 1, "adapter": inventory_adapter, "runtime": runtime, "qualification": qualification,
        "scope": scope, "not_evidence": "no live presence or inventory receipt is created by configuration"})
    presence = {"schema_version": 1, "protocol": "aq.scoped-material-presence.v1", "runtime": runtime,
                "runtime_qualification": qualification, "adapter": inventory_adapter,
                "adapter_qualification": inventory_evidence,
                "inventory_semantics": "provisionable_only_and_scoped_presence_v1",
                "observation_protocol": "grant_issuance_bounded_boottime_v1", "maximum_age_ms": 30000,
                "maximum_ttl_ms": 30000}
    presence_ref = b.add("aq.scoped-material-presence-qualification/e0-qual", presence)
    material = b.add("aq.server-material", {
        "schema_version": 1, "server_source": server_source, "adapter": "aq.server-material", "version": 1,
        "scope": "authenticated exact-attempt private material owner; owner ChatGPT login for the desktop worker only"})

    definitions = b.definitions
    sizes = [len(d["body"].encode()) for d in definitions]
    stack.require(len(definitions) <= 64 and max(sizes) <= 2 * 1024 * 1024 and sum(sizes) <= 8 * 1024 * 1024,
                  "definition bounds")
    harness = {"harness": "codex", "cli_version": "0.166.0", "adapter": adapter, "environment": environment,
               "qualification": harness_q,
               "models": [{"provider": model["provider"], "model": model["model"],
                           "reasoning_configuration_reference": REASONING_REFERENCE,
                           "native_reasoning": model["native_reasoning"]}],
               "capabilities": CAPABILITIES,
               "limits": {"context_bytes": CONTEXT_BYTES, "max_output_tokens": MAX_OUTPUT_TOKENS,
                          "wall_time_ms": EXECUTION_MS, "memory_bytes": MEMORY_BYTES, "cpu_millis": 1000,
                          "process_limit": 64}}
    source = seed["source"]
    profiled = {"revision": PROFILED_REVISION,
                "qualified_runtimes": [{"runtime": runtime, "qualification": qualification,
                                        "capabilities": CAPABILITIES, "features": FEATURES,
                                        "material_delivery": {"adapter": material, "version": 1}}],
                "projects": [{"project_id": project, "enabled": True, "priority": 0,
                              "source": {"kind": "immutable",
                                         "object": {"format": source["base"]["algorithm"],
                                                    "hex": source["base"]["hex"]}},
                              "retention_handle": source["retention_handle"]}],
                "context_qualifications": [fact], "definition_bodies": definitions}
    intelligence = {"revision": 1, "credentials": [{
        "kind": "subscription-login",
        "credential": {"profile": credential, "provider": credential_pin["provider"],
                       "account_reference": "owner-chatgpt-account",
                       "endpoint_reference": "codex-default-chatgpt-endpoint",
                       "revision": credential_pin["revision"], "enabled": True, "provisionable": False},
        "allowed_node_ids": [node["node_id"]], "secret_file": credential_pin["server_secret_file"],
        "delivery_mode": "server_material_v1"}], "qualified_harnesses": [harness]}
    native = {
        "schema_version": 1, "state_dir": STATE, "git_socket": f"{STATE}/git.sock", "git_root": f"{STATE}/git",
        "capacity": {"slots": 1, "memory_bytes": MEMORY_BYTES, "cpu_millis": 1000, "disk_bytes": DISK_BYTES},
        "runtime": {"runtime": runtime, "qualification": qualification}, "adapter": adapter, "renderer": renderer,
        "adapter_manifest": f"{STATE}/config/adapter.json", "adapter_manifest_digest": sha(canonical(primary)),
        "capabilities": CAPABILITIES, "features": FEATURES,
        "negotiation": {"contract": LONG_CONTRACT, "features": NEGOTIATION_FEATURES},
        "source_profile": source_profile, "maximum_preparation_ms": 30000,
        "inventory_definition": f"{STATE}/config/inventory.json", "inventory_reference": presence_ref,
        "inventory_adapter": inventory_adapter, "inventory_evidence": inventory_evidence,
        "harness_qualifications": [harness_q], "inventory_ttl_ms": 30000,
        "executor": {"kind": PROFILE, "environment": environment,
                     "client_manifest": f"{STATE}/config/client.json", "client_manifest_digest": client_ref["digest"],
                     "composite_definition": f"{STATE}/config/composite.json", "terminal_clock_skew_ms": 1000,
                     "settings": {"host_root": f"{STATE}/host", "columns": 80, "rows": 24,
                                  "permissions_digest": permission_digest,
                                  "harness": {"harness": "codex", "cli_version": "0.166.0", "adapter": adapter,
                                              "environment": environment, "required_capabilities": CAPABILITIES[2:]},
                                  "model": {"provider": model["provider"], "model": model["model"],
                                            "reasoning": model["native_reasoning"],
                                            "max_output_tokens": MAX_OUTPUT_TOKENS, "spending_budget": None}}}}
    files = {"native.json": canonical(native), "adapter.json": canonical(primary), "client.json": canonical(client_manifest),
             "composite.json": canonical(composite), "inventory.json": canonical(presence)}
    stack.require(all(len(v.encode()) <= 65536 for v in files.values())
                  and len(files["inventory.json"].encode()) <= 8192, "worker bundle file bounds")
    return dict(definitions=definitions, profiled=profiled, intelligence=intelligence, files=files, policy=policy,
                permissions=permissions, permission_plan=permission_plan, fact=fact, renderer=renderer,
                node=node, seed=seed, harness=harness)


def catalog_rev2(stack, built):
    artifact = json.loads((stack.e0_source() / "scripts/dev-catalog.json").read_text())
    stack.require(canonical(artifact) == (stack.e0_source() / "scripts/dev-catalog.json").read_text().strip(),
                  "E0 dev catalog is not canonical")
    catalog = artifact["catalog"]
    stack.require(identities(catalog, artifact["instructions"]) == artifact["identities"],
                  "identity recomputation does not reproduce the E0 catalog")
    model = stack.PINS["model"]
    module_id, module_version = MODULE.split("@")
    catalog["modules"].append({
        "id": module_id, "version": int(module_version), "kind": "capability",
        "purpose": "Make one bounded owner-requested change in the scratch repository",
        "instruction": "instructions/e0-qual-bounded-edit.md", "delivery": "startup", "required": True,
        "dependencies": [], "required_adapters": [],
        "requirements": [{"permission": p, "required": True} for p in built["permissions"]]})
    artifact["instructions"][MODULE] = (
        "# Bounded scratch edit\n\nChange only what the assignment asks for, inside the authorized worktree of the "
        "owner's scratch repository. Re-read each changed file before reporting. Report the exact files changed and "
        "the evidence you checked. Leave unrelated files untouched.\n")
    agent_id, agent_version = AGENT.split("@")
    catalog["agents"].append({
        "id": agent_id, "version": int(agent_version),
        "role": "Implement the exact scoped scratch assignment and report evidence.", "behaviors": [],
        "capabilities": [MODULE], "intelligence": INTELLIGENCE, "execution": EXECUTION, "allowed_overrides": []})
    intel_id, intel_version = INTELLIGENCE.split("@")
    catalog["intelligence"].append({
        "id": intel_id, "version": int(intel_version),
        "candidates": [{"id": "owner-codex-sol", "provider": model["provider"], "model": model["model"],
                        "reasoning_configuration_reference": REASONING_REFERENCE, "harnesses": ["codex"]}],
        "selection_policy": {"schema_version": 1, "fallback_on": [], "allow_unknown_availability": False,
                             "observation_max_age_ms": 30000, "max_output_tokens": MAX_OUTPUT_TOKENS,
                             "max_attempts": 1, "max_lineage_elapsed_ms": 1800000,
                             "required_capabilities": CAPABILITIES,
                             "budget": {"max_cost_microunits": None, "currency": None,
                                        "require_complete_accounting": False,
                                        "require_hard_cost_enforcement": False, "unknown_cost": "allow_bounded"}}})
    exec_id, exec_version = EXECUTION.split("@")
    catalog["execution"].append({
        "id": exec_id, "version": int(exec_version), "harnesses": ["codex"], "environments": [ENVIRONMENT],
        "context_limit_bytes": CONTEXT_BYTES, "permissions": built["permissions"],
        "credential_profile_references": [f"{CREDENTIAL}@1"]})
    artifact["identities"] = identities(catalog, artifact["instructions"])
    return canonical(artifact)


def identities(catalog, instructions):
    out = {}
    for m in catalog["modules"]:
        m = dict(m)
        for key in ("dependencies", "requirements", "required_adapters"):
            m.setdefault(key, [])
        key = f"{m['id']}@{m['version']}"
        out["module:" + key] = sha(canonical({"metadata": m, "markdown": instructions[key]}))
    for a in catalog["agents"]:
        a = dict(a)
        a.setdefault("allowed_overrides", [])
        out[f"agent:{a['id']}@{a['version']}"] = sha(canonical(a))
    for kind in ("intelligence", "execution"):
        for entry in catalog[kind]:
            out[f"{kind}:{entry['id']}@{entry['version']}"] = sha(canonical(entry))
    return out


def binding_rev2(stack, built, catalog_digest):
    seed, node = built["seed"], built["node"]
    return dict(schema_version=1, project_id=stack.PROJECT, name="E0 qualification scratch", enabled=True,
                repository_id=stack.REPOSITORY, repository_binding_generation=1,
                repository_adapter_reference="aq.git.configured", graph_adapter_reference="aq.graph.configured",
                graph_workspace_id=seed["workspace_id"], catalog_digest=catalog_digest,
                allowed_agent_profiles=[AGENT], allowed_intelligence_profiles=[INTELLIGENCE],
                allowed_execution_profiles=[EXECUTION], allowed_node_ids=[node["node_id"]], allowed_worker_groups=[],
                allowed_environments=[ENVIRONMENT],
                policy=dict(identity=built["policy"]["id"], revision="1", digest=built["policy"]["digest"]),
                context=[], ceilings=dict(permissions=built["permissions"], context_bytes=CONTEXT_BYTES,
                                          wall_time_ms=EXECUTION_MS, memory_bytes=MEMORY_BYTES, cpu_millis=1000,
                                          process_limit=64, cost_limit_microunits=None, currency=None,
                                          require_hard_cost_enforcement=False))


def configure(stack):
    p = stack.layout()
    built = build(stack)
    head = (f"intelligence = {toml_value(built['intelligence'])}\n"
            f"profiled_execution = {toml_value(built['profiled'])}\n")
    # The Codex host registers a terminal relay; without [terminals] the server answers 503
    # identity_unavailable and the attempt stops right after `started` (I0 run 9).
    relay_key = p["qual"] / "secrets" / TERMINAL_KEY_FILE
    if not relay_key.exists():
        stack.private_bytes(relay_key, os.urandom(32))
    config = head + stack.base_config() + (
        f'[terminals.protection]\nactive_key_id = "{TERMINAL_KEY_ID}"\n'
        f'keys = [{{ id = "{TERMINAL_KEY_ID}", file = "/qual-vault/{TERMINAL_KEY_FILE}" }}]\n')
    parsed = tomllib.loads(config)
    stack.require(parsed["intelligence"] == without_none(built["intelligence"])
                  and parsed["profiled_execution"] == without_none(built["profiled"]), "TOML round trip differs")
    catalog = catalog_rev2(stack, built)
    worker_dir = p["qual"] / "worker"
    worker_dir.mkdir(mode=0o700, exist_ok=True)
    worker_dir.chmod(0o700)
    for name, text in built["files"].items():
        stack.private_bytes(worker_dir / name, text.encode(), replace=True)
    credential = json.loads((p["secret"] / "qual_worker.json").read_text())
    stack.require(credential["node_id"] == built["node"]["node_id"], "worker credential belongs to another node")
    _, _, worker_url = stack.urls()
    worker_toml = (f"server_url = {json.dumps(worker_url)}\nnode_id = {json.dumps(credential['node_id'])}\n"
                   f"credential = {json.dumps(credential['credential'])}\n")
    stack.private_bytes(worker_dir / "worker.toml", worker_toml.encode(), replace=True)
    auth, auth_digest = stack.owner_auth_file()
    secret_name = Path(stack.PINS["credential_profile"]["server_secret_file"]).name
    stack.private_bytes(p["qual"] / "secrets" / secret_name, auth.read_bytes(), replace=True)
    stack.private_bytes(p["qual"] / "config.toml", config.encode(), replace=True)
    stack.private_bytes(p["qual"] / "catalog-rev2.json", catalog.encode(), replace=True)
    public = dict(
        catalog_rev2_digest=sha(catalog), config_sha256=sha(config),
        definitions=[{"id": d["artifact"]["id"], "digest": d["artifact"]["digest"], "bytes": len(d["body"].encode())}
                     for d in built["definitions"]],
        renderer=built["renderer"], fact={k: v for k, v in built["fact"].items() if k != "subject"},
        permission_plan_digest=sha(canonical(built["permission_plan"])),
        worker_files={k: sha(v) for k, v in built["files"].items()}, node_id=built["node"]["node_id"])
    stack.receipt("configure.json", public)
    return dict(public, owner_login_installed=True,
                owner_login_matches_source=sha(auth.read_bytes()) == auth_digest)


def docker_run(stack, *args, timeout=600):
    result = subprocess.run(["docker", "run", "--rm", "--pull=never", *args], capture_output=True, text=True,
                            timeout=timeout)
    return result.returncode, result.stdout[-4000:], result.stderr[-4000:]


def apply(stack):
    p = stack.layout()
    configured = stack.receipt("configure.json")
    stack.require(configured is not None and (p["qual"] / "config.toml").is_file(), "run configure first")
    graph, server, _ = stack.urls()
    out = dict(steps=[])
    # 1. Publish the generated vault into this project's own qual_vault volume and restart the server on it.
    stack.compose("up", "-d", "--no-build", "--force-recreate", "--wait", "--wait-timeout", "300", "server",
                  capture=False)
    out["steps"].append("server restarted on generated /qual-vault")
    # 2. Catalog revision 2 and binding revision 2 through the public API only.
    catalog = (p["qual"] / "catalog-rev2.json").read_text()
    digest = sha(catalog)
    stack.require(digest == configured["catalog_rev2_digest"], "catalog file changed since configure")
    active = stack.http(server + "/api/profiles/catalogs")
    if active.get("active_digest") != digest:
        stack.require(active.get("revision") == 1, f"unexpected catalog revision {active.get('revision')}")
        stack.http(server + "/api/profiles/catalogs/activate", dict(operation_id=stack.OP_CATALOG_2,
                   expected_revision=1, artifact_json=catalog), allow_missing=False)
    out["catalog"] = {k: stack.http(server + "/api/profiles/catalogs").get(k) for k in ("active_digest", "revision")}
    built = build(stack)
    binding = binding_rev2(stack, built, digest)
    current = stack.http(server + "/api/projects/" + stack.PROJECT)
    if current.get("revision") == 1:
        stack.http(server + "/api/projects/" + stack.PROJECT, dict(operation_id=stack.OP_BINDING_2,
                   expected_revision=1, binding=binding), allow_missing=False)
    current = stack.http(server + "/api/projects/" + stack.PROJECT)
    out["binding_revision"] = current.get("revision")
    # 3. Fresh worker state volume owned by this project; private bundle installed by a root init container.
    image = stack.PINS["images"]["worker_i0"]
    existing = stack.docker("volume", "ls", "-q").split()
    if WORKER_VOLUME not in existing:
        stack.docker("volume", "create", "--label", f"com.docker.compose.project={stack.PROJECT_NAME}", WORKER_VOLUME)
    code, _, err = docker_run(
        stack, "--network=none", "--user=0:0", "--cap-drop=ALL", "--cap-add=CHOWN", "--cap-add=DAC_OVERRIDE",
        "--cap-add=FOWNER", "-v", f"{WORKER_VOLUME}:{STATE}", "-v", f"{p['qual'] / 'worker'}:/bundle:ro",
        "--entrypoint=/bin/sh", image, "-c",
        "set -eu; umask 077; chown 10001:10001 /state; chmod 0700 /state; "
        "for d in config host git; do install -d -o 10001 -g 10001 -m 0700 /state/$d; done; "
        "for f in native.json adapter.json client.json composite.json inventory.json worker.toml; do "
        "install -o 10001 -g 10001 -m 0600 /bundle/$f /state/config/$f; done")
    stack.require(code == 0, f"worker bundle install failed: {err[-500:]}")
    out["steps"].append(f"worker bundle installed in {WORKER_VOLUME}")
    # 4. Offline validation + preflight of the installed executor. No admission, no provider call.
    code, stdout, stderr = docker_run(
        stack, "--network=host", "--user=10001:10001", "--read-only", "--tmpfs=/tmp:mode=1777", "--cap-drop=ALL",
        "--security-opt=no-new-privileges", "-e", "RUST_LOG=debug", "-v", f"{WORKER_VOLUME}:{STATE}",
        f"--entrypoint={RUNTIME_DIR}/aq-worker", image, "--config", f"{STATE}/config/worker.toml",
        "check-native", "--native-config", f"{STATE}/config/native.json")
    check = dict(exit_code=code, stdout=stdout.strip(), stderr=stderr.strip())
    (p["evidence"] / "check-native.json").write_text(json.dumps(check, indent=1, sort_keys=True) + "\n")
    out["check_native"] = check
    workers = stack.http(server + "/api/workers") or []
    out["workers"] = [{k: w.get(k) for k in ("id", "name", "status")} for w in workers]
    stack.receipt("apply.json", out)
    return out
