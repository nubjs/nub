/* capprobe.c — one static binary that measures BOTH build-jail designs' kernel
 * prerequisites in whatever environment it is dropped into.
 *
 * Design B (reroot / "virgin world"): unprivileged user namespace + uid_map +
 * mount ns + pivot_root + overlayfs.  Design A (allowlist): Landlock LSM.
 *
 * Two traps this exists to avoid, both of which produce a FALSE GREEN:
 *  - `unshare(CLONE_NEWUSER)` returns 0 on a host where the userns is restricted;
 *    it hands back an INERT namespace.  The honest gate is the uid_map write.
 *  - a Landlock syscall that returns a valid fd proves the syscall is reachable,
 *    not that enforcement happens.  So we restrict_self and then assert a read
 *    that succeeded before now fails.
 *
 * Build: gcc -static -O2 capprobe.c -o capprobe
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sched.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <linux/types.h>

#ifndef __NR_landlock_create_ruleset
# if defined(__x86_64__)
#  define __NR_landlock_create_ruleset 444
#  define __NR_landlock_add_rule       445
#  define __NR_landlock_restrict_self  446
# elif defined(__aarch64__)
#  define __NR_landlock_create_ruleset 444
#  define __NR_landlock_add_rule       445
#  define __NR_landlock_restrict_self  446
# endif
#endif
#define LANDLOCK_CREATE_RULESET_VERSION (1U << 0)
#define LL_ACCESS_FS_READ_FILE         (1ULL << 2)

struct ll_ruleset_attr { __u64 handled_access_fs; };

static void say(const char *k, int rc) {
  if (rc == 0) printf("%s=OK\n", k);
  else printf("%s=FAIL errno=%d(%s)\n", k, errno, strerror(errno));
  fflush(stdout);
}

/* ---- Design B: the reroot chain, run cumulatively in one child ------------ */
static int reroot_chain(void) {
  int rc;
  /* uid/gid MUST be read BEFORE the unshare.  After CLONE_NEWUSER and before a
   * map exists the process reads back as the overflow uid (65534), and a map
   * line built from that can never satisfy the kernel's single-extent rule --
   * it fails with EPERM and looks exactly like a restricted host. */
  unsigned outer_uid = getuid(), outer_gid = getgid();

  rc = unshare(CLONE_NEWUSER);
  say("B1_unshare_NEWUSER", rc);
  /* deliberately continue even on success: B1 is the untrustworthy rung */

  int f = open("/proc/self/setgroups", O_WRONLY);
  if (f >= 0) { ssize_t w = write(f, "deny", 4); (void)w; close(f); }

  char map[64];
  snprintf(map, sizeof map, "1000 %u 1\n", outer_uid);
  f = open("/proc/self/uid_map", O_WRONLY);
  if (f < 0) { say("B2_uid_map_write", -1); return 2; }
  rc = (write(f, map, strlen(map)) == (ssize_t)strlen(map)) ? 0 : -1;
  close(f);
  say("B2_uid_map_write", rc);            /* <-- THE REAL GATE for design B */
  if (rc) return 2;

  snprintf(map, sizeof map, "1000 %u 1\n", outer_gid);
  f = open("/proc/self/gid_map", O_WRONLY);
  if (f >= 0) { ssize_t w = write(f, map, strlen(map)); (void)w; close(f); }

  rc = unshare(CLONE_NEWNS);
  say("B3_unshare_NEWNS", rc);
  if (rc) return 3;

  rc = mount("none", "/", NULL, MS_REC | MS_PRIVATE, NULL);
  say("B4_make_rprivate", rc);

  mkdir("/tmp/capprobe-root", 0755);
  rc = mount("tmpfs", "/tmp/capprobe-root", "tmpfs", 0, "size=64m,mode=0755");
  say("B5_mount_tmpfs", rc);
  if (rc) return 5;

  /* overlayfs in a userns: the 650x freshness lever.  Needs a real lowerdir. */
  mkdir("/tmp/capprobe-ov", 0755);
  if (mount("tmpfs", "/tmp/capprobe-ov", "tmpfs", 0, "size=64m,mode=0755") == 0) {
    mkdir("/tmp/capprobe-ov/lo", 0755); mkdir("/tmp/capprobe-ov/up", 0755);
    mkdir("/tmp/capprobe-ov/wk", 0755); mkdir("/tmp/capprobe-ov/mnt", 0755);
    rc = mount("overlay", "/tmp/capprobe-ov/mnt", "overlay", 0,
               "lowerdir=/tmp/capprobe-ov/lo,upperdir=/tmp/capprobe-ov/up,workdir=/tmp/capprobe-ov/wk");
    if (rc) rc = mount("overlay", "/tmp/capprobe-ov/mnt", "overlay", 0,
               "lowerdir=/tmp/capprobe-ov/lo,upperdir=/tmp/capprobe-ov/up,workdir=/tmp/capprobe-ov/wk,userxattr");
    say("B6_mount_overlayfs", rc);
  } else say("B6_mount_overlayfs", -1);

  mkdir("/tmp/capprobe-root/oldroot", 0755);
  mkdir("/tmp/capprobe-root/proc", 0755);
  if (chdir("/tmp/capprobe-root")) { say("B7_pivot_root", -1); return 7; }
  rc = (int)syscall(SYS_pivot_root, ".", "oldroot");
  say("B7_pivot_root", rc);
  if (rc) return 7;
  if (chdir("/")) return 7;
  mount("oldroot", "oldroot", NULL, MS_REC | MS_PRIVATE, NULL);
  rc = umount2("/oldroot", MNT_DETACH);
  say("B8_detach_oldroot", rc);
  return 0;
}

