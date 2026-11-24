"""Validation of sandbox template recipes, build configurations, jobs and allowlist entries."""
import importlib.util
import json
from pathlib import Path
import re
from urllib.parse import urlsplit

_spec = importlib.util.spec_from_file_location('credential_scan', Path(__file__).with_name('credential_scan.py'))
scan = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scan)


def has_fields(value, fields):
    return isinstance(value, dict) and value.keys() == fields


def require(condition, message):
    if not condition:
        raise ValueError(message)


def require_fields(value, fields, message):
    require(has_fields(value, fields), message)


def require_list(value, bounds, message):
    if not isinstance(value, list) or len(value) not in bounds:
        raise ValueError(message)


CONTROL_TEXT = re.compile(r'[\x00-\x1f\x7f]')


def public_text(value, limit):
    if not isinstance(value, str) or not 1 <= len(value.encode()) <= limit:
        return False
    return not (CONTROL_TEXT.search(value) or scan.secret_shaped(value))


def text(value, limit=128, pattern=None):
    require(public_text(value, limit), 'invalid or secret-shaped field')
    if pattern and not re.fullmatch(pattern, value):
        raise ValueError('invalid or secret-shaped field')
    return value


def pinned(value):
    text(value, 512, r'[a-z0-9][a-z0-9./:_-]*@sha256:[0-9a-f]{64}')
    return value


def parent_image(recipe, config, entries=()):
    if 'parent' not in recipe:
        # AQ supplies the base template for a parsed project recipe.
        try:
            return config['base_template']
        except KeyError:
            raise ValueError('a project recipe needs a configured base_template') from None
    parent = recipe['parent']
    if parent == config['os_image']:
        return parent
    if config.get('universal_image') and parent in {'codex-universal', config['universal_image']}:
        return config['universal_image']
    return allowlisted_image(parent, entries)


def allowlisted_image(parent, entries):
    for entry in entries:
        if parent == template_identity(entry):
            return entry['image_reference']
    raise ValueError('parent must be a pinned configured base or allowlisted toolchain')


def validate_install_steps(steps):
    require_list(steps, range(65), 'invalid install steps')
    for step in steps:
        validate_install_step(step)


def validate_install_step(step):
    require_list(step, range(1, 33), 'install step must be an argv array')
    for arg in step:
        text(arg, 4096)
    if step[0].startswith('-'):
        raise ValueError('invalid install executable')


def public_repository_url(url):
    return url.scheme == 'https' and bool(url.hostname) and not any((url.username, url.password, url.query, url.fragment))


def validate_prefetch(prefetch, config):
    require_list(prefetch, range(17), 'invalid repository prefetch')
    if prefetch and config.get('registry_visibility') != 'private':
        raise ValueError('repository objects require a private target registry')
    for item in prefetch:
        validate_prefetch_item(item)


def validate_prefetch_item(item):
    require_fields(item, {'repository', 'commit'}, 'invalid prefetch fields')
    repo = text(item['repository'], 512)
    url = urlsplit(repo)
    if not public_repository_url(url):
        raise ValueError('prefetch requires a non-secret HTTPS repository URL')
    text(item['commit'], 40, r'[0-9a-f]{40}')


def validate_recipe(recipe, config, entries=()):
    if not isinstance(recipe, dict) or set(recipe) - {'schema_version', 'name', 'version', 'parent', 'install_steps', 'prefetch'}:
        raise ValueError('unknown recipe fields')
    require(recipe.get('schema_version') == 1, 'unsupported recipe schema')
    text(recipe.get('name'), 64, r'[a-z][a-z0-9-]*')
    text(recipe.get('version'), 64, r'[A-Za-z0-9_.+-]+')
    text(recipe.get('parent'), 512)
    parent_image(recipe, config, entries)
    validate_install_steps(recipe.get('install_steps'))
    validate_prefetch(recipe.get('prefetch', []), config)
    return recipe


def relative_allowlist(value, message):
    path = text(value, 512)
    if Path(path).is_absolute() or scan.safe_path(path) != path:
        raise ValueError(message)
    return path


def template_identity(entry):
    return entry['name'] + '@' + entry['version']


