#!/bin/bash
# Prints a path to a bpftool that ACTUALLY WORKS on this kernel.
# Ubuntu ships /usr/sbin/bpftool as a version-matching WRAPPER that prints a
# "bpftool not found for kernel <ver>" advisory to stdout and does nothing.
# Selecting it silently produced an empty BTF symbol list that read as "no
# hooks present" — so each candidate is validated by making it do the real job.
for c in /usr/lib/linux-tools-*/bpftool /usr/sbin/bpftool /usr/bin/bpftool \
         $(command -v bpftool 2>/dev/null); do
	[ -x "$c" ] || continue
	n=$("$c" btf dump file /sys/kernel/btf/vmlinux format raw 2>/dev/null | wc -l)
	[ "$n" -gt 1000 ] && { echo "$c"; exit 0; }
done
echo "NO-WORKING-BPFTOOL" >&2
exit 1
