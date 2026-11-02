#!/usr/bin/env bash
# Explicit local qualification fixture, never a production isolation profile.
set -euo pipefail
qualification_probe=false
qualification_mode=tests
if [[ ${1:-} == --runner ]]; then qualification_mode=runner; shift; fi
if [[ ${1:-} == --probe ]]; then qualification_probe=true; shift; fi
if [[ $# -gt 1 || ( $qualification_mode == runner && $# -ne 0 ) ]]; then echo "usage: $0 [--probe] [test-filter] | --runner [--probe]" >&2; exit 2; fi
qualification_filter=${1:-codex_process_scenarios}
qualification_outer_pid=$$
qualification_outer_pidns=$(readlink /proc/thread-self/ns/pid)
qualification_uid=$(id -u)
qualification_gid=$(id -g)
# Literal private NSS/time files; no operator /etc, home, root or service state.
exec {qualification_passwd_fd}< <(printf 'aq:x:%s:%s:AQ fixture:/home/aq:/bin/bash\n' "$qualification_uid" "$qualification_gid")
exec {qualification_group_fd}< <(printf 'aq:x:%s:\n' "$qualification_gid")
exec {qualification_nss_fd}< <(printf 'passwd: files\ngroup: files\nhosts: files\n')
exec {qualification_hosts_fd}< <(printf '127.0.0.1 localhost\n::1 localhost\n')
exec {qualification_timezone_fd}< <(printf 'Etc/UTC\n')
qualification_bwrap=(/usr/bin/bwrap --die-with-parent --new-session
  --unshare-user --unshare-net --unshare-pid --unshare-uts --hostname aq-qualification --cap-drop ALL
  --ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /sbin /sbin --ro-bind /lib /lib
  --proc /proc --dev /dev --tmpfs /tmp --tmpfs /home --tmpfs /root --tmpfs /opt
  --dir /opt/aq-qualification --dir /etc
  --ro-bind-data "$qualification_passwd_fd" /etc/passwd --ro-bind-data "$qualification_group_fd" /etc/group
  --ro-bind-data "$qualification_nss_fd" /etc/nsswitch.conf --ro-bind-data "$qualification_hosts_fd" /etc/hosts
  --ro-bind-data "$qualification_timezone_fd" /etc/timezone
  --symlink /usr/share/zoneinfo/Etc/UTC /etc/localtime --symlink /proc/mounts /etc/mtab
  --chdir /tmp --clearenv --setenv PATH /usr/bin:/bin --setenv TERM xterm-256color
  --setenv TMPDIR /var/lib/aq-qualification --setenv AQ_NATIVE_FIXTURE_CONFINED v1
  --setenv AQ_NATIVE_OUTER_PID "$qualification_outer_pid" --setenv AQ_NATIVE_OUTER_PIDNS "$qualification_outer_pidns")
if [[ $qualification_mode == runner ]]; then
  # Explicit fixture disk capacity; no Git profile/reservation/minimum is lowered.
  qualification_bwrap+=(--size 25769803776 --tmpfs /var/lib)
else
  qualification_bwrap+=(--tmpfs /var/lib)
fi
if [[ -d /lib64 ]]; then qualification_bwrap+=(--ro-bind /lib64 /lib64); fi
if [[ $qualification_probe == false ]]; then
  if [[ $qualification_mode == runner ]]; then
    : "${AQ_NATIVE_WORKER_BINARY:?explicit compiled worker binary required}"
    : "${AQ_NATIVE_WORKER_SHA256:?explicit worker digest required}"
    : "${AQ_NATIVE_GIT_BINARY:?explicit aq-git executable required}"
    : "${AQ_NATIVE_GIT_SHA256:?explicit aq-git digest required}"
    : "${AQ_NATIVE_FIXTURE_CONTROL_SOCKET:?explicit owned fixture socket required}"
    [[ -S $AQ_NATIVE_FIXTURE_CONTROL_SOCKET ]]
    qualification_target=$AQ_NATIVE_WORKER_BINARY
    qualification_target_name=aq-worker
  else
    : "${AQ_NATIVE_TEST_BINARY:?explicit compiled test binary required}"
    qualification_target=$AQ_NATIVE_TEST_BINARY
    qualification_target_name=aq-worker-tests
  fi
  : "${AQ_NATIVE_HOST_BINARY:?explicit pinned helper required}"
  : "${AQ_NATIVE_HOST_SHA256:?explicit helper digest required}"
  : "${AQ_NATIVE_CODEX_BINARY:?explicit pinned image required}"
  : "${AQ_NATIVE_CODEX_SHA256:?explicit image digest required}"
  : "${AQ_NATIVE_TMUX_BINARY:?explicit pinned tmux required}"
  : "${AQ_NATIVE_TMUX_SHA256:?explicit tmux digest required}"
  qualification_test=$(realpath -- "$qualification_target")
  qualification_host=$(realpath -- "$AQ_NATIVE_HOST_BINARY")
  qualification_codex=$(realpath -- "$AQ_NATIVE_CODEX_BINARY")
  qualification_tmux=$(realpath -- "$AQ_NATIVE_TMUX_BINARY")
  for qualification_file in "$qualification_test" "$qualification_host" "$qualification_codex" "$qualification_tmux"; do [[ -f $qualification_file && -x $qualification_file ]]; done
  qualification_bwrap+=(
    --ro-bind "$qualification_test" "/opt/aq-qualification/$qualification_target_name"
    --ro-bind "$qualification_host" /opt/aq-qualification/aq-session-host
    --ro-bind "$qualification_codex" /opt/aq-qualification/codex
    --ro-bind "$qualification_tmux" /opt/aq-qualification/tmux
    --setenv AQ_NATIVE_HOST_BINARY /opt/aq-qualification/aq-session-host --setenv AQ_NATIVE_HOST_SHA256 "$AQ_NATIVE_HOST_SHA256"
    --setenv AQ_NATIVE_CODEX_BINARY /opt/aq-qualification/codex --setenv AQ_NATIVE_CODEX_SHA256 "$AQ_NATIVE_CODEX_SHA256"
    --setenv AQ_NATIVE_TMUX_BINARY /opt/aq-qualification/tmux --setenv AQ_NATIVE_TMUX_SHA256 "$AQ_NATIVE_TMUX_SHA256")
  if [[ $qualification_mode == runner ]]; then
    qualification_git=$(realpath -- "$AQ_NATIVE_GIT_BINARY")
    [[ -f $qualification_git && -x $qualification_git ]]
    qualification_launcher=$(realpath -- "$(dirname -- "${BASH_SOURCE[0]}")/native-runner-fixture.py")
    qualification_bwrap+=(
      --ro-bind "$qualification_git" /opt/aq-qualification/aq-git
      --ro-bind "$qualification_launcher" /opt/aq-qualification/native-runner-fixture.py
      --ro-bind "$AQ_NATIVE_FIXTURE_CONTROL_SOCKET" /opt/aq-qualification/fixture-control.sock
      --setenv AQ_NATIVE_WORKER_SHA256 "$AQ_NATIVE_WORKER_SHA256"
      --setenv AQ_NATIVE_GIT_SHA256 "$AQ_NATIVE_GIT_SHA256")
    if [[ -n ${AQ_NATIVE_AGENT_CLIENT_BINARY:-} ]]; then
      : "${AQ_NATIVE_AGENT_CLIENT_SHA256:?explicit agent client digest required}"
      qualification_client=$(realpath -- "$AQ_NATIVE_AGENT_CLIENT_BINARY")
      [[ -f $qualification_client && -x $qualification_client ]]
      qualification_bwrap+=(--ro-bind "$qualification_client" /opt/aq-qualification/aq-agent
        --setenv AQ_NATIVE_AGENT_CLIENT_SHA256 "$AQ_NATIVE_AGENT_CLIENT_SHA256")
    fi
  fi
fi
qualification_bwrap+=(--remount-ro /opt --remount-ro /)
# No content/body or environment dumps in the optional external trace.
qualification_trace=()
if [[ -n ${AQ_NATIVE_TRACE_FILE:-} ]]; then
  qualification_trace=(/usr/bin/strace -f -s 180
    -e trace=execve,execveat,openat,newfstatat,readlink,readlinkat,access,connect,getsockname,getpeername
    -o "$AQ_NATIVE_TRACE_FILE")
fi
exec "${qualification_trace[@]}" "${qualification_bwrap[@]}" -- /usr/bin/python3 -c '
import ctypes, errno, os, socket, stat, subprocess, sys
assert ctypes.CDLL(None).prctl(38, 1, 0, 0, 0) == 0, "no_new_privs unavailable"
status = open("/proc/thread-self/status").read()
for name in ("CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"):
    assert int(next(v.split(":", 1)[1].strip() for v in status.splitlines() if v.startswith(name + ":")), 16) == 0
pidns = os.readlink("/proc/thread-self/ns/pid")
assert pidns != os.environ["AQ_NATIVE_OUTER_PIDNS"], "host PID namespace remains visible"
assert not os.path.exists("/proc/" + os.environ["AQ_NATIVE_OUTER_PID"] + "/root"), "outside parent root alias remains visible"
assert not os.path.exists("/proc/" + os.environ["AQ_NATIVE_OUTER_PID"] + "/fd"), "outside parent fd alias remains visible"
assert os.readlink("/proc/1/ns/pid") == pidns
assert not os.listdir("/home") and not os.listdir("/root"), "operator homes visible"
assert not os.listdir("/proc/1/root/home") and not os.listdir("/proc/1/root/root"), "proc root exposes operator homes"
expected_etc = {"passwd", "group", "nsswitch.conf", "hosts", "timezone", "localtime", "mtab"}
assert set(os.listdir("/etc")) == expected_etc and set(os.listdir("/proc/1/root/etc")) == expected_etc, "operator etc image visible"
visible_mounts = {int(line.split()[0]) for line in open("/proc/thread-self/mountinfo")}
for owner in ("1", "thread-self"):
    try:
        entries = os.listdir("/proc/" + owner + "/fd")
    except PermissionError:
        continue
    for entry in entries:
        target = "/proc/" + owner + "/fd/" + entry
        try:
            if stat.S_ISDIR(os.stat(target).st_mode):
                info = open("/proc/" + owner + "/fdinfo/" + entry).read()
                mount_id = int(next(line.split(":",1)[1] for line in info.splitlines() if line.startswith("mnt_id:")))
                assert mount_id in visible_mounts, "provisioning directory fd escapes private root"
        except (FileNotFoundError, PermissionError):
            pass
assert not os.listdir("/var/lib"), "host service state visible"
os.mkdir("/var/lib/aq-qualification", 0o700)
assert os.stat("/var/lib/aq-qualification").st_mode & 0o777 == 0o700
route = subprocess.run(["/usr/sbin/ip", "route", "show", "default"], check=True, capture_output=True)
assert not route.stdout.strip(), "external route exists"
probe = socket.socket()
try:
    assert probe.connect_ex(("192.0.2.1", 9)) in (errno.ENETUNREACH, errno.EHOSTUNREACH)
finally:
    probe.close()
listener = socket.socket(); listener.bind(("127.0.0.1", 0)); listener.listen(1)
client = socket.create_connection(listener.getsockname(), timeout=1); accepted, _ = listener.accept()
accepted.close(); client.close(); listener.close()
print("qualification guard: private PID/proc roots; trusted image/minimal etc; loopback only; private state", flush=True)
if sys.argv[1] == "true": raise SystemExit(0)
if sys.argv[3] == "runner":
    os.execv("/usr/bin/python3", ["python3", "/opt/aq-qualification/native-runner-fixture.py"])
os.execv("/opt/aq-qualification/aq-worker-tests", ["aq-worker-tests", sys.argv[2], "--ignored", "--nocapture", "--test-threads=1"])
' "$qualification_probe" "$qualification_filter" "$qualification_mode"
