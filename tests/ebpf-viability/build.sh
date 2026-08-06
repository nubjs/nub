#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
BPFTOOL=${BPFTOOL:-$(command -v bpftool || ls /usr/lib/linux-tools-*/bpftool 2>/dev/null | head -1)}
[ -n "$BPFTOOL" ] || { echo "no bpftool"; exit 1; }
[ -s vmlinux.h ] || $BPFTOOL btf dump file /sys/kernel/btf/vmlinux format c > vmlinux.h
head -3 vmlinux.h | grep -q "__VMLINUX_H__" || { echo "BAD vmlinux.h:"; head -5 vmlinux.h; exit 1; }
ARCH=$(uname -m | sed 's/aarch64/arm64/;s/x86_64/x86/')
clang -g -O2 -target bpf -D__TARGET_ARCH_${ARCH} -I. -c fileprobe.bpf.c -o fileprobe.bpf.o
$BPFTOOL gen skeleton fileprobe.bpf.o > fileprobe.skel.h
clang -g -O2 -I. fileprobe.c -o fileprobe -lbpf -lelf -lz
clang -O2 workload.c -o workload
clang -O0 fixture.c -o fixture
echo "BUILD OK"
