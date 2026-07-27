/*
 * confine_probe — does UNPRIVILEGED Landlock + seccomp actually ENFORCE here?
 *
 * The userns probe answers "can we bwrap"; this answers the separate question the
 * build-jail tier depends on: with no user namespace and no privilege at all, can a
 * process still confine itself? Landlock ABI>=1 gives filesystem, ABI>=4 gives TCP.
 * Every mode is self-testing: it restricts itself, then performs the action that must
 * fail and the control action that must succeed, and prints one token per outcome.
 *
 * Build:  cc -static -O1 -o confine_probe confine_probe.c
 * Modes:  fs <allow_dir> <denied_path> <allowed_path> | net | seccomp | abi
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stddef.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef __NR_landlock_create_ruleset
#define __NR_landlock_create_ruleset 444
#define __NR_landlock_add_rule 445
#define __NR_landlock_restrict_self 446
#endif

#define LL_FS_EXECUTE (1ULL << 0)
#define LL_FS_WRITE_FILE (1ULL << 1)
#define LL_FS_READ_FILE (1ULL << 2)
#define LL_FS_READ_DIR (1ULL << 3)
#define LL_NET_BIND_TCP (1ULL << 0)
#define LL_NET_CONNECT_TCP (1ULL << 1)
#define LL_RULE_PATH_BENEATH 1
#define LL_RULE_NET_PORT 2

struct ll_ruleset_attr {
    __u64 handled_access_fs;
    __u64 handled_access_net;
};
struct ll_path_beneath_attr {
    __u64 allowed_access;
    __s32 parent_fd;
} __attribute__((packed));

static long ll_abi(void) { return syscall(__NR_landlock_create_ruleset, NULL, 0, 1U); }

static int nnp(void) { return prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0); }

static int mode_fs(const char *allow_dir, const char *denied, const char *allowed) {
    long abi = ll_abi();
    if (abi < 0) { printf("LANDLOCK_UNAVAILABLE errno=%d\n", errno); return 1; }
    struct ll_ruleset_attr ra = { .handled_access_fs = LL_FS_EXECUTE | LL_FS_WRITE_FILE |
                                                       LL_FS_READ_FILE | LL_FS_READ_DIR,
                                  .handled_access_net = 0 };
    /* ABI 1 predates handled_access_net, so its attr struct is 8 bytes, not 16. */
    size_t attr_sz = (abi >= 4) ? sizeof(ra) : sizeof(__u64);
    int rs = syscall(__NR_landlock_create_ruleset, &ra, attr_sz, 0);
    if (rs < 0) { printf("FS_CREATE_RULESET_FAIL errno=%d (%s)\n", errno, strerror(errno)); return 1; }
    int dfd = open(allow_dir, O_PATH | O_CLOEXEC);
    if (dfd < 0) { printf("FS_OPEN_ALLOWDIR_FAIL errno=%d\n", errno); return 1; }
    struct ll_path_beneath_attr pb = { .allowed_access = LL_FS_EXECUTE | LL_FS_READ_FILE |
                                                         LL_FS_READ_DIR,
                                       .parent_fd = dfd };
    if (syscall(__NR_landlock_add_rule, rs, LL_RULE_PATH_BENEATH, &pb, 0) < 0) {
        printf("FS_ADD_RULE_FAIL errno=%d (%s)\n", errno, strerror(errno)); return 1;
    }
    if (nnp() < 0) { printf("FS_NNP_FAIL errno=%d\n", errno); return 1; }
    if (syscall(__NR_landlock_restrict_self, rs, 0) < 0) {
        printf("FS_RESTRICT_SELF_FAIL errno=%d (%s)\n", errno, strerror(errno)); return 1;
    }
    printf("FS_RESTRICTED_OK abi=%ld\n", abi);
    int f = open(denied, O_RDONLY);
    printf(f < 0 ? "FS_DENIED_READ_BLOCKED errno=%d (%s)\n" : "FS_DENIED_READ_LEAKED fd=%d\n",
           f < 0 ? errno : f, f < 0 ? strerror(errno) : "");
    if (f >= 0) close(f);
    int g = open(allowed, O_RDONLY);
    printf(g >= 0 ? "FS_ALLOWED_READ_OK\n" : "FS_ALLOWED_READ_BROKEN errno=%d\n", g < 0 ? errno : 0);
    if (g >= 0) close(g);
    return 0;
}

