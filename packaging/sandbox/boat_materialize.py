"""Materialize an already allowlisted OCI image in an isolated Boat build org."""
import datetime
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import stat
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import Request, build_opener, HTTPRedirectHandler
import uuid

_spec = importlib.util.spec_from_file_location('template_tool', Path(__file__).with_name('template_tool.py'))
tool = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tool)


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args):
        return None


class Client:
    def __init__(self, cfg):
        self.url = cfg.get('api_url', 'https://boat.dev/api/v1').rstrip('/')
        parsed = urlsplit(self.url)
        if (parsed.scheme != 'https' and not (parsed.scheme == 'http' and parsed.hostname in {'127.0.0.1', '::1'})) or parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError('invalid boat endpoint')
        fd = os.open(cfg['key_file'], os.O_RDONLY | os.O_NOFOLLOW)
        try:
            meta = os.fstat(fd)
            if not stat.S_ISREG(meta.st_mode) or meta.st_mode & 0o077 or meta.st_uid != os.geteuid() or meta.st_size > 4096:
                raise ValueError('build key must be a private node-local file')
            self.key = os.read(fd, 4097).decode().strip()
        finally:
            os.close(fd)
        if not self.key or any(ord(c) < 32 for c in self.key):
            raise ValueError('invalid build key')
        self.org = cfg['build_org']
        self.opener = build_opener(NoRedirect)

    def request(self, method, path, body=None):
        headers = {'Authorization': 'Bearer ' + self.key, 'X-Boat-Org': self.org}
        if method == 'DELETE':
            headers['X-Ascii-Confirm-Delete'] = path.rsplit('/', 1)[1]
        data = None if body is None else tool.canonical(body)
        if data is not None:
            headers['Content-Type'] = 'application/json'
        try:
            return self.opener.open(Request(self.url + path, data=data, method=method, headers=headers), timeout=120)
        except (HTTPError, URLError, TimeoutError) as error:
            raise ValueError('boat build control channel unavailable') from None

    def json(self, method, path, body=None):
        with self.request(method, path, body) as response:
            value = tool.scan.json_bytes(response.read(tool.scan.MAX_JSON + 1))
        if value.get('ok') is not True:
            raise ValueError('boat build request failed')
        return value

    def download(self, path, dest):
        with self.request('GET', path) as response, dest.open('xb') as output:
            size = 0
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > tool.scan.MAX_ARCHIVE:
                    raise ValueError('snapshot size limit')
                output.write(chunk)


def wait(read, ready):
    deadline = time.monotonic() + 600
    while True:
        value = read()
        if ready(value):
            return value
        if time.monotonic() >= deadline:
            raise ValueError('boat build readiness timeout')
        time.sleep(1)


PERSONAL = 'personal'


def separate_scopes(cfg):
    """Org accounts build in a separate org. A personal account (no org) has a
    single billing scope; separation is then the owner's short-lived, on-demand
    build key or boat CLI, never a standing build key on the attempt node."""
    if cfg['build_org'] == PERSONAL or cfg['attempt_org'] == PERSONAL:
        return cfg['build_org'] == cfg['attempt_org'] == PERSONAL
    return cfg['build_org'] != cfg['attempt_org']


