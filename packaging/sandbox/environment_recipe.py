#!/usr/bin/env python3
"""Parse a project environment recipe (.aq/environments/<name>.md).

Spec 0017 section 6.1. A recipe has YAML-style front matter (name, default,
provides) and the sections Install (required), Check and Warm. It has no parent:
AQ supplies the base template. Bounds come from the caller's limits, never from
constants here. Refusal messages never echo recipe content.
"""
import hashlib
import importlib.util
from pathlib import Path
import re
import shlex

_spec = importlib.util.spec_from_file_location('credential_scan', Path(__file__).with_name('credential_scan.py'))
scan = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scan)

FRONT_MATTER_KEYS = {'name', 'default', 'provides'}
SECTIONS = ('Install', 'Check', 'Warm')
NAME = r'[a-z][a-z0-9-]*'
LABEL = r'[A-Za-z0-9_.@:-]+'
LIMIT_KEYS = ('max_install_steps', 'max_step_args', 'max_arg_bytes', 'max_recipe_bytes')


def _text(value, limit, pattern=None):
    if (not isinstance(value, str) or not 1 <= len(value.encode()) <= limit
            or any(ord(c) < 32 or ord(c) == 127 for c in value)
            or scan.secret_shaped(value) or (pattern and not re.fullmatch(pattern, value))):
        raise ValueError('invalid or secret-shaped recipe field')
    return value


def _limits(limits):
    if not isinstance(limits, dict):
        raise ValueError('recipe limits are required')
    for key in LIMIT_KEYS:
        if not isinstance(limits.get(key), int) or isinstance(limits[key], bool) or limits[key] < 1:
            raise ValueError('invalid recipe limit: ' + key)
    return limits


def _front_matter(lines, limits):
    if not lines or lines[0] != '---':
        raise ValueError('recipe must start with front matter')
    try:
        end = lines.index('---', 1)
    except ValueError:
        raise ValueError('unterminated front matter') from None
    fields = {}
    for line in lines[1:end]:
        if not line.strip():
            continue
        key, separator, value = line.partition(':')
        if not separator or key != key.strip() or not key:
            raise ValueError('invalid front matter line')
        if key == 'parent':
            raise ValueError('a recipe cannot name a parent; AQ supplies the base template')
        if key not in FRONT_MATTER_KEYS:
            raise ValueError('unknown front matter key')
        if key in fields:
            raise ValueError('duplicate front matter key')
        fields[key] = value.strip()
    if 'name' not in fields:
        raise ValueError('recipe name is required')
    name = _text(fields['name'], 64, NAME)
    flag = fields.get('default', 'false')
    if flag not in ('true', 'false'):
        raise ValueError('default must be true or false')
    provides = []
    if 'provides' in fields:
        value = fields['provides']
        if not (value.startswith('[') and value.endswith(']')):
            raise ValueError('provides must be a list')
        inner = value[1:-1].strip()
        provides = [item.strip() for item in inner.split(',')] if inner else []
        for label in provides:
            _text(label, 128, LABEL)
        if len(set(provides)) != len(provides):
            raise ValueError('duplicate provides label')
    return name, flag == 'true', provides, end + 1


def _sections(lines, start):
    sections = {}
    current = None
    for line in lines[start:]:
        if line.startswith('#'):
            if not line.startswith('## ') or line[3:] not in SECTIONS:
                raise ValueError('unknown recipe section')
            current = line[3:]
            if current in sections:
                raise ValueError('duplicate recipe section')
            sections[current] = []
        elif not line.strip():
            continue
        elif current is None or not line.startswith('- '):
            raise ValueError('recipe content must be a list item inside a section')
        else:
            sections[current].append(line[2:])
    if 'Install' not in sections:
        raise ValueError('recipe needs an Install section')
    return sections


def parse_environment(path, limits):
    """Return the Parsed recipe shape for the recipe file at `path`."""
    limits = _limits(limits)
    with Path(path).open('rb') as stream:
        data = stream.read(limits['max_recipe_bytes'] + 1)
    if len(data) > limits['max_recipe_bytes']:
        raise ValueError('recipe is over the byte limit')
    try:
        content = data.decode('utf-8')
    except UnicodeError:
        raise ValueError('recipe must be UTF-8') from None
    lines = content.split('\n')
    if lines and lines[-1] == '':
        lines.pop()
    name, default, provides, start = _front_matter(lines, limits)
    sections = _sections(lines, start)
    arg_bytes = limits['max_arg_bytes']
    if len(sections['Install']) > limits['max_install_steps']:
        raise ValueError('too many install steps')
    install_steps = []
    for item in sections['Install']:
        _text(item, arg_bytes * limits['max_step_args'])
        step = shlex.split(item)
        if not 1 <= len(step) <= limits['max_step_args']:
            raise ValueError('install step has too many or no arguments')
        for arg in step:
            _text(arg, arg_bytes)
        if step[0].startswith('-'):
            raise ValueError('invalid install executable')
        install_steps.append(step)
    commands = {}
    for section in ('Check', 'Warm'):
        items = sections.get(section, [])
        if len(items) > limits['max_install_steps']:
            raise ValueError('too many ' + section.lower() + ' commands')
        commands[section] = [_text(item, arg_bytes) for item in items]
    return {'name': name, 'default': default, 'provides': provides,
            'install_steps': install_steps, 'checks': commands['Check'],
            'warm': commands['Warm'], 'sha256': hashlib.sha256(data).hexdigest()}
