/*
 * landlock_deny — can Landlock express nub's build-jail policy shape?
 *
 * nub's build_jail_surface() is GRANT-THE-DIRECTORY-THEN-DENY:
 *     "./" -> r, <package_dir> -> rw, $tooldirs -> r, $tmp -> rw
 *     /etc/shadow -> DENY, /etc/gshadow -> DENY      (deny INSIDE a granted /etc read)
 *     plus the bounded `.env*` deny floor
 * The epic's correctness invariant is that a post-launch-created file inside a granted
 * directory must still read back — which is why enumerate-to-exclude is rejected.
 *
 * Landlock is documented as allow-list only, so the open question is whether a NESTED
 * rule can narrow access inside an already-granted hierarchy. This program answers it.
 *
 * E1  nested rule with allowed_access = 0                 (does the kernel accept it?)
 * E1b nested rule with a non-empty but READ-less access   (fallback deny idiom)
 * E2  post-launch-created file in a granted dir reads back (the epic's invariant)
 * E3  is a deny inode-bound? delete + recreate the denied path, then read
 *
 * Build: cc -O1 -o landlock_deny landlock_deny.c
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <linux/types.h>
#include <unistd.h>

#ifndef __NR_landlock_create_ruleset
#define __NR_landlock_create_ruleset 444
#define __NR_landlock_add_rule 445
#define __NR_landlock_restrict_self 446
#endif

#define LL_EXECUTE (1ULL << 0)
#define LL_WRITE_FILE (1ULL << 1)
#define LL_READ_FILE (1ULL << 2)
#define LL_READ_DIR (1ULL << 3)
#define LL_REMOVE_FILE (1ULL << 5)
#define LL_MAKE_REG (1ULL << 8)
#define LL_RULE_PATH_BENEATH 1

struct rs_attr {
    __u64 handled_access_fs;
    __u64 handled_access_net;
};
struct pb_attr {
    __u64 allowed_access;
    __s32 parent_fd;
} __attribute__((packed));

static long abi(void) { return syscall(__NR_landlock_create_ruleset, NULL, 0, 1U); }

/* Returns 0 on success, -errno on failure. */
static int add_rule(int rs, const char *path, __u64 access) {
    int fd = open(path, O_PATH | O_CLOEXEC);
    if (fd < 0) return -errno;
    struct pb_attr pb = { .allowed_access = access, .parent_fd = fd };
    int r = syscall(__NR_landlock_add_rule, rs, LL_RULE_PATH_BENEATH, &pb, 0);
    int e = r < 0 ? errno : 0;
    close(fd);
    return r < 0 ? -e : 0;
}

static void try_read(const char *label, const char *path) {
    int f = open(path, O_RDONLY);
    if (f < 0) {
        printf("  %-34s READ_BLOCKED errno=%d (%s)\n", label, errno, strerror(errno));
    } else {
        char buf[64] = { 0 };
        ssize_t n = read(f, buf, sizeof(buf) - 1);
        for (char *p = buf; *p; p++) if (*p == '\n') *p = ' ';
        printf("  %-34s READ_OK bytes=%zd content=[%s]\n", label, n, buf);
        close(f);
    }
}

