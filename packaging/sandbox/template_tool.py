#!/usr/bin/env python3
"""Non-secret template recipe validation and fixed build assembly."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from urllib.parse import urlsplit

_spec = importlib.util.spec_from_file_location('credential_scan', Path(__file__).with_name('credential_scan.py'))
scan = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scan)

FINALIZE = ('getent passwd runner >/dev/null && getent passwd agent >/dev/null && '
            'command -v tmux >/dev/null && command -v bwrap >/dev/null && '
            'command -v git >/dev/null && test -s /opt/aq/manifest.json')
# Sandboxes are single-use, so Cargo's incremental cache is pure disk cost.
# Only these three settings are overridden; other repository profile values stand.
CARGO_ENV = ('ENV CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only '
             'CARGO_PROFILE_TEST_DEBUG=line-tables-only')


def text(value, limit=128, pattern=None):
    if (not isinstance(value, str) or not 1 <= len(value.encode()) <= limit
            or any(ord(c) < 32 or ord(c) == 127 for c in value)
            or scan.secret_shaped(value) or (pattern and not re.fullmatch(pattern, value))):
        raise ValueError('invalid or secret-shaped field')
    return value


def pinned(value):
    text(value, 512, r'[a-z0-9][a-z0-9./:_-]*@sha256:[0-9a-f]{64}')
    return value


def parent_image(recipe, config, entries=()):
    if 'parent' not in recipe:
        # A parsed project recipe has no parent: AQ supplies the base template.
        if 'base_template' not in config:
            raise ValueError('a project recipe needs a configured base_template')
        return config['base_template']
    parent = recipe['parent']
    if parent == config['os_image']:
        return parent
    if config.get('universal_image') and parent in {'codex-universal', config['universal_image']}:
        return config['universal_image']
    for entry in entries:
        if parent == entry['name'] + '@' + entry['version']:
            return entry['image_reference']
    raise ValueError('parent must be a pinned configured base or allowlisted toolchain')


def validate_recipe(recipe, config, entries=()):
    if not isinstance(recipe, dict) or set(recipe) - {'schema_version', 'name', 'version', 'parent', 'install_steps', 'prefetch'}:
        raise ValueError('unknown recipe fields')
    if recipe.get('schema_version') != 1:
        raise ValueError('unsupported recipe schema')
    text(recipe.get('name'), 64, r'[a-z][a-z0-9-]*')
    text(recipe.get('version'), 64, r'[A-Za-z0-9_.+-]+')
    text(recipe.get('parent'), 512)
    parent_image(recipe, config, entries)
    steps = recipe.get('install_steps')
    if not isinstance(steps, list) or len(steps) > 64:
        raise ValueError('invalid install steps')
    for step in steps:
        if not isinstance(step, list) or not 1 <= len(step) <= 32:
            raise ValueError('install step must be an argv array')
        for arg in step:
            text(arg, 4096)
        if step[0].startswith('-'):
            raise ValueError('invalid install executable')
    prefetch = recipe.get('prefetch', [])
    if not isinstance(prefetch, list) or len(prefetch) > 16:
        raise ValueError('invalid repository prefetch')
    if prefetch and config.get('registry_visibility') != 'private':
        raise ValueError('repository objects require a private target registry')
    for item in prefetch:
        if not isinstance(item, dict) or set(item) != {'repository', 'commit'}:
            raise ValueError('invalid prefetch fields')
        repo = text(item['repository'], 512)
        url = urlsplit(repo)
        if url.scheme != 'https' or not url.hostname or url.username or url.password or url.query or url.fragment:
            raise ValueError('prefetch requires a non-secret HTTPS repository URL')
        text(item['commit'], 40, r'[0-9a-f]{40}')
    return recipe


def dockerfile(recipe, base, config, stage=None):
    lines = ['FROM ' + base + (' AS ' + stage if stage else ''), 'ENV NODE_DISABLE_COMPILE_CACHE=1', 'USER root']
    if base == config.get('universal_image'):
        lines += ['RUN apt-get update && apt-get install -y --no-install-recommends '
                  'ca-certificates tmux bubblewrap bash perl libcurl4 libexpat1 libssl3 zlib1g '
                  'libstdc++6 libgcc-s1 libatomic1 ncurses-term passwd && rm -rf /var/lib/apt/lists/*',
                  'RUN (getent group runner || groupadd --gid 10001 runner) && '
                  '(getent passwd runner || useradd --uid 10001 --gid runner --create-home --shell /bin/bash runner) && '
                  '(getent group agent || groupadd --gid 10002 agent) && '
                  '(getent passwd agent || useradd --uid 10002 --gid agent --create-home --shell /bin/bash agent)',
                  'COPY --from=' + config['os_image'] + ' /opt/git /opt/git',
                  'ENV PATH="/opt/aq/bin:/opt/git/bin:/usr/local/bin:/usr/bin:/bin" LANG=C.UTF-8 TERM=xterm-256color']
    lines += ['RUN ' + json.dumps(step) for step in recipe['install_steps']]
    for number, item in enumerate(recipe.get('prefetch', [])):
        dest = f'/opt/aq-repositories/{number}.git'
        lines += ['RUN ' + json.dumps(['git', 'init', '--bare', dest]),
                  'RUN ' + json.dumps(['git', '-C', dest, 'fetch', '--no-tags', '--depth=1',
                                       item['repository'], item['commit']])]
    lines += [CARGO_ENV,
              'WORKDIR /workspace',
              'COPY --from=' + config['runtime_image'] + ' /opt/aq /opt/aq',
              'RUN ' + json.dumps(['/bin/sh', '-ec', FINALIZE])]
    return '\n'.join(lines) + '\n'


def validate_config(config):
    required = {'schema_version', 'role', 'docker_context', 'buildkit_builder', 'builder_identity',
                'os_image', 'runtime_image', 'target_repository', 'registry_visibility', 'allowlist'}
    if not isinstance(config, dict) or not required <= set(config) or set(config) - required - {'universal_image', 'docker_binary', 'base_template', 'limits', 'skopeo_image'}:
        raise ValueError('invalid build configuration fields')
    if config['schema_version'] != 1 or config['role'] != 'template-build':
        raise ValueError('an explicit template-build node is required')
    for key in ['docker_context', 'buildkit_builder', 'builder_identity']:
        text(config[key], 128, r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    for key in ['os_image', 'runtime_image', 'universal_image', 'base_template', 'skopeo_image']:
        if key in config:
            pinned(config[key])
    if config['os_image'].split('@')[0].rsplit('/', 1)[-1] != 'aq-sandbox-os':
        raise ValueError('configured OS must be aq-sandbox-os')
    if config['runtime_image'].split('@')[0].rsplit('/', 1)[-1] != 'aq-sandbox-runtime':
        raise ValueError('configured runtime must be aq-sandbox-runtime')
    if 'universal_image' in config and config['universal_image'].split('@')[0].rsplit('/', 1)[-1] != 'codex-universal':
        raise ValueError('configured universal base must be codex-universal')
    text(config['target_repository'], 256, r'[a-z0-9][a-z0-9.:-]*/[a-z0-9][a-z0-9/_.-]*')
    if config['registry_visibility'] not in {'private', 'public'}:
        raise ValueError('registry visibility must be explicit')
    path = text(config['allowlist'], 512)
    if Path(path).is_absolute() or scan.safe_path(path) != path:
        raise ValueError('allowlist must be a relative vault path')
    if 'limits' in config:
        limits = config['limits']
        if not isinstance(limits, dict):
            raise ValueError('limits must be an object')
        for key in ['log_bytes', 'build_wall_ms']:
            if not isinstance(limits.get(key), int) or isinstance(limits[key], bool) or limits[key] < 1:
                raise ValueError('invalid limit: ' + key)
    if 'docker_binary' in config:
        if not Path(text(config['docker_binary'], 512)).is_absolute():
            raise ValueError('docker_binary must be absolute')
    return config


def read_json(path):
    with Path(path).open('rb') as stream:
        return scan.json_bytes(stream.read(scan.MAX_JSON + 1))


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode()


def validate_entry(entry):
    fields = {'name', 'version', 'image_digest', 'image_reference', 'provenance',
              'harness_versions', 'materializations'}
    if not isinstance(entry, dict) or set(entry) != fields:
        raise ValueError('invalid allowlist entry fields')
    text(entry['name'], 64, r'[a-z][a-z0-9-]*')
    text(entry['version'], 64, r'[A-Za-z0-9_.+-]+')
    pinned(entry['image_reference'])
    if not scan.DIGEST.fullmatch(entry['image_digest']) or not entry['image_reference'].endswith('@' + entry['image_digest']):
        raise ValueError('allowlist digest mismatch')
    provenance = entry['provenance']
    expected = {'image_digest', 'recipe_digest', 'base_digest', 'runtime_bundle_digest', 'harness_versions',
                'built_at', 'builder_identity', 'finalize_policy', 'credential_scan_policy'}
    if not isinstance(provenance, dict) or set(provenance) != expected:
        raise ValueError('invalid allowlist provenance')
    if provenance['image_digest'] != entry['image_digest']:
        raise ValueError('provenance image digest mismatch')
    for key in ['recipe_digest', 'base_digest', 'runtime_bundle_digest']:
        if not scan.DIGEST.fullmatch(provenance[key]):
            raise ValueError('invalid provenance digest')
    text(provenance['builder_identity'], 128, r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    text(provenance['built_at'], 40, r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?\+00:00')
    if provenance['finalize_policy'] != 'aq-finalize-v1' or provenance['credential_scan_policy'] != 'aq-credential-scan-v1':
        raise ValueError('allowlist entry has no supported admission evidence')
    versions = entry['harness_versions']
    if not isinstance(versions, dict) or not 1 <= len(versions) <= 32 or versions != provenance['harness_versions']:
        raise ValueError('inconsistent advertised harness versions')
    for name, version in versions.items():
        text(name, 128, r'[A-Za-z0-9_.+-]+')
        text(version, 128, r'[A-Za-z0-9_.+-]+')
    if not isinstance(entry['materializations'], dict) or len(canonical(entry['materializations'])) > 65536:
        raise ValueError('invalid materialization metadata')
    if 'boat' in entry['materializations']:
        boat = entry['materializations']['boat']
        fields = {'image_digest','saved_snapshot','snapshot_id','scan_policy','snapshot_archive_digest',
                  'build_org','builder_identity','built_at'}
        if not isinstance(boat, dict) or set(boat) != fields or boat['image_digest'] != entry['image_digest']:
            raise ValueError('boat materialization digest mismatch')
        for key in ['saved_snapshot','snapshot_id','build_org','builder_identity']:
            text(boat[key],128,r'[A-Za-z0-9][A-Za-z0-9_.-]*')
        text(boat['built_at'],40,r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?\+00:00')
        if boat['scan_policy'] != 'aq-credential-scan-v1' or not scan.DIGEST.fullmatch(boat['snapshot_archive_digest']):
            raise ValueError('boat snapshot scan evidence missing')
    if scan.secret_shaped(canonical(entry)):
        raise ValueError('credential in allowlist metadata')
    return entry


def read_allowlist(path):
    if not path.exists():
        return {'schema_version': 1, 'entries': []}
    value = read_json(path)
    if not isinstance(value, dict) or set(value) != {'schema_version', 'entries'} or value['schema_version'] != 1:
        raise ValueError('unsupported allowlist schema')
    entries = value['entries']
    if not isinstance(entries, list) or len(entries) > 1024:
        raise ValueError('allowlist entry limit')
    names = set()
    for entry in entries:
        validate_entry(entry)
        key = (entry['name'], entry['version'])
        if key in names:
            raise ValueError('duplicate allowlist name/version')
        names.add(key)
    return value


def write_json(path, value):
    with path.open('xb') as output:
        output.write(canonical(value) + b'\n')


def build_env():
    # Provider keys, SSH agent, attempt variables, build args/secrets and arbitrary
    # host files are never forwarded. Docker's own context/registry auth stays local.
    return {key: os.environ[key] for key in ['PATH', 'HOME', 'XDG_CONFIG_HOME', 'DOCKER_CONFIG'] if key in os.environ}


class BuildFailed(Exception):
    """An environment job stopped at `step` (parse, install, check, scan, publish or run).

    `log` is bounded and redacted, or a fixed message that never echoes recipe content.
    """

    def __init__(self, step, log):
        super().__init__('environment build failed at ' + step)
        self.step = step
        self.log = log


REDACTED = b'[redacted]'


def redact(data):
    """Replace token-shaped strings and credential assignment values using the scanner's patterns."""
    def value_only(match):
        if scan.CREDENTIAL_LABEL.search(match[1].decode('ascii').upper()):
            return match[0][:match.start(2) - match.start(0)] + REDACTED
        return match[0]
    data = scan.TOKEN_PATTERN.sub(REDACTED, data)
    return scan.JSON_ASSIGNMENT.sub(value_only, scan.ASSIGNMENT.sub(value_only, data))


