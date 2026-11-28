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
import time
import tomllib
import urllib.error
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener, urlopen

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
TOOLS = ("aq-worker", "aq-session-host", "aq-agent", "aq-git")
DIGEST = re.compile(r"sha256:[a-f0-9]{64}")
SLUG = re.compile(r"[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+")
MANIFEST_ACCEPT = "application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json"
# One allowlisted registry path of the gateway's nginx configuration, as nginx_config writes it.
IMAGE_ROUTE = re.compile(r"^[ \t]*location = /v2/\S* \{ limit_except GET \{ deny all; \} proxy_pass http://[^;\s]+; \}\n", re.M)
# nginx starts its reloaded workers asynchronously: the gateway is asked again before it is refused.
GATEWAY_ATTEMPTS, GATEWAY_RETRY_SECS = 10, 0.5


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
    request = Request(f"http://{registry}{manifest_path}", headers={"Accept": MANIFEST_ACCEPT})
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


def image_routes(registry, paths):
    return "".join(
        f"    location = {path} {{ limit_except GET {{ deny all; }} proxy_pass http://{registry}; }}\n"
        for path in sorted(set(paths))
    )


def with_image_routes(config, registry, paths):
    """`config` with its image allowlist replaced by exactly `paths`; everything else is kept."""
    first = IMAGE_ROUTE.search(config)
    if first is None:
        raise ValueError("The gateway's nginx.conf has no image routes to replace: regenerate it with a full build.")
    return config[:first.start()] + image_routes(registry, paths) + IMAGE_ROUTE.sub("", config[first.start():])


def nginx_config(args, registry, paths):
    listen, certs = listener_settings(args)
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
{image_routes(registry, paths)}    # The worker protocol's own /v2/ paths (the agent channel's credential) are not the
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


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *_args):
        return None


def gateway_serves(gateway_url, path):
    """Whether a GET through the gateway answers `path` as the source registry would: a manifest
    with its pinned bytes, a blob (not read) under its own digest."""
    pinned = path.rsplit("/", 1)[1]
    manifest = "/manifests/" in path
    request = Request(gateway_url.rstrip("/") + path, headers={"Accept": MANIFEST_ACCEPT} if manifest else {})
    try:
        with build_opener(NoRedirect()).open(request, timeout=20) as response:
            if response.status != 200:
                return False
            if manifest:
                return "sha256:" + hashlib.sha256(response.read(65537)).hexdigest() == pinned
            served = response.headers.get("Docker-Content-Digest")
            return "/blobs/" not in path or served in (None, pinned)
    except (urllib.error.URLError, OSError, ValueError):
        return False


def verify_gateway(gateway_url, paths):
    """Refuse the publication unless the gateway serves every path of the template image."""
    for attempt in range(GATEWAY_ATTEMPTS):
        missing = [path for path in sorted(set(paths)) if not gateway_serves(gateway_url, path)]
        if not missing:
            return
        if attempt + 1 < GATEWAY_ATTEMPTS:
            time.sleep(GATEWAY_RETRY_SECS)
    raise ValueError(f"The install gateway {gateway_url} does not serve {len(missing)} of the {len(set(paths))} paths "
                     f"of the bundle's template image, so no machine could pull it: {', '.join(missing[:4])}")


def reload_gateway(args):
    """Check and reload the gateway's nginx, started as `nginx -p OUTPUT/ -c nginx.conf`."""
    command = [str(getattr(args, "nginx", None) or "nginx"), "-p", str(args.output.absolute()) + "/", "-c", "nginx.conf"]
    for flags in (["-t", "-q"], ["-s", "reload"]):
        done = subprocess.run(command + flags, capture_output=True, text=True, timeout=30)
        if done.returncode:
            raise ValueError(f"The install gateway did not take its new image routes ({' '.join(flags)}): {done.stderr.strip()[-400:]}")


def serve_template_image(args, native, config=None):
    """Before a bundle is published, make the gateway at `--gateway-url` serve exactly the manifest
    and blobs of the template image its native template names, and check it with a GET of each.

    `config` is a full build's new nginx.conf; a restage replaces only the image routes of the one
    in `--output`. If `--output` has no nginx.conf the gateway is configured elsewhere and is only
    checked. A configuration nginx refuses is put back as it was.
    """
    entry, = native["executor"]["settings"]["host"]["docker_local"]["templates"]
    registry, paths = registry_paths(entry["image"], entry["digest"])
    path = args.output / "nginx.conf"
    current = path.read_text() if path.is_file() else None
    if config is None and current is not None:
        config = with_image_routes(current, registry, paths)
    if config is not None and config != current:
        path.write_text(config)
        try:
            reload_gateway(args)
        except (ValueError, OSError, subprocess.SubprocessError):
            path.write_text(current) if current is not None else path.unlink()
            raise
    verify_gateway(args.gateway_url, paths)


