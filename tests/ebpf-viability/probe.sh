#!/bin/bash
# Answers the deployment-gating environment questions on a real GitHub Actions
# ubuntu-24.04 runner, then re-runs the completeness / honesty / overhead /
# coverage measurements there. Every capability question is answered by
# ATTEMPTING the operation, never by inferring it from a kernel version.
set -uo pipefail
cd "$(dirname "$0")"
T=/dev/shm/target

hr() { echo; echo "=============== $* ==============="; }

hr "ENVIRONMENT"
uname -a
echo "PAGESIZE=$(getconf PAGESIZE)  NPROC=$(nproc)"
grep -E '^(NAME|VERSION)=' /etc/os-release
echo "workspace fs: $(stat -f -c %T .)   /dev/shm fs: $(stat -f -c %T /dev/shm)"

hr "Q1 BTF present?"
ls -la /sys/kernel/btf/vmlinux 2>&1 || echo "ABSENT"

hr "Q2 which hooks are in BTF? (with a negative control)"
BPFTOOL=${BPFTOOL:-$(sudo bash ./pick-bpftool.sh)}
echo "bpftool: $BPFTOOL"
sudo "$BPFTOOL" btf dump file /sys/kernel/btf/vmlinux format raw 2>/dev/null \
  | grep -oE "'(security_[a-z_0-9]+|vfs_[a-z_0-9]+|wake_up_new_task)'" | tr -d "'" | sort -u > /tmp/btf-syms.txt
echo "security_*/vfs_* symbols in BTF: $(wc -l < /tmp/btf-syms.txt)"
# POSITIVE CONTROL on the ENUMERATION itself. Without this a broken bpftool
# yields an empty list, every row reads "absent", and that looks like a real
# finding of "no hooks on this kernel". It already happened once.
if [ "$(wc -l < /tmp/btf-syms.txt)" -lt 50 ]; then
  echo "!! BTF ENUMERATION IS BROKEN (found <50 security_*/vfs_* symbols)."
  echo "!! Every 'absent' below is an artefact of the instrument. Do not read them."
fi
echo "security_path_* in BTF: $(grep -c '^security_path_' /tmp/btf-syms.txt)"
for s in security_file_open security_inode_rename security_inode_link \
         security_inode_unlink security_inode_create security_inode_setattr \
         security_mmap_file security_inode_mkdir security_inode_symlink \
         wake_up_new_task security_bprm_committed_creds \
         security_path_rename security_path_link security_path_unlink \
         security_path_mkdir security_path_symlink security_path_chmod \
         security_path_truncate security_path_notify \
         nub_this_symbol_does_not_exist; do
  grep -qx "$s" /tmp/btf-syms.txt && echo "  PRESENT  $s" || echo "  absent   $s"
done
echo "(nub_this_symbol_does_not_exist is the negative control; it MUST read absent)"

hr "Q3 LSM / sysctls / feature probe"
cat /sys/kernel/security/lsm 2>&1 || echo "no /sys/kernel/security/lsm"
echo "unprivileged_bpf_disabled=$(cat /proc/sys/kernel/unprivileged_bpf_disabled 2>&1)"
sudo "$BPFTOOL" feature probe 2>/dev/null | grep -iE "program_type (tracing|lsm) is|map_type ringbuf is" || echo "(feature probe gave nothing)"

hr "Q4 BUILD"
BPFTOOL=$BPFTOOL bash ./build.sh || { echo "BUILD FAILED"; exit 1; }

hr "Q5 LOAD + ATTACH  (the real answer to 'is BPF_PROG_TYPE_TRACING loadable here')"
: > $T
sudo bash ./rununder.sh --rb-bytes 8388608 -- ./workload 10 $T 2>&1 | grep -E 'WORKLOAD|RESULT|FAILED|NEVER'

hr "Q6 COMPLETENESS — delivered vs the in-kernel oracle, 5 runs, 8MB ring"
for i in 1 2 3 4 5; do
  sudo bash ./rununder.sh --rb-bytes 8388608 --out /tmp/ev.txt -- ./workload 300000 $T 2>&1 | grep -E 'WORKLOAD|RESULT'
  echo "   independent check: lines written by consumer = $(sudo wc -l < /tmp/ev.txt)"
done

