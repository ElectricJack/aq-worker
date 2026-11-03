"""Offline guards for the security-sensitive delta; Docker probes are opt-in."""
import hashlib
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]


def test_pinned_profile_has_only_the_reviewed_namespace_mount_delta():
    source = json.loads((ROOT / 'vendor/docker-seccomp/source.json').read_bytes())
    baseline_bytes = (ROOT / 'vendor/docker-seccomp/default.json').read_bytes()
    candidate_bytes = (ROOT / source['derived_profile']).read_bytes()
    assert hashlib.sha256(baseline_bytes).hexdigest() == source['source_sha256']
    assert hashlib.sha256(candidate_bytes).hexdigest() == source['derived_sha256']
    baseline = json.loads(baseline_bytes)
    candidate = json.loads(candidate_bytes)
    additions = candidate['syscalls'][len(baseline['syscalls']):]
    assert len(additions) == 3
    candidate['syscalls'] = candidate['syscalls'][:len(baseline['syscalls'])]
    assert candidate == baseline
    assert candidate['defaultAction'] == 'SCMP_ACT_ERRNO'
    assert additions == [
        {'names': ['clone'], 'action': 'SCMP_ACT_ALLOW',
         'args': [{'index': 0, 'value': 0x10000000, 'valueTwo': 0x10000000,
                   'op': 'SCMP_CMP_MASKED_EQ'}], 'includes': {'arches': ['amd64']}},
        {'names': ['unshare'], 'action': 'SCMP_ACT_ALLOW',
         'args': [{'index': 0, 'value': 0x10000000, 'op': 'SCMP_CMP_EQ'}]},
        {'names': ['mount', 'umount2', 'pivot_root'], 'action': 'SCMP_ACT_ALLOW'},
    ]


def test_added_rules_exclude_neighboring_namespace_operations_and_keep_clone3_fallback():
    profile = json.loads((ROOT / 'tools/qualification/seccomp/codex-bwrap-amd64.json').read_bytes())
    additions = profile['syscalls'][-3:]
    clone = additions[0]['args'][0]
    unshare = additions[1]['args'][0]
    # No newly allowed clone namespace operation without CLONE_NEWUSER.
    for flags in (0, 0x20000, 0x40000000, 0x20000000, 0x08000000):
        assert flags & clone['value'] != clone['valueTwo']
        assert flags != unshare['value']
    # No unshare of combined namespaces or CLONE_FS via the new exact rule.
    for flags in (0x10020000, 0x50000000, 0x10000200):
        assert flags != unshare['value']
    fallback = [r for r in profile['syscalls'] if r['names'] == ['clone3']]
    assert fallback == [{'names': ['clone3'], 'action': 'SCMP_ACT_ERRNO',
                         'errnoRet': 38, 'excludes': {'caps': ['CAP_SYS_ADMIN']}}]
