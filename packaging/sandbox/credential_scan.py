#!/usr/bin/env python3
"""Fail-closed OCI layer scanner. No archive member is extracted to a host path."""
import gzip
import hashlib
import json
from pathlib import Path
import posixpath
import re
import tarfile
import tempfile

MAX_ARCHIVE = 32 * 1024**3
MAX_EXPANDED = 64 * 1024**3
MAX_ENTRIES = 1_000_000
MAX_JSON = 1024 * 1024
DIGEST = re.compile(r'sha256:[0-9a-f]{64}\Z')
TOKEN_PATTERN = re.compile(
    rb'(?:sk-(?:proj-|ant-)?[A-Za-z0-9_-]{20,}|gh[pousr]_[A-Za-z0-9]{20,}'
    rb'|github_pat_[A-Za-z0-9_]{20,}|AIza[A-Za-z0-9_-]{30,}|AKIA[A-Z0-9]{16}'
    rb'|https?://[^\s/:\x22\x27]+:[^\s/@\x22\x27]+@'
    rb'|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{16,}'
    rb'|(?i:authorization\s*:\s*(?:bearer|basic)\s+[A-Za-z0-9_+/=-]{4,})'
    rb'|-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----)')
ASSIGNMENT = re.compile(rb'(?<![A-Za-z0-9_])([A-Za-z][A-Za-z0-9_]{0,127})[\x22\x27]?\s*=\s*[\x22\x27]?([A-Za-z0-9_+/.-]{4,})')
JSON_ASSIGNMENT = re.compile(rb'"([A-Za-z][A-Za-z0-9_]{0,127})"\s*:\s*"([A-Za-z0-9_+/=.:,-]{4,4096})"')
CREDENTIAL_LABEL = re.compile(r'(?:API_KEY|TOKEN|SECRET|PASSWORD|CREDENTIALS?|ACCESS_KEY_ID|SECRET_ACCESS_KEY|OPENAI_KEY)\Z')


def secret_shaped(value):
    data = value.encode() if isinstance(value, str) else value
    return bool(TOKEN_PATTERN.search(data) or any(
        CREDENTIAL_LABEL.search(match[1].decode('ascii').upper())
        for match in ASSIGNMENT.finditer(data)) or any(
        CREDENTIAL_LABEL.search(match[1].decode('ascii').upper())
        # Public protocol format identifier embedded in the Codex mapper,
        # not an authentication value. Keep this exception exact and JSON-only.
        and (match[1], match[2]) != (b'credentials', b'private-codex-chatgpt-auth-json')
        for match in JSON_ASSIGNMENT.finditer(data)))


def json_bytes(data):
    if len(data) > MAX_JSON:
        raise ValueError('JSON size limit')
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError('duplicate JSON field')
            result[key] = value
        return result
    try:
        return json.loads(data, object_pairs_hook=pairs)
    except (UnicodeError, json.JSONDecodeError, RecursionError) as error:
        raise ValueError('invalid JSON') from error


def safe_path(name):
    if len(name) > 4096 or '\\' in name or '\x00' in name or name.startswith('/'):
        raise ValueError('unsafe archive path')
    parts = name.split('/')
    if '..' in parts:
        raise ValueError('unsafe archive path')
    return '/'.join(part for part in parts if part not in ('', '.'))


def credential_path(name):
    parts = name.lower().strip('/').split('/')
    base = parts[-1]
    return (base in {'auth.json', '.git-credentials', '.netrc', '.claude.json',
                     'application_default_credentials.json', 'credentials.db',
                     'oauth_creds.json', '.credentials.json'}
            or any(p in {'.codex', '.claude', '.gemini', '.aws', '.azure'} for p in parts[:-1])
            or any('/' + p + '/' in '/' + name.lower() + '/' for p in
                   ['.config/gcloud', '.config/gh', '.config/doctl', '.config/oci'])
            or ('.ssh' in parts[:-1] and base.startswith('id_'))
            or ('.docker' in parts[:-1] and base == 'config.json'))


def credential_env(name):
    name = name.upper()
    return (any(word in name for word in ['API_KEY', 'ACCESS_TOKEN', 'AUTH_TOKEN',
                                         'SECRET', 'PASSWORD', 'CREDENTIAL'])
            or name.endswith('_TOKEN')
            or name in {'AWS_ACCESS_KEY_ID', 'AWS_SESSION_TOKEN', 'GITHUB_TOKEN',
                        'GH_TOKEN', 'GITLAB_TOKEN', 'HF_TOKEN', 'NPM_TOKEN',
                        'AZURE_OPENAI_KEY', 'GOOGLE_API_KEY', 'GEMINI_API_KEY'})