def bounded_log(data, limit):
    """Redact bytes `data`, then keep the last `limit` bytes of the text."""
    if len(data) > 2 * limit:
        # Redact a window, not a whole huge log; drop the partial first line so a cut token never survives.
        data = data[-2 * limit:]
        data = data[data.find(b'\n') + 1:]
    redacted = redact(data).decode('utf-8', 'replace')
    encoded = redacted.encode()
    if len(encoded) <= limit:
        return redacted
    return encoded[-limit:].decode('utf-8', 'ignore')


def tail(path, limit):
    with Path(path).open('rb') as stream:
        size = stream.seek(0, os.SEEK_END)
        stream.seek(max(0, size - 2 * limit - 1))
        return stream.read()


def captured(command, directory, number):
    """A RUN that records the output and exit status of a shell command instead of failing the build."""
    script = f'({command}\n) > {directory}/{number}.out 2>&1; echo $? > {directory}/{number}.exit'
    return 'RUN ' + json.dumps(['sh', '-c', script])


def environment_dockerfile(env, base, config, run=None):
    """The template stage, then a checks stage whose results a scratch stage exposes for export.

    With `run` (a try), a run stage copies the build context (the repository) into /workspace,
    runs the command there the way checks run, and stage try-output exposes checks and run results.
    """
    lines = [dockerfile(env, base, config, stage='template').rstrip('\n'),
             'FROM template AS checks', 'RUN mkdir /aq-checks']
    lines += [captured(command, '/aq-checks', number) for number, command in enumerate(env['checks'])]
    lines += ['FROM scratch AS check-output', 'COPY --from=checks /aq-checks /']
    if run is not None:
        lines += ['FROM template AS run', 'COPY . /workspace', 'RUN mkdir /aq-run', captured(run, '/aq-run', 0),
                  'FROM scratch AS try-output', 'COPY --from=checks /aq-checks /checks',
                  'COPY --from=run /aq-run /run']
    return '\n'.join(lines) + '\n'


