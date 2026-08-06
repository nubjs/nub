// SPDX-License-Identifier: GPL-2.0
// Discriminating experiment: BPF_MAP_TYPE_RINGBUF transport with an in-kernel
// drop counter, against an in-kernel "seen" counter incremented by the SAME
// probe invocation. seen is the oracle: it is the exact number of times the
// hook fired for the target process tree, so delivered/seen is a completeness
// ratio with no cross-run denominator.
#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

const volatile __u32 targ_tgid = 0;
const volatile __u8 emit_path = 1;
const volatile __u64 submit_flags = 0; /* 2 = BPF_RB_NO_WAKEUP */

#define PATH_MAX_LEN 256

struct event {
	__u32 pid;
	__u32 tgid;
	__u32 kind;
	__u64 ts;
	char path[PATH_MAX_LEN];
	char path2[PATH_MAX_LEN];
};

struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, 256 * 1024); /* overridden from userspace */
} rb SEC(".maps");

struct counters {
	__u64 seen;
	__u64 submitted;
	__u64 dropped;
};

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct counters);
} cnt SEC(".maps");

/* Tracked descendants of the target, so exec'd children are counted too. */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, __u32);
	__type(value, __u8);
} tree SEC(".maps");

static __always_inline struct counters *ctrs(void)
{
	__u32 k = 0;
	return bpf_map_lookup_elem(&cnt, &k);
}

static __always_inline int in_scope(__u32 tgid)
{
	if (!targ_tgid)
		return 1;
	if (tgid == targ_tgid)
		return 1;
	return bpf_map_lookup_elem(&tree, &tgid) != NULL;
}

/* kind: 1 file_open 2 inode_rename 3 inode_link 4 inode_unlink
 * 5 inode_create 6 inode_setattr 7 mmap_file 8 inode_mkdir 9 inode_symlink */
static __always_inline int emit(struct path *p, struct dentry *d,
				struct dentry *d2, __u32 kind)
{
	__u64 id = bpf_get_current_pid_tgid();
	__u32 tgid = id >> 32;

	if (!in_scope(tgid))
		return 0;

	struct counters *c = ctrs();
	if (!c)
		return 0;
	__sync_fetch_and_add(&c->seen, 1);

	struct event *e = bpf_ringbuf_reserve(&rb, sizeof(*e), 0);
	if (!e) {
		__sync_fetch_and_add(&c->dropped, 1);
		return 0;
	}
	e->pid = (__u32)id;
	e->tgid = tgid;
	e->kind = kind;
	e->ts = bpf_ktime_get_ns();
	e->path[0] = 0;
	e->path2[0] = 0;
	if (emit_path) {
		if (p)
			bpf_d_path(p, e->path, PATH_MAX_LEN);
		else if (d) {
			struct qstr q = BPF_CORE_READ(d, d_name);
			bpf_probe_read_kernel_str(e->path, PATH_MAX_LEN, q.name);
		}
		if (d2) {
			struct qstr q2 = BPF_CORE_READ(d2, d_name);
			bpf_probe_read_kernel_str(e->path2, PATH_MAX_LEN, q2.name);
		}
	}
	bpf_ringbuf_submit(e, submit_flags);
	__sync_fetch_and_add(&c->submitted, 1);
	return 0;
}

SEC("fentry/security_file_open")
int BPF_PROG(p_file_open, struct file *file)
{
	return emit(&file->f_path, NULL, NULL, 1);
}

SEC("fentry/security_inode_rename")
int BPF_PROG(p_rename, struct inode *od, struct dentry *odent,
	     struct inode *nd, struct dentry *ndent)
{
	return emit(NULL, odent, ndent, 2);
}

SEC("fentry/security_inode_link")
int BPF_PROG(p_link, struct dentry *old, struct inode *dir, struct dentry *new)
{
	return emit(NULL, old, new, 3);
}

SEC("fentry/security_inode_unlink")
int BPF_PROG(p_unlink, struct inode *dir, struct dentry *d)
{
	return emit(NULL, d, NULL, 4);
}

SEC("fentry/security_inode_create")
int BPF_PROG(p_create, struct inode *dir, struct dentry *d, umode_t mode)
{
	return emit(NULL, d, NULL, 5);
}

SEC("fentry/security_inode_setattr")
int BPF_PROG(p_setattr, struct mnt_idmap *m, struct dentry *d, struct iattr *a)
{
	return emit(NULL, d, NULL, 6);
}

SEC("fentry/security_mmap_file")
int BPF_PROG(p_mmap, struct file *file, unsigned long prot, unsigned long flags)
{
	/* bpf_d_path is NOT allowed here (fentry allowlist), so fall back to
	 * the dentry name. Skip anonymous mappings. */
	if (!file)
		return 0;
	if (!(prot & 0x2)) /* PROT_WRITE only */
		return 0;
	struct dentry *d = BPF_CORE_READ(file, f_path.dentry);
	return emit(NULL, d, NULL, 7);
}

SEC("fentry/security_inode_mkdir")
int BPF_PROG(p_mkdir, struct inode *dir, struct dentry *d, umode_t mode)
{
	return emit(NULL, d, NULL, 8);
}

SEC("fentry/security_inode_symlink")
int BPF_PROG(p_symlink, struct inode *dir, struct dentry *d, const char *name)
{
	return emit(NULL, d, NULL, 9);
}

/* Process-tree tracking: one kernel site, no syscall-name list. */
SEC("fentry/wake_up_new_task")
int BPF_PROG(p_newtask, struct task_struct *task)
{
	__u64 id = bpf_get_current_pid_tgid();
	__u32 parent_tgid = id >> 32;

	if (!in_scope(parent_tgid))
		return 0;
	__u32 child_tgid = BPF_CORE_READ(task, tgid);
	__u8 one = 1;
	bpf_map_update_elem(&tree, &child_tgid, &one, BPF_ANY);
	return 0;
}