/* B9/B10 need their own children: NEWPID only affects descendants. */
static void pid_net_probe(void) {
  unsigned outer_uid = getuid();
  int rc = unshare(CLONE_NEWUSER);
  (void)rc;
  int f = open("/proc/self/setgroups", O_WRONLY);
  if (f >= 0) { ssize_t w = write(f, "deny", 4); (void)w; close(f); }
  char map[64]; snprintf(map, sizeof map, "1000 %u 1\n", outer_uid);
  f = open("/proc/self/uid_map", O_WRONLY);
  if (f < 0 || write(f, map, strlen(map)) != (ssize_t)strlen(map)) {
    printf("B9_unshare_NEWPID=SKIP (no userns)\nB10_unshare_NEWNET=SKIP (no userns)\nB11_mount_proc=SKIP (no userns)\n");
    return;
  }
  close(f);
  rc = unshare(CLONE_NEWPID | CLONE_NEWNS);
  say("B9_unshare_NEWPID", rc);
  int rn = unshare(CLONE_NEWNET);
  say("B10_unshare_NEWNET", rn);
  if (rc) { printf("B11_mount_proc=SKIP\n"); return; }
  pid_t p = fork();
  if (p == 0) {           /* pid 1 of the new pid ns — the only place proc mounts */
    mount("none", "/", NULL, MS_REC | MS_PRIVATE, NULL);
    mkdir("/tmp/capprobe-proc", 0755);
    int r = mount("proc", "/tmp/capprobe-proc", "proc", 0, NULL);
    say("B11_mount_proc", r);
    fflush(stdout); _exit(0);
  }
  int st; waitpid(p, &st, 0);
}

/* ---- Design A: Landlock, with a real enforcement assertion --------------- */
static void landlock_probe(void) {
  long abi = syscall(__NR_landlock_create_ruleset, NULL, 0, LANDLOCK_CREATE_RULESET_VERSION);
  if (abi < 0) { printf("A1_landlock_abi=FAIL errno=%d(%s)\n", errno, strerror(errno)); 
                 printf("A2_no_new_privs=SKIP\nA3_restrict_self=SKIP\nA4_enforcement_verified=SKIP\n"); return; }
  printf("A1_landlock_abi=OK version=%ld\n", abi);

  /* positive control: this read works BEFORE any restriction */
  int fd = open("/etc/hostname", O_RDONLY);
  if (fd < 0) fd = open("/proc/version", O_RDONLY);
  if (fd < 0) { printf("A4_enforcement_verified=INCONCLUSIVE (no readable control file)\n"); return; }
  close(fd);

  struct ll_ruleset_attr attr = { .handled_access_fs = LL_ACCESS_FS_READ_FILE };
  int rs = (int)syscall(__NR_landlock_create_ruleset, &attr, sizeof attr, 0);
  if (rs < 0) { printf("A1b_create_ruleset=FAIL errno=%d(%s)\n", errno, strerror(errno));
                printf("A2_no_new_privs=SKIP\nA3_restrict_self=SKIP\nA4_enforcement_verified=SKIP\n"); return; }
  printf("A1b_create_ruleset=OK\n");

  int nnp = prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  say("A2_no_new_privs", nnp);

  int r = (int)syscall(__NR_landlock_restrict_self, rs, 0);
  say("A3_restrict_self", r);
  if (r) { printf("A4_enforcement_verified=SKIP\n"); return; }

  /* with zero rules added and READ_FILE handled, every file read must now fail */
  int fd2 = open("/etc/hostname", O_RDONLY);
  if (fd2 < 0) fd2 = open("/proc/version", O_RDONLY);
  if (fd2 < 0) printf("A4_enforcement_verified=OK (read denied post-restrict, errno=%d)\n", errno);
  else { printf("A4_enforcement_verified=FAIL (read still permitted -> NOT enforcing)\n"); close(fd2); }
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);   /* unbuffered: _exit() in a forked probe must not lose a line */
  printf("PROBE_UID=%u PROBE_EUID=%u\n", getuid(), geteuid());
  FILE *fp = fopen("/proc/sys/kernel/apparmor_restrict_unprivileged_userns", "r");
  if (fp) { int v = -1; if (fscanf(fp, "%d", &v) != 1) v = -1; fclose(fp);
            printf("SYSCTL_apparmor_restrict_unprivileged_userns=%d\n", v); }
  else printf("SYSCTL_apparmor_restrict_unprivileged_userns=absent\n");
  fp = fopen("/proc/sys/user/max_user_namespaces", "r");
  if (fp) { long v = -1; if (fscanf(fp, "%ld", &v) != 1) v = -1; fclose(fp);
            printf("SYSCTL_max_user_namespaces=%ld\n", v); }
  fp = fopen("/sys/kernel/security/lsm", "r");
  if (fp) { char b[256] = {0}; if (fgets(b, sizeof b, fp)) { b[strcspn(b,"\n")]=0; printf("LSM_LIST=%s\n", b);} fclose(fp); }
  else printf("LSM_LIST=unreadable\n");

  if (argc > 1 && !strcmp(argv[1], "--landlock-only")) { landlock_probe(); return 0; }

  fflush(stdout);
  pid_t p = fork();
  if (p == 0) { landlock_probe(); fflush(stdout); _exit(0); }
  int st; waitpid(p, &st, 0);

  fflush(stdout);
  p = fork();
  if (p == 0) { { int _r=reroot_chain(); fflush(stdout); _exit(_r); } }
  waitpid(p, &st, 0);

  fflush(stdout);
  p = fork();
  if (p == 0) { pid_net_probe(); fflush(stdout); _exit(0); }
  waitpid(p, &st, 0);
  return 0;
}
