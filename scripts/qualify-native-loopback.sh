#!/usr/bin/env bash
# Explicit local qualification fixture, never a production isolation profile.
set -euo pipefail

qualification_probe=false
if [[ ${1:-} == --probe ]]; then
  qualification_probe=true
  shift
fi
if [[ $# -gt 1 ]]; then
  echo "usage: $0 [--probe] [test-filter]" >&2
  exit 2
fi
qualification_filter=${1:-codex_process_scenarios}
qualification_bwrap=(/usr/bin/bwrap --die-with-parent --unshare-user --unshare-net --cap-drop ALL
  --ro-bind / / --tmpfs /tmp --tmpfs /home --tmpfs /root --tmpfs /opt --tmpfs /var/lib
  --dir /opt/aq-qualification --proc /proc --dev /dev --chdir /tmp --clearenv
  --setenv PATH /usr/bin:/bin --setenv TERM xterm-256color
  --setenv TMPDIR /var/lib/aq-qualification
  --setenv AQ_NATIVE_FIXTURE_CONFINED v1)

if [[ $qualification_probe == false ]]; then
  : "${AQ_NATIVE_TEST_BINARY:?explicit compiled test binary required}"
  : "${AQ_NATIVE_HOST_BINARY:?explicit pinned helper required}"
  : "${AQ_NATIVE_HOST_SHA256:?explicit helper digest required}"
  : "${AQ_NATIVE_CODEX_BINARY:?explicit pinned Codex image required}"
  : "${AQ_NATIVE_CODEX_SHA256:?explicit Codex digest required}"
  : "${AQ_NATIVE_TMUX_BINARY:?explicit pinned tmux required}"
  : "${AQ_NATIVE_TMUX_SHA256:?explicit tmux digest required}"
  qualification_test=$(realpath -- "$AQ_NATIVE_TEST_BINARY")
  qualification_host=$(realpath -- "$AQ_NATIVE_HOST_BINARY")
  qualification_codex=$(realpath -- "$AQ_NATIVE_CODEX_BINARY")
  qualification_tmux=$(realpath -- "$AQ_NATIVE_TMUX_BINARY")
  for qualification_file in "$qualification_test" "$qualification_host" "$qualification_codex" "$qualification_tmux"; do
    [[ -f $qualification_file && -x $qualification_file ]]
  done
  qualification_bwrap+=(
    --ro-bind "$qualification_test" /opt/aq-qualification/aq-worker-tests
    --ro-bind "$qualification_host" /opt/aq-qualification/aq-session-host
    --ro-bind "$qualification_codex" /opt/aq-qualification/codex
    --ro-bind "$qualification_tmux" /opt/aq-qualification/tmux
    --setenv AQ_NATIVE_HOST_BINARY /opt/aq-qualification/aq-session-host
    --setenv AQ_NATIVE_HOST_SHA256 "$AQ_NATIVE_HOST_SHA256"
    --setenv AQ_NATIVE_CODEX_BINARY /opt/aq-qualification/codex
    --setenv AQ_NATIVE_CODEX_SHA256 "$AQ_NATIVE_CODEX_SHA256"
    --setenv AQ_NATIVE_TMUX_BINARY /opt/aq-qualification/tmux
    --setenv AQ_NATIVE_TMUX_SHA256 "$AQ_NATIVE_TMUX_SHA256")
fi

qualification_bwrap+=(--remount-ro /opt)

# Only file/process/socket-address metadata. No read/write bodies or environment
# dumps: private auth and provider traffic must not enter this optional trace.
qualification_trace=()
if [[ -n ${AQ_NATIVE_TRACE_FILE:-} ]]; then
  qualification_trace=(/usr/bin/strace -f -s 180
    -e trace=execve,execveat,openat,newfstatat,readlink,readlinkat,access,connect,getsockname,getpeername
    -o "$AQ_NATIVE_TRACE_FILE")
fi

exec "${qualification_trace[@]}" "${qualification_bwrap[@]}" -- /usr/bin/python3 -c '
import ctypes, errno, os, socket, subprocess, sys
assert ctypes.CDLL(None).prctl(38, 1, 0, 0, 0) == 0, "no_new_privs unavailable"
status = open("/proc/self/status").read()
for name in ("CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"):
    assert int(next(v.split(":", 1)[1].strip() for v in status.splitlines() if v.startswith(name + ":")), 16) == 0, "capability set is not empty"
assert not os.listdir("/home"), "operator homes must be independently masked"
assert not os.listdir("/root"), "operator root home must be independently masked"
assert not os.listdir("/var/lib"), "host service state must be independently masked"
os.mkdir("/var/lib/aq-qualification", 0o700)
assert os.stat("/var/lib/aq-qualification").st_mode & 0o777 == 0o700
route = subprocess.run(["/usr/sbin/ip", "route", "show", "default"], check=True, capture_output=True)
assert not route.stdout.strip(), "qualification namespace must have no external route"
probe = socket.socket()
try:
    assert probe.connect_ex(("192.0.2.1", 9)) in (errno.ENETUNREACH, errno.EHOSTUNREACH), "external egress was not denied"
finally:
    probe.close()
listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen(1)
client = socket.create_connection(listener.getsockname(), timeout=1)
accepted, _ = listener.accept()
accepted.close(); client.close(); listener.close()
print("qualification guard: loopback works; no external route; operator homes/service state masked", flush=True)
if sys.argv[1] == "true":
    raise SystemExit(0)
os.execv("/opt/aq-qualification/aq-worker-tests", ["aq-worker-tests", sys.argv[2], "--ignored", "--nocapture", "--test-threads=1"])
' "$qualification_probe" "$qualification_filter"