static int tcp_connect(void) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    if (s < 0) return -errno;
    struct sockaddr_in sa = { .sin_family = AF_INET, .sin_port = htons(443) };
    sa.sin_addr.s_addr = htonl(0x01010101); /* 1.1.1.1 */
    int r = connect(s, (struct sockaddr *)&sa, sizeof(sa));
    int e = r < 0 ? errno : 0;
    close(s);
    return r < 0 ? -e : 0;
}

static int mode_net(void) {
    long abi = ll_abi();
    printf("NET_ABI=%ld\n", abi);
    if (abi < 4) { printf("NET_UNSUPPORTED_ABI\n"); return 1; }
    int pre = tcp_connect();
    printf("NET_PRECONTROL_CONNECT=%d (%s)\n", pre, pre == 0 ? "reachable" : strerror(-pre));
    struct ll_ruleset_attr ra = { .handled_access_fs = 0,
                                  .handled_access_net = LL_NET_BIND_TCP | LL_NET_CONNECT_TCP };
    int rs = syscall(__NR_landlock_create_ruleset, &ra, sizeof(ra), 0);
    if (rs < 0) { printf("NET_CREATE_RULESET_FAIL errno=%d (%s)\n", errno, strerror(errno)); return 1; }
    /* No rules added at all => every handled net access is denied. */
    if (nnp() < 0) { printf("NET_NNP_FAIL errno=%d\n", errno); return 1; }
    if (syscall(__NR_landlock_restrict_self, rs, 0) < 0) {
        printf("NET_RESTRICT_SELF_FAIL errno=%d (%s)\n", errno, strerror(errno)); return 1;
    }
    printf("NET_RESTRICTED_OK\n");
    int post = tcp_connect();
    printf(post == 0 ? "NET_CONNECT_LEAKED\n" : "NET_CONNECT_BLOCKED errno=%d (%s)\n",
           post == 0 ? 0 : -post, post == 0 ? "" : strerror(-post));
    return 0;
}

static int mode_seccomp(void) {
    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_socket, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (EPERM & SECCOMP_RET_DATA)),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = { .len = sizeof(f) / sizeof(f[0]), .filter = f };
    int pre = socket(AF_INET, SOCK_STREAM, 0);
    printf("SECCOMP_PRECONTROL_SOCKET=%d\n", pre);
    if (pre >= 0) close(pre);
    if (nnp() < 0) { printf("SECCOMP_NNP_FAIL errno=%d (%s)\n", errno, strerror(errno)); return 1; }
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog) < 0) {
        printf("SECCOMP_INSTALL_FAIL errno=%d (%s)\n", errno, strerror(errno)); return 1;
    }
    printf("SECCOMP_INSTALLED_OK\n");
    int post = socket(AF_INET, SOCK_STREAM, 0);
    printf(post < 0 ? "SECCOMP_SOCKET_BLOCKED errno=%d (%s)\n" : "SECCOMP_SOCKET_LEAKED fd=%d\n",
           post < 0 ? errno : post, post < 0 ? strerror(errno) : "");
    if (post >= 0) close(post);
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 2) { printf("usage: confine_probe abi|fs|net|seccomp\n"); return 2; }
    if (!strcmp(argv[1], "abi")) { printf("LANDLOCK_ABI=%ld errno=%d\n", ll_abi(), errno); return 0; }
    if (!strcmp(argv[1], "fs")) {
        if (argc < 5) { printf("usage: fs <allow_dir> <denied> <allowed>\n"); return 2; }
        return mode_fs(argv[2], argv[3], argv[4]);
    }
    if (!strcmp(argv[1], "net")) return mode_net();
    if (!strcmp(argv[1], "seccomp")) return mode_seccomp();
    printf("unknown mode\n");
    return 2;
}
