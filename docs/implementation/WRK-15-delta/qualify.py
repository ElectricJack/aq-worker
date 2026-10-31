import pathlib,sys,subprocess,os,json,hashlib,tarfile,tempfile
revision=sys.argv[1];source=pathlib.Path('/tmp/aq-wrk15-delta-clean-'+revision[:8]);repo='/home/jkern/dev/aq-worker';subprocess.run(['git','-C',repo,'worktree','add','--detach',str(source),revision],check=True)
target='/tmp/wrk15-delta-clean-target-'+revision[:8];env=dict(os.environ,CARGO_TARGET_DIR=target,CARGO_INCREMENTAL='0')
def run(command,cwd,log):
 with open('/tmp/wrk15-delta-clean-'+log+'.log','w')as f:subprocess.run(command,cwd=cwd,env=env,stdout=f,stderr=subprocess.STDOUT,check=True)
run(['cargo','package','-p','aq-observation-engine','-p','aq-observation-redb-prototype','--exclude-lockfile','--no-verify','--offline'],source,'package')
archive=pathlib.Path(target)/'package/aq-observation-redb-prototype-0.0.2.crate';artifacts=[(archive,None),(pathlib.Path(target)/'package/aq-observation-engine-0.0.1.crate',None),(pathlib.Path('/tmp/wrk15-clean-target-50c9e48/package/aq-observation-0.1.0.crate'),'e3ee918942b425bf71509577856f332b1116d2c8c9acd480c4f0da12c28096a0'),(pathlib.Path('/tmp/con03-producer-target/package/aq-lifecycle-protocol-0.1.0.crate'),'945b03a99701035eb7cf6679bd9baa9ad2b50a7cb6329b9ca8de2cf8bd58b92b'),(pathlib.Path('/home/jkern/dev/.aq-qualification/session-host-2026-10-31/aq-session-protocol-0.1.0.crate'),'479778d8ea76f0605b9a20460e0bd3fda73b699337b1e80783ada0d291a68090')]
standalone=pathlib.Path(tempfile.mkdtemp(prefix='wrk15-delta-extracted-'))
for path,digest in artifacts:
 actual=hashlib.sha256(path.read_bytes()).hexdigest();assert digest is None or actual==digest,(path,actual)
 with tarfile.open(path)as tar:tar.extractall(standalone,filter='data')
(standalone/'Cargo.toml').write_text('''[workspace]
resolver="3"
members=["aq-observation-engine-0.0.1","aq-observation-redb-prototype-0.0.2","aq-observation-0.1.0","aq-lifecycle-protocol-0.1.0","aq-session-protocol-0.1.0"]
[patch.crates-io]
aq-observation-engine={path="aq-observation-engine-0.0.1"}
aq-observation={path="aq-observation-0.1.0"}
aq-lifecycle-protocol={path="aq-lifecycle-protocol-0.1.0"}
aq-session-protocol={path="aq-session-protocol-0.1.0"}
''');env['CARGO_TARGET_DIR']=target+'-standalone'
env['AQ_OBSERVATION_BENCH_REPETITIONS']='90'
for cmd,name in [(['cargo','test','-p','aq-observation-engine','-p','aq-observation-redb-prototype','-p','aq-observation','--offline','--','--nocapture'],'tests'),(['cargo','clippy','-p','aq-observation-engine','-p','aq-observation-redb-prototype','--all-targets','--offline','--','-D','warnings'],'clippy'),(['cargo','fmt','--package','aq-observation-engine','--package','aq-observation-redb-prototype','--check'],'fmt'),(['cargo','run','-p','aq-observation-engine','--example','export','--offline','--','--check'],'drift'),(['cargo','run','--release','-p','aq-observation-redb-prototype','--example','measure_engine','--offline'],'engine-measure')]:run(cmd,standalone,name)

result={'source_revision':subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'archive':str(archive),'archive_sha256':hashlib.sha256(archive.read_bytes()).hexdigest(),'standalone_root':str(standalone),'artifacts':{str(p):hashlib.sha256(p.read_bytes()).hexdigest()for p,_ in artifacts},'qualification':'standalone candidate47 +reference35 tests/clippy/fmt/drift/90repetitions-per-configuration passed','clean_worktree_status':subprocess.check_output(['git','status','--porcelain=v1'],cwd=source,text=True)}
pathlib.Path('/tmp/wrk15-delta-clean-qualification.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