hr "Q7 HONESTY — starvation MUST make the drop counter fire"
echo "-- 4KB ring, N=300000 (expect large dropped=, and delivered+dropped == seen)"
sudo bash ./rununder.sh --rb-bytes 4096 -- ./workload 300000 $T 2>&1 | grep -E 'RESULT'
echo "-- stalled consumer, 8MB ring, N=20000"
sudo bash ./rununder.sh --rb-bytes 8388608 --stall-ms 5 -- ./workload 20000 $T 2>&1 | grep -E 'RESULT'
echo "-- POSITIVE CONTROL: 8MB ring, no stall, N=20000 (expect dropped=0)"
sudo bash ./rununder.sh --rb-bytes 8388608 -- ./workload 20000 $T 2>&1 | grep -E 'RESULT'

hr "Q8 OVERLAYFS DOUBLING — controlled (GHA container jobs are overlayfs)"
sudo mkdir -p /tmp/ovl/{lower,upper,work,merged}
if sudo mount -t overlay overlay -olowerdir=/tmp/ovl/lower,upperdir=/tmp/ovl/upper,workdir=/tmp/ovl/work /tmp/ovl/merged; then
  sudo touch /tmp/ovl/merged/target
  echo "-- 1000 opens on OVERLAYFS"; sudo bash ./rununder.sh --rb-bytes 8388608 -- ./workload 1000 /tmp/ovl/merged/target 2>&1 | grep RESULT
  echo "-- 1000 opens on TMPFS (control)"; sudo bash ./rununder.sh --rb-bytes 8388608 -- ./workload 1000 $T 2>&1 | grep RESULT
  sudo umount /tmp/ovl/merged
else
  echo "overlay mount refused"
fi

hr "Q9 OVERHEAD on this runner (wall clock measured inside the workload)"
echo "-- baseline (no tracer)"; for i in 1 2 3; do ./workload 200000 $T 2>&1; done
echo "-- ebpf ringbuf, paths on, consumer persists every event"
for i in 1 2 3; do sudo bash ./rununder.sh --rb-bytes 8388608 --out /tmp/ev.txt -- ./workload 200000 $T 2>&1 | grep -E 'WORKLOAD|RESULT'; done
echo "-- strace -f --seccomp-bpf -e trace=%file"
for i in 1 2; do strace -f --seccomp-bpf -e trace=%file -o /tmp/st.txt ./workload 200000 $T 2>&1; done
echo "   strace lines=$(wc -l < /tmp/st.txt)"
echo "-- strace -f -e trace=%file"
for i in 1 2; do strace -f -e trace=%file -o /tmp/st.txt ./workload 200000 $T 2>&1; done
echo "   strace lines=$(wc -l < /tmp/st.txt)"

hr "Q10 COVERAGE — known-answer fixture, eBPF VFS layer vs strace syscall classes"
rm -rf /tmp/fx-ebpf /tmp/fx-strace
sudo bash ./rununder.sh --rb-bytes 8388608 --out /tmp/cov-ebpf.txt -- ./fixture /tmp/fx-ebpf 2>&1 | grep -E 'FIXTURE|RESULT'
strace -f -y -e trace=%file,%desc,%network,%process -o /tmp/cov-strace.txt ./fixture /tmp/fx-strace 2>&1 | grep FIXTURE
echo "  ebpf events=$(sudo wc -l < /tmp/cov-ebpf.txt)  strace lines=$(wc -l < /tmp/cov-strace.txt)"
sudo chmod 644 /tmp/cov-ebpf.txt
python3 ./coverage-score.py "eBPF-VFS=/tmp/cov-ebpf.txt" "strace-classes=/tmp/cov-strace.txt"
echo
echo "-- renameat2 vs renameat: which syscall did glibc actually issue here?"
grep -cE '\brenameat2\(' /tmp/cov-strace.txt | sed 's/^/   renameat2 lines: /'
grep -cE '\brenameat\(' /tmp/cov-strace.txt  | sed 's/^/   renameat  lines: /'
grep -cE '^\s*rename\(' /tmp/cov-strace.txt  | sed 's/^/   rename    lines: /'
echo "   eBPF RENAME events: $(grep -c '^RENAME' /tmp/cov-ebpf.txt)"

hr "DONE"
