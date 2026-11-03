"""Synthetic credentials only: prove I0 preserves rotation and refuses conflicts."""
import importlib.util
import json
import os
from pathlib import Path
import stat

import pytest

SCRIPT = Path(__file__).resolve().parents[3] / 'scripts/i0-auth-writeback.py'


def helper():
    assert SCRIPT.is_file(), 'I0 auth writeback helper is missing'
    spec = importlib.util.spec_from_file_location('i0_auth_writeback', SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def auth(path, version):
    path.write_text(json.dumps({'auth_mode': 'chatgpt', 'OPENAI_API_KEY': None,
                               'tokens': {'access_token': 'synthetic-access-'+version,
                                          'refresh_token': 'synthetic-refresh-'+version,
                                          'id_token': 'synthetic-id-'+version,
                                          'account_id': 'synthetic-account'}}))
    path.chmod(0o600)
    return path


def inputs(tmp_path):
    return tuple(auth(tmp_path/name, 'old') for name in ('owner', 'baseline', 'refreshed'))


def test_rotated_login_is_atomically_written_with_private_mode(tmp_path):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    old_inode = owner.stat().st_ino
    auth(refreshed, 'new')
    assert module.writeback(owner, baseline, refreshed) == 'refreshed'
    assert owner.read_bytes() == refreshed.read_bytes()
    assert owner.stat().st_ino != old_inode
    assert stat.S_IMODE(owner.stat().st_mode) == 0o600
    assert baseline.read_bytes() != owner.read_bytes()


def test_concurrent_owner_refresh_preserves_both_versions(tmp_path):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    auth(owner, 'other-session')
    auth(refreshed, 'container')
    before = owner.read_bytes(), refreshed.read_bytes()
    with pytest.raises(ValueError, match='owner changed'):
        module.writeback(owner, baseline, refreshed)
    assert (owner.read_bytes(), refreshed.read_bytes()) == before


def test_symlink_and_invalid_login_refuse_without_disclosing_material(tmp_path):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    link = tmp_path/'link'
    link.symlink_to(refreshed)
    with pytest.raises(ValueError):
        module.writeback(owner, baseline, link)
    refreshed.write_text('{"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-secret"}')
    with pytest.raises(ValueError) as error:
        module.writeback(owner, baseline, refreshed)
    assert 'synthetic-secret' not in str(error.value)
    assert owner.read_bytes() == baseline.read_bytes()


def test_unchanged_login_keeps_owner_inode(tmp_path):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    old_inode = owner.stat().st_ino
    assert module.writeback(owner, baseline, refreshed) == 'unchanged'
    assert owner.stat().st_ino == old_inode


def test_missing_launch_login_does_not_modify_owner(tmp_path):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    refreshed.unlink()
    before = owner.read_bytes()
    assert module.writeback(owner, baseline, refreshed) == 'absent'
    assert owner.read_bytes() == before


def test_unsafe_mode_and_oversized_login_refuse(tmp_path):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    refreshed.chmod(0o644)
    with pytest.raises(ValueError):
        module.writeback(owner, baseline, refreshed)
    refreshed.chmod(0o600)
    refreshed.write_bytes(b'x'*65537)
    with pytest.raises(ValueError):
        module.writeback(owner, baseline, refreshed)
    assert owner.read_bytes() == baseline.read_bytes()


def test_reading_an_older_login_accepts_access_time_changes(tmp_path):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    for path in (owner, baseline, refreshed):
        os.utime(path, (1, 2))
    assert module.writeback(owner, baseline, refreshed) == 'unchanged'


def test_duplicate_auth_fields_refuse_without_modifying_owner(tmp_path):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    before = owner.read_bytes()
    refreshed.write_text(refreshed.read_text().replace(
        '"auth_mode": "chatgpt"', '"auth_mode": "apikey", "auth_mode": "chatgpt"'))
    with pytest.raises(ValueError):
        module.writeback(owner, baseline, refreshed)
    assert owner.read_bytes() == before


@pytest.mark.parametrize('invalid', [
    'missing_account', 'blank_account', 'long_account', 'spaced_token',
    'unicode_token', 'long_token', 'unknown_root', 'unknown_token',
    'invalid_refresh', 'long_refresh',
])
def test_native_invalid_login_preserves_owner_and_candidate(tmp_path, invalid):
    module = helper()
    owner, baseline, refreshed = inputs(tmp_path)
    value = json.loads(refreshed.read_bytes())
    if invalid == 'missing_account':
        del value['tokens']['account_id']
    elif invalid == 'blank_account':
        value['tokens']['account_id'] = ''
    elif invalid == 'long_account':
        value['tokens']['account_id'] = 'x'*513
    elif invalid == 'spaced_token':
        value['tokens']['access_token'] = 'synthetic private token'
    elif invalid == 'unicode_token':
        value['tokens']['access_token'] = 'synthetic-\u00e9'
    elif invalid == 'long_token':
        value['tokens']['access_token'] = 'x'*32769
    elif invalid == 'unknown_root':
        value['unsupported'] = 'synthetic-private-value'
    elif invalid == 'unknown_token':
        value['tokens']['unsupported'] = 'synthetic-private-value'
    elif invalid == 'invalid_refresh':
        value['last_refresh'] = ['synthetic-private-value']
    elif invalid == 'long_refresh':
        value['last_refresh'] = 'x'*129
    refreshed.write_text(json.dumps(value))
    before = owner.read_bytes(), refreshed.read_bytes()
    with pytest.raises(ValueError) as error:
        module.writeback(owner, baseline, refreshed)
    assert 'synthetic-private-value' not in str(error.value)
    assert (owner.read_bytes(), refreshed.read_bytes()) == before