def run_buildx(docker, cfg, context, target, output, log_path, timeout, dockerfile_path=None):
    command = [docker, '--context', cfg['docker_context'], 'buildx', 'build',
               '--builder', cfg['buildkit_builder'], '--platform', 'linux/amd64',
               '--provenance=false', '--sbom=false', '--progress=plain', '--target', target,
               '--output', output]
    if dockerfile_path is not None:
        command += ['--file', str(dockerfile_path)]
    command.append(str(context))
    with Path(log_path).open('wb') as log:
        try:
            subprocess.run(command, env=build_env(), check=True, stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=log, timeout=timeout)
        except subprocess.TimeoutExpired:
            log.write(b'\nbuild timed out\n')
            raise
        except OSError:
            log.write(b'could not run docker buildx\n')
            raise


def build_environment(env, config, output):
    """Build a parsed project recipe on the configured builder and return a Build.

    Raises BuildFailed(step, log): `install` when BuildKit fails to build the image, `check` when a
    Check command fails, `scan` when the credential scan refuses the image. `output` must not exist;
    the scanned archive is left at `output/image.oci.tar`. `config['limits']` supplies `log_bytes`
    and `build_wall_ms`.
    """
    cfg = validate_config(config)
    if 'limits' not in cfg:
        raise ValueError('environment builds need limits in the build configuration')
    limit = cfg['limits']['log_bytes']
    timeout = cfg['limits']['build_wall_ms'] / 1000
    base = parent_image(env, cfg)
    output = Path(output).absolute()
    if ',' in str(output) or '\n' in str(output):
        raise ValueError('unsupported output path')
    docker = cfg.get('docker_binary') or shutil.which('docker')
    if not docker:
        raise ValueError('Docker BuildKit is required on the configured build node')
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    archive = output / 'image.oci.tar'
    with tempfile.TemporaryDirectory(prefix='aq-environment-context-') as directory:
        work = Path(directory)
        context = work / 'context'
        context.mkdir()
        (context / 'Dockerfile').write_text(environment_dockerfile(env, base, cfg))
        log_path = work / 'build.log'
        checked = work / 'checks'

        def buildx(step, target, spec):
            try:
                run_buildx(docker, cfg, context, target, spec, log_path, timeout)
            except (OSError, subprocess.SubprocessError):
                raise BuildFailed(step, bounded_log(tail(log_path, limit), limit)) from None

        buildx('install', 'template', f'type=oci,dest={archive},compression=gzip,force-compression=true')
        results = []
        if env['checks']:
            buildx('check', 'check-output', f'type=local,dest={checked}')
            results = check_results(env, checked, limit)
    try:
        scanned = scan.scan_oci(archive)
    except ValueError:
        raise BuildFailed('scan', 'the credential scan refused the image') from None
    digest = scanned['image_digest']
    return {'name': env['name'], 'default': env['default'], 'digest': digest, 'image': cfg['target_repository'] + '@' + digest,
            'recipe_sha256': env['sha256'], 'provides': env['provides'],
            'toolchain': [{'command': command, 'output': out} for command, (code, out) in results],
            'warm': env['warm'],
            'harnesses': [{'harness': item['name'], 'version': item['version']}
                          for item in scanned['bundle_manifest']['harnesses']]}


