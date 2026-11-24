#!/usr/bin/env python3
"""Enforce the ratcheted build budgets in build-budget.toml.

Two numbers decide how slow this workspace is to build and test, and both only
ever grew until they were measured (see AGENTS.md "Build health"):

* integration test binaries: every `tests/*.rs` file (or `[[test]]` target) is
  a separate executable that links the whole library and its dependencies;
* library size: the lines of each crate's lib, which rustc compiles as one unit.

Both may only go down. The check measures the base branch (origin/main by
default) and this tree, and fails when this tree has more test binaries, or more
lines in a crate that has a ratchet, than the base. So a pull request never edits
build-budget.toml to keep the gain, and two pull requests never conflict on it.

The numbers in build-budget.toml are ceilings. Growth is allowed only when this
tree raises a ceiling above the base's value, an explicit, reviewed edit that
says why; the tree may then grow up to the new ceiling. Without a base (no
origin/main), the ceilings alone are checked.

    python3 scripts/build-budget.py                  # check against origin/main (CI lint job)
    python3 scripts/build-budget.py --base REF       # check against another ref
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUDGET = ROOT / "build-budget.toml"


def metadata(root=ROOT):
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def library_target(targets):
    return next((t for t in targets if {"lib", "proc-macro"}.intersection(t["kind"])), None)


def source_lines(path):
    with path.open("rb") as handle:
        return sum(1 for _ in handle)


def lib_lines(package):
    """Lines of .rs source under the lib target's directory, excluding other targets."""
    targets = package["targets"]
    lib = library_target(targets)
    if lib is None:
        return None
    src = Path(lib["src_path"]).parent
    others = {Path(t["src_path"]) for t in targets if t is not lib}
    bin_dir = src / "bin"
    total = 0
    for path in sorted(src.rglob("*.rs")):
        if path in others or bin_dir in path.parents:
            continue
        total += source_lines(path)
    return total


def test_targets(package):
    return [t for t in package["targets"] if "test" in t["kind"]]


def unbuilt_test_files(packages, root):
    unbuilt = []
    for package in packages:
        tests = Path(package["manifest_path"]).parent / "tests"
        targets = {Path(t["src_path"]).resolve() for t in test_targets(package)}
        for path in sorted([*tests.glob("*.rs"), *tests.glob("*/main.rs")]):
            if path.resolve() not in targets:
                unbuilt.append(str(path.relative_to(root)))
    return unbuilt


def measure(root=ROOT):
    meta = metadata(root)
    members = set(meta["workspace_members"])
    packages = [p for p in meta["packages"] if p["id"] in members]
    test_binaries = sum(len(test_targets(p)) for p in packages)
    unbuilt = unbuilt_test_files(packages, root)
    lines = {}
    for package in packages:
        count = lib_lines(package)
        if count is not None:
            lines[package["name"]] = count
    return test_binaries, lines, unbuilt


def base_state(ref):
    """(test binaries, lib lines, budget) of `ref`, or None when the ref is missing."""
    if subprocess.run(["git", "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"],
                      cwd=ROOT, capture_output=True).returncode != 0:
        return None
    with tempfile.TemporaryDirectory(prefix="build-budget-base-") as tmp:
        tree = Path(tmp) / "base"
        subprocess.run(["git", "worktree", "add", "--detach", "--quiet", str(tree), ref],
                       cwd=ROOT, check=True, capture_output=True)
        try:
            test_binaries, lines, _ = measure(tree)
            budget = tomllib.loads((tree / BUDGET.name).read_text())
        finally:
            subprocess.run(["git", "worktree", "remove", "--force", str(tree)],
                           cwd=ROOT, capture_output=True)
    return test_binaries, lines, budget


def allowed(base_count, ceiling, base_ceiling):
    """The most this tree may have: the base's count, or a ceiling this tree raised."""
    if base_count is None:
        return ceiling
    if base_ceiling is not None and ceiling > base_ceiling:
        return max(base_count, ceiling)
    return base_count


def check_test_binaries(test_binaries, test_budget, base, errors):
    base_tests, _, base_budget = base or (None, {}, {})
    test_limit = allowed(base_tests, test_budget, base_budget.get("test_binaries", {}).get("max"))
    print(f"build-budget: integration test binaries {test_binaries} (limit {test_limit}, ceiling {test_budget})")
    if test_binaries > test_limit:
        errors.append(
            f"{test_binaries} integration test binaries, more than {test_limit}. "
            "Add new tests to an existing test binary instead of a new test target.")

def check_line_target(name, count, line_target, errors):
    if count > line_target * 3 // 4:
        print(f"build-budget: {name} lib {count} lines (target {line_target})")
    if count > line_target:
        errors.append(
            f"{name} lib is {count} lines, above the {line_target}-line target. "
            "Split it into smaller crates.")


def check_libraries(lines, policy, base_name, base):
    errors = []
    _, base_lines, base_budget = base or (None, {}, {})
    base_ratchets = base_budget.get("lib_lines", {}).get("ratchet", {})
    line_target = policy["target"]
    ratchets = policy.get("ratchet", {})
    for name, count in sorted(lines.items(), key=lambda item: -item[1]):
        if name in ratchets:
            limit = allowed(base_lines.get(name),
                            ratchets[name], base_ratchets.get(name))
            print(f"build-budget: {name} lib {count} lines (limit {limit}, ceiling {ratchets[name]}, target {line_target})")
            if count > limit:
                errors.append(
                    f"{name} lib is {count} lines, more than {limit} on {base_name} (target "
                    f"{line_target}). Move code out of the crate (or delete some) rather than "
                    "raising its ceiling.")
        else:
            check_line_target(name, count, line_target, errors)
    for name in sorted(set(ratchets) - set(lines)):
        print(f"build-budget: note: ratchet for {name} names no workspace lib; remove it.")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default="origin/main", help="ref to compare with (default origin/main)")
    if "--update" in sys.argv[1:]:
        print("build-budget: --update is gone: the check compares with origin/main, so a lower count "
              "needs no edit of build-budget.toml. Run the script without arguments.", file=sys.stderr)
        return 2
    args = parser.parse_args()

    budget = tomllib.loads(BUDGET.read_text())
    test_budget = budget["test_binaries"]["max"]
    test_binaries, lines, unbuilt = measure()
    base = base_state(args.base)
    message = f"no {args.base}; checking the ceilings only" if base is None else f"comparing with {args.base}"
    print("build-budget: " + message)

    errors = []
    check_test_binaries(test_binaries, test_budget, base, errors)

    for path in unbuilt:
        errors.append(
            f"{path} is not a test target, so it is never built or run (the crate sets "
            "autotests = false). Make it a module of an existing tests/<group>/main.rs.")

    errors.extend(check_libraries(lines, budget["lib_lines"], args.base, base))

    for error in errors:
        print(f"::error::build-budget: {error}" if os.environ.get("GITHUB_ACTIONS")
              else f"build-budget: error: {error}", file=sys.stderr)
    if errors:
        return 1
    print("build-budget: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
