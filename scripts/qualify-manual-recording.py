#!/usr/bin/env python3
"""Qualify the production manual runner in a disposable, networkless Docker container.

Run from any directory; pass --raw-limit to exercise the 128 MiB capture cap.
Requires Docker and the locally available aq-bootstrap-tmux-proof:26.04 image.
"""
import argparse, pathlib, subprocess, tempfile, time, uuid, re

parser = argparse.ArgumentParser()
parser.add_argument("--raw-limit", action="store_true")
parser.add_argument("--container-file", type=pathlib.Path)
args = parser.parse_args()
repository = pathlib.Path(__file__).resolve().parent.parent

def run(*args, **kw):
    return subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kw)
with tempfile.TemporaryDirectory(prefix='aq-recording-fixture-') as tmp:
    root = pathlib.Path(tmp)
    repo = root / 'repo'; repo.mkdir()
    run('git','-C',str(repo),'init','-q')
    run('git','-C',str(repo),'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','--allow-empty','-qm','fixture')
    ref = run('git','-C',str(repo),'rev-parse','HEAD').stdout.decode().strip()
    source = (repository / 'crates/aq-worker/src/bin/aq-worker/manual.rs').read_text()
    runner = re.search(r'const RUNNER: &str = r#"(.*?)"#;', source, re.S).group(1)
    (root/'runner').write_text(runner)
    (root/'ref').write_text(ref)
    if args.raw_limit:
        (root/'agent').write_text("head -c 134218752 /dev/zero | tr '\\0' x; printf AFTER-LIMIT > /aq/out/agent-completed\n")
    else:
        (root/'agent').write_text("printf 'FIXTURE-BEGIN\\n'; i=0; while [ $i -lt 1400 ]; do printf 'fixture-line-%04d-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\\n' \"$i\"; i=$((i+1)); done; printf 'FIXTURE-END\\n'\n")
    setup = "mkdir -p /aq/in /aq/out; cp /fixture/agent /aq/in/agent.sh; printf /fixture/repo >/aq/in/repository; cp /fixture/ref /aq/in/source; date -d '+10 minutes' +%s >/aq/out/lease; exec /bin/bash /fixture/runner"
    name='aq-recording-fixture-'+uuid.uuid4().hex[:12]
    try:
        run('docker','run','-d','--network','none','--name',name,'-v',f'{root}:/fixture:ro','aq-bootstrap-tmux-proof:26.04','/bin/bash','-c',setup)
        for _ in range(300):
            p=subprocess.run(['docker','exec',name,'test','-e','/aq/out/transcript.done'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            if p.returncode==0: break
            time.sleep(.1)
        else: raise RuntimeError('capture did not finish')
        if args.raw_limit:
            size = int(run('docker','exec',name,'sh','-c','wc -c < /aq/out/transcript.raw').stdout)
            assert size == 134217728, size
            for _ in range(100):
                if subprocess.run(['docker','exec',name,'test','-e','/aq/out/agent-completed'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode == 0:
                    break
                time.sleep(.1)
            else:
                raise RuntimeError('agent blocked after raw limit')
            evidence = f'{size} bytes; agent continued after the raw limit'
        else:
            raw=run('docker','exec',name,'cat','/aq/out/transcript.raw').stdout
            assert raw.startswith(b'FIXTURE-BEGIN\r\n') and raw.endswith(b'FIXTURE-END\r\n')
            assert b'fixture-line-0700-' in raw and len(raw)>65536 and raw.count(b'\n')>500
            evidence = f'{len(raw)} bytes, {raw.count(bytes([10]))} lines, beginning/middle/end'
        if args.container_file:
            # The opt-in Rust harness drains the real container through tick().
            args.container_file.write_text(name)
        else:
            run('docker','exec',name,'sh','-c',': > /aq/out/transcript.drained')
        for _ in range(600 if args.container_file else 50):
            p=subprocess.run(['docker','inspect','--format','{{.State.Running}}',name],capture_output=True)
            if p.stdout.strip()==b'false': break
            time.sleep(.1)
        else: raise RuntimeError('runner did not exit after drain')
        print(f'PASS production RUNNER disposable Docker no-inference: {evidence}; drain exit')
    finally:
        subprocess.run(['docker','rm','-f',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