def validate_images(config):
    for key in ['os_image', 'runtime_image', 'universal_image', 'base_template', 'skopeo_image']:
        if key in config:
            pinned(config[key])
    require_image(config['os_image'], 'aq-sandbox-os', 'OS')
    require_image(config['runtime_image'], 'aq-sandbox-runtime', 'runtime')
    if 'universal_image' in config:
        require_image(config['universal_image'], 'codex-universal', 'universal base')


def require_image(reference, image, role):
    if reference.split('@')[0].rsplit('/', 1)[-1] != image:
        raise ValueError(f'configured {role} must be {image}')


def positive_integer(value):
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def require_positive(limits, key):
    if not positive_integer(limits.get(key)):
        raise ValueError('invalid limit: ' + key)


def validate_build_limits(config):
    if 'limits' not in config:
        return
    limits = config['limits']
    if not isinstance(limits, dict):
        raise ValueError('limits must be an object')
    for key in ['log_bytes', 'build_wall_ms']:
        require_positive(limits, key)


def validate_build_role(config):
    required = {'schema_version', 'role', 'docker_context', 'buildkit_builder', 'builder_identity',
                'os_image', 'runtime_image', 'target_repository', 'registry_visibility', 'allowlist'}
    if not isinstance(config, dict) or not required <= config.keys() <= required | {'universal_image', 'docker_binary', 'base_template', 'limits', 'skopeo_image'}:
        raise ValueError('invalid build configuration fields')
    require((config['schema_version'], config['role']) == (1, 'template-build'), 'an explicit template-build node is required')


