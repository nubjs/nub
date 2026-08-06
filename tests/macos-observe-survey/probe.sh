#!/bin/bash
# Survey probe: enumerate every macOS observation mechanism available on a hosted
# macOS runner and score each against a known ground-truth workload.
#
# Deliberately NOT `set -e` — every section must run even when an earlier one fails.
# EVERY command gets </dev/null: v1 hung for 25 minutes because at least one tool
# (dtrace -h with no input, ktrace/kmutil pagers) blocked reading stdin, and a hung
# section costs a whole 30-minute runner slot.

R="${RUNNER_TEMP:-/tmp}/observe-survey"
rm -rf "$R"; mkdir -p "$R"
cd "$R" || exit 1
SRC="${GITHUB_WORKSPACE:-$(pwd)}/tests/macos-observe-survey"

sect() { echo; echo "########## $* ##########  [t+$SECONDS s]"; }
run()  { echo "\$ $*"; "$@" </dev/null 2>&1 | sed 's/^/  /'; }

sect "0. ENVIRONMENT"
run sw_vers
run uname -a
run id -un
run csrutil status
run nvram boot-args
run sysctl -n kern.bootargs
run sysctl -n kern.osversion
run sysctl -n hw.optional.arm64
run df -h /

sect "1. DTRACE smoke: does it run at all?"
sudo dtrace -n 'BEGIN { trace("hello"); exit(0); }' </dev/null > dt_smoke.txt 2>&1
echo "EXIT=$?"; sed 's/^/  /' dt_smoke.txt

sect "2. DTRACE: FULL PROVIDER ENUMERATION  <<< THE CENTRAL QUESTION"
sudo dtrace -l </dev/null > probes.txt 2> probes.err
echo "dtrace -l EXIT=$?  [t+$SECONDS s]"
echo "stderr:"; sed 's/^/  /' probes.err
echo "total probe lines: $(wc -l < probes.txt)"
echo
echo "--- EVERY provider present, with probe counts (descending) ---"
awk 'NR>1 {print $2}' probes.txt | sort | uniq -c | sort -rn
echo
echo "--- explicit membership test for the providers this survey turns on ---"
for P in fsinfo vfs vminfo io sysinfo syscall fbt sdt proc sched profile tick \
         mach_trap pid lockstat lockprof dtrace ip tcp udp mptcp nfsv3 nfsv4 \
         sysevent boost route sandbox fpuinfo vtrace mib; do
  n=$(awk -v p="$P" 'NR>1 && $2==p {c++} END{print c+0}' probes.txt)
  printf '  %-12s probes_in_dtrace_l=%s\n' "$P" "$n"
done

sect "2b. DTRACE: -P query for the four that decide the recommendation"
for P in fsinfo io vminfo vfs; do
  echo "\$ sudo dtrace -l -P $P"
  sudo dtrace -l -P "$P" </dev/null > "p_$P.txt" 2>&1
  echo "  EXIT=$?  lines=$(wc -l < "p_$P.txt")"
  head -25 "p_$P.txt" | sed 's/^/  /'
done

sect "3. fsinfo PROBE INVENTORY (if present) + VFS-layer fbt hunt"
echo "--- every fsinfo probe name ---"
awk 'NR>1 && $2=="fsinfo" {print $5}' probes.txt | sort | tr '\n' ' '
echo
echo "count: $(awk 'NR>1 && $2=="fsinfo"' probes.txt | wc -l)"
echo
echo "--- fbt probes on the VFS dispatch layer ---"
for PAT in 'VNOP_RENAME' 'VNOP_RENAMEX' 'VNOP_LINK' 'VNOP_CLONEFILE' 'VNOP_CREATE' \
           'VNOP_REMOVE' 'VNOP_SETATTR' 'VNOP_WRITE' 'VNOP_MMAP' 'VNOP_PAGEOUT' \
           'vn_rename' 'namei' 'vn_authorize_rename'; do
  n=$(awk -v f="$PAT" 'NR>1 && $2=="fbt" && $4==f {c++} END{print c+0}' probes.txt)
  printf '  fbt::%-22s probes=%s\n' "$PAT" "$n"
done
echo "total fbt VNOP_* probes: $(awk 'NR>1 && $2=="fbt" && $4 ~ /^VNOP_/' probes.txt | wc -l)"
echo "--- sample ---"
awk 'NR>1 && $2=="fbt" && $4 ~ /^VNOP_/ {print "  "$2"::"$4":"$5}' probes.txt | head -25

sect "4. DTRACE syscall-provider probe existence (the five reported missing + controls)"
for S in rename renameat renamex_np renameatx_np unlink unlinkat link linkat \
         clonefile clonefileat fclonefileat chmod fchmod fchmodat lchmod \
         utimes utimensat futimens ftruncate truncate fchown chown lchown \
         setattrlist fsetattrlist setattrlistat chflags fchflags \
         setxattr fsetxattr exchangedata copyfile mkdir mkdirat rmdir \
         symlink symlinkat open openat write mmap; do
  n=$(awk -v f="$S" 'NR>1 && $2=="syscall" && $4==f {c++} END{print c+0}' probes.txt)
  printf '  %-16s syscall-provider probes=%s%s\n' "$S" "$n" \
    "$([ "$n" = 0 ] && echo '   <<< NO PROBE')"