def check_results(env, directory, limit):
    """(command, (exit, output)) per check; raises BuildFailed('check') naming the failing checks."""
    results = [(command, read_check(directory, number, limit)) for number, command in enumerate(env['checks'])]
    failed = [(command, code, out) for command, (code, out) in results if code != 0]
    if failed:
        report = ''.join(f'$ {command}\n(exit {code})\n{out}\n' for command, code, out in failed)
        raise BuildFailed('check', bounded_log(report.encode(), limit))
    return results


def try_environment(env, config, repo, run):
    """Build a parsed recipe with the repository as context, run its checks and `run`, export nothing.

    Only a local directory of check and run results leaves the builder: no OCI archive, no scan, no
    publication. Returns `{"exit", "output"}` for `run`, or None without one. Raises BuildFailed
    `install` (BuildKit failed), `check` (a check failed) or `run` (the command left no result).
    """
    cfg = validate_config(config)
    if 'limits' not in cfg:
        raise ValueError('environment builds need limits in the build configuration')
    limit = cfg['limits']['log_bytes']
    timeout = cfg['limits']['build_wall_ms'] / 1000
    base = parent_image(env, cfg)
    repo = Path(repo).resolve(strict=True)
    if not repo.is_dir():
        raise ValueError('the repository must be a directory')
    docker = cfg.get('docker_binary') or shutil.which('docker')
    if not docker:
        raise ValueError('Docker BuildKit is required on the configured build node')
    with tempfile.TemporaryDirectory(prefix='aq-environment-try-') as directory:
        work = Path(directory)
        definition = work / 'Dockerfile'
        definition.write_text(environment_dockerfile(env, base, cfg, run=run))
        # BuildKit reads <Dockerfile>.dockerignore beside --file, so the repository is not modified.
        (work / 'Dockerfile.dockerignore').write_text('.git\n**/.git\n')
        log_path = work / 'build.log'
        exported = work / 'exported'
        if ',' in str(exported) or '\n' in str(exported):
            raise ValueError('unsupported temporary path')
        target = 'check-output' if run is None else 'try-output'
        try:
            run_buildx(docker, cfg, repo, target, f'type=local,dest={exported}', log_path, timeout,
                       dockerfile_path=definition)
        except (OSError, subprocess.SubprocessError):
            raise BuildFailed('install', bounded_log(tail(log_path, limit), limit)) from None
        if run is None:
            check_results(env, exported, limit)
            return None
        check_results(env, exported / 'checks', limit)
        if not (exported / 'run' / '0.exit').is_file():
            raise BuildFailed('run', 'the command produced no result')
        code, output = read_check(exported / 'run', 0, limit)
        return {'exit': code, 'output': output}