def credential_link_path(name):
    # Conservatively refuse aliases of credential directories rather than
    # interpreting a later layer through a mutable symlink graph.
    return credential_path(name) or any(part in {
        '.codex', '.claude', '.gemini', '.ssh', '.docker', '.aws', '.azure', '.config'
    } for part in name.lower().strip('/').split('/'))


def scan_content(stream, capture=False):
    tail = b''
    collected = bytearray() if capture else None
    while data := stream.read(1024 * 1024):
        if secret_shaped(tail + data):
            raise ValueError('credential content detected')
        tail = (tail + data)[-4096:]
        if capture:
            if len(collected) + len(data) > MAX_JSON:
                raise ValueError('bundle manifest size limit')
            collected.extend(data)
    return bytes(collected) if capture else None


def validate_bundle(value):
    if not isinstance(value, dict) or value.get('schema_version') != 1 or value.get('platform') != 'linux/amd64':
        raise ValueError('unsupported bundle manifest')
    harnesses = value.get('harnesses')
    if not isinstance(harnesses, list) or not 1 <= len(harnesses) <= 32:
        raise ValueError('invalid bundle harnesses')
    names = set()
    for item in [value.get(k) for k in ['runner', 'session_host', 'aq_git', 'node']] + harnesses:
        if not isinstance(item, dict):
            raise ValueError('invalid bundle artifact')
        for key in ['name', 'version']:
            if not isinstance(item.get(key), str) or not re.fullmatch(r'[A-Za-z0-9_.+@-]{1,128}', item[key]):
                raise ValueError('invalid bundle artifact metadata')
        path = item.get('path', '')
        if not path.startswith('/opt/aq/') or safe_path(path[1:]) != path[1:]:
            raise ValueError('invalid bundle artifact path')
        if not re.fullmatch(r'[0-9a-f]{64}', item.get('sha256', '')) or type(item.get('bytes')) is not int or item['bytes'] <= 0:
            raise ValueError('invalid bundle artifact digest')
    for item in harnesses:
        if item['name'] in names:
            raise ValueError('duplicate harness')
        names.add(item['name'])
    return value