done
echo "total syscall::: probes: $(awk 'NR>1 && $2=="syscall"' probes.txt | wc -l)"

sect "5. GROUND-TRUTH WORKLOAD (build + dry run)"
clang -O0 -g -o workload "$SRC/workload.c" </dev/null 2>&1 | sed 's/^/  /'
echo "build EXIT=$?"
mkdir -p wl; ./workload "$R/wl" </dev/null > wl_expected.txt 2>&1
echo "dry-run EXIT=$?  ops=$(grep -c '^OP ' wl_expected.txt)"
grep -c 'ERRNO' wl_expected.txt | sed 's/^/  ops that FAILED: /'
rm -rf wl

sect "6. fs_usage DIFFERENTIAL"
mkdir -p wl
sudo fs_usage -w -f filesys workload </dev/null > fsusage.txt 2>&1 &
FSU=$!
command sleep 4
./workload "$R/wl" </dev/null > /dev/null 2>&1
echo "workload EXIT=$?"
command sleep 2
sudo pkill -INT -f 'fs_usage -w' 2>/dev/null; kill "$FSU" 2>/dev/null
command sleep 1
echo "fs_usage lines: $(wc -l < fsusage.txt)"
echo "--- which of our 36 ops appear in fs_usage output at all? ---"
for S in open write fchmod chmod lchmod fchmodat rename renameat renamex_np \
         link linkat symlink clonefile clonefileat unlink unlinkat mkdir mkdirat \
         rmdir truncate ftruncate fchown utimes utimensat futimens \
         setattrlist fsetattrlist setattrlistat chflags setxattr mmap WrData PgOut; do
  printf '  %-16s %s\n' "$S" "$(grep -cw "$S" fsusage.txt)"
done
echo "--- first 120 lines verbatim ---"
head -120 fsusage.txt
rm -rf wl

sect "7. DTRACE syscall-provider DIFFERENTIAL (same workload)"
cat > trace.d <<'EOD'
#pragma D option quiet
#pragma D option bufsize=64m
#pragma D option switchrate=10hz
syscall::rename*:entry, syscall::unlink*:entry, syscall::link*:entry,
syscall::clonefile*:entry, syscall::*chmod*:entry, syscall::*truncate:entry,
syscall::*chown:entry, syscall::*attrlist*:entry, syscall::*utime*:entry,
syscall::*xattr:entry, syscall::mmap:entry, syscall::open*:entry,
syscall::write:entry, syscall::mkdir*:entry, syscall::rmdir:entry,
syscall::chflags:entry, syscall::symlink*:entry
/execname == "workload"/
{ printf("DT %s\n", probefunc); }
dtrace:::ERROR { printf("DT_ERROR\n"); }
EOD
mkdir -p wl
sudo dtrace -s trace.d -o dtrace_out.txt </dev/null 2> dtrace_err.txt &
DTP=$!
command sleep 6
./workload "$R/wl" </dev/null > /dev/null 2>&1
echo "workload EXIT=$?"
command sleep 2
sudo pkill -INT -f 'dtrace -s trace.d' 2>/dev/null; kill "$DTP" 2>/dev/null
command sleep 2
echo "--- dtrace stderr (the channel that hid the pointer-truncation bug) ---"
sed 's/^/  /' dtrace_err.txt
echo "--- captured ops ---"
sort dtrace_out.txt | uniq -c | sort -rn
rm -rf wl

sect "7b. fsinfo DIFFERENTIAL (only meaningful if section 2 found fsinfo probes)"
cat > fsi.d <<'EOD'
#pragma D option quiet
fsinfo:::
/execname == "workload"/
{ printf("FSI %s name=%s dir=%s path=%s\n", probename,
         args[0]->fi_name, args[0]->fi_dirname, args[0]->fi_pathname); }
dtrace:::ERROR { printf("FSI_ERROR\n"); }
EOD
mkdir -p wl
sudo dtrace -s fsi.d -o fsinfo_out.txt </dev/null 2> fsinfo_err.txt &
FIP=$!
command sleep 6
./workload "$R/wl" </dev/null > /dev/null 2>&1
command sleep 2
sudo pkill -INT -f 'dtrace -s fsi.d' 2>/dev/null; kill "$FIP" 2>/dev/null
command sleep 2
echo "--- fsinfo stderr ---"; sed 's/^/  /' fsinfo_err.txt
echo "--- fsinfo captured ($(wc -l < fsinfo_out.txt) lines) ---"
head -80 fsinfo_out.txt
echo "--- fsinfo probe tally ---"
awk '/^FSI /{print $2}' fsinfo_out.txt | sort | uniq -c | sort -rn
rm -rf wl

