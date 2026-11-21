#!/usr/bin/env python3
"""Build the docker-local sandbox template image from this checkout, in one step.

It runs the existing pieces in order and adds nothing of its own to the image:

1. downloads the public inputs named by packaging/sandbox/pins.json (each
   verified against its pinned SHA-256) and builds the `aqv2` input with
   packaging/sandbox/build-aqv2.sh from an aq-server checkout;
2. exports the two build contexts with scripts/package-sandbox.py and builds
   aq-sandbox-os and aq-sandbox-runtime from them;
3. assembles a recipe (default: the `rust` recipe) on top, with the same
   Dockerfile packaging/sandbox/template_tool.py generates;
4. loads the result into the dedicated sandbox daemon, pushes it to a loopback
   registry (started here when nothing answers on the port) and writes the
   digest reference the worker's native.json pins.

The image is NOT admitted: no credential scan runs and no template allowlist
is changed. It is for a local development worker, whose native.json lists the
digest itself (aq-server scripts/local-stack writes that file).

Usage:
  packaging/local/build-template.py --server-checkout ../aq-server --output template.json
Prints and writes {"image": "<registry>/<repository>@sha256:…", "digest": "sha256:…", …}.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
SANDBOX = ROOT / "packaging/sandbox"
DEFAULT_RECIPE = ROOT / "docs/implementation/C3b-public-constants-evidence/rust.recipe.json"
# The Claude CLI is pinned: it must not update itself or phone home from a sandbox.
TEMPLATE_ENV = "ENV DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1"


def log(message):
    print(f"build-template: {message}", file=sys.stderr, flush=True)


def run(*command, **options):
    log(" ".join(str(part) for part in command))
    return subprocess.run([str(part) for part in command], check=True, **options)


def sha256(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def download(url, target, expected):
    if target.exists() and sha256(target) == expected:
        return
    log(f"download {url}")
    partial = target.with_name(target.name + ".part")
    with urllib.request.urlopen(url, timeout=120) as response, open(partial, "wb") as output:
        shutil.copyfileobj(response, output)
    if sha256(partial) != expected:
        partial.unlink()
        raise SystemExit(f"build-template: {url} does not match its pinned SHA-256")
    partial.rename(target)


def public_inputs(pins, cache, server_checkout):
    """The eight files scripts/package-sandbox.py reads, by their pinned names."""
    inputs = cache / "inputs"
    archives = cache / "archives"
    inputs.mkdir(parents=True, exist_ok=True)
    archives.mkdir(parents=True, exist_ok=True)
    for name, pin in pins["inputs"].items():
        target = inputs / name
        if target.exists() and sha256(target) == pin["sha256"]:
            continue
        if name == "aqv2":
            if server_checkout is None:
                raise SystemExit("build-template: --server-checkout is required to build the aqv2 input")
            out = cache / "aqv2-build"
            shutil.rmtree(out, ignore_errors=True)
            run(SANDBOX / "build-aqv2.sh", server_checkout, out)
            shutil.copyfile(out / "aqv2", target)
        elif "archive_member" in pin:
            archive = archives / (name + "-" + pin["archive_sha256"][:16])
            download(pin["url"], archive, pin["archive_sha256"])
            with tarfile.open(archive) as tar, open(target, "wb") as output:
                shutil.copyfileobj(tar.extractfile(pin["archive_member"]), output)
        else:
            download(pin["url"], target, pin["sha256"])
        if sha256(target) != pin["sha256"]:
            target.unlink()
            raise SystemExit(f"build-template: input {name} does not match its pinned SHA-256")
    return inputs


def registry_answers(registry):
    try:
        with urllib.request.urlopen(f"http://{registry}/v2/", timeout=5) as response:
            return response.status == 200
    except OSError:
        return False


def ensure_registry(registry, name):
    if registry_answers(registry):
        return
    host, _, port = registry.rpartition(":")
    if host not in ("127.0.0.1", "localhost"):
        raise SystemExit(f"build-template: nothing answers on {registry} and only a loopback registry is started here")
    run("docker", "run", "-d", "--restart", "unless-stopped", "--name", name,
        "-p", f"127.0.0.1:{port}:5000", "registry:2", stdout=subprocess.DEVNULL)
    for _ in range(30):
        if registry_answers(registry):
            return
        import time
        time.sleep(1)
    raise SystemExit(f"build-template: the registry container {name} did not answer on {registry}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server-checkout", type=Path, help="aq-server checkout holding the pinned aqv2 revision")
    parser.add_argument("--output", type=Path, required=True, help="where to write the template reference (JSON)")
    parser.add_argument("--cache", type=Path,
                        default=Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "agent-q/template")
    parser.add_argument("--recipe", type=Path, default=DEFAULT_RECIPE)
    parser.add_argument("--revision", default="HEAD", help="committed worker revision to package")
    parser.add_argument("--registry", default="127.0.0.1:5138", help="loopback registry that gives the image its digest")
    parser.add_argument("--registry-container", default="aq-template-registry")
    parser.add_argument("--repository", default="aq/template-unadmitted")
    parser.add_argument("--socket", default=None, help="sandbox daemon socket (default $XDG_RUNTIME_DIR/aq-sandbox/podman.sock)")
    args = parser.parse_args()
    socket = args.socket or os.path.join(os.environ["XDG_RUNTIME_DIR"], "aq-sandbox/podman.sock")
    if not Path(socket).is_socket():
        raise SystemExit(f"build-template: no sandbox daemon socket at {socket}; run packaging/local/sandbox-daemon.sh")
    cache = args.cache.resolve()
    cache.mkdir(parents=True, exist_ok=True)
    revision = subprocess.check_output(["git", "rev-parse", "--verify", args.revision + "^{commit}"],
                                       cwd=ROOT, text=True).strip()
    pins = json.loads(subprocess.check_output(["git", "show", revision + ":packaging/sandbox/pins.json"], cwd=ROOT))
    recipe = json.loads(args.recipe.read_text())
    inputs = public_inputs(pins, cache, args.server_checkout.resolve() if args.server_checkout else None)

    contexts = cache / "contexts"
    shutil.rmtree(contexts, ignore_errors=True)
    run(sys.executable, ROOT / "scripts/package-sandbox.py", "--revision", revision,
        "--inputs", inputs, "--output", contexts, stdout=subprocess.DEVNULL)
    tag = revision[:12]
    os_image, runtime_image = f"aq-sandbox-os:local-{tag}", f"aq-sandbox-runtime:local-{tag}"
    run("docker", "build", "--platform", "linux/amd64", "-t", os_image, contexts / "os")
    run("docker", "build", "--platform", "linux/amd64", "-t", runtime_image, contexts / "runtime")

    tool = load("template_tool", SANDBOX / "template_tool.py")
    dockerfile = tool.dockerfile(recipe, os_image, {"os_image": os_image, "runtime_image": runtime_image})
    name = f"{recipe['name']}-{recipe['version']}"
    local = f"aq-local-template:{name}-{tag}"
    with tempfile.TemporaryDirectory(prefix="aq-template-") as context:
        (Path(context) / "Dockerfile").write_text(dockerfile + TEMPLATE_ENV + "\n")
        run("docker", "build", "--platform", "linux/amd64", "-t", local, context)

    ensure_registry(args.registry, args.registry_container)
    target = f"{args.registry}/{args.repository}:{name}-{tag}"
    podman = [ROOT / "scripts/sandbox-daemon/aq-sandbox-podman.sh", "podman", "--url", "unix://" + socket]
    log("copy the image into the sandbox daemon")
    save = subprocess.Popen(["docker", "save", local], stdout=subprocess.PIPE)
    subprocess.run([str(part) for part in podman] + ["load"], stdin=save.stdout, check=True, stdout=sys.stderr)
    if save.wait():
        raise SystemExit("build-template: docker save failed")
    with tempfile.TemporaryDirectory(prefix="aq-template-") as scratch:
        digest_file = Path(scratch) / "digest"
        run(*podman, "push", "--tls-verify=false", "--digestfile", digest_file,
            "docker.io/library/" + local, target, stdout=sys.stderr)
        digest = digest_file.read_text().strip()
    if not digest.startswith("sha256:") or len(digest) != 71:
        raise SystemExit("build-template: the push reported no digest")
    image = f"{args.registry}/{args.repository}@{digest}"
    # The worker asks the daemon for exactly this reference.
    run(*podman, "pull", "--tls-verify=false", image, stdout=sys.stderr)
    claude = pins["inputs"]["claude"]["version"]
    result = {"image": image, "digest": digest, "recipe": name, "worker_revision": revision,
              "harnesses": [{"harness": "claude-code", "version": claude}]}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