def _scan_oci(path):
    path = Path(path)
    if path.stat().st_size > MAX_ARCHIVE:
        raise ValueError('OCI archive size limit')
    with tempfile.TemporaryDirectory(prefix='aq-oci-scan-') as directory:
        blobs = {}
        metadata = {}
        total = 0
        entries = 0
        seen = set()
        with tarfile.open(path, mode='r|*') as archive:
            for item in archive:
                entries += 1
                if entries > MAX_ENTRIES:
                    raise ValueError('OCI entry limit')
                name = safe_path(item.name)
                if name in seen:
                    raise ValueError('duplicate OCI member')
                seen.add(name)
                if item.isdir():
                    continue
                if not item.isfile():
                    raise ValueError('invalid OCI member')
                total += item.size
                if total > MAX_ARCHIVE:
                    raise ValueError('OCI expanded size limit')
                stream = archive.extractfile(item)
                if name in {'oci-layout', 'index.json'}:
                    metadata[name] = json_bytes(stream.read(MAX_JSON + 1))
                elif re.fullmatch(r'blobs/sha256/[0-9a-f]{64}', name):
                    dest = Path(directory) / name.rsplit('/', 1)[1]
                    digest = hashlib.sha256()
                    with dest.open('xb') as output:
                        while data := stream.read(1024 * 1024):
                            digest.update(data)
                            output.write(data)
                    if dest.stat().st_size != item.size or digest.hexdigest() != dest.name:
                        raise ValueError('OCI blob digest mismatch')
                    blobs['sha256:' + dest.name] = dest
                else:
                    raise ValueError('unsupported OCI archive member')
        if metadata.get('oci-layout') != {'imageLayoutVersion': '1.0.0'}:
            raise ValueError('invalid OCI layout')
        used = set()
        def descriptor(desc):
            if not isinstance(desc, dict) or not DIGEST.fullmatch(desc.get('digest', '')):
                raise ValueError('invalid OCI descriptor')
            digest = desc['digest']
            if digest not in blobs or type(desc.get('size')) is not int or desc['size'] != blobs[digest].stat().st_size:
                raise ValueError('OCI descriptor size or blob missing')
            used.add(digest)
            return blobs[digest]
        def json_descriptor(desc):
            with descriptor(desc).open('rb') as source:
                return json_bytes(source.read(MAX_JSON + 1))
        index = metadata.get('index.json', {})
        # Single-platform export only; build disables attestations and extra manifests.
        if index.get('schemaVersion') != 2 or len(index.get('manifests', [])) != 1:
            raise ValueError('only one OCI image manifest is supported')
        desc = index['manifests'][0]
        if desc.get('mediaType') != 'application/vnd.oci.image.manifest.v1+json':
            raise ValueError('unsupported OCI manifest')
        manifest = json_descriptor(desc)
        if manifest.get('schemaVersion') != 2 or manifest.get('mediaType') != desc['mediaType']:
            raise ValueError('invalid OCI image manifest')
        cfg_desc = manifest.get('config', {})
        if cfg_desc.get('mediaType') != 'application/vnd.oci.image.config.v1+json':
            raise ValueError('unsupported OCI config')
        cfg = json_descriptor(cfg_desc)
        if cfg.get('os') != 'linux' or cfg.get('architecture') != 'amd64':
            raise ValueError('unsupported image platform')
        if secret_shaped(json.dumps(cfg)):
            raise ValueError('credential in image config or history')
        image_config = cfg.get('config', {})
        for env in image_config.get('Env', []) or []:
            if not isinstance(env, str) or '=' not in env or credential_env(env.split('=', 1)[0]):
                raise ValueError('credential environment variable')
        layers = manifest.get('layers', [])
        diff_ids = cfg.get('rootfs', {}).get('diff_ids', [])
        if not 1 <= len(layers) <= 256 or len(layers) != len(diff_ids):
            raise ValueError('invalid OCI layers')
        installed_manifest = None
        expanded = 0
        for layer, diff_id in zip(layers, diff_ids):
            blob = descriptor(layer)
            media = layer.get('mediaType')
            if media not in {'application/vnd.oci.image.layer.v1.tar', 'application/vnd.oci.image.layer.v1.tar+gzip'}:
                raise ValueError('unsupported OCI layer compression')
            # Bound decompression and verify the uncompressed DiffID before tar parsing.
            unpacked = Path(directory) / 'layer.tar'
            digest = hashlib.sha256()
            opener = gzip.open if media.endswith('+gzip') else open
            with opener(blob, 'rb') as source, unpacked.open('wb') as output:
                while data := source.read(1024 * 1024):
                    expanded += len(data)
                    if expanded > MAX_EXPANDED:
                        raise ValueError('OCI expanded layer size limit')
                    digest.update(data)
                    output.write(data)
            if 'sha256:' + digest.hexdigest() != diff_id:
                raise ValueError('OCI layer DiffID mismatch')
            layer_names = set()
            replacement = None
            deleted = False
            with tarfile.open(unpacked, mode='r|') as archive:
                for item in archive:
                    entries += 1
                    if entries > MAX_ENTRIES:
                        raise ValueError('OCI entry limit')
                    name = safe_path(item.name)
                    if name in layer_names:
                        raise ValueError('duplicate layer member')
                    layer_names.add(name)
                    # Whiteouts affect the final view, never erase scan findings.
                    if name in {'opt/aq/.wh.manifest.json', 'opt/.wh.aq', '.wh.opt',
                                '.wh..wh..opq', 'opt/.wh..wh..opq', 'opt/aq/.wh..wh..opq'}:
                        deleted = True
                    if credential_path(name) and not item.isdir():
                        raise ValueError('credential path detected')
                    if item.issym() or item.islnk():
                        target = posixpath.normpath(posixpath.join(posixpath.dirname(name), item.linkname)) if item.issym() else item.linkname
                        if credential_link_path(name) or credential_link_path(target):
                            raise ValueError('credential link detected')
                    if name in {'opt', 'opt/aq', 'opt/aq/manifest.json'} and not item.isdir():
                        if name != 'opt/aq/manifest.json' or not item.isfile():
                            deleted = True
                    if item.isfile():
                        data = scan_content(archive.extractfile(item), name == 'opt/aq/manifest.json')
                        if data is not None:
                            replacement = validate_bundle(json_bytes(data))
            if deleted:
                installed_manifest = None
            if replacement is not None:
                installed_manifest = replacement
        if used != set(blobs):
            raise ValueError('unreferenced OCI blobs refuse admission')
        if installed_manifest is None:
            raise ValueError('bundle manifest missing from final image')
        return {'image_digest': desc['digest'], 'bundle_manifest': installed_manifest,
                'layers_scanned': len(layers)}


def scan_oci(path):
    """Verify a single-platform OCI archive and scan *every* layer, including deleted files."""
    try:
        return _scan_oci(path)
    except (OSError, EOFError, tarfile.TarError, KeyError, TypeError, AttributeError, RecursionError) as error:
        raise ValueError('invalid OCI image archive') from error