def materialize(config_path, identity, image_path, output_path):
    cfg = tool.read_json(config_path)
    required = {'schema_version','role','build_org','attempt_org','key_file','builder_identity','allowlist'}
    if (not isinstance(cfg, dict) or not required <= set(cfg) or set(cfg) - required - {'api_url'}
            or cfg['schema_version'] != 1 or cfg['role'] != 'boat-template-build'):
        raise ValueError('explicit boat build node required')
    for key in ['build_org','attempt_org','builder_identity']:
        tool.text(cfg[key],128,r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    if not separate_scopes(cfg) or not Path(cfg['key_file']).is_absolute():
        raise ValueError('template and attempt scopes must be separate')
    relative = tool.text(cfg['allowlist'],512)
    if Path(relative).is_absolute() or tool.scan.safe_path(relative) != relative:
        raise ValueError('allowlist must be relative to build configuration')
    allowlist = Path(config_path).resolve().parent / relative
    matches = [e for e in tool.read_allowlist(allowlist)['entries'] if e['name']+'@'+e['version'] == identity]
    if len(matches) != 1:
        raise ValueError('template must already be allowlisted')
    entry = matches[0]
    scanned = tool.scan.scan_oci(image_path)
    if scanned['image_digest'] != entry['image_digest']:
        raise ValueError('OCI digest mismatch')
    output = Path(output_path)
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    client = Client(cfg)
    name = 'aq-template-' + entry['image_digest'][7:23] + '-' + uuid.uuid4().hex[:8]
    sandbox = None
    published = False
    try:
        body = {'name':name,'type':'small','ttlSeconds':1800,'noEnv':True,'env':{},'monitoring':False}
        if cfg['build_org'] != PERSONAL:
            body['visibility'] = 'private'  # personal sandboxes are always private
        value = client.json('POST','/sandboxes',body)
        sandbox = tool.text(value['sandbox']['id'],128,r'[A-Za-z0-9_-]+')
        base = '/sandboxes/' + sandbox
        wait(lambda: client.json('GET',base), lambda v: v['sandbox']['state'] in {'ready','running','idle'})
        image = entry['image_reference']
        # Public digest pulls only: no registry credentials or Docker config are
        # forwarded. A stopped keeper makes this a used, snapshot-retained image.
        command = ('set -eu; for i in $(seq 1 60); do sudo docker info >/dev/null 2>&1 && break; sleep 1; done; '
                   'sudo docker pull ' + image + ' >/dev/null; '
                   'sudo docker image inspect --format \'{{range .RepoDigests}}{{println .}}{{end}}\' ' + image)
        result = client.json('POST',base+'/commands',{'command':command,'timeoutSeconds':600})
        if result.get('exitCode') != 0 or image not in result.get('stdout','').splitlines():
            raise ValueError('pulled OCI digest not verified')
        result = client.json('POST',base+'/commands',{'command':
                    'sudo docker create --name aq-template-keeper --network=none --restart=no '+image,
                    'timeoutSeconds':60})
        if result.get('exitCode') != 0:
            raise ValueError('template keeper failed')
        client.json('POST','/saved-snapshots',{'sandboxId':sandbox,'name':name})
        saved = wait(lambda: client.json('GET','/saved-snapshots/'+name),
                     lambda v: v['snapshot']['status'] in {'ready','failed'})['snapshot']
        if saved['status'] != 'ready':
            raise ValueError('template capture failed')
        snapshot = tool.text(saved['snapshotId'],128,r'[A-Za-z0-9_-]+')
        archive = output/'snapshot.tar'
        client.download('/snapshots/'+snapshot+'/files?path=/',archive)
        report = tool.scan.scan_snapshot(archive)
        record = {'image_digest':entry['image_digest'],'saved_snapshot':name,'snapshot_id':snapshot,
                  'scan_policy':report['credential_scan_policy'],'snapshot_archive_digest':report['archive_digest'],
                  'build_org':cfg['build_org'],'builder_identity':cfg['builder_identity'],
                  'built_at':datetime.datetime.now(datetime.timezone.utc).isoformat()}
        tool.write_json(output/'boat-materialization.json',record)
        tool.write_json(output/'scan-report.json',report)
        with allowlist.with_suffix(allowlist.suffix+'.lock').open('a') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX)
            current = tool.read_allowlist(allowlist)
            found = next(e for e in current['entries'] if e['name']+'@'+e['version'] == identity)
            if found != entry:
                raise ValueError('allowlist changed during materialization')
            found['materializations']['boat'] = record
            tool.validate_entry(found)
            fd, temporary = tempfile.mkstemp(prefix='.boat-allowlist-', dir=allowlist.parent)
            try:
                with os.fdopen(fd,'wb') as stream:
                    stream.write(tool.canonical(current)+b'\n'); stream.flush(); os.fsync(stream.fileno())
                os.replace(temporary,allowlist)
                directory = os.open(allowlist.parent,os.O_RDONLY|os.O_DIRECTORY)
                try: os.fsync(directory)
                finally: os.close(directory)
            finally:
                if os.path.exists(temporary): os.unlink(temporary)
            published = True
        return record
    finally:
        # Build VMs hold no attempt work. Always request deletion; the explicit
        # TTL also bounds compute if the reply was lost or cleanup is unavailable.
        if sandbox is not None:
            client.json('DELETE','/sandboxes/'+sandbox)
        if not published:
            client.json('DELETE','/saved-snapshots/'+name)