sect "8. ENDPOINTSECURITY: SDK SURFACE"
SDKP=$(xcrun --show-sdk-path </dev/null 2>/dev/null)
ESINC="$SDKP/usr/include/EndpointSecurity"
echo "SDK=$SDKP"
run ls "$ESINC"
run ls -la /System/Library/Extensions/EndpointSecurity.kext
echo "--- NOTIFY event count: $(grep -oE 'ES_EVENT_TYPE_NOTIFY_[A-Z_0-9]+' "$ESINC/ESTypes.h" 2>/dev/null | sort -u | wc -l) ---"

sect "9. ENDPOINTSECURITY: es_new_client() UNDER FOUR SIGNING STATES  <<< THE VERDICT"
clang -O0 -g -o es_probe "$SRC/es_probe.c" -lEndpointSecurity -lbsm </dev/null 2> es_build.txt
echo "es_probe build EXIT=$?"; sed 's/^/  /' es_build.txt

cat > es.entitlements <<'EOD'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.developer.endpoint-security.client</key><true/>
</dict></plist>
EOD
cat > es2.entitlements <<'EOD'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.developer.endpoint-security.client</key><true/>
  <key>com.apple.private.tcc.allow</key><array><string>kTCCServiceSystemPolicyAllFiles</string></array>
</dict></plist>
EOD

echo "--- 9a. UNSIGNED as built, run as root ---"
sudo ./es_probe 0 </dev/null 2>&1 | sed 's/^/  /'
echo "--- 9b. AD-HOC SIGNED, no entitlement, root ---"
cp es_probe es_b && codesign --force -s - es_b </dev/null 2>&1 | sed 's/^/  /'
sudo ./es_b 0 </dev/null 2>&1 | sed 's/^/  /'
echo "--- 9c. AD-HOC SIGNED WITH the ES entitlement, root ---"
cp es_probe es_c && codesign --force -s - --entitlements es.entitlements es_c </dev/null 2>&1 | sed 's/^/  /'
run codesign -d --entitlements - es_c
sudo ./es_c 0 </dev/null 2>&1 | sed 's/^/  /'
echo "--- 9d. entitled + private TCC allow, root ---"
cp es_probe es_d && codesign --force -s - --entitlements es2.entitlements es_d </dev/null 2>&1 | sed 's/^/  /'
sudo ./es_d 0 </dev/null 2>&1 | sed 's/^/  /'
echo "--- 9e. entitled, run as NON-root (isolates the root requirement) ---"
./es_c 0 </dev/null 2>&1 | sed 's/^/  /'

sect "10. ENDPOINTSECURITY DIFFERENTIAL (skipped unless 9c/9d succeeded)"
mkdir -p wl
sudo ./es_d 22 workload </dev/null > es_events.txt 2>&1 &
ESP=$!
command sleep 6
./workload "$R/wl" </dev/null > /dev/null 2>&1
command sleep 3
sudo pkill -f 'es_d 22' 2>/dev/null; kill "$ESP" 2>/dev/null
command sleep 1
echo "ES lines: $(wc -l < es_events.txt)"
head -150 es_events.txt
echo "--- ES event tally ---"
awk '/^ES /{print $2}' es_events.txt | sort | uniq -c | sort -rn
rm -rf wl

sect "11. /dev/fsevents"
run ls -la /dev/fsevents
sudo node -e 'const fs=require("fs");try{const fd=fs.openSync("/dev/fsevents","r");console.log("open OK fd="+fd);fs.closeSync(fd);}catch(e){console.log("open FAILED: "+e.message);}' </dev/null 2>&1 | sed 's/^/  /'

sect "12. kdebug / ktrace availability"
run which ktrace
run ktrace --help
echo "--- can a plain root process configure kdebug? ---"
sudo ktrace trace -S -f C0x01 -c /usr/bin/true </dev/null > kt.txt 2>&1
echo "  ktrace trace EXIT=$?"; head -20 kt.txt | sed 's/^/  /'

sect "13. DTRACE DROP ACCOUNTING (deliberately starved buffer)"
cat > drop.d <<'EOD'
#pragma D option quiet
#pragma D option bufsize=4k
#pragma D option bufpolicy=fill
syscall:::entry { printf("%s %d %d %d\n", probefunc, arg0, arg1, arg2); }
EOD
sudo dtrace -s drop.d -o drop_out.txt </dev/null 2> drop_err.txt &
DP=$!
command sleep 5
sudo pkill -INT -f 'dtrace -s drop.d' 2>/dev/null; kill "$DP" 2>/dev/null
command sleep 2
echo "--- drop stderr: does dtrace REPORT drops, and how? ---"
sed 's/^/  /' drop_err.txt
echo "captured lines: $(wc -l < drop_out.txt)"
run dtrace -V

sect "14. DONE"
echo "elapsed ${SECONDS}s"
ls -la "$R"
