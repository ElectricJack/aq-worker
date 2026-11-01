#define _GNU_SOURCE
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
static int save(const char *p,const void *b,size_t n){int f=open(p,O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW,0600);if(f<0)return 1;size_t k=0;while(k<n){ssize_t w=write(f,(const char*)b+k,n-k);if(w<=0)return 2;k+=(size_t)w;}if(fsync(f))return 3;return close(f)!=0;}
int main(int argc,char **argv){
 if(argc!=4)return 10;
 if(strcmp(argv[3],"exit")==0)return 0;
 if(strcmp(argv[3],"wait")!=0)return 11;
 int f=open(argv[1],O_RDONLY|O_NOFOLLOW);if(f<0)return 12;
 struct stat s;if(fstat(f,&s)||!S_ISREG(s.st_mode)||s.st_nlink!=1||(s.st_mode&0777)!=0600||s.st_size<1||s.st_size>4096)return 13;
 char payload[4097];ssize_t n=read(f,payload,sizeof(payload));if(n!=s.st_size)return 14;char extra;if(read(f,&extra,1)!=0)return 15;close(f);
 if(save(argv[2],payload,(size_t)n))return 16;
 char cwd[PATH_MAX],name[PATH_MAX];if(!getcwd(cwd,sizeof(cwd)))return 17;
 if(snprintf(name,sizeof(name),"%s.cwd",argv[2])>=(int)sizeof(name))return 18;
 if(save(name,cwd,strlen(cwd)))return 19;
 const char message[]="synthetic native fixture running\r\n";if(write(1,message,sizeof(message)-1)<0)return 20;
 for(;;){
  struct timespec delay={0,10000000};nanosleep(&delay,0);
#ifdef AQ_TEST_PERIODIC_OUTPUT
  /* Explicit tail-probe build only; normal fixture behavior stays unchanged. */
  static unsigned ticks=0;
  if(++ticks%25==0){
   const char tick[]="synthetic native fixture output tick\r\n";
   if(write(1,tick,sizeof(tick)-1)<0)return 21;
  }
#endif
 }
}
