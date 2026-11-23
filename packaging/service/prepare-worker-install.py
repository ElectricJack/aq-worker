#!/usr/bin/env python3
"""Build a downloadable worker bundle and private worker-only nginx ingress."""
import argparse
import hashlib
import ipaddress
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
TOOLS = ("aq-worker", "aq-session-host", "aq-agent", "aq-git")
DIGEST = re.compile(r"sha256:[a-f0-9]{64}")
SLUG = re.compile(r"[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+")


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def copy_pin(source, target, expected):
    if sha(source) != expected.removeprefix("sha256:"):
        raise ValueError(f"The bytes of {source.name} do not match the active worker template.")
    shutil.copyfile(source, target)
    target.chmod(0o755 if source.stat().st_mode & 0o111 else 0o644)


def package_tmux(pin, stage):
    source = Path(pin["path"])
    target = stage / "tmux"
    copy_pin(source, target, pin["artifact"]["digest"])
    libraries = subprocess.run(["ldd", source], capture_output=True, text=True, timeout=10).stdout
    for line in libraries.splitlines():
        fields = line.split()
        if fields[:2] != ["libtinfo.so.6", "=>"]:
            continue
        # Keep the versioned ncurses symbols available to the bundled executable.
        # Only this copy of tmux loads the bundled library.
        directory = stage / "tmux-libs"
        directory.mkdir()
        shutil.copyfile(fields[2], directory / "libtinfo.so.6")
        shutil.copyfile("/usr/share/doc/libtinfo6/copyright", directory / "copyright")
        subprocess.run(["patchelf", "--add-rpath", "$ORIGIN/tmux-libs", target], check=True)
        pin["artifact"]["version"] += "-with-tinfo"
        pin["artifact"]["digest"] = sha(target)
        break


def require_loopback_registry(registry):
    host, port = registry.split(":")
    if not ipaddress.ip_address(host).is_loopback or not 0 < int(port) < 65536:
        raise ValueError("Use a numeric loopback source registry.")


def registry_reference(image, pinned):
    registry, repo = image.split("/", 1)
    require_loopback_registry(registry)
    repo, image_digest = repo.split("@")
    if not (SLUG.fullmatch(repo) and DIGEST.fullmatch(pinned)):
        raise ValueError("The active image must be pinned by digest.")
    if image_digest != pinned:
        raise ValueError("The active image must be pinned by digest.")
    return registry, repo


def manifest_blobs(registry, manifest_path, pinned):
    request = Request(f"http://{registry}{manifest_path}", headers={
        "Accept": "application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json",
    })
    with urlopen(request, timeout=10) as response:
        raw = response.read(65537)
    if len(raw) > 65536 or "sha256:" + hashlib.sha256(raw).hexdigest() != pinned:
        raise ValueError("The source registry did not return the pinned image manifest.")
    manifest = json.loads(raw)
    blobs = [manifest["config"], *manifest["layers"]]
    if not 1 <= len(blobs) <= 128 or any(not DIGEST.fullmatch(blob["digest"]) for blob in blobs):
        raise ValueError("Unexpected image manifest.")
    return blobs


def registry_paths(image, pinned):
    registry, repo = registry_reference(image, pinned)
    manifest_path = f"/v2/{repo}/manifests/{pinned}"
    blobs = manifest_blobs(registry, manifest_path, pinned)
    return registry, ["/v2/", manifest_path, *(f"/v2/{repo}/blobs/{blob['digest']}" for blob in blobs)]


def safe_json(value):
    encoded = json.dumps(value, indent=2) + "\n"
    if re.search(r"sk-ant-|sk-proj-|aqn_[A-Za-z0-9_-]{25,}|aqe_[A-Za-z0-9_-]{25,}|aqsv1_[a-f0-9]{64}", encoded):
        raise ValueError("A setup file contains credential material.")
    return encoded


def render_bootstrap(bundle_digest):
    template = (HERE / "aq-bootstrap.sh").read_text()
    config = (HERE / "worker-install-config.py").read_text().rstrip().replace("@AQ_HOST_BUNDLE_SHA256@", bundle_digest)
    return template.replace("@AQ_ARCHIVE_SHA256@", bundle_digest).replace("@AQ_INSTALL_CONFIG@", config)


def listener_address(listen):
    host, port = listen.split(":")
    address = ipaddress.IPv4Address(host)
    specific = not address.is_unspecified and not address.is_multicast
    if not 0 < int(port) < 65536 or not specific:
        raise ValueError("Use a specific private IPv4 address and port.")
    return address


