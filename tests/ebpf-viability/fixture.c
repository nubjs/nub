/* Known-answer coverage fixture. Every op writes a UNIQUELY-NAMED file so a
 * scanner can score each mechanism by token, not by count. argv[1] = a fresh
 * directory. Exits 0 only if every op succeeded, so a missing token means the
 * tracer missed it rather than the op never happening. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static char root[512];
static int fails;

#define CK(expr, name)                                                     \
	do {                                                               \
		if ((expr) < 0) {                                          \
			fprintf(stderr, "FIXTURE-FAIL %s: %m\n", name);    \
			fails++;                                           \
		}                                                          \
	} while (0)

static char *P(const char *n)
{
	static char b[8][1024];
	static int i;
	i = (i + 1) % 8;
	snprintf(b[i], sizeof(b[i]), "%s/%s", root, n);
	return b[i];
}

int main(int argc, char **argv)
{
	snprintf(root, sizeof(root), "%s", argv[1]);
	CK(mkdir(root, 0755), "mkdir_root");

	/* 1 create+write */
	int fd = open(P("t01_created.txt"), O_CREAT | O_RDWR, 0644);
	CK(fd, "t01_create");
	CK(write(fd, "hello", 5), "t01_write");

	/* 2 ftruncate (fd-only, no path in the syscall) */
	CK(ftruncate(fd, 4096), "t02_ftruncate");
	/* 3 fchmod (fd-only) */
	CK(fchmod(fd, 0600), "t03_fchmod");
	close(fd);

	/* 4 rename — BOTH ends must be attributed */
	CK(rename(P("t01_created.txt"), P("t04_renamedest.txt")), "t04_rename");

	/* 5 hard link — both ends */
	CK(link(P("t04_renamedest.txt"), P("t05_linkdest.txt")), "t05_link");

	/* 6 linkat with a dirfd */
	int dfd = open(root, O_RDONLY | O_DIRECTORY);
	CK(dfd, "t06_diropen");
	CK(linkat(dfd, "t04_renamedest.txt", dfd, "t06_linkatdest.txt", 0),
	   "t06_linkat");

	/* 7 symlink */
	CK(symlink("t04_renamedest.txt", P("t07_symlinkdest.txt")),
	   "t07_symlink");

	/* 8 mkdir */
	CK(mkdir(P("t08_madedir"), 0755), "t08_mkdir");

	/* 9 unlink */
	CK(unlink(P("t05_linkdest.txt")), "t09_unlink");

	/* 10 chmod by path */
	CK(chmod(P("t06_linkatdest.txt"), 0640), "t10_chmod");

	/* 11 MAP_SHARED|PROT_WRITE store with the fd CLOSED before the store.
	 * Nothing short of full emulation sees the store; the sound proxy on
	 * every mechanism is the writable open, which this shape requires. */
	int mf = open(P("t11_mmapdest.bin"), O_CREAT | O_RDWR, 0644);
	CK(mf, "t11_open");
	CK(ftruncate(mf, 4096), "t11_trunc");
	char *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mf, 0);
	if (m == MAP_FAILED) {
		fprintf(stderr, "FIXTURE-FAIL t11_mmap: %m\n");
		fails++;
	} else {
		close(mf);
		memcpy(m, "MMAPSENTINEL", 12);
		msync(m, 4096, MS_SYNC);
		munmap(m, 4096);
	}

	/* 12 forked child does its own op — tests process-tree following */
	pid_t pid = fork();
	if (pid == 0) {
		int c = open(P("t12_childwrite.txt"), O_CREAT | O_RDWR, 0644);
		if (c >= 0) {
			(void)!write(c, "child", 5);
			close(c);
		}
		_exit(0);
	}
	int st;
	waitpid(pid, &st, 0);

	/* 13 exec'd grandchild does its own op */
	pid = fork();
	if (pid == 0) {
		char cmd[2048];
		snprintf(cmd, sizeof(cmd), "cp %s %s", P("t04_renamedest.txt"),
			 P("t13_execchild.txt"));
		execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
		_exit(127);
	}
	waitpid(pid, &st, 0);
	if (access(P("t13_execchild.txt"), F_OK) < 0) {
		fprintf(stderr, "FIXTURE-FAIL t13_execchild\n");
		fails++;
	}

	fprintf(stderr, "FIXTURE done fails=%d\n", fails);
	return fails ? 1 : 0;
}
