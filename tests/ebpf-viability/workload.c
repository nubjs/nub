/* N open/close pairs on one file. Prints elapsed ms on stderr. */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	long n = atol(argv[1]);
	const char *p = argv[2];
	struct timespec a, b;
	clock_gettime(CLOCK_MONOTONIC, &a);
	for (long i = 0; i < n; i++) {
		int fd = open(p, O_RDONLY);
		if (fd < 0) {
			perror("open");
			return 1;
		}
		close(fd);
	}
	clock_gettime(CLOCK_MONOTONIC, &b);
	double ms = (b.tv_sec - a.tv_sec) * 1000.0 +
		    (b.tv_nsec - a.tv_nsec) / 1e6;
	fprintf(stderr, "WORKLOAD n=%ld elapsed_ms=%.1f\n", n, ms);
	return 0;
}
