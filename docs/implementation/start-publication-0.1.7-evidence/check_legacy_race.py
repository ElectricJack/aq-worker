"""Verify the slow-writer regression rejects the former publication semantics.

Run only against a disposable extraction of the source release. Restore its exact
source bytes even if the test or assertion fails; never modify the task checkout.
"""
import hashlib
import json
from pathlib import Path
import subprocess
import sys

root = Path(sys.argv[1]).resolve()
source = root / 'crates/aq-session-host/src/lib.rs'
original = source.read_bytes()
text = original.decode()
start = text.index('fn durable_write_with(')
insertion = text.index('    use std::{ffi::CString, os::unix::ffi::OsStrExt};', start)
legacy = '''    if !replace {
        let mut file = OpenOptions::new()
            .write(true).create_new(true).mode(0o600).open(path)?;
        write(&mut file)?;
        file.sync_all()?;
        File::open(path.parent().context("state parent")?)?.sync_all()?;
        return Ok(());
    }
'''
mutated = (text[:insertion] + legacy + text[insertion:]).encode()
command = ['cargo', 'test', '--locked', '--manifest-path', str(root / 'Cargo.toml'),
           '-p', 'aq-session-host', '--lib',
           'io_tests::new_request_is_invisible_to_polling_reader_during_slow_write',
           '--', '--exact', '--nocapture']
try:
    source.write_bytes(mutated)
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, check=False)
    print(json.dumps({'command': command, 'exit_code': result.returncode,
                      'original_sha256': hashlib.sha256(original).hexdigest(),
                      'mutated_sha256': hashlib.sha256(mutated).hexdigest()}, indent=2))
    print(result.stdout)
    assert result.returncode != 0, 'regression did not detect legacy publication'
    assert 'partial request was published' in result.stdout, 'unexpected test failure'
finally:
    source.write_bytes(original)
assert source.read_bytes() == original
print('Legacy publication rejected; exact source bytes restored.')
