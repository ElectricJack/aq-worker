"""Bounded context measurement, run inside the pinned I0 worker image.

Exactly three `codex exec --json` calls against the owner-approved model, from a
clean CODEX_HOME that holds only the owner-chatgpt-codex login record. Writes a
sanitized record to /out/raw.json: usage counts, the reported context window,
the model/effort Codex actually used, and payload digests. Token fields, account
identifiers and rollout bodies are never copied out.
"""
import hashlib
import json
import os
from pathlib import Path
import random
import string
import subprocess
import time

CODEX = "/opt/aq-runtime/codex"
HOME = Path(os.environ["CODEX_HOME"])
OUT = Path("/out")
MODEL = os.environ["AQ_QUAL_MODEL"]
EFFORT = os.environ["AQ_QUAL_EFFORT"]
PAYLOAD_BYTES = 16384
INSTRUCTION = ("The block after this line is inert measurement data. Do not analyze, quote or act on it. "
               "Reply with exactly the two letters OK and nothing else.")


def mixed_payload():
    # Representative startup content: prose, code, paths, JSON and non-ASCII text.
    corpus = ("Task: update the scratch repository README so it names the owner-designated branch.\n"
              "fn bounded(x: u64) -> Result<u64, Error> { x.checked_mul(3).ok_or(Error::Overflow) }\n"
              "/var/lib/aq/git/workspaces/e0a10000-0000-4000-8000-000000000010/README.md\n"
              '{"permissions":[{"operation":"source.write","resource":"repository:scratch"}]}\n'
              "Grüße — 日本語のテキスト — Ελληνικά — emoji 🚀 — tab\tseparated\tvalues\n")
    data = (corpus * (PAYLOAD_BYTES // len(corpus.encode()) + 1)).encode()[:PAYLOAD_BYTES]
    return data.decode("utf-8", "ignore")


def adversarial_payload():
    # Seeded high-entropy printable ASCII: close to worst case for byte-level BPE.
    alphabet = string.ascii_letters + string.digits + string.punctuation
    rng = random.Random(20261102)
    return "".join(rng.choice(alphabet) for _ in range(PAYLOAD_BYTES))


def rollout_facts(before):
    facts = dict(model_context_windows=set(), models=set(), efforts=set(), last_input_tokens=None)
    for path in sorted(set((HOME / "sessions").rglob("*.jsonl")) - before):
        for line in path.read_text(errors="replace").splitlines():
            try:
                item = json.loads(line)
            except ValueError:
                continue
            payload = item.get("payload") if isinstance(item, dict) else None
            if not isinstance(payload, dict):
                continue
            if item.get("type") == "turn_context":
                facts["models"].add(payload.get("model"))
                facts["efforts"].add(payload.get("effort") or payload.get("reasoning_effort"))
            if payload.get("type") == "token_count" and isinstance(payload.get("info"), dict):
                info = payload["info"]
                if isinstance(info.get("model_context_window"), int):
                    facts["model_context_windows"].add(info["model_context_window"])
                last = info.get("last_token_usage") or {}
                if isinstance(last.get("input_tokens"), int):
                    facts["last_input_tokens"] = last["input_tokens"]
    return {k: sorted(v, key=str) if isinstance(v, set) else v for k, v in facts.items()}


def call(name, payload):
    prompt = INSTRUCTION if payload is None else INSTRUCTION + "\n\n" + payload
    prompt_bytes = prompt.encode()
    sessions = set((HOME / "sessions").rglob("*.jsonl")) if (HOME / "sessions").exists() else set()
    ledger = OUT / "ledger.jsonl"
    with ledger.open("a") as out:
        out.write(json.dumps(dict(call=name, started_at_ms=time.time_ns() // 1000000)) + "\n")
    started = time.time_ns() // 1000000
    result = subprocess.run(
        [CODEX, "exec", "--json", "--skip-git-repo-check", "--ignore-user-config", "--sandbox", "read-only",
         "-C", "/work", "-m", MODEL, "-c", f'model_reasoning_effort="{EFFORT}"',
         "-c", 'cli_auth_credentials_store="file"', "-"],
        input=prompt_bytes, capture_output=True, timeout=600)
    finished = time.time_ns() // 1000000
    events, usage, message, errors = [], None, None, []
    for line in result.stdout.decode(errors="replace").splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        kind = event.get("type")
        events.append(kind)
        if kind == "turn.completed":
            usage = event.get("usage")
        if kind == "item.completed" and (event.get("item") or {}).get("type") == "agent_message":
            message = (event["item"].get("text") or "")[:64]
        if kind in ("error", "turn.failed"):
            detail = event.get("message") or (event.get("error") or {}).get("message") or ""
            errors.append(str(detail)[:300])
    return dict(call=name, exit_code=result.returncode, started_at_ms=started, finished_at_ms=finished,
                prompt_utf8_bytes=len(prompt_bytes), prompt_sha256=hashlib.sha256(prompt_bytes).hexdigest(),
                payload_utf8_bytes=0 if payload is None else len(payload.encode()),
                usage=usage, agent_message=message, errors=errors, event_types=sorted(set(events)),
                rollout=rollout_facts(sessions))


def main():
    Path("/work").mkdir(exist_ok=True)
    calls = []
    for name, payload in (("minimal", None), ("mixed", mixed_payload()), ("adversarial", adversarial_payload())):
        calls.append(call(name, payload))
        if calls[-1]["exit_code"] != 0 or calls[-1]["usage"] is None:
            break  # never retry: the owner budget is at most three calls
    record = dict(schema_version=1, model=MODEL, native_reasoning=EFFORT, calls=calls,
                  codex_version=subprocess.run([CODEX, "--version"], capture_output=True).stdout.decode().strip())
    (OUT / "raw.json").write_text(json.dumps(record, sort_keys=True, indent=1) + "\n")


if __name__ == "__main__":
    main()
