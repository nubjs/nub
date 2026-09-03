#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <mach/mach_time.h>
extern int __cxa_atexit(void (*)(void*), void*, void*);
static uint64_t t_atexit; static int n_atexit;
static void emit(const char* tag){ const char* fd=getenv("TS_FD"); if(!fd) return; char b[96]; int n=snprintf(b,sizeof b,"%s %llu\n",tag,(unsigned long long)mach_absolute_time()); write(atoi(fd),b,n); }
__attribute__((constructor)) static void init(void){ emit("ctor"); }
int my_atexit(void (*f)(void)){ uint64_t a=mach_absolute_time(); int r=atexit(f); t_atexit+=mach_absolute_time()-a; n_atexit++; return r; }
void my_exit(int c){ emit("exit"); char b[64]; const char* fd=getenv("TS_FD"); if(fd){int n=snprintf(b,sizeof b,"atexit %d %llu\n",n_atexit,(unsigned long long)t_atexit); write(atoi(fd),b,n);} exit(c); }
void my__exit(int c){ emit("_exit"); _exit(c); }
__attribute__((used)) static struct { const void* r; const void* o; } interposers[] __attribute__((section("__DATA,__interpose"))) = {
  { (const void*)my_atexit, (const void*)atexit },
  { (const void*)my_exit, (const void*)exit },
  { (const void*)my__exit, (const void*)_exit },
};