int main(int argc, char **argv) {
    long a = abi();
    printf("LANDLOCK_ABI=%ld\n", a);
    if (a < 0) { printf("LANDLOCK_UNAVAILABLE\n"); return 1; }

    const char *mode = argc > 1 ? argv[1] : "zero";
    /* "zero" => nested rule allowed_access = 0 ; "readless" => EXECUTE only (non-empty) */
    __u64 nested = strcmp(mode, "readless") == 0 ? LL_EXECUTE : 0;
    printf("MODE=%s nested_allowed_access=0x%llx\n", mode, (unsigned long long)nested);

    system("rm -rf /tmp/lltest && mkdir -p /tmp/lltest/etcsim /tmp/lltest/ws");
    system("echo PASSWDLINE > /tmp/lltest/etcsim/passwd");
    system("echo SHADOWHASH > /tmp/lltest/etcsim/shadow");
    system("echo PUBLICSRC  > /tmp/lltest/ws/index.js");
    system("echo ENVSECRET  > /tmp/lltest/ws/.env");

    struct rs_attr ra = { .handled_access_fs = LL_EXECUTE | LL_WRITE_FILE | LL_READ_FILE |
                                               LL_READ_DIR | LL_REMOVE_FILE | LL_MAKE_REG,
                          .handled_access_net = 0 };
    size_t sz = (a >= 4) ? sizeof(ra) : sizeof(__u64);
    int rs = syscall(__NR_landlock_create_ruleset, &ra, sz, 0);
    if (rs < 0) { printf("CREATE_RULESET_FAIL errno=%d\n", errno); return 1; }

    /* GRANT the directories (the "grant-the-directory" half). */
    __u64 rd = LL_READ_FILE | LL_READ_DIR | LL_EXECUTE;
    __u64 rw = rd | LL_WRITE_FILE | LL_MAKE_REG | LL_REMOVE_FILE;
    int r;
    r = add_rule(rs, "/tmp/lltest/etcsim", rd);
    printf("GRANT /tmp/lltest/etcsim  rc=%d %s\n", r, r ? strerror(-r) : "");
    r = add_rule(rs, "/tmp/lltest/ws", rw);
    printf("GRANT /tmp/lltest/ws      rc=%d %s\n", r, r ? strerror(-r) : "");
    /* the interpreter + libs, so exec/read of the shell still works for `system()` */
    add_rule(rs, "/usr", rd); add_rule(rs, "/bin", rd); add_rule(rs, "/lib", rd);
    add_rule(rs, "/lib64", rd); add_rule(rs, "/etc", rd);

    /* DENY inside the grant (the "then-deny" half) — THE experiment. */
    r = add_rule(rs, "/tmp/lltest/etcsim/shadow", nested);
    printf("DENY  /tmp/lltest/etcsim/shadow  add_rule_rc=%d %s\n", r, r ? strerror(-r) : "ACCEPTED");
    int deny_shadow_ok = (r == 0);
    r = add_rule(rs, "/tmp/lltest/ws/.env", nested);
    printf("DENY  /tmp/lltest/ws/.env        add_rule_rc=%d %s\n", r, r ? strerror(-r) : "ACCEPTED");

    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0) { printf("NNP_FAIL\n"); return 1; }
    if (syscall(__NR_landlock_restrict_self, rs, 0) < 0) {
        printf("RESTRICT_SELF_FAIL errno=%d (%s)\n", errno, strerror(errno)); return 1;
    }
    printf("RESTRICTED_OK (deny rules accepted: %s)\n", deny_shadow_ok ? "yes" : "NO");

    printf("== E1: deny inside a granted hierarchy ==\n");
    try_read("control  etcsim/passwd", "/tmp/lltest/etcsim/passwd");
    try_read("DENIED   etcsim/shadow", "/tmp/lltest/etcsim/shadow");
    try_read("control  ws/index.js",   "/tmp/lltest/ws/index.js");
    try_read("DENIED   ws/.env",       "/tmp/lltest/ws/.env");

    printf("== E2: post-launch-created file in a granted dir must READ BACK ==\n");
    int nf = open("/tmp/lltest/ws/generated.js", O_CREAT | O_WRONLY, 0644);
    if (nf < 0) { printf("  create rc=-1 errno=%d (%s)\n", errno, strerror(errno)); }
    else { write(nf, "MADEAFTER\n", 10); close(nf); }
    try_read("post-launch ws/generated.js", "/tmp/lltest/ws/generated.js");

    printf("== E3: is the deny inode-bound? delete+recreate the denied path ==\n");
    int u = unlink("/tmp/lltest/ws/.env");
    printf("  unlink ws/.env rc=%d %s\n", u, u ? strerror(errno) : "");
    int rf = open("/tmp/lltest/ws/.env", O_CREAT | O_WRONLY, 0644);
    if (rf < 0) { printf("  recreate rc=-1 errno=%d (%s)\n", errno, strerror(errno)); }
    else { write(rf, "NEWSECRET\n", 10); close(rf); }
    try_read("recreated ws/.env", "/tmp/lltest/ws/.env");

    printf("== E4: a NEW .env-shaped file that never had a deny rule ==\n");
    int nf2 = open("/tmp/lltest/ws/.env.local", O_CREAT | O_WRONLY, 0644);
    if (nf2 >= 0) { write(nf2, "LATESECRET\n", 11); close(nf2); }
    try_read("post-launch ws/.env.local", "/tmp/lltest/ws/.env.local");
    return 0;
}
