#!/usr/bin/env python3
"""Build checksum-pinned user-local firewall support; never collect credentials."""
from pathlib import Path
import argparse,gzip,hashlib,json,shutil,subprocess,tarfile,tempfile,os
parser=argparse.ArgumentParser(description="Package Ubuntu amd64 iptables and its private libraries for AQ rootless networking")
parser.add_argument('--output', type=Path, required=True)
args=parser.parse_args();base=args.output;base.mkdir(parents=True,exist_ok=True);gateway=base
if subprocess.check_output(['dpkg','--print-architecture'],text=True).strip()!='amd64':
 raise SystemExit('Build this runtime on Ubuntu amd64.')
with tempfile.TemporaryDirectory(prefix='aq-network-tools-') as name:
 root=Path(name);(root/'bin').mkdir();(root/'libexec').mkdir();(root/'lib/xtables').mkdir(parents=True);(root/'licenses').mkdir()
 binaries=['xtables-nft-multi','xtables-legacy-multi']
 files=[]
 for name in binaries:
  source=Path('/usr/sbin')/name;target=root/'libexec'/name;shutil.copyfile(source,target);target.chmod(0o755);files.append(source)
 extensions=[Path(p) for p in subprocess.check_output(['dpkg-query','-L','iptables'],text=True).splitlines() if '/xtables/' in p and p.endswith('.so')]
 for source in extensions:
  shutil.copyfile(source,root/'lib/xtables'/source.name);files.append(source)
 libs={}
 for source in files:
  output=subprocess.check_output(['ldd',str(source)],text=True)
  for line in output.splitlines():
   if ' => /' not in line:continue
   soname=line.strip().split()[0];path=Path(line.split(' => ',1)[1].split()[0])
   if soname.startswith(('libc.so','libm.so','libpthread.so','libdl.so','librt.so')):continue
   libs[soname]=path
 for soname,source in libs.items():shutil.copyfile(source,root/'lib'/soname)
 for family in ('iptables','ip6tables'):
  for suffix in ('','-save','-restore'):
   command=family+suffix
   text=f'''#!/bin/sh
set -eu
base=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export XTABLES_LIBDIR="$base/lib/xtables"
export LD_LIBRARY_PATH="$base/lib${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}"
export XTABLES_LOCKFILE="${{XDG_RUNTIME_DIR:?}}/aq-sandbox/xtables.lock"
exec "$base/libexec/xtables-nft-multi" {command} "$@"
'''
   p=root/'bin'/command;p.write_text(text);p.chmod(0o755)
 packages=['iptables','libxtables12','libmnl0','libnftnl11','libip4tc2','libip6tc2','libnetfilter-conntrack3','libnfnetlink0']
 for package in packages:
  copyright=Path('/usr/share/doc')/package/'copyright'
  if copyright.exists():shutil.copyfile(copyright,root/'licenses'/f'{package}-copyright')
 versions=subprocess.check_output(['dpkg-query','-W','-f','${Package}\t${Version}\n',*packages],text=True)
 (root/'SOURCE.txt').write_text('AQ private rootless firewall support, Ubuntu 24.04 amd64 packages.\nSource packages and corresponding source archives are available at https://archive.ubuntu.com/ubuntu/pool/main/ under iptables, libmnl, libnftnl, libnetfilter-conntrack and libnfnetlink.\n'+versions)
 manifest={'format':1,'packages':versions.splitlines(),'libraries':sorted(libs),'files':{str(p.relative_to(root)):{'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'bytes':p.stat().st_size} for p in sorted(root.rglob('*')) if p.is_file()}}
 (root/'manifest.json').write_text(json.dumps(manifest,sort_keys=True,indent=2)+'\n')
 archive=base/'network-tools.tar.gz'
 def normalized(info):
  info.mtime=0;info.uid=info.gid=0;info.uname=info.gname='';return info
 with archive.open('wb') as stream:
  with gzip.GzipFile(fileobj=stream,mode='wb',filename='',mtime=0,compresslevel=9) as compressed:
   with tarfile.open(fileobj=compressed,mode='w') as tar:
    for p in sorted(root.iterdir()):tar.add(p,arcname=p.name,filter=normalized)
 digest=hashlib.sha256(archive.read_bytes()).hexdigest();shutil.copyfile(archive,gateway/(digest+'.tar.gz'));(gateway/(digest+'.tar.gz')).chmod(0o644)
 shutil.copytree(root,base/'runtime',dirs_exist_ok=True)
 (base/'artifact.json').write_text(json.dumps({'sha256':digest,'bytes':archive.stat().st_size,'libraries':sorted(libs)}));print(json.dumps({'sha256':digest,'bytes':archive.stat().st_size,'libraries':sorted(libs)}))
 result=subprocess.run([str(root/'bin/iptables'),'--version'],capture_output=True,text=True)
 print(result.stdout.strip());assert result.returncode==0,result.stderr
