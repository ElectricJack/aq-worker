#!/usr/bin/env python3
"""Validate the worker-machine/v1 fixtures against schema.json."""
import copy
import json
import pathlib

import jsonschema

here = pathlib.Path(__file__).resolve().parent
schema = json.loads((here / "schema.json").read_text())
validator = jsonschema.Draft202012Validator(schema)
fixtures = sorted((here / "fixtures").glob("report-*.json"))
assert len(fixtures) == 2, fixtures
for path in fixtures:
    validator.validate(json.loads(path.read_text()))

base = json.loads(fixtures[0].read_text())
bad = []
case = copy.deepcopy(base)
case["cpu_busy_bps"] = 10001
bad.append(case)
case = copy.deepcopy(base)
case["source"] = "windows"
bad.append(case)
case = copy.deepcopy(base)
case["extra"] = 1
bad.append(case)
for case in bad:
    assert not validator.is_valid(case), case
print("worker-machine-1: 2 fixtures and 3 invalid cases checked")
