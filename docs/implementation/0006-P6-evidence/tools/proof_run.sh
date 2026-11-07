#!/bin/bash
# usage: proof_run.sh RUN_DIR TASK_ID
S=/tmp/claude-1000/-home-jkern-dev/aaba98a0-49a7-4b3a-8a61-fc71413a3d6c/scratchpad; R=$1; T=$2
mkdir -p "$R"; date -u +%FT%TZ > "$R/started-at"
setsid python3 -I "$S/tools/answer_watch.py" "$T" "$R/owner-answer.jsonl" 3000 > "$R/answer-watch.out" 2>&1 &
W=$!
AQ_MS1_COMPOSE_EXTRA=$S/compose.debug.yaml AQ_MS1_WORKER_SRC=$HOME/dev/aq-platform/wt/chan-aq-cli timeout 2400 ~/.local/share/aq-ms1/up.sh run > "$R/run.log" 2>&1
echo "exit=$?" >> "$R/run.log"
kill $W 2>/dev/null
tail -3 "$R/run.log" | cut -c1-300
