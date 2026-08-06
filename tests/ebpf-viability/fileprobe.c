// Userspace consumer for fileprobe.bpf.c
//   fileprobe --pid N [--rb-bytes N] [--out FILE] [--no-path] [--stall-ms N]
// Prints, at exit: seen (in-kernel oracle) / submitted / dropped (in-kernel)
// and delivered (counted in userspace).
#include <argp.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <bpf/libbpf.h>
#include <bpf/bpf.h>
#include "fileprobe.skel.h"

#define PATH_MAX_LEN 256
struct event {
	unsigned int pid, tgid, kind;
	unsigned long long ts;
	char path[PATH_MAX_LEN];
	char path2[PATH_MAX_LEN];
};
struct counters {
	unsigned long long seen, submitted, dropped;
};

static volatile int stop;
static unsigned long long delivered;
static FILE *out;
static int stall_ms;
static const char *kinds[] = { "?",	 "OPEN",   "RENAME", "LINK", "UNLINK",
			       "CREATE", "SETATTR", "MMAPW",  "MKDIR", "SYMLINK" };

static void sigh(int s)
{
	(void)s;
	stop = 1;
}

static int on_event(void *ctx, void *data, size_t sz)
{
	(void)ctx;
	(void)sz;
	struct event *e = data;
	delivered++;
	if (out) {
		if (e->path2[0])
			fprintf(out, "%s pid=%u %s -> %s\n",
				kinds[e->kind < 10 ? e->kind : 0], e->pid,
				e->path, e->path2);
		else
			fprintf(out, "%s pid=%u %s\n",
				kinds[e->kind < 10 ? e->kind : 0], e->pid,
				e->path);
	}
	if (stall_ms) {
		struct timespec ts = { stall_ms / 1000,
				       (stall_ms % 1000) * 1000000L };
		nanosleep(&ts, NULL);
	}
	return 0;
}

int main(int argc, char **argv)
{
	unsigned int targ = 0, rb_bytes = 256 * 1024;
	int emit_path = 1;
	unsigned long long submit_flags = 0;
	const char *outpath = NULL;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--pid"))
			targ = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--rb-bytes"))
			rb_bytes = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--out"))
			outpath = argv[++i];
		else if (!strcmp(argv[i], "--no-path"))
			emit_path = 0;
		else if (!strcmp(argv[i], "--no-wakeup"))
			submit_flags = 2;
		else if (!strcmp(argv[i], "--stall-ms"))
			stall_ms = atoi(argv[++i]);
	}

	struct fileprobe_bpf *skel = fileprobe_bpf__open();
	if (!skel) {
		fprintf(stderr, "open failed\n");
		return 1;
	}
	skel->rodata->targ_tgid = targ;
	skel->rodata->emit_path = emit_path;
	skel->rodata->submit_flags = submit_flags;
	if (bpf_map__set_max_entries(skel->maps.rb, rb_bytes)) {
		fprintf(stderr, "set rb size failed\n");
		return 1;
	}
	if (fileprobe_bpf__load(skel)) {
		fprintf(stderr, "LOAD FAILED: %s\n", strerror(errno));
		return 1;
	}
	if (fileprobe_bpf__attach(skel)) {
		fprintf(stderr, "ATTACH FAILED: %s\n", strerror(errno));
		return 1;
	}

	if (outpath) {
		out = fopen(outpath, "w");
		if (!out) {
			perror("fopen");
			return 1;
		}
	}

	struct ring_buffer *rb =
		ring_buffer__new(bpf_map__fd(skel->maps.rb), on_event, NULL, NULL);
	if (!rb) {
		fprintf(stderr, "ringbuf new failed\n");
		return 1;
	}

	signal(SIGINT, sigh);
	signal(SIGTERM, sigh);
	fprintf(stderr, "READY rb_bytes=%u targ=%u path=%d\n", rb_bytes, targ,
		emit_path);
	fflush(stderr);

	while (!stop)
		ring_buffer__poll(rb, 100);

	/* Drain whatever is still queued after the signal. */
	for (int i = 0; i < 20; i++)
		if (ring_buffer__consume(rb) <= 0)
			break;

	unsigned int k = 0;
	struct counters c = { 0, 0, 0 };
	bpf_map__lookup_elem(skel->maps.cnt, &k, sizeof(k), &c, sizeof(c), 0);
	if (out)
		fclose(out);
	printf("RESULT seen=%llu submitted=%llu dropped=%llu delivered=%llu\n",
	       c.seen, c.submitted, c.dropped, delivered);
	fflush(stdout);
	ring_buffer__free(rb);
	fileprobe_bpf__destroy(skel);
	return 0;
}