def validate_config(config):
    validate_build_role(config)
    for key in ['docker_context', 'buildkit_builder', 'builder_identity']:
        text(config[key], 128, r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    validate_images(config)
    text(config['target_repository'], 256, r'[a-z0-9][a-z0-9.:-]*/[a-z0-9][a-z0-9/_.-]*')
    require(config['registry_visibility'] in {'private', 'public'}, 'registry visibility must be explicit')
    relative_allowlist(config['allowlist'], 'allowlist must be a relative vault path')
    validate_build_limits(config)
    if 'docker_binary' in config and not Path(text(config['docker_binary'], 512)).is_absolute():
        raise ValueError('docker_binary must be absolute')
    return config


def read_json(path):
    with Path(path).open('rb') as stream:
        return scan.json_bytes(stream.read(scan.MAX_JSON + 1))


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode()


def validate_provenance_digests(provenance):
    for key in ['recipe_digest', 'base_digest', 'runtime_bundle_digest']:
        if not scan.DIGEST.fullmatch(provenance[key]):
            raise ValueError('invalid provenance digest')


def validate_provenance(entry):
    provenance = entry['provenance']
    expected = {'image_digest', 'recipe_digest', 'base_digest', 'runtime_bundle_digest', 'harness_versions',
                'built_at', 'builder_identity', 'finalize_policy', 'credential_scan_policy'}
    require_fields(provenance, expected, 'invalid allowlist provenance')
    require(provenance['image_digest'] == entry['image_digest'], 'provenance image digest mismatch')
    validate_provenance_digests(provenance)
    text(provenance['builder_identity'], 128, r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    text(provenance['built_at'], 40, r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?\+00:00')
    if (provenance['finalize_policy'], provenance['credential_scan_policy']) != ('aq-finalize-v1', 'aq-credential-scan-v1'):
        raise ValueError('allowlist entry has no supported admission evidence')


def validate_harness_versions(entry):
    provenance = entry['provenance']
    versions = entry['harness_versions']
    if not isinstance(versions, dict) or not 1 <= len(versions) <= 32:
        raise ValueError('inconsistent advertised harness versions')
    require(versions == provenance['harness_versions'], 'inconsistent advertised harness versions')
    for name, version in versions.items():
        text(name, 128, r'[A-Za-z0-9_.+-]+')
        text(version, 128, r'[A-Za-z0-9_.+-]+')


def validate_materializations(entry):
    if not isinstance(entry['materializations'], dict) or len(canonical(entry['materializations'])) > 65536:
        raise ValueError('invalid materialization metadata')
    if 'boat' in entry['materializations']:
        validate_boat(entry['materializations']['boat'], entry['image_digest'])


def validate_boat(boat, image_digest):
    fields = {'image_digest','saved_snapshot','snapshot_id','scan_policy','snapshot_archive_digest',
              'build_org','builder_identity','built_at'}
    if not has_fields(boat, fields) or boat['image_digest'] != image_digest:
        raise ValueError('boat materialization digest mismatch')
    for key in ['saved_snapshot','snapshot_id','build_org','builder_identity']:
        text(boat[key],128,r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    text(boat['built_at'],40,r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?\+00:00')
    if boat['scan_policy'] != 'aq-credential-scan-v1' or not scan.DIGEST.fullmatch(boat['snapshot_archive_digest']):
        raise ValueError('boat snapshot scan evidence missing')


def validate_entry(entry):
    fields = {'name', 'version', 'image_digest', 'image_reference', 'provenance',
              'harness_versions', 'materializations'}
    require_fields(entry, fields, 'invalid allowlist entry fields')
    text(entry['name'], 64, r'[a-z][a-z0-9-]*')
    text(entry['version'], 64, r'[A-Za-z0-9_.+-]+')
    pinned(entry['image_reference'])
    if not scan.DIGEST.fullmatch(entry['image_digest']) or not entry['image_reference'].endswith('@' + entry['image_digest']):
        raise ValueError('allowlist digest mismatch')
    validate_provenance(entry)
    validate_harness_versions(entry)
    validate_materializations(entry)
    require(not scan.secret_shaped(canonical(entry)), 'credential in allowlist metadata')
    return entry


def validate_entries(entries):
    names = set()
    for entry in entries:
        validate_entry(entry)
        key = (entry['name'], entry['version'])
        if key in names:
            raise ValueError('duplicate allowlist name/version')
        names.add(key)


def read_allowlist(path):
    if not path.exists():
        return {'schema_version': 1, 'entries': []}
    value = read_json(path)
    if not has_fields(value, {'schema_version', 'entries'}) or value['schema_version'] != 1:
        raise ValueError('unsupported allowlist schema')
    entries = value['entries']
    require_list(entries, range(1025), 'allowlist entry limit')
    validate_entries(entries)
    return value


UUID = r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}'
JOB_FIELDS = {'job_id', 'kind', 'project_id', 'project', 'commit', 'recipes', 'run', 'base_template',
              'target_repository', 'limits'}
LIMIT_FIELDS = {'max_install_steps', 'max_step_args', 'max_arg_bytes', 'max_prefetch', 'max_recipe_bytes',
                'build_wall_ms', 'build_cpu_millis', 'build_memory_bytes', 'log_bytes', 'try_concurrency',
                'keep_versions'}


def validate_job_limits(limits):
    require_fields(limits, LIMIT_FIELDS, 'invalid job limits')
    for key in limits:
        require_positive(limits, key)


def validate_job_recipes(job, kind):
    recipes = job['recipes']
    if not isinstance(recipes, list) or not recipes:
        raise ValueError('a job names distinct recipes')
    require(len(set(map(str, recipes))) == len(recipes), 'a job names distinct recipes')
    for name in recipes:
        text(name, 64, r'[a-z][a-z0-9-]*')
    validate_job_run(job, kind, recipes)


def validate_job_run(job, kind, recipes):
    if kind == 'try':
        if len(recipes) != 1:
            raise ValueError('a try names one recipe')
        if job['run'] is not None:
            text(job['run'], job['limits']['max_arg_bytes'])
    elif job['run'] is not None:
        raise ValueError('a build job has no run command')


def validate_job(job, kind):
    """A server Job (plan Shared shapes) of `kind`; a try names one recipe, a build has no run."""
    require_fields(job, JOB_FIELDS, 'invalid job fields')
    require(job['kind'] == kind, 'job kind does not match the operation')
    text(job['job_id'], 36, UUID)
    text(job['project_id'], 36, UUID)
    text(job['project'], 128, r'[A-Za-z0-9][A-Za-z0-9_.-]*')
    text(job['commit'], 40, r'[0-9a-f]{40}')
    pinned(job['base_template'])
    validate_job_limits(job['limits'])
    validate_job_recipes(job, kind)
    return job
