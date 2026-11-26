# Wait for the fixed PostgreSQL setup before running the existing harness ($1).
set -uo pipefail
umask 077
trap 'rm -f /aq/private/material' EXIT
trap 'exit 143' TERM
trap 'exit 129' HUP
trap 'exit 130' INT

status=96
for ((i=0; i<60; i++)); do
  if [ -f /run/aq-postgres/status ]; then
    IFS= read -r status </run/aq-postgres/status || status=96
    break
  fi
  sleep 1
done
case "$status" in
  0) printf 'aq-runner: task PostgreSQL ready on a private Unix socket\n' ;;
  95) printf 'aq-runner: postgres_capability_missing\n' >&2; exit 95 ;;
  *) printf 'aq-runner: postgres_startup_failed\n' >&2; exit 96 ;;
esac

unset PGHOSTADDR PGSERVICE PGSERVICEFILE PGOPTIONS PGPASSWORD PGPASSFILE
/bin/bash -c "$1"
rc=$?
# Only the fixed pre-harness gate can report these failure codes.
case "$rc" in
  95|96) exit 97 ;;
  *) exit "$rc" ;;
esac
