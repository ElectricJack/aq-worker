"""Supervisor-as-owner: answer the proof task's agent question once it appears.

The answer text is supplied by the supervisor (this agent) for the phase 6
proof, through the owner action question.answer on the admin listener."""
import json, sys, time, urllib.request, uuid
ORIGIN = "http://127.0.0.1:28080"
TASK, OUT = sys.argv[1], sys.argv[2]
ANSWER = ("Yes: include the deprecated API, in a separate 'Deprecations' line. "
          "(Answer supplied by the supervisor acting as owner for the spec 0006 phase 6 proof.)")
H = {"Content-Type": "application/json", "X-AQ-Request": "1"}
def call(path, body):
    r = urllib.request.Request(ORIGIN + path, data=json.dumps(body).encode(), headers=H, method="POST")
    try:
        with urllib.request.urlopen(r, timeout=15) as resp:
            return resp.status, json.load(resp)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")
    except (urllib.error.URLError, OSError, ValueError):
        return 0, {}
log = open(OUT, "a")
def note(**k):
    k["at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()); log.write(json.dumps(k) + "\n"); log.flush(); print(json.dumps(k)[:600], flush=True)
deadline = time.time() + float(sys.argv[3] if len(sys.argv) > 3 else 3600)
while time.time() < deadline:
    status, page = call("/api/v1/commands/question.list", {"params": {}})
    items = [i for i in page.get("items", []) if TASK in json.dumps(i)]
    open_items = [i for i in items if i.get("state") in ("pending", "open")]
    if open_items:
        q = open_items[0]
        note(event="question_seen", question=q)
        did, rev = q.get("decision_id") or q.get("id"), str(q.get("decision_revision") or q.get("revision"))
        op = str(uuid.uuid4())
        req = {"operation_id": op, "kind": "question.answer", "subject": {"type": "decision", "id": did, "revision": rev},
               "expected_versions": {"decision": rev},
               "payload": {"operation_id": op, "expected_revision": rev, "answer": ANSWER}}
        status, resp = call("/api/v1/actions", req)
        note(event="owner_answer", http=status, request=req, response=resp.get("data", resp))
        status, shown = call("/api/v1/commands/question.show", {"params": {"decision_id": did}})
        note(event="question_after", http=status, question=shown)
        break
    time.sleep(3)
else:
    note(event="timeout")
