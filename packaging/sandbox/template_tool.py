#!/usr/bin/env python3
"""Non-secret template recipe validation and fixed build assembly."""
import datetime
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from typing import NamedTuple


def load_sibling(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


contract = load_sibling("template_contract")
scan = contract.scan
text = contract.text
pinned = contract.pinned
parent_image = contract.parent_image
validate_recipe = contract.validate_recipe
relative_allowlist = contract.relative_allowlist
template_identity = contract.template_identity
validate_config = contract.validate_config
read_json = contract.read_json
canonical = contract.canonical
validate_entry = contract.validate_entry
read_allowlist = contract.read_allowlist
validate_job = contract.validate_job
UUID = contract.UUID
LIMIT_FIELDS = contract.LIMIT_FIELDS
JOB_FIELDS = contract.JOB_FIELDS


FINALIZE = ('getent passwd runner >/dev/null && getent passwd agent >/dev/null && '
            'command -v tmux >/dev/null && command -v bwrap >/dev/null && '
            'command -v git >/dev/null && test -s /opt/aq/manifest.json')
# Sandboxes are single-use, so Cargo's incremental cache is pure disk cost.
# Only these three settings are overridden; other repository profile values stand.
CARGO_ENV = ('ENV CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only '
             'CARGO_PROFILE_TEST_DEBUG=line-tables-only')


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


def replace_allowlist(allowlist, encoded, prefix):
    fd, temporary = tempfile.mkstemp(prefix=prefix, dir=allowlist.parent)
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


def buildkit_docker(cfg):
    docker = cfg.get('docker_binary') or shutil.which('docker')
    if not docker:
        raise ValueError('Docker BuildKit is required on the configured build node')
    return docker


class Buildx(NamedTuple):
    docker: str
    config: dict
    context: Path
    log_path: Path

    def run(self, target, output, dockerfile_path=None):
        command = [self.docker, '--context', self.config['docker_context'], 'buildx', 'build',
                   '--builder', self.config['buildkit_builder'], '--platform', 'linux/amd64',
                   '--provenance=false', '--sbom=false', '--progress=plain', '--target', target,
                   '--output', output]
        if dockerfile_path is not None:
            command += ['--file', str(dockerfile_path)]
        command.append(str(self.context))
        with self.log_path.open('wb') as log:
            try:
                subprocess.run(command, env=build_env(), check=True, stdin=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL, stderr=log, timeout=self.config['limits']['build_wall_ms'] / 1000)
            except subprocess.TimeoutExpired:
                log.write(b'\nbuild timed out\n')
                raise
            except OSError:
                log.write(b'could not run docker buildx\n')
                raise


def validate_output_path(path, error='unsupported output path'):
    if ',' in str(path) or '\n' in str(path):
        raise ValueError(error)


def environment_config(config):
    cfg = validate_config(config)
    if 'limits' not in cfg:
        raise ValueError('environment builds need limits in the build configuration')
    return cfg


def build_environment(env, config, output):
    """Build a parsed project recipe on the configured builder and return a Build.

    Raises BuildFailed(step, log): `install` when BuildKit fails to build the image, `check` when a
    Check command fails, `scan` when the credential scan refuses the image. `output` must not exist;
    the scanned archive is left at `output/image.oci.tar`. `config['limits']` supplies `log_bytes`
    and `build_wall_ms`.
    """
    cfg = environment_config(config)
    limit = cfg['limits']['log_bytes']
    base = parent_image(env, cfg)
    output = Path(output).absolute()
    validate_output_path(output)
    docker = buildkit_docker(cfg)
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    archive = output / 'image.oci.tar'
    with tempfile.TemporaryDirectory(prefix='aq-environment-context-') as directory:
        work = Path(directory)
        context = work / 'context'
        context.mkdir()
        (context / 'Dockerfile').write_text(environment_dockerfile(env, base, cfg))
        log_path = work / 'build.log'
        checked = work / 'checks'
        builder = Buildx(docker, cfg, context, log_path)

        def buildx(step, target, spec):
            try:
                builder.run(target, spec)
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
    cfg = environment_config(config)
    limit = cfg['limits']['log_bytes']
    base = parent_image(env, cfg)
    repo = Path(repo).resolve(strict=True)
    if not repo.is_dir():
        raise ValueError('the repository must be a directory')
    docker = buildkit_docker(cfg)
    with tempfile.TemporaryDirectory(prefix='aq-environment-try-') as directory:
        work = Path(directory)
        definition = work / 'Dockerfile'
        definition.write_text(environment_dockerfile(env, base, cfg, run=run))
        # BuildKit reads <Dockerfile>.dockerignore beside --file, so the repository is not modified.
        (work / 'Dockerfile.dockerignore').write_text('.git\n**/.git\n')
        log_path = work / 'build.log'
        exported = work / 'exported'
        validate_output_path(exported, 'unsupported temporary path')
        target = 'check-output' if run is None else 'try-output'
        try:
            Buildx(docker, cfg, repo, log_path).run(target, f'type=local,dest={exported}', definition)
        except (OSError, subprocess.SubprocessError):
            raise BuildFailed('install', bounded_log(tail(log_path, limit), limit)) from None
        return try_results(env, exported, limit, run)


def try_results(env, exported, limit, run):
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


def job_config(config, job):
    """The builder config with the job's base template, target repository and limits."""
    return validate_config({**config, 'base_template': job['base_template'],
                            'target_repository': job['target_repository'], 'limits': job['limits']})


def recipe_path(root, name):
    path = (root / '.aq' / 'environments' / (name + '.md')).resolve(strict=True)
    if not path.is_relative_to(root) or not path.is_file():
        raise BuildFailed('parse', 'the recipe file is outside the repository or not a file')
    return path


def load_recipe(repo, name, limits):
    """Parse .aq/environments/<name>.md of the checked-out repository; failures are BuildFailed('parse')."""
    environment_recipe = load_sibling('environment_recipe')
    root = Path(repo).resolve(strict=True)
    try:
        path = recipe_path(root, name)
        env = environment_recipe.parse_environment(path, limits)
    except OSError:
        raise BuildFailed('parse', 'the recipe file is missing or unreadable') from None
    except ValueError as error:
        # Parser messages are fixed text and never echo recipe content.
        raise BuildFailed('parse', str(error)) from None
    if env['name'] != name:
        raise BuildFailed('parse', 'the recipe name does not match its file name')
    return env


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
    validate_output_path(output)
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


def build_archive(docker, cfg, definition, output):
    with tempfile.TemporaryDirectory(prefix='aq-template-context-') as directory:
        context = Path(directory)
        (context / 'Dockerfile').write_text(definition)
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


class TemplateBuild(NamedTuple):
    recipe: dict
    config: dict
    base: str

    def reviewed_entry(self, scanned):
        versions = {item['name']: item['version'] for item in scanned['bundle_manifest']['harnesses']}
        provenance = {'image_digest': scanned['image_digest'],
                      'recipe_digest': 'sha256:' + hashlib.sha256(canonical(self.recipe)).hexdigest(),
                      'base_digest': self.base.split('@')[1],
                      'runtime_bundle_digest': self.config['runtime_image'].split('@')[1],
                      'harness_versions': versions,
                      'built_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                      'builder_identity': self.config['builder_identity'],
                      'finalize_policy': 'aq-finalize-v1', 'credential_scan_policy': 'aq-credential-scan-v1'}
        entry = {'name': self.recipe['name'], 'version': self.recipe['version'],
                 'image_digest': scanned['image_digest'],
                 'image_reference': self.config['target_repository'] + '@' + scanned['image_digest'],
                 'provenance': provenance, 'harness_versions': versions, 'materializations': {}}
        validate_entry(entry)
        return entry

    def publish(self, allowlist, entry):
        allowlist.parent.mkdir(parents=True, exist_ok=True)
        with allowlist.with_suffix(allowlist.suffix + '.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            current = read_allowlist(allowlist)
            # Revalidate parents and unique names under the same lock as publication.
            if parent_image(self.recipe, self.config, current['entries']) != self.base:
                raise ValueError('allowlisted parent changed during build')
            if any(template_identity(e) == template_identity(entry) for e in current['entries']):
                raise ValueError('recipe version concurrently allowlisted')
            current['entries'].append(entry)
            encoded = canonical(current) + b'\n'
            if len(encoded) > scan.MAX_JSON:
                raise ValueError('allowlist size limit')
            replace_allowlist(allowlist, encoded, prefix=".allowlist-")


def build(recipe_path, config_path, output_path):
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
    validate_output_path(output)
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    docker = buildkit_docker(cfg)
    definition = dockerfile(recipe, base, cfg)
    build_archive(docker, cfg, definition, output)
    scanned = scan.scan_oci(output / 'image.oci.tar')
    plan = TemplateBuild(recipe, cfg, base)
    entry = plan.reviewed_entry(scanned)
    # Persist all reviewable artifacts before the allowlist commit point.
    write_json(output / 'recipe.json', recipe)
    write_json(output / 'provenance.json', entry['provenance'])
    write_json(output / 'bundle-manifest.json', scanned['bundle_manifest'])
    (output / 'Dockerfile').write_text(definition)
    plan.publish(allowlist, entry)
    return entry


def main():
    import argparse
    import sys
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
    args = parser.parse_args()
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
