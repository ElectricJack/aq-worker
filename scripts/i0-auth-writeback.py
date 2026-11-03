#!/usr/bin/env python3
"""Explicit I0 login reconciliation; prints status only, never credential data.

Requires quiescent owner writers: compare/rename is not a multiwriter transaction.
"""
import argparse
import json
import os
from pathlib import Path
import stat
import tempfile


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError
        result[key] = value
    return result


def identity(info):
    # A successful read can update atime; only mutation and identity fields count.
    return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_gid,
            info.st_nlink, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def graphic(value, maximum, *, empty=False):
    return (isinstance(value, str) and (empty or bool(value))
            and len(value) <= maximum
            and all(33 <= ord(character) <= 126 for character in value))


def private_login(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                    or stat.S_IMODE(info.st_mode) != 0o600 or not 0 < info.st_size <= 65536):
                raise ValueError
            data = stream.read(65537)
            after = os.fstat(stream.fileno())
        named = path.lstat()
        if (len(data) != info.st_size or identity(after) != identity(info)
                or identity(named) != identity(info)):
            raise ValueError
        value = json.loads(data, object_pairs_hook=unique_object)
        token_keys = {'access_token', 'refresh_token', 'id_token', 'account_id'}
        if (not isinstance(value, dict)
                or set(value) - {'auth_mode', 'OPENAI_API_KEY', 'tokens', 'last_refresh'}
                or value.get('auth_mode') != 'chatgpt' or value.get('OPENAI_API_KEY') is not None
                or not isinstance(value.get('tokens'), dict)
                or set(value['tokens']) != token_keys
                or any(not graphic(value['tokens'][key], 32768)
                       for key in token_keys - {'account_id'})
                or not graphic(value['tokens']['account_id'], 512)
                or (value.get('last_refresh') is not None
                    and not graphic(value['last_refresh'], 128, empty=True))):
            raise ValueError
        return data
    except (OSError, ValueError, TypeError, AttributeError):
        raise ValueError('private ChatGPT login refused') from None


def writeback(owner: Path, baseline: Path, refreshed: Path) -> str:
    original = private_login(baseline)
    current = private_login(owner)
    # lexists distinguishes an absent launch from a dangling symlink.
    if not os.path.lexists(refreshed):
        return 'absent'
    updated = private_login(refreshed)
    if updated == original:
        return 'unchanged'
    if current == updated:
        return 'refreshed'
    if current != original:
        raise ValueError('owner changed; preserve both login files for reconciliation')
    temporary = None
    try:
        fd, temporary = tempfile.mkstemp(prefix='.i0-auth-', dir=owner.parent)
        with os.fdopen(fd, 'wb') as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(updated)
            stream.flush()
            os.fsync(stream.fileno())
        if private_login(owner) != original:
            raise ValueError('owner changed; preserve both login files for reconciliation')
        os.replace(temporary, owner)
        temporary = None
        directory = os.open(owner.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
        return 'refreshed'
    finally:
        if temporary is not None:
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--owner', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--refreshed', type=Path, required=True)
    args = parser.parse_args()
    try:
        print(writeback(args.owner, args.baseline, args.refreshed))
    except (ValueError, OSError):
        parser.exit(2, 'login reconciliation refused; preserve container and private state\n')


if __name__ == '__main__':
    main()
