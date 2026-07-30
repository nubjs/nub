#!/bin/bash
# Name the kill. Runs 2 and 3 established that `chroot /` succeeds for every signature
# variant while ANY real reroot is SIGKILLed — even a minimal jail, even with SIP off, even
# with dyld and the shared cache in place. Two candidate explanations, and they differ:
#
#   (A) `chroot /` is not a chroot as far as AMFI is concerned. If the check compares the
#       process root vnode against the global rootvnode, `chroot /` leaves them equal, the
#       gate never arms, and the "deciding experiment" in the research doc tests nothing.
#   (B) A real reroot fails for an unrelated reason (dyld cannot map the cache) and AMFI is
#       not involved at all.
#
# SIGKILL produces no crash report, so the kernel log is the only instrument that can tell
# them apart. This job is deliberately tiny so it runs in ~2 minutes.

set -u
export LC_ALL=C
W=/tmp/mj; MIN=/tmp/minjail
sudo rm -rf "$MIN" "$W"; mkdir -p "$W"
run() { echo "\$ $*"; "$@"; local rc=$?; echo "[exit $rc]"; return $rc; }
sub() { printf '\n----- %s -----\n' "$*"; }

sub "test bed"
run sw_vers; echo "SIP: $(csrutil status 2>&1)"

sub "build the minimal jail"
CS=""
for d in /System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld /System/Library/dyld; do
  ls "$d"/dyld_shared_cache_arm64e* >/dev/null 2>&1 && CS="$d" && break
done
sudo mkdir -p "$MIN"/{bin,usr/lib,System/Library/dyld,System/Volumes/Preboot/Cryptexes/OS/System/Library,dev}
sudo cp /bin/echo "$MIN/bin/echo_plain"
sudo cp /bin/echo "$MIN/bin/echo_adhoc"; sudo codesign -f -s - "$MIN/bin/echo_adhoc" 2>&1
sudo cp -p /usr/lib/dyld "$MIN/usr/lib/dyld"
sudo cp -p "$CS"/dyld_shared_cache_arm64e* "$MIN/System/Library/dyld/"
sudo ln -sfn /System/Library/dyld "$MIN/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld"
# Also place the cache at the Cryptexes path as REAL files, in case dyld refuses to follow a
# symlink for the cache — distinguishes "cache not found" from "AMFI killed it".
sudo mkdir -p "$MIN/cryptexcopy" && sudo cp -p "$CS"/dyld_shared_cache_arm64e "$MIN/cryptexcopy/" 2>/dev/null
sudo mount -t devfs devfs "$MIN/dev" 2>&1
run sudo du -sh "$MIN"

MARK=$(date -u +%Y-%m-%d\ %H:%M:%S)
sub "the four cells"
run sudo chroot /    "/bin/echo" chroot-slash-apple
run sudo chroot /    "$MIN/bin/echo_adhoc" chroot-slash-adhoc
run sudo chroot "$MIN" /bin/echo_plain real-reroot-plain
run sudo chroot "$MIN" /bin/echo_adhoc real-reroot-adhoc

sub "AMFI kernel log since $MARK — the instrument that names the kill"
sudo log show --start "$MARK" --style compact \
  --predicate 'senderImagePath CONTAINS "AppleMobileFileIntegrity" OR eventMessage CONTAINS "chroot" OR eventMessage CONTAINS "not allowed" OR eventMessage CONTAINS "killing pid"' 2>&1 \
  | grep -viE 'log run noninteractively|use_watchroot' | tail -40

sub "dmesg"
sudo dmesg 2>/dev/null | grep -iE 'amfi|chroot|kill|signature' | tail -20

sub "does dyld report a cache problem when it is the CHILD that runs?"
# DYLD_PRINT_* on the chroot command itself describes /usr/sbin/chroot, NOT the jailed child
# — a trap that made runs 2 and 3 look like dyld was succeeding inside the jail. Pass the
# variable through so it applies to the child.
run sudo chroot "$MIN" /usr/bin/env DYLD_PRINT_LIBRARIES=1 /bin/echo_adhoc x 2>&1 | tail -10
run sudo env DYLD_PRINT_LIBRARIES=1 chroot "$MIN" /bin/echo_adhoc x 2>&1 | tail -10
exit 0
