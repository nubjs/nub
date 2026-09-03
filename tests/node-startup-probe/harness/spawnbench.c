#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <mach/mach_time.h>
extern char **environ;
static double ms(uint64_t t){ static mach_timebase_info_data_t tb; if(!tb.denom) mach_timebase_info(&tb); return (double)t*tb.numer/tb.denom/1e6; }
int main(int argc, char** argv){
  int runs=atoi(argv[1]); char** cmd=argv+2;
  double mn[6]={1e9,1e9,1e9,1e9,1e9,1e9}; double md[6][512]; int nk=0;
  for(int i=0;i<runs;i++){
    int p[2]; pipe(p); int devnull=open("/dev/null",O_WRONLY);
    posix_spawn_file_actions_t fa; posix_spawn_file_actions_init(&fa);
    posix_spawn_file_actions_adddup2(&fa,devnull,1); posix_spawn_file_actions_adddup2(&fa,devnull,2);
    posix_spawn_file_actions_addclose(&fa,p[0]);
    char fdenv[32]; snprintf(fdenv,sizeof fdenv,"TS_FD=%d",p[1]);
    char* envp[64]; int ne=0; for(char** e=environ;*e&&ne<60;e++) envp[ne++]=*e; envp[ne++]=fdenv; envp[ne++]=getenv("INTERPOSE_LIB")?strdup(({static char b[512];snprintf(b,sizeof b,"DYLD_INSERT_LIBRARIES=%s",getenv("INTERPOSE_LIB"));b;})):"DYLD_INSERT_LIBRARIES=/tmp/nub-startup-bench/libinterpose2.dylib"; envp[ne]=0;
    pid_t pid; uint64_t t0=mach_absolute_time();
    if(posix_spawn(&pid,cmd[0],&fa,0,cmd,envp)){perror("spawn");return 1;}
    close(p[1]); close(devnull);
    char buf[4096]; int len=0,r; while((r=read(p[0],buf+len,sizeof buf-1-len))>0) len+=r; buf[len]=0;
    int st; waitpid(pid,&st,0); uint64_t t1=mach_absolute_time(); close(p[0]);
    unsigned long long tc=0,te=0,tx=0,ta=0; int na=0; char* s=buf; char tag[16]; unsigned long long v;
    while(sscanf(s,"%15s %llu",tag,&v)==2){ if(!strcmp(tag,"ctor"))tc=v; else if(!strcmp(tag,"exit"))te=v; else if(!strcmp(tag,"_exit"))tx=v; else if(!strcmp(tag,"atexit")){ na=(int)v; char* q=strchr(s,' '); q=strchr(q+1,' '); ta=strtoull(q,0,10);} s=strchr(s,'\n'); if(!s)break; s++; }
    if(!tc||!te){ if(i==0) fprintf(stderr,"(no marks: tc=%llu te=%llu tx=%llu)\n",tc,te,tx); }
    double total=ms(t1-t0), pre=tc?ms(tc-t0):0, inproc=(tc&&te)?ms(te-tc):0, handlers=(te&&tx)?ms(tx-te):0, post=te?ms(t1-(tx?tx:te)):0, at=ms(ta);
    double vals[6]={total,pre,inproc,handlers,post,at}; for(int k=0;k<6;k++){ if(vals[k]<mn[k])mn[k]=vals[k]; md[k][i]=vals[k]; }
    nk=na;
  }
  printf("%-28s total %6.2f | exec+dyld %6.2f | in-process %6.2f | exit-handlers %6.2f | kernel-teardown %6.2f | atexit(n=%d) %5.2f  [mins of %d, ms]\n", cmd[0]+ (strrchr(cmd[0],'/')?strrchr(cmd[0],'/')-cmd[0]+1:0), mn[0],mn[1],mn[2],mn[3],mn[4],nk,mn[5],runs);
  return 0;
}