def listener_tls(args, address):
    tls = bool(args.tls_cert and args.tls_key)
    paired = (args.tls_cert is None) == (args.tls_key is None)
    private_listener = address.is_loopback or (address.is_private and tls)
    if not paired or not private_listener:
        raise ValueError("A LAN listener requires a private address, certificate and key.")
    return tls


def listener_settings(args):
    tls = listener_tls(args, listener_address(args.listen))
    if not 0 < args.worker_port < 65536:
        raise ValueError("Invalid upstream worker port.")
    listen = args.listen + (" ssl" if tls else "")
    certs = f"ssl_certificate {quote_path(args.tls_cert)}; ssl_certificate_key {quote_path(args.tls_key)};" if tls else ""
    return listen, certs


def quote_path(path):
    return '"' + str(path.absolute()).replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\$") + '"'


def nginx_config(args, registry, paths):
    listen, certs = listener_settings(args)
    image_routes = "\n".join(
        f"location = {path} {{ limit_except GET {{ deny all; }} proxy_pass http://{registry}; }}"
        for path in sorted(set(paths))
    )
    return f"""worker_processes 1;
pid nginx.pid;
error_log stderr warn;
events {{ worker_connections 128; }}
http {{
  access_log off;
  client_max_body_size 0;
  client_body_temp_path client-body;
  proxy_temp_path proxy-body;
  fastcgi_temp_path fastcgi;
  uwsgi_temp_path uwsgi;
  scgi_temp_path scgi;
  map $http_upgrade $upgrade_connection {{ default upgrade; '' close; }}
  server {{
    listen {listen};
    {certs}
    proxy_http_version 1.1;
    proxy_request_buffering off;
    proxy_buffering off;
    proxy_connect_timeout 5s;
    proxy_read_timeout 300s;
    location ~ "^/install/[a-f0-9]{{64}}\\.sh$" {{
      limit_except GET {{ deny all; }}
      root {quote_path(args.output)};
      default_type text/plain;
      add_header Cache-Control no-store;
    }}
    location ~ "^/install/[a-f0-9]{{64}}\\.tar\\.gz$" {{
      limit_except GET {{ deny all; }}
      root {quote_path(args.output)};
      default_type application/gzip;
    }}
    location /install/ {{ return 404; }}
    {image_routes}
    # The worker protocol's own /v2/ paths (the agent channel's credential) are not the
    # image registry's: without this an enrolled machine cannot launch an agent.
    location /v2/worker/ {{
      proxy_pass http://127.0.0.1:{args.worker_port};
    }}
    location /v2/ {{ return 404; }}
    location / {{
      proxy_set_header Upgrade $http_upgrade;
      proxy_set_header Connection $upgrade_connection;
      proxy_pass http://127.0.0.1:{args.worker_port};
    }}
  }}
}}
"""


def installation_metadata(service):
    repos = [{key: row[key] for key in ("repository_id", "identity", "binding_generation", "object_format")} for row in service.get("repositories", [])]
    manual = service.get("manual", {})
    slugs = manual.get("source_slugs", {})
    if any(not SLUG.fullmatch(slug) for slug in slugs.values()):
        raise ValueError("Use owner/repository names in the source map.")
    metadata = {
        "manual_sessions": "manual" in service,
        "source_slugs": slugs,
    }
    return repos, metadata


def stage_tools(args, stage, host):
    for name in TOOLS:
        src = args.worker if name == "aq-worker" else args.runtime / name
        if name == "aq-session-host":
            copy_pin(src, stage / name, host["helper"]["artifact"]["digest"])
        else:
            shutil.copyfile(src, stage / name)
        (stage / name).chmod(0o755)
    package_tmux(host["tmux"], stage)


def stage_definitions(args, stage, native):
    executor = native["executor"]
    settings = executor["settings"]
    definitions = [
        (native, "adapter_manifest", "adapter.json"),
        (executor, "composite_definition", "composite.json"),
        (native, "inventory_definition", "inventory.json"),
    ]
    if codex := settings.get("codex"):
        definitions.append((codex, "composite_definition", "codex-composite.json"))
        if codex.get("runtime_bundle") is not None:
            if not args.codex_helpers:
                raise ValueError("Include --codex-helpers for the configured Codex runtime bundle.")
            codex["runtime_bundle"] = "/bundle/codex-bundle"
    for section, key, name in definitions:
        # Preserve the exact pinned bytes, including the original formatting.
        raw = Path(section[key]).read_bytes()
        safe_json(json.loads(raw))
        (stage / name).write_bytes(raw)
        section[key] = "/" + name


