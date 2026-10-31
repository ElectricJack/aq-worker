import pathlib,sys,subprocess,os,json,hashlib,tarfile,tempfile
revision=sys.argv[1];source=pathlib.Path('/tmp/aq-wrk15-reserve-clean-'+revision[:8]);repo='/home/jkern/dev/aq-worker';subprocess.run(['git','-C',repo,'worktree','add','--detach',str(source),revision],check=True)
target='/tmp/wrk15-reserve-clean-target-'+revision[:8];env=dict(os.environ,CARGO_TARGET_DIR=target,CARGO_INCREMENTAL='0')
def run(command,cwd,log):
 with open('/tmp/wrk15-reserve-clean-'+log+'.log','w')as f:subprocess.run(command,cwd=cwd,env=env,stdout=f,stderr=subprocess.STDOUT,check=True)
run(['cargo','package','-p','aq-observation-redb-prototype','--exclude-lockfile','--no-verify','--offline'],source,'package')
archive=pathlib.Path(target)/'package/aq-observation-redb-prototype-0.0.5.crate';artifacts=[(archive,None),(pathlib.Path('/tmp/wrk15-delta-clean-target-80d79133/package/aq-observation-engine-0.0.1.crate'),'d4531ac14096e8257961b0d1f68c99e7b84cc83e3654b066a222ff540e511d4b'),(pathlib.Path('/tmp/aq22-transport-package-target/package/aq-observation-admission-0.1.0.crate'),'7de84f372c0a963c8f61d8e075e4744779bb2b125676ed1df90c165dd982fab6'),(pathlib.Path('/tmp/wrk15-clean-target-50c9e48/package/aq-observation-0.1.0.crate'),'e3ee918942b425bf71509577856f332b1116d2c8c9acd480c4f0da12c28096a0'),(pathlib.Path('/tmp/con03-producer-target/package/aq-lifecycle-protocol-0.1.0.crate'),'945b03a99701035eb7cf6679bd9baa9ad2b50a7cb6329b9ca8de2cf8bd58b92b'),(pathlib.Path('/home/jkern/dev/.aq-qualification/session-host-2026-10-31/aq-session-protocol-0.1.0.crate'),'479778d8ea76f0605b9a20460e0bd3fda73b699337b1e80783ada0d291a68090')]
standalone=pathlib.Path(tempfile.mkdtemp(prefix='wrk15-reserve-extracted-'))
for path,digest in artifacts:
 actual=hashlib.sha256(path.read_bytes()).hexdigest();assert digest is None or actual==digest,(path,actual)
 with tarfile.open(path)as tar:tar.extractall(standalone,filter='data')
(standalone/'Cargo.toml').write_text('''[workspace]
resolver="3"
members=["aq-observation-admission-0.1.0","aq-observation-engine-0.0.1","aq-observation-redb-prototype-0.0.5","aq-observation-0.1.0","aq-lifecycle-protocol-0.1.0","aq-session-protocol-0.1.0"]
[patch.crates-io]
aq-observation-admission={path="aq-observation-admission-0.1.0"}
aq-observation-engine={path="aq-observation-engine-0.0.1"}
aq-observation={path="aq-observation-0.1.0"}
aq-lifecycle-protocol={path="aq-lifecycle-protocol-0.1.0"}
aq-session-protocol={path="aq-session-protocol-0.1.0"}
''');env['CARGO_TARGET_DIR']=target+'-standalone'
env['AQ_OBSERVATION_HEADROOM_MODE']='fingerprints'
for cmd,name in [(['cargo','test','-p','aq-observation-engine','-p','aq-observation-redb-prototype','-p','aq-observation','-p','aq-observation-admission','--offline','--','--nocapture'],'tests'),(['cargo','clippy','-p','aq-observation-engine','-p','aq-observation-redb-prototype','--all-targets','--offline','--','-D','warnings'],'clippy'),(['cargo','fmt','--package','aq-observation-engine','--package','aq-observation-redb-prototype','--check'],'fmt'),(['cargo','run','-p','aq-observation-engine','--example','export','--offline','--','--check'],'drift'),(['cargo','test','-p','aq-observation-redb-prototype','--example','headroom','--offline','--','--nocapture'],'headroom-tests'),(['cargo','run','--release','-p','aq-observation-redb-prototype','--example','headroom','--offline'],'headroom'),(['cargo','run','--release','-p','aq-observation-redb-prototype','--example','headroom','--offline','--','--check-old-root','/tmp/.tmpad7OgO/owner'],'old-refusal'),(['cargo','build','--release','-p','aq-observation-redb-prototype','--example','owner_peer','--offline'],'binary')]:run(cmd,standalone,name)

result={'source_revision':subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'archive':str(archive),'archive_sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'standalone_root':str(standalone),'artifacts':{str(p):hashlib.sha256(p.read_bytes()).hexdigest()for p,_ in artifacts},'qualification':'standalone candidate72 +reference35 +admission10 tests and headroom example2 tests; strictlint/fmt/drift; default64MiB/16MiB saturation+reservedcontrols+64boots; old0.0.4 byte-preserving refusal; owner_peer build passed; no physical ENOSPC progress guarantee','binary':str(pathlib.Path(env['CARGO_TARGET_DIR'])/'release/examples/owner_peer'),'binary_sha256':hashlib.sha256((pathlib.Path(env['CARGO_TARGET_DIR'])/'release/examples/owner_peer').read_bytes()).hexdigest(),'clean_worktree_status':subprocess.check_output(['git','status','--porcelain=v1'],cwd=source,text=True)}
pathlib.Path('/tmp/wrk15-reserve-clean-qualification.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
