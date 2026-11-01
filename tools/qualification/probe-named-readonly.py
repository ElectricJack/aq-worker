#!/usr/bin/env python3
"""Explicit Linux kernel feasibility probe; never invokes an agent/provider.
Requires local cc/static libc, unprivileged user namespaces and bubblewrap.
These are opt-in qualification tools, not aq-worker runtime dependencies.
"""
import tempfile,pathlib,subprocess,json
with tempfile.TemporaryDirectory(prefix='aq-named-readonly-') as root:
 root=pathlib.Path(root);image=root/'image';image.mkdir();state=root/'state';state.mkdir(mode=0o700);exe=image/'codex'
 code=b'''#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>
#include <stdlib.h>
#include <errno.h>
#include <string.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/mount.h>
int main(int argc,char **argv){
 char path[4096],fdpath[100],line[200];int n=readlink("/proc/self/exe",path,sizeof(path)-1);if(n<0)return 2;path[n]=0;
 char *canonical=realpath(path,NULL);if(!canonical)return 3;
 int fd=open(path,O_RDONLY|O_NOFOLLOW|O_CLOEXEC);if(fd<0)return 4;struct statvfs fs;struct stat st;struct statx sx;
 if(fstatvfs(fd,&fs)||fstat(fd,&st)||statx(fd,"",AT_EMPTY_PATH,STATX_MNT_ID,&sx))return 5;
 if(!(fs.f_flag&ST_RDONLY))return 13;
 if(argc==4&&((unsigned long long)st.st_dev!=strtoull(argv[2],NULL,10)||(unsigned long long)st.st_ino!=strtoull(argv[3],NULL,10)))return 14;
 printf("path:%s canonical:%s argc:%d readonly:%d dev:%llu ino:%llu mount:%llu\\n",path,canonical,argc,(int)((fs.f_flag&ST_RDONLY)!=0),(unsigned long long)st.st_dev,(unsigned long long)st.st_ino,(unsigned long long)sx.stx_mnt_id);free(canonical);
 int writable=open(path,O_WRONLY);printf("named_write_open:%d errno:%d\\n",writable,errno);if(writable>=0){close(writable);return 6;}if(errno!=EROFS&&errno!=ETXTBSY)return 7;
 snprintf(fdpath,sizeof(fdpath),"/proc/self/fd/%d",fd);writable=open(fdpath,O_WRONLY);printf("fd_alias_write_open:%d errno:%d\\n",writable,errno);if(writable>=0){close(writable);return 8;}if(errno!=EROFS&&errno!=ETXTBSY)return 9;
 int changed=mount(NULL,"/opt/aq-image",NULL,MS_REMOUNT|MS_BIND,NULL);printf("remount_rw:%d errno:%d\\n",changed,errno);if(changed==0||errno!=EPERM)return 10;
 int caps=0,nnpriv=0;FILE *status=fopen("/proc/self/status","r");
 while(fgets(line,sizeof(line),status)){
  if(!strncmp(line,"Cap",3)){fputs(line,stdout);caps++;char *value=strchr(line,':');if(!value||strtoull(value+1,NULL,16)!=0)return 15;}
  else if(!strncmp(line,"NoNewPrivs:",11)){fputs(line,stdout);nnpriv=atoi(line+11);}
 }
 fclose(status);if(caps!=5||nnpriv!=1)return 16;
 fflush(stdout);
 if(argc==1){char dev[40],ino[40];snprintf(dev,sizeof(dev),"%llu",(unsigned long long)st.st_dev);snprintf(ino,sizeof(ino),"%llu",(unsigned long long)st.st_ino);if(symlink(path,"/state/codex-linux-sandbox"))return 11;execl("/state/codex-linux-sandbox","/state/codex-linux-sandbox","helper",dev,ino,NULL);return 12;}
 return 0;}
'''
 subprocess.run(['/usr/bin/cc','-static','-O2','-x','c','-o',str(exe),'-'],input=code,env={'PATH':'/usr/bin:/bin'},check=True)
 command=['/usr/bin/bwrap','--unshare-user','--unshare-net','--unshare-pid','--die-with-parent','--cap-drop','ALL','--ro-bind','/usr','/usr','--ro-bind','/lib','/lib','--ro-bind','/lib64','/lib64','--proc','/proc','--dev','/dev','--tmpfs','/tmp','--ro-bind',str(image),'/opt/aq-image','--bind',str(state),'/state','/usr/bin/python3','-c','import os,errno,subprocess,ctypes\nassert ctypes.CDLL(None).prctl(38,1,0,0,0)==0\ntry:\n fd=os.open("/opt/aq-image/codex",os.O_WRONLY);os.close(fd);raise RuntimeError("inactive image writable")\nexcept OSError as e:\n print("inactive_named_write_errno:",e.errno,flush=True);assert e.errno==errno.EROFS\np=subprocess.run(["/usr/bin/unshare","--user","--map-root-user","--mount","/usr/bin/mount","-o","remount,rw,bind","/opt/aq-image"],capture_output=True,text=True,env={"PATH":"/usr/bin:/bin"},timeout=5)\nprint("descendant_remount_exit:",p.returncode,"detail:",p.stderr.strip(),flush=True);assert p.returncode==32 and "mount: /opt/aq-image: permission denied" in p.stderr\nos.execve("/opt/aq-image/codex",["/opt/aq-image/codex"],{"PATH":"/usr/bin:/bin"})']
 result=subprocess.run(command,env={'PATH':'/usr/bin:/bin'},capture_output=True,text=True,timeout=15)
 print('exit',result.returncode);print(result.stdout,end='');print(result.stderr,end='');raise SystemExit(result.returncode)
