set -uo pipefail
IFS= read -r token || [ -n "$token" ]; token=${token%$'\r'}
[ -n "$token" ] || { echo "no token on stdin" >&2; exit 1; }
export HOME=/home/agent/.aq/claude CLAUDE_CONFIG_DIR=/home/agent/.aq/claude DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_DISABLE_AUTO_MEMORY=1 PATH=/opt/aq/bin:/usr/local/bin:/usr/bin:/bin
mkdir -p "$HOME" /aq/work && cd /aq/work
echo '{"permissions":{"defaultMode":"bypassPermissions"},"sandbox":{"enabled":false},"disableAllHooks":true,"enableAllProjectMcpServers":false,"cleanupPeriodDays":1,"forceLoginMethod":"claudeai"}' > "$HOME/settings.json"
: > /p/out.jsonl
results() { grep -c '"type":"result"' /p/out.jsonl; }
waitfor() { local n=$1 i=0; while [ "$(results)" -lt "$n" ]; do sleep 1; i=$((i+1)); [ $i -lt 240 ] || { echo "timeout waiting for result $n" >> /p/timeline; return 1; }; done; }
feeder() {
  echo "t_msg1=$(date +%s%3N)" >> /p/timeline; cat /p/msg1.json
  sleep 3
  echo "t_msg2_mid_turn=$(date +%s%3N)" >> /p/timeline; cat /p/msg2.json
  waitfor 1 || return; echo "t_result1_seen=$(date +%s%3N)" >> /p/timeline
  echo "t_stdin_closed=$(date +%s%3N)" >> /p/timeline
}
feeder | CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 /opt/aq/bin/claude -p --output-format stream-json --verbose --input-format stream-json --model claude-sonnet-5-5 --permission-mode bypassPermissions --setting-sources user --settings "$HOME/settings.json" --strict-mcp-config --no-session-persistence --disable-slash-commands > /p/out.jsonl 2>/p/err
rc=${PIPESTATUS[1]}
echo "t_claude_exit=$(date +%s%3N) rc=$rc" >> /p/timeline