def installation_metadata(service):
    """A worker takes each repository from the server's offer: the service file lists none."""
    if "repositories" in service or "source_slugs" in service.get("manual", {}):
        raise ValueError("Remove `repositories` and `[manual.source_slugs]` from the service file: a worker takes each repository from the server's offer.")
    return {"manual_sessions": "manual" in service}


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
    host["docker_local"].pop("workspace_bind_paths", None)


# Files a bundle takes from this checkout; everything else is pinned on the operator's host.
CHECKOUT_FILES = ("aq-install.sh", "worker-install-config.py", "aq-wsl-autostart.ps1", "sandbox-daemon")


def stage_scripts(stage):
    for name in ("aq-install.sh", "worker-install-config.py"):
        shutil.copyfile(HERE / name, stage / name)
    shutil.copyfile(ROOT / "packaging/windows/aq-wsl-autostart.ps1", stage / "aq-wsl-autostart.ps1")
    shutil.copytree(ROOT / "scripts/sandbox-daemon", stage / "sandbox-daemon")


def stage_runtime(args, stage):
    stage_scripts(stage)
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
    metadata = installation_metadata(service)
    args.output.mkdir(parents=True, exist_ok=True)
    download = args.output / "install"
    download.mkdir(exist_ok=True)
    config = nginx_config(args, registry, routes)
    if getattr(args, "gateway_url", None):
        serve_template_image(args, native, config)
    with tempfile.TemporaryDirectory(dir=args.output) as temp:
        stage = Path(temp) / "bundle"
        stage.mkdir()
        stage_tools(args, stage, host)
        stage_definitions(args, stage, native)
        set_bundle_paths(native, host)
        for name, value in [("native", native), ("bootstrap", metadata)]:
            (stage / (name + ".json")).write_text(safe_json(value))
        stage_runtime(args, stage)
        digest, installer_digest = publish_bundle(stage, download)
    (args.output / "nginx.conf").write_text(config)
    finish(args, digest, installer_digest)


def finish(args, digest, installer_digest):
    published = {"installer_sha256": installer_digest, "bundle_sha256": digest, "worker_revision": worker_revision(args.worker)}
    if getattr(args, "manifest", None):
        # What aq-server's [workers.install] manifest_file reads on every request: written last and
        # atomically, so the dashboard's install command names this bundle as soon as it is served.
        staged = args.manifest.with_name(args.manifest.name + ".next")
        staged.write_text(json.dumps(published, indent=2) + "\n")
        staged.replace(args.manifest)
    print(json.dumps(published))
    return published


def restage(args):
    """Republish `--base-bundle` with this checkout's worker installer files and `--worker`.

    The base's host-pinned contents (session host, aq-agent, aq-git, tmux, Podman, Codex helpers,
    native template and definitions, repositories) are kept byte for byte, so a worker built in CI
    ships with the runtime the operator qualified. With `--gateway-url` the gateway is made to serve
    the template image the base's native template names before anything is published (see
    serve_template_image). `--max-workspace-bytes` sets that native template limit for every
    machine that installs the bundle.
    """
    download = args.output / "install"
    download.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=args.output) as temp:
        stage = Path(temp) / "bundle"
        with tarfile.open(args.base_bundle) as base:
            base.extractall(stage, filter="data")
        if getattr(args, "gateway_url", None):
            serve_template_image(args, json.loads((stage / "native.json").read_text()))
        for name in CHECKOUT_FILES:
            path = stage / name
            shutil.rmtree(path) if path.is_dir() else path.unlink()
        stage_scripts(stage)
        shutil.copyfile(args.worker, stage / "aq-worker")
        (stage / "aq-worker").chmod(0o755)
        if args.max_workspace_bytes is not None:
            native = json.loads((stage / "native.json").read_text())
            native["executor"]["settings"]["host"]["max_workspace_bytes"] = args.max_workspace_bytes
            (stage / "native.json").write_text(safe_json(native))
        digest, installer_digest = publish_bundle(stage, download)
    return finish(args, digest, installer_digest)


def worker_revision(worker):
    """The revision the worker reports ("0.5.1+a1b2c3d4e5f6" gives "a1b2c3d4e5f6"), or None."""
    try:
        version = subprocess.run([worker, "--version"], capture_output=True, text=True, timeout=10).stdout.split()[-1]
    except (OSError, IndexError, subprocess.SubprocessError):
        return None
    return version.partition("+")[2] or None


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("worker", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ("runtime", "service", "podman", "base-bundle", "codex-helpers", "tls-cert", "tls-key", "manifest"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--listen", default="127.0.0.1:28443")
    parser.add_argument("--worker-port", type=int, default=28081)
    parser.add_argument("--max-workspace-bytes", type=int)
    parser.add_argument("--gateway-url", help="the running install gateway (as http://127.0.0.1:28443): publish only "
                        "once it serves the manifest and blobs of the bundle's template image")
    parser.add_argument("--nginx", default="nginx", help="the nginx that runs the gateway from --output")
    parsed = parser.parse_args()
    if parsed.base_bundle:
        restage(parsed)
    elif parsed.runtime and parsed.service and parsed.podman:
        build(parsed)
    else:
        parser.error("give --base-bundle, or --runtime, --service and --podman for a full build")
