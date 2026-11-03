set -u
cd /work/srv
echo "== env"; env | grep '^CARGO_' | sort
echo "== /.cargo/config.toml"; cat /.cargo/config.toml
echo "== limits"; echo "memory.max=$(cat /sys/fs/cgroup/memory.max) cpu.max=$(cat /sys/fs/cgroup/cpu.max) nproc=$(nproc)"
echo "== linker"; rustc -vV | sed -n '1p;/host/p'
cargo fetch --locked -q && echo fetched
start=$(date +%s)
python3 - <<'PY'
import resource, subprocess, sys
r = subprocess.run(["cargo","test","--locked","-p","aq-server","--test","config"], stdout=open("/work/test.out","w"), stderr=open("/work/test.err","w"))
print("cargo exit", r.returncode)
print("largest single process max RSS (KiB):", resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss)
PY
echo "wall seconds: $(( $(date +%s) - start ))"
echo "== cgroup"; echo "memory.peak bytes=$(cat /sys/fs/cgroup/memory.peak)"; grep -E '^(oom|oom_kill|max) ' /sys/fs/cgroup/memory.events
grep -E '^(anon|file) ' /sys/fs/cgroup/memory.stat
echo "== jobs evidence"; grep -c Compiling /work/test.err; grep -E "test result|Finished|Running" /work/test.out /work/test.err | head
echo "== linker used"; grep -m1 -o 'fuse-ld=[a-z]*' /work/link.txt 2>/dev/null || true
