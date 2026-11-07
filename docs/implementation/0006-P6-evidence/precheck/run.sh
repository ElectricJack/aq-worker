set -uo pipefail
token="$(cat /p/material)"
while IFS= read -r kv; do [ -n "$kv" ] && export "$kv"; done < /p/env
mkdir -p "$HOME" && echo '{"permissions":{"defaultMode":"bypassPermissions"},"sandbox":{"enabled":false},"disableAllHooks":true,"forceLoginMethod":"claudeai"}' > "$HOME/settings.json"
mkdir -p /aq/work && cd /aq/work
echo 'Use your Bash tool to run exactly this one command, then reply with its raw output only: echo "AQ_CHANNEL=$AQ_CHANNEL"; echo "PATH=$PATH"; command -v aq; aq --version; id -u' | \
CLAUDE_CODE_OAUTH_TOKEN="$token" IS_SANDBOX=1 /opt/aq/bin/claude -p --output-format stream-json --verbose --model claude-sonnet-5-5 --permission-mode bypassPermissions --setting-sources user --settings "$HOME/settings.json" --strict-mcp-config --no-session-persistence --disable-slash-commands > /p/out.jsonl 2>/p/err.txt
echo rc=$?
