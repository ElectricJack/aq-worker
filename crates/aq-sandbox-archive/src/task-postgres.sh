# Docker executes this setup as the template's unprivileged agent user.
set -uo pipefail
umask 077
root=/run/aq-postgres
# An uncertain create/start is never a reason to initialize a second database.
mkdir "$root/initializer" 2>/dev/null || exit 0
bindir=
fail() {
  if [ -n "$bindir" ] && [ -f "$root/data/postmaster.pid" ]; then
    "$bindir/pg_ctl" -D "$root/data" -m immediate -w -t 5 stop >/dev/null 2>&1 || true
  fi
  status "$1"
  exit "$1"
}
status() {
  printf '%s\n' "$1" >"$root/status.tmp" && chmod 0644 "$root/status.tmp" &&
    mv "$root/status.tmp" "$root/status"
}
[ "$EUID" -ne 0 ] || fail 95
bindir="$(pg_config --bindir 2>/dev/null)" || fail 95
for tool in initdb pg_ctl postgres psql; do
  [ -x "$bindir/$tool" ] || fail 95
done
# The harness UID may differ from the database UID. Only processes in this
# sandbox see the socket. The cluster data itself is private to its owner.
mkdir -m 0755 "$root/socket" || fail 96
unset PGHOSTADDR PGSERVICE PGSERVICEFILE PGOPTIONS PGPASSWORD PGPASSFILE
export PGHOST="$root/socket" PGPORT=5432 PGUSER=postgres PGDATABASE=postgres
timeout 25s "$bindir/initdb" -D "$root/data" --no-sync --no-locale --encoding=UTF8 \
  -U postgres --auth-local=trust --auth-host=reject >"$root/init.log" 2>&1 || fail 96
"$bindir/pg_ctl" -D "$root/data" -l "$root/postgres.log" -w -t 20 \
  -o "-c listen_addresses='' -c unix_socket_directories='$PGHOST' -c unix_socket_permissions=0777 -c port=$PGPORT -c shared_buffers=32MB -c max_connections=20" \
  start >/dev/null 2>&1 || fail 96
[ "$(timeout 5s "$bindir/psql" -XqAtc 'SELECT 1')" = 1 ] || fail 96
status 0 || fail 96
