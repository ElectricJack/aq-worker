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


def _require(condition, error):
    if not condition:
        raise ValueError(error)


def _safe_field(value, limit):
    if not isinstance(value, str) or not 1 <= len(value.encode()) <= limit:
        return False
    return all(char >= " " and char != "\x7f" for char in value) and not scan.secret_shaped(value)


def _text(value, limit, pattern=None):
    error = 'invalid or secret-shaped recipe field'
    _require(_safe_field(value, limit), error)
    _require(not pattern or re.fullmatch(pattern, value), error)
    return value


def _positive_limit(value):
    return isinstance(value, int) and not isinstance(value, bool) and value >= 1


def _limits(limits):
    _require(isinstance(limits, dict), 'recipe limits are required')
    for key in LIMIT_KEYS:
        _require(_positive_limit(limits.get(key)), 'invalid recipe limit: ' + key)
    return limits


def _add_field(fields, line):
    key, separator, value = line.partition(':')
    _require(separator and key, 'invalid front matter line')
    _require(key == key.strip(), 'invalid front matter line')
    _require(key != 'parent', 'a recipe cannot name a parent; AQ supplies the base template')
    _require(key in FRONT_MATTER_KEYS, 'unknown front matter key')
    _require(key not in fields, 'duplicate front matter key')
    fields[key] = value.strip()


def _provides(fields):
    value = fields.get('provides', '[]')
    _require(value.startswith('[') and value.endswith(']'), 'provides must be a list')
    inner = value[1:-1].strip()
    provides = [_text(item.strip(), 128, LABEL) for item in inner.split(',')] if inner else []
    _require(len(set(provides)) == len(provides), 'duplicate provides label')
    return provides


def _front_matter(lines):
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
        _add_field(fields, line)
    _require('name' in fields, 'recipe name is required')
    name = _text(fields['name'], 64, NAME)
    flag = fields.get('default', 'false')
    _require(flag in ('true', 'false'), 'default must be true or false')
    return name, flag == 'true', _provides(fields), end + 1


def _new_section(sections, line):
    _require(line.startswith('## ') and line[3:] in SECTIONS, 'unknown recipe section')
    name = line[3:]
    _require(name not in sections, 'duplicate recipe section')
    sections[name] = []
    return name


def _sections(lines, start):
    sections = {}
    current = None
    for line in lines[start:]:
        if line.startswith('#'):
            current = _new_section(sections, line)
        elif not line.strip():
            continue
        elif current is None or not line.startswith('- '):
            raise ValueError('recipe content must be a list item inside a section')
        else:
            sections[current].append(line[2:])
    _require('Install' in sections, 'recipe needs an Install section')
    return sections


def _install_steps(items, limits):
    arg_bytes = limits['max_arg_bytes']
    _require(len(items) <= limits['max_install_steps'], 'too many install steps')
    install_steps = []
    for item in items:
        _text(item, arg_bytes * limits['max_step_args'])
        step = shlex.split(item)
        _require(1 <= len(step) <= limits['max_step_args'], 'install step has too many or no arguments')
        for arg in step:
            _text(arg, arg_bytes)
        _require(not step[0].startswith('-'), 'invalid install executable')
        install_steps.append(step)
    return install_steps


def _lines(data):
    try:
        content = data.decode('utf-8')
    except UnicodeError:
        raise ValueError('recipe must be UTF-8') from None
    return content.split('\n')


def parse_environment(path, limits):
    """Return the Parsed recipe shape for the recipe file at `path`."""
    limits = _limits(limits)
    with Path(path).open('rb') as stream:
        data = stream.read(limits['max_recipe_bytes'] + 1)
    _require(len(data) <= limits['max_recipe_bytes'], 'recipe is over the byte limit')
    lines = _lines(data)
    name, default, provides, start = _front_matter(lines)
    sections = _sections(lines, start)
    install_steps = _install_steps(sections['Install'], limits)
    arg_bytes = limits['max_arg_bytes']
    commands = {}
    for section in ('Check', 'Warm'):
        items = sections.get(section, [])
        _require(len(items) <= limits['max_install_steps'], 'too many ' + section.lower() + ' commands')
        commands[section] = [_text(item, arg_bytes) for item in items]
    return {'name': name, 'default': default, 'provides': provides,
            'install_steps': install_steps, 'checks': commands['Check'],
            'warm': commands['Warm'], 'sha256': hashlib.sha256(data).hexdigest()}