def publish(config, directory, digest):
    """Copy the scanned archive in `directory` to target_repository@digest with the pinned Skopeo image.

    `--preserve-digests` keeps the registry digest equal to the scanned digest. Raises
    BuildFailed('publish') with the bounded, redacted Skopeo output.
    """
    limit = config['limits']['log_bytes']
    docker = config.get('docker_binary') or shutil.which('docker')
    if not docker:
        raise ValueError('Docker is required on the configured build node')
    command = [docker, '--context', config['docker_context'], 'run', '--rm',
               '--mount', f'type=bind,src={directory},dst=/aq-publish,readonly',
               config['skopeo_image'], 'copy', '--preserve-digests',
               'oci-archive:/aq-publish/image.oci.tar',
               'docker://' + config['target_repository'] + '@' + digest]
    with tempfile.TemporaryDirectory(prefix='aq-environment-publish-') as work:
        log_path = Path(work) / 'publish.log'
        with log_path.open('wb') as log:
            try:
                subprocess.run(command, env=build_env(), check=True, stdin=subprocess.DEVNULL,
                               stdout=log, stderr=log, timeout=config['limits']['build_wall_ms'] / 1000)
            except (OSError, subprocess.SubprocessError):
                log.flush()
                raise BuildFailed('publish', bounded_log(tail(log_path, limit), limit)) from None


