#!/usr/bin/env python3
"""Capture an OpenCode ChatGPT subscription login for vault enrollment (I3).

Runs `opencode auth login -p openai -m "ChatGPT Pro/Plus (headless)"` (a
device-code flow the operator completes in a browser) with HOME and every
XDG_* directory inside a fresh 0700 temporary directory. The resulting
`$XDG_DATA_HOME/opencode/auth.json` is checked to hold exactly one `openai`
OAuth login (the shape `validate_chatgpt_auth_json` accepts), copied byte for
byte to a new 0600 output file for the C5b upload, and the temporary directory
is removed. Only the material's SHA-256 and expiry are printed, never token
data.

Usage: opencode-enroll.py --opencode /path/to/opencode --out /path/login.json
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile

MAX_AUTH_BYTES = 64 * 1024
MAX_TOKEN = 16 * 1024
METHOD = 'ChatGPT Pro/Plus (headless)'
OAUTH_FIELDS = {'type', 'refresh', 'access', 'expires', 'accountId'}


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate key')
        result[key] = value
    return result


def token(value):
    return (isinstance(value, str) and 0 < len(value) <= MAX_TOKEN
            and all(33 <= ord(c) <= 126 for c in value))


def validate(material):
    """Return `openai.expires` (epoch ms) or raise ValueError."""
    if not 0 < len(material) <= MAX_AUTH_BYTES:
        raise ValueError('login size')
    try:
        auth = json.loads(material.decode('utf-8'), object_pairs_hook=unique_object)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError('login JSON') from error
    if not isinstance(auth, dict) or set(auth) != {'openai'}:
        raise ValueError('login must hold only the openai entry')
    entry = auth['openai']
    if (not isinstance(entry, dict) or not set(entry) <= OAUTH_FIELDS
            or entry.get('type') != 'oauth'
            or not token(entry.get('refresh')) or not token(entry.get('access'))
            or ('accountId' in entry and entry['accountId'] is not None
                and not token(entry['accountId']))):
        raise ValueError('openai entry must be a ChatGPT OAuth login')
    expires = entry.get('expires')
    if type(expires) is not int or not 0 < expires < 2**63:
        raise ValueError('login expiry')
    return expires


def read_private(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_AUTH_BYTES:
            raise ValueError('login file')
        return stream.read(MAX_AUTH_BYTES + 1)


def write_new(path, material):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        os.fchmod(fd, 0o600)
        view = memoryview(material)
        while view:
            view = view[os.write(fd, view):]
        os.fsync(fd)
    finally:
        os.close(fd)


def login_environment(home):
    env = {
        'HOME': str(home),
        'XDG_CONFIG_HOME': str(home / 'config'),
        'XDG_DATA_HOME': str(home / 'data'),
        'XDG_CACHE_HOME': str(home / 'cache'),
        'XDG_STATE_HOME': str(home / 'state'),
        'PATH': '/usr/local/bin:/usr/bin:/bin',
        'OPENCODE_DISABLE_AUTOUPDATE': '1',
        'OPENCODE_DISABLE_PROJECT_CONFIG': '1',
        'OPENCODE_DISABLE_CLAUDE_CODE': '1',
        'npm_config_offline': 'true',
    }
    for name in ('TERM', 'LANG', 'LC_ALL'):
        if name in os.environ:
            env[name] = os.environ[name]
    return env


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--opencode', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args(argv)
    if not args.opencode.is_absolute() or not args.out.is_absolute():
        parser.error('--opencode and --out must be absolute paths')
    if os.path.lexists(args.out):
        parser.error('--out already exists')
    home = Path(tempfile.mkdtemp(prefix='opencode-enroll-'))
    try:
        os.chmod(home, 0o700)
        work = home / 'work'
        work.mkdir(mode=0o700)
        result = subprocess.run(
            [str(args.opencode), 'auth', 'login', '-p', 'openai', '-m', METHOD],
            env=login_environment(home), cwd=work, check=False)
        if result.returncode != 0:
            print(f'opencode auth login failed (exit {result.returncode})', file=sys.stderr)
            return 1
        try:
            material = read_private(home / 'data/opencode/auth.json')
            expires = validate(material)
        except (OSError, ValueError) as error:
            print(f'no usable OpenCode ChatGPT login: {error}', file=sys.stderr)
            return 1
        write_new(args.out, material)
    finally:
        shutil.rmtree(home, ignore_errors=True)
    when = datetime.datetime.fromtimestamp(expires / 1000, datetime.timezone.utc)
    print(json.dumps({
        'kind': 'opencode_chatgpt_auth',
        'out': str(args.out),
        'sha256': hashlib.sha256(material).hexdigest(),
        'expires': when.isoformat(timespec='seconds'),
    }))
    return 0


if __name__ == '__main__':
    sys.exit(main())
