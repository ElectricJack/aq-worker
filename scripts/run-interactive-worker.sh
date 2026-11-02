#!/usr/bin/env bash
# Practical local node: ordinary production CLI, persistent state, real network.
# Public executable bytes get a separate read-only filesystem; operator homes
# and credentials are not mounted. No fixture admission/provider logic lives here.
set -euo pipefail
interactive_inspect=false
if [[ ${1:-} == --inspect ]]; then interactive_inspect=true; shift; fi
if [[ $# != 1 ]]; then echo "usage: $0 [--inspect] BUNDLE_DIRECTORY" >&2; exit 2; fi
interactive_bundle=$(realpath -- "$1")
interactive_script=$(realpath -- "$(dirname -- "${BASH_SOURCE[0]}")/interactive-worker-bootstrap.py")
[[ -d $interactive_bundle && -f $interactive_bundle/images.json ]]
mkdir -p -- "$interactive_bundle/config" "$interactive_bundle/worker-state/host" "$interactive_bundle/worker-state/home"
chmod 700 -- "$interactive_bundle" "$interactive_bundle/config" "$interactive_bundle/worker-state" "$interactive_bundle/worker-state/host" "$interactive_bundle/worker-state/home"
if [[ $interactive_inspect == false && -e $interactive_bundle/worker-state/journal/journal.json ]]; then
  echo "Existing native journal preserved; select a fresh boot state explicitly." >&2; exit 2
fi
interactive_rows=$(python3 - "$interactive_bundle/images.json" <<'PY'
import json, pathlib, stat, sys
v=json.loads(pathlib.Path(sys.argv[1]).read_bytes())
assert set(v)=={'schema_version','programs'} and v['schema_version']==1
expected={'aq-worker','aq-session-host','aq-agent','aq-git','codex','tmux'}
assert isinstance(v['programs'],list) and len(v['programs'])==len(expected)
for p in v['programs']:
 assert set(p)=={'name','path','sha256','bytes'} and p['name'] in expected
 expected.remove(p['name'])
 path=pathlib.Path(p['path']).resolve(strict=True); m=path.stat()
 assert path.is_absolute() and not any(c in str(path) for c in '\t\r\n')
 assert stat.S_ISREG(m.st_mode) and m.st_mode&0o111 and not m.st_mode&0o022
 assert 0<m.st_size==p['bytes']<=512*1024*1024
 assert len(p['sha256'])==64 and all(c in '0123456789abcdef' for c in p['sha256'])
 print(p['name'],path,p['sha256'],p['bytes'],sep='\t')
assert not expected
PY
)
interactive_uid=$(id -u); interactive_gid=$(id -g)
exec {interactive_passwd}< <(printf 'aq:x:%s:%s:Agent Q:/state/home:/bin/bash\n' "$interactive_uid" "$interactive_gid")
exec {interactive_group}< <(printf 'aq:x:%s:\n' "$interactive_gid")
exec {interactive_nss}< <(printf 'passwd: files\ngroup: files\nhosts: files dns\n')
interactive_args=(/usr/bin/bwrap --die-with-parent --new-session --unshare-user --unshare-pid --unshare-uts
  --hostname aq-interactive-worker --cap-drop ALL
  --ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /sbin /sbin --ro-bind /lib /lib
  --proc /proc --dev /dev --tmpfs /tmp --tmpfs /home --tmpfs /root --dir /etc
  --size 1073741824 --tmpfs /opt --dir /opt/aq-runtime
  --bind "$interactive_bundle/worker-state" /state --ro-bind "$interactive_bundle/config" /config
  --ro-bind "$interactive_script" /opt/interactive-worker-bootstrap.py
  --ro-bind-data "$interactive_passwd" /etc/passwd --ro-bind-data "$interactive_group" /etc/group
  --ro-bind-data "$interactive_nss" /etc/nsswitch.conf
  --ro-bind /etc/hosts /etc/hosts --ro-bind /etc/resolv.conf /etc/resolv.conf
  --ro-bind /etc/ssl/certs /etc/ssl/certs --symlink /proc/mounts /etc/mtab
  --clearenv --setenv PATH /usr/bin:/bin --setenv HOME /state/home
  --setenv TERM xterm-256color --setenv LANG C.UTF-8 --chdir /state)
if [[ -d /lib64 ]]; then interactive_args+=(--ro-bind /lib64 /lib64); fi
while IFS=$'\t' read -r interactive_name interactive_path interactive_sha interactive_bytes; do
  exec {interactive_image}< "$interactive_path"
  [[ $(sha256sum "/proc/self/fd/$interactive_image" | cut -d' ' -f1) == "$interactive_sha" ]]
  [[ $(stat -Lc %s "/proc/self/fd/$interactive_image") == "$interactive_bytes" ]]
  interactive_args+=(--perms 0755 --file "$interactive_image" "/opt/aq-runtime/$interactive_name")
done <<< "$interactive_rows"
interactive_args+=(--remount-ro /opt --remount-ro /)
# Networking is deliberately inherited for real AQ control and native provider
# TLS. This is a trusted local node, not a network-isolation qualification.
printf '%s\n' "$$" > "$interactive_bundle/runtime-host.pid"
exec "${interactive_args[@]}" -- /usr/bin/python3 /opt/interactive-worker-bootstrap.py "$interactive_inspect"