UUID = r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}'
JOB_FIELDS = {'job_id', 'kind', 'project_id', 'project', 'commit', 'recipes', 'run', 'base_template',
              'target_repository', 'limits'}
LIMIT_FIELDS = {'max_install_steps', 'max_step_args', 'max_arg_bytes', 'max_prefetch', 'max_recipe_bytes',
                'build_wall_ms', 'build_cpu_millis', 'build_memory_bytes', 'log_bytes', 'try_concurrency',
                'keep_versions'}


def validate_job(job, kind):
    """A server Job (plan Shared shapes) of `kind`; a try names one recipe, a build has no run."""
    if not isinstance(job, dict) or set(job) != JOB_FIELDS:
        raise ValueError('invalid job fields')
    if job['kind'] != kind:
        raise ValueError('job kind does not match the operation')
    text(job['job_id'], 36, UUID)
    text(job['project_id'], 36, UUID)
    text(job['project'], 128, r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    text(job['commit'], 40, r'[0-9a-f]{40}')
    pinned(job['base_template'])
    limits = job['limits']
    if not isinstance(limits, dict) or set(limits) != LIMIT_FIELDS:
        raise ValueError('invalid job limits')
    for key, value in limits.items():
        if not isinstance(value, int) or isinstance(value, bool) or value < 1:
            raise ValueError('invalid limit: ' + key)
    recipes = job['recipes']
    if not isinstance(recipes, list) or not recipes or len(set(map(str, recipes))) != len(recipes):
        raise ValueError('a job names distinct recipes')
    for name in recipes:
        text(name, 64, r'[a-z][a-z0-9-]*')
    if kind == 'try':
        if len(recipes) != 1:
            raise ValueError('a try names one recipe')
        if job['run'] is not None:
            text(job['run'], limits['max_arg_bytes'])
    elif job['run'] is not None:
        raise ValueError('a build job has no run command')
    return job


def job_config(config, job):
    """The builder config with the job's base template, target repository and limits."""
    return validate_config({**config, 'base_template': job['base_template'],
                            'target_repository': job['target_repository'], 'limits': job['limits']})


def load_recipe(repo, name, limits):
    """Parse .aq/environments/<name>.md of the checked-out repository; failures are BuildFailed('parse')."""
    environment_recipe = load_sibling('environment_recipe')
    root = Path(repo).resolve(strict=True)
    try:
        path = (root / '.aq' / 'environments' / (name + '.md')).resolve(strict=True)
        if not path.is_relative_to(root) or not path.is_file():
            raise BuildFailed('parse', 'the recipe file is outside the repository or not a file')
        env = environment_recipe.parse_environment(path, limits)
    except OSError:
        raise BuildFailed('parse', 'the recipe file is missing or unreadable') from None
    except ValueError as error:
        # Parser messages are fixed text and never echo recipe content.
        raise BuildFailed('parse', str(error)) from None
    if env['name'] != name:
        raise BuildFailed('parse', 'the recipe name does not match its file name')
    return env


def load_sibling(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def job_result(job, builds=(), failure=None, run=None):
    return {'job_id': job['job_id'], 'outcome': 'failed' if failure else 'built',
            'builds': [] if failure else list(builds), 'failure': failure, 'run': None if failure else run}


def environment_build(job_path, repo, config_path, output):
    """Run a build Job: parse, build, check and scan every recipe, then publish each scanned archive.

    Nothing is published until every recipe of the job has passed its scan. Returns a Job result;
    never writes the allowlist (the server admits environments).
    """
    job = validate_job(read_json(job_path), 'build')
    cfg = job_config(read_json(config_path), job)
    if 'skopeo_image' not in cfg:
        raise ValueError('environment builds need a pinned skopeo_image')
    output = Path(output).absolute()
    if ',' in str(output) or '\n' in str(output):
        raise ValueError('unsupported output path')
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    name = None
    try:
        builds = []
        for name in job['recipes']:
            env = load_recipe(repo, name, job['limits'])
            builds.append(build_environment(env, cfg, output / name))
        for name, build in zip(job['recipes'], builds):
            publish(cfg, output / name, build['digest'])
    except BuildFailed as failed:
        return job_result(job, failure={'recipe': name, 'step': failed.step, 'log': failed.log})
    return job_result(job, builds=builds)


def environment_try(job_path, repo, config_path):
    """Run a try Job: build its recipe with the repository, run checks and the command; publish nothing."""
    job = validate_job(read_json(job_path), 'try')
    cfg = job_config(read_json(config_path), job)
    name = job['recipes'][0]
    try:
        env = load_recipe(repo, name, job['limits'])
        run = try_environment(env, cfg, repo, job['run'])
    except BuildFailed as failed:
        return job_result(job, failure={'recipe': name, 'step': failed.step, 'log': failed.log})
    return job_result(job, run=run)


def read_check(directory, number, limit):
    """Exit status and redacted, bounded output of check `number`; a missing result is a failure."""
    try:
        code = int((Path(directory) / f'{number}.exit').read_text().strip())
        with (Path(directory) / f'{number}.out').open('rb') as stream:
            data = stream.read(limit + 1)
    except (OSError, ValueError):
        return 1, 'check produced no result'
    return code, bounded_log(data, limit)


def build(recipe_path, config_path, output_path):
    import datetime
    import fcntl
    import hashlib
    import os
    import shutil
    import subprocess
    import tempfile

    cfg = validate_config(read_json(config_path))
    vault = Path(config_path).resolve().parent
    allowlist = vault / cfg['allowlist']
    # Config and vault are trusted operator inputs, never task/attempt inputs.
    entries = read_allowlist(allowlist)['entries']
    recipe = validate_recipe(read_json(recipe_path), cfg, entries)
    if any(e['name'] == recipe['name'] and e['version'] == recipe['version'] for e in entries):
        raise ValueError('recipe name/version already allowlisted; bump version')
    base = parent_image(recipe, cfg, entries)
    output = Path(output_path).absolute()
    if ',' in str(output) or '\n' in str(output):
        raise ValueError('unsupported output path')
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    docker = cfg.get('docker_binary') or shutil.which('docker')
    if not docker:
        raise ValueError('Docker BuildKit is required on the configured build node')
    with tempfile.TemporaryDirectory(prefix='aq-template-context-') as directory:
        context = Path(directory)
        (context / 'Dockerfile').write_text(dockerfile(recipe, base, cfg))
        # Provider keys, SSH agent, attempt variables, build args/secrets and arbitrary
        # host files are never forwarded. Docker's own context/registry auth stays local.
        env = build_env()
        command = [docker, '--context', cfg['docker_context'], 'buildx', 'build',
                   '--builder', cfg['buildkit_builder'], '--platform', 'linux/amd64',
                   '--provenance=false', '--sbom=false', '--output',
                   f'type=oci,dest={output / "image.oci.tar"},compression=gzip,force-compression=true',
                   str(context)]
        try:
            subprocess.run(command, env=env, check=True, stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=3600)
        except (OSError, subprocess.SubprocessError) as error:
            raise ValueError('BuildKit build/finalize failed; template not admitted') from error
    scanned = scan.scan_oci(output / 'image.oci.tar')
    versions = {item['name']: item['version'] for item in scanned['bundle_manifest']['harnesses']}
    provenance = {'image_digest': scanned['image_digest'],
                  'recipe_digest': 'sha256:' + hashlib.sha256(canonical(recipe)).hexdigest(),
                  'base_digest': base.split('@')[1],
                  'runtime_bundle_digest': cfg['runtime_image'].split('@')[1],
                  'harness_versions': versions,
                  'built_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                  'builder_identity': cfg['builder_identity'],
                  'finalize_policy': 'aq-finalize-v1', 'credential_scan_policy': 'aq-credential-scan-v1'}
    entry = {'name': recipe['name'], 'version': recipe['version'],
             'image_digest': scanned['image_digest'],
             'image_reference': cfg['target_repository'] + '@' + scanned['image_digest'],
             'provenance': provenance, 'harness_versions': versions, 'materializations': {}}
    validate_entry(entry)
    # Persist all reviewable artifacts before the allowlist commit point.
    write_json(output / 'recipe.json', recipe)
    write_json(output / 'provenance.json', provenance)
    write_json(output / 'bundle-manifest.json', scanned['bundle_manifest'])
    (output / 'Dockerfile').write_text(dockerfile(recipe, base, cfg))
    allowlist.parent.mkdir(parents=True, exist_ok=True)
    with allowlist.with_suffix(allowlist.suffix + '.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        current = read_allowlist(allowlist)
        # Revalidate parents and unique names under the same lock as publication.
        if parent_image(recipe, cfg, current['entries']) != base:
            raise ValueError('allowlisted parent changed during build')
        if any(e['name'] == recipe['name'] and e['version'] == recipe['version'] for e in current['entries']):
            raise ValueError('recipe version concurrently allowlisted')
        current['entries'].append(entry)
        encoded = canonical(current) + b'\n'
        if len(encoded) > scan.MAX_JSON:
            raise ValueError('allowlist size limit')
        fd, temporary = tempfile.mkstemp(prefix='.allowlist-', dir=allowlist.parent)
        try:
            with os.fdopen(fd, 'wb') as stream:
                stream.write(encoded)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, allowlist)
            dirfd = os.open(allowlist.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(dirfd)
            finally:
                os.close(dirfd)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
    return entry


def main():
    import argparse
    import signal
    import sys
    # A stopped job (SIGTERM) unwinds, so subprocess children are killed and temporary directories removed.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(128 + signal.SIGTERM))
    parser = argparse.ArgumentParser(description='Build and scan pinned sandbox templates')
    sub = parser.add_subparsers(dest='command', required=True)
    builder = sub.add_parser('build')
    builder.add_argument('recipe', type=Path)
    builder.add_argument('--build-config', type=Path, required=True)
    builder.add_argument('--output', type=Path, required=True)
    scanner = sub.add_parser('scan')
    scanner.add_argument('archive', type=Path)
    boat = sub.add_parser('boat-materialize')
    boat.add_argument('template', help='allowlisted name@version')
    boat.add_argument('--build-config', type=Path, required=True)
    boat.add_argument('--oci', type=Path, required=True)
    boat.add_argument('--output', type=Path, required=True)
    environment = sub.add_parser('environment-build')
    environment.add_argument('--job', type=Path, required=True)
    environment.add_argument('--repo', type=Path, required=True)
    environment.add_argument('--build-config', type=Path, required=True)
    environment.add_argument('--output', type=Path, required=True)
    trial = sub.add_parser('environment-try')
    trial.add_argument('--job', type=Path, required=True)
    trial.add_argument('--repo', type=Path, required=True)
    trial.add_argument('--build-config', type=Path, required=True)
    redaction = sub.add_parser('redact-log', help='print the tail of a log file, redacted like build logs')
    redaction.add_argument('--input', type=Path, required=True)
    redaction.add_argument('--limit', type=int, required=True)
    args = parser.parse_args()
    if args.command == 'redact-log':
        sys.stdout.write(bounded_log(tail(args.input, max(args.limit, 0)), max(args.limit, 0)))
        return 0
    try:
        if args.command == 'boat-materialize':
            spec = importlib.util.spec_from_file_location('boat_materialize', Path(__file__).with_name('boat_materialize.py'))
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            result = module.materialize(args.build_config, args.template, args.oci, args.output)
        elif args.command == 'build':
            result = build(args.recipe, args.build_config, args.output)
        elif args.command == 'environment-build':
            result = environment_build(args.job, args.repo, args.build_config, args.output)
        elif args.command == 'environment-try':
            result = environment_try(args.job, args.repo, args.build_config)
        else:
            result = scan.scan_oci(args.archive)
        print(json.dumps(result, sort_keys=True))
    except (ValueError, OSError, KeyError, TypeError, RecursionError):
        # Never echo untrusted recipe fields, paths, build output or secret bytes.
        print('template refused: invalid input, build/finalize failure or credential scan failure', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
