// Emits one instance of every filesystem mutation we care about observing, so a tracer
// under test can be scored against a known ground-truth list. Each operation prints
// "OP <name> rc=<n>" to stdout so a missing rc!=0 can be told from a missing trace event.
#define _DARWIN_C_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/attr.h>
#include <sys/clonefile.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/xattr.h>

static const char *DIR;
static char P[16][1024];

static void note(const char *op, int rc) {
  printf("OP %s rc=%d%s\n", op, rc, rc < 0 ? " ERRNO" : "");
  fflush(stdout);
}

static char *p(int i, const char *name) {
  snprintf(P[i], sizeof(P[i]), "%s/%s", DIR, name);
  return P[i];
}

int main(int argc, char **argv) {
  if (argc < 2) { fprintf(stderr, "usage: workload <dir>\n"); return 2; }
  DIR = argv[1];
  printf("WORKLOAD pid=%d dir=%s\n", getpid(), DIR);
  fflush(stdout);

  // 1. open/creat + write (the baseline every tracer must see)
  int fd = open(p(0, "a.txt"), O_CREAT | O_RDWR | O_TRUNC, 0644);
  note("open_creat:a.txt", fd);
  note("write:a.txt", (int)write(fd, "hello world padding padding\n", 28));

  // 2. fd-based mutations — no path in the syscall arguments at all
  note("fchmod:a.txt(fd)", fchmod(fd, 0600));
  note("ftruncate:a.txt(fd)", ftruncate(fd, 8));
  note("fchown:a.txt(fd)", fchown(fd, getuid(), getgid()));
  struct timespec ts[2] = {{1700000000, 0}, {1700000000, 0}};
  note("futimens:a.txt(fd)", futimens(fd, ts));
  struct attrlist al; memset(&al, 0, sizeof(al));
  al.bitmapcount = ATTR_BIT_MAP_COUNT; al.commonattr = ATTR_CMN_MODTIME;
  struct timespec mt = {1700000001, 0};
  note("fsetattrlist:a.txt(fd)", fsetattrlist(fd, &al, &mt, sizeof(mt), 0));

  // 3. mmap(MAP_SHARED|PROT_WRITE) — a durable mutation with NO write syscall
  note("ftruncate:a.txt(fd)->4096", ftruncate(fd, 4096));
  void *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  note("mmap_shared:a.txt", m == MAP_FAILED ? -1 : 0);
  if (m != MAP_FAILED) {
    memset(m, 'Z', 4096);              // <-- the invisible mutation
    note("msync:a.txt", msync(m, 4096, MS_SYNC));
    note("munmap:a.txt", munmap(m, 4096));
  }
  close(fd);

  // 4. rename family — BOTH ends must be recovered
  note("rename:a.txt->b.txt", rename(p(1, "a.txt"), p(2, "b.txt")));
  int dirfd = open(DIR, O_RDONLY);
  note("open_dir", dirfd);
  note("renameat:b.txt->c.txt", renameat(dirfd, "b.txt", dirfd, "c.txt"));
  int f2 = open(p(3, "swap.txt"), O_CREAT | O_RDWR, 0644);
  note("open_creat:swap.txt", f2);
  note("write:swap.txt", (int)write(f2, "swap\n", 5));
  close(f2);
  note("renamex_np:c.txt<->swap.txt(SWAP)",
       renamex_np(p(4, "c.txt"), p(5, "swap.txt"), RENAME_SWAP));

  // 5. link / symlink / clone — two paths each
  note("link:c.txt->hard.txt", link(p(6, "c.txt"), p(7, "hard.txt")));
  note("linkat:c.txt->hard2.txt", linkat(dirfd, "c.txt", dirfd, "hard2.txt", 0));
  note("symlink:c.txt->sym.txt", symlink(p(8, "c.txt"), p(9, "sym.txt")));
  note("clonefile:c.txt->clone.txt", clonefile(p(10, "c.txt"), p(11, "clone.txt"), 0));
  note("clonefileat:c.txt->clone2.txt", clonefileat(dirfd, "c.txt", dirfd, "clone2.txt", 0));

  // 6. path-taking metadata mutations that were unsubscribed in the old adapter
  note("chmod:c.txt", chmod(p(12, "c.txt"), 0640));
  note("fchmodat:c.txt", fchmodat(dirfd, "c.txt", 0644, 0));
  note("lchmod:sym.txt", lchmod(p(13, "sym.txt"), 0777));
  note("utimensat:c.txt", utimensat(dirfd, "c.txt", ts, 0));
  struct timespec mt2 = {1700000002, 0};
  note("setattrlistat:c.txt", setattrlistat(dirfd, "c.txt", &al, &mt2, sizeof(mt2), 0));
  note("chflags:c.txt", chflags(p(14, "c.txt"), 0));
  note("truncate:c.txt", truncate(p(15, "c.txt"), 4));

  // 7. directory + removal
  char sub[1024]; snprintf(sub, sizeof(sub), "%s/sub", DIR);
  note("mkdir:sub", mkdir(sub, 0755));
  note("mkdirat:sub2", mkdirat(dirfd, "sub2", 0755));
  note("unlink:hard.txt", unlinkat(dirfd, "hard.txt", 0));
  note("unlinkat:hard2.txt", unlinkat(dirfd, "hard2.txt", 0));
  note("rmdir:sub", rmdir(sub));
  note("unlinkat_dir:sub2", unlinkat(dirfd, "sub2", AT_REMOVEDIR));

  // 8. xattr
  note("setxattr:c.txt", setxattr(p(0, "c.txt"), "user.probe", "v", 1, 0, 0));

  close(dirfd);
  printf("WORKLOAD done\n");
  fflush(stdout);
  return 0;
}