def set_bundle_paths(native, host):
    for key in ("helper", "tmux"):
        host[key]["path"] = "/bundle/" + Path(host[key]["path"]).name
    for key in ("git_root", "git_socket", "state_dir"):
        native[key] = "/bundle/" + key
    if "node_git" in native:  # a node-git enrolled template; the installer sets the path
        native["node_git"]["repositories"] = "/bundle/node_git"
    host["root"] = "/bundle/host"
    host["docker_local"]["socket"] = "/bundle/podman.sock"


def stage_runtime(args, stage):
    for name in ("aq-install.sh", "worker-install-config.py"):
        shutil.copyfile(HERE / name, stage / name)
    shutil.copyfile(ROOT / "packaging/windows/aq-wsl-autostart.ps1", stage / "aq-wsl-autostart.ps1")
    scripts = ROOT / "scripts/sandbox-daemon"
    shutil.copytree(scripts, stage / "sandbox-daemon")
    with tarfile.open(stage / "podman-linux-amd64.tar.gz", "w:gz", compresslevel=3) as archive:
        archive.add(args.podman, arcname="podman-linux-amd64")
    if args.codex_helpers:
        for name in ("codex-code-mode-host", "codex-path/rg", "codex-resources/bwrap"):
            target = stage / "codex-bundle" / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(args.codex_helpers / name, target)
            target.chmod(0o755)


def archive_bundle(stage, download):
    output = stage.parent / "worker.tar.gz"
    with tarfile.open(output, "w:gz", compresslevel=3) as archive:
        for item in sorted(stage.iterdir()):
            archive.add(item, arcname=item.name)
    digest = sha(output)
    output.replace(download / (digest + ".tar.gz"))
    return digest


def publish_bundle(stage, download):
    digest = archive_bundle(stage, download)
    bootstrap = stage.parent / "aq-worker.sh"
    bootstrap.write_text(render_bootstrap(digest))
    installer_digest = sha(bootstrap)
    bootstrap.replace(download / (installer_digest + ".sh"))
    return digest, installer_digest


def build(args):
    service = tomllib.loads(args.service.read_text())
    native = json.loads(Path(service["native_template"]).read_text())
    host = native["executor"]["settings"]["host"]
    entry, = host["docker_local"]["templates"]
    registry, routes = registry_paths(entry["image"], entry["digest"])
    repos, metadata = installation_metadata(service)
    args.output.mkdir(parents=True, exist_ok=True)
    download = args.output / "install"
    download.mkdir(exist_ok=True)
    config = nginx_config(args, registry, routes)
    with tempfile.TemporaryDirectory(dir=args.output) as temp:
        stage = Path(temp) / "bundle"
        stage.mkdir()
        stage_tools(args, stage, host)
        stage_definitions(args, stage, native)
        set_bundle_paths(native, host)
        for name, value in [("native", native), ("repositories", repos), ("bootstrap", metadata)]:
            (stage / (name + ".json")).write_text(safe_json(value))
        stage_runtime(args, stage)
        digest, installer_digest = publish_bundle(stage, download)
    (args.output / "nginx.conf").write_text(config)
    published = {"installer_sha256": installer_digest, "bundle_sha256": digest, "worker_revision": worker_revision(args.worker)}
    if getattr(args, "manifest", None):
        # What aq-server's [workers.install] manifest_file reads on every request: written last and
        # atomically, so the dashboard's install command names this bundle as soon as it is served.
        staged = args.manifest.with_name(args.manifest.name + ".next")
        staged.write_text(json.dumps(published, indent=2) + "\n")
        staged.replace(args.manifest)
    print(json.dumps(published))


def worker_revision(worker):
    """The revision the worker reports ("0.5.1+a1b2c3d4e5f6" gives "a1b2c3d4e5f6"), or None."""
    try:
        version = subprocess.run([worker, "--version"], capture_output=True, text=True, timeout=10).stdout.split()[-1]
    except (OSError, IndexError, subprocess.SubprocessError):
        return None
    return version.partition("+")[2] or None


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("runtime", "worker", "service", "podman", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ("codex-helpers", "tls-cert", "tls-key", "manifest"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--listen", default="127.0.0.1:28443")
    parser.add_argument("--worker-port", type=int, default=28081)
    build(parser.parse_args())
