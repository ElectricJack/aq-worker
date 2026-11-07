"""Render a Claude stream-json transcript as Bash commands and their results."""
import json, sys
calls = {}
for line in open(sys.argv[1]):
    try: e = json.loads(line)
    except ValueError: continue
    if e.get("type") == "assistant":
        for c in e["message"]["content"]:
            if c.get("type") == "tool_use":
                calls[c["id"]] = c; print("### TOOL", c["name"], json.dumps(c["input"])[:2000])
            elif c.get("type") == "text": print("### TEXT\n" + c["text"])
    elif e.get("type") == "user":
        for c in e["message"].get("content", []):
            if isinstance(c, dict) and c.get("type") == "tool_result":
                body = c.get("content")
                if isinstance(body, list): body = "\n".join(x.get("text", "") for x in body)
                print("### RESULT" + (" (error)" if c.get("is_error") else "") + "\n" + str(body))
    elif e.get("type") == "result":
        print("### FINAL", e.get("subtype"), "is_error=", e.get("is_error"), "turns=", e.get("num_turns"), "duration_ms=", e.get("duration_ms"))
