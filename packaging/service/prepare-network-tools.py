#!/usr/bin/env python3
"""Build checksum-pinned user-local firewall support; never collect credentials."""
import argparse
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

PACKAGES = (
    "iptables", "libxtables12", "libmnl0", "libnftnl11", "libip4tc2",
    "libip6tc2", "libnetfilter-conntrack3", "libnfnetlink0",
)
SYSTEM_LIBRARIES = ("libc.so", "libm.so", "libpthread.so", "libdl.so", "librt.so")


def copy_executables(root):
    sources = []
    for name in ("xtables-nft-multi", "xtables-legacy-multi"):
        source = Path("/usr/sbin") / name
        target = root / "libexec" / name
        shutil.copyfile(source, target)
        target.chmod(0o755)
        sources.append(source)
    return sources


def copy_extensions(root):
    listing = subprocess.check_output(["dpkg-query", "-L", "iptables"], text=True)
    extensions = [Path(p) for p in listing.splitlines() if "/xtables/" in p and p.endswith(".so")]
    for source in extensions:
        shutil.copyfile(source, root / "lib/xtables" / source.name)
    return extensions


def library_paths(source):
    output = subprocess.check_output(["ldd", str(source)], text=True)
    for line in output.splitlines():
        soname, separator, target = line.partition(" => /")
        if separator and not soname.strip().startswith(SYSTEM_LIBRARIES):
            yield soname.strip(), Path("/" + target.split()[0])


def copy_libraries(root, sources):
    libraries = dict(entry for source in sources for entry in library_paths(source))
    for soname, source in libraries.items():
        shutil.copyfile(source, root / "lib" / soname)
    return sorted(libraries)


def write_wrapper(path):
    path.write_text(f'''#!/bin/sh
set -eu
base=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export XTABLES_LIBDIR="$base/lib/xtables"
export LD_LIBRARY_PATH="$base/lib${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}"
export XTABLES_LOCKFILE="${{XDG_RUNTIME_DIR:?}}/aq-sandbox/xtables.lock"
exec "$base/libexec/xtables-nft-multi" {path.name} "$@"
''')
    path.chmod(0o755)


def package_sources(root):
    for package in PACKAGES:
        copyright = Path("/usr/share/doc") / package / "copyright"
        if copyright.exists():
            shutil.copyfile(copyright, root / "licenses" / f"{package}-copyright")
    versions = subprocess.check_output(["dpkg-query", "-W", "-f", "${Package}\t${Version}\n", *PACKAGES], text=True)
    (root / "SOURCE.txt").write_text(
        "AQ private rootless firewall support, Ubuntu 24.04 amd64 packages.\n"
        "Source packages and corresponding source archives are available at "
        "https://archive.ubuntu.com/ubuntu/pool/main/ under iptables, libmnl, libnftnl, "
        "libnetfilter-conntrack and libnfnetlink.\n" + versions
    )
    return versions.splitlines()


def file_record(path):
    return {"sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "bytes": path.stat().st_size}


def stage_runtime(root):
    for directory in ("bin", "libexec", "lib/xtables", "licenses"):
        (root / directory).mkdir(parents=True)
    libraries = copy_libraries(root, copy_executables(root) + copy_extensions(root))
    for family in ("iptables", "ip6tables"):
        for suffix in ("", "-save", "-restore"):
            write_wrapper(root / "bin" / (family + suffix))
    packages = package_sources(root)
    files = {str(p.relative_to(root)): file_record(p) for p in sorted(root.rglob("*")) if p.is_file()}
    manifest = {"format": 1, "packages": packages, "libraries": libraries, "files": files}
    (root / "manifest.json").write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
    return libraries


def normalized(info):
    info.mtime = info.uid = info.gid = 0
    info.uname = info.gname = ""
    return info


def archive_runtime(root, archive):
    with archive.open("wb") as stream:
        with gzip.GzipFile(fileobj=stream, mode="wb", filename="", mtime=0, compresslevel=9) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as tar:
                for path in sorted(root.iterdir()):
                    tar.add(path, arcname=path.name, filter=normalized)


def build(base):
    base.mkdir(parents=True, exist_ok=True)
    if subprocess.check_output(["dpkg", "--print-architecture"], text=True).strip() != "amd64":
        raise SystemExit("Build this runtime on Ubuntu amd64.")
    with tempfile.TemporaryDirectory(prefix="aq-network-tools-") as name:
        root = Path(name)
        libraries = stage_runtime(root)
        archive = base / "network-tools.tar.gz"
        archive_runtime(root, archive)
        record = {**file_record(archive), "libraries": libraries}
        pinned = base / (record["sha256"] + ".tar.gz")
        shutil.copyfile(archive, pinned)
        pinned.chmod(0o644)
        shutil.copytree(root, base / "runtime", dirs_exist_ok=True)
        encoded = json.dumps(record)
        (base / "artifact.json").write_text(encoded)
        print(encoded)
        result = subprocess.run([str(root / "bin/iptables"), "--version"], capture_output=True, text=True)
        print(result.stdout.strip())
        assert result.returncode == 0, result.stderr


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Package Ubuntu amd64 iptables and its private libraries for AQ rootless networking")
    parser.add_argument("--output", type=Path, required=True)
    build(parser.parse_args().output)
