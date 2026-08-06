#!/bin/bash
# Survey probe: enumerate every macOS observation mechanism available on a hosted
# macos-15 runner and score each against a known ground-truth workload.
# Deliberately NOT `set -e` — every section must run even when an earlier one fails.

R="${RUNNER_TEMP:-/tmp}/observe-survey"
rm -rf "$R"; mkdir -p "$R"
cd "$R" || exit 1

sect() { echo; echo "########## $* ##########"; }
run()  { echo "\$ $*"; "$@" 2>&1 | sed 's/^/  /'; }

sect "0. ENVIRONMENT"
run sw_vers
run uname -a
run id -un
echo "\$ csrutil status"; csrutil status > csr.txt 2>&1; echo "  EXIT=$?"; sed 's/^/  /' csr.txt
echo "\$ sudo nvram boot-args"; sudo nvram boot-args > ba.txt 2>&1; echo "  EXIT=$?"; sed 's/^/  /' ba.txt
run sysctl -n kern.bootargs
run sysctl -n kern.osversion
run sudo sysctl -n security.mac.amfi.force_policy 2>/dev/null
run df -h /
run mount

sect "1. DTRACE: does it run at all?"
sudo dtrace -n 'BEGIN { trace("hello"); exit(0); }' > dt_smoke.txt 2>&1
echo "EXIT=$?"; sed 's/^/  /' dt_smoke.txt

sect "2. DTRACE: FULL PROVIDER ENUMERATION  <<< THE CENTRAL QUESTION"
sudo dtrace -l > probes.txt 2> probes.err
echo "dtrace -l EXIT=$?"
echo "stderr:"; sed 's/^/  /' probes.err
echo "total probe lines: $(wc -l < probes.txt)"
echo
echo "--- providers present, with probe counts (descending) ---"
awk 'NR>1 {print $2}' probes.txt | sort | uniq -c | sort -rn

echo
echo "--- per-provider explicit -P query (EXIT + probe count) ---"
for P in fsinfo vfs vminfo io sysinfo syscall fbt sdt proc sched profile tick \
         mach_trap pid lockstat dtrace ip tcp udp vfs_ops fsx nfs plockstat \
         mptcp priv unix; do
  out=$(sudo dtrace -l -P "$P" 2>&1)
  rc=$?
  n=$(printf '%s\n' "$out" | grep -c "$P" )
  printf '  %-12s exit=%-3s probes=%-7s %s\n' "$P" "$rc" "$n" \
    "$(printf '%s' "$out" | head -1 | cut -c1-90)"
done

sect "3. DTRACE: VFS-LAYER PROBE HUNT (fbt on VNOP_*/vn_*, sdt vfs probes)"
echo "syscall provider entry probes: $(sudo dtrace -l -P syscall 2>/dev/null | grep -c entry)"
echo "fbt total: $(grep -c ' fbt ' probes.txt)"
echo
for PAT in 'fbt::VNOP_*:entry' 'fbt::vn_*:entry' 'fbt::vnode_*:entry' \
           'fbt::hfs_vnop_*:entry' 'fbt::apfs_vnop_*:entry' 'fbt::nameiat:entry' \
           'fbt::vn_authorize_*:entry' 'fbt::kauth_authorize_action:entry'; do
  out=$(sudo dtrace -l -n "$PAT" 2>&1); rc=$?
  n=$(printf '%s\n' "$out" | grep -cE '^\s*[0-9]+')
  printf '  %-34s exit=%s matched=%s\n' "$PAT" "$rc" "$n"
done
echo
echo "--- first 40 fbt VNOP_ probes ---"
sudo dtrace -l -n 'fbt::VNOP_*:' 2>&1 | head -40
echo
echo "--- sdt probes mentioning vnode/vfs/fs ---"
grep -iE ' sdt ' probes.txt | grep -iE 'vnode|vfs|fsevent|namei' | head -30
echo "count: $(grep -iE ' sdt ' probes.txt | grep -icE 'vnode|vfs|fsevent|namei')"

sect "4. DTRACE: PER-SYSCALL PROBE EXISTENCE (the five reported missing + controls)"
for S in rename renameat renamex_np renameatx_np unlink unlinkat link linkat \
         clonefile clonefileat fclonefileat chmod fchmod fchmodat lchmod \
         utimes utimensat futimens ftruncate truncate fchown chown lchown \
         setattrlist fsetattrlist setattrlistat chflags fchflags \
         setxattr fsetxattr removexattr open openat open_nocancel write pwrite \
         mkdir mkdirat rmdir symlink symlinkat exchangedata mmap copyfile fcntl \
         open_dprotected_np guarded_open_np openbyid_np; do
  n=$(grep -cE "^[[:space:]]*[0-9]+[[:space:]]+syscall[[:space:]]+[^[:space:]]*[[:space:]]+${S}[[:space:]]" probes.txt)
  printf '  %-22s syscall-provider probes=%s\n' "$S" "$n"
done
echo
echo "--- every syscall-provider probe whose name contains 'rename' / 'clone' / 'utim' / 'chmod' ---"
grep -E '^[[:space:]]*[0-9]+[[:space:]]+syscall' probes.txt | grep -E 'rename|clone|utim|chmod|attrlist' | sort -k4

sect "5. BUILD THE GROUND-TRUTH WORKLOAD"
SRC="$GITHUB_WORKSPACE/tests/macos-observe-survey"
run clang -O0 -g -o workload "$SRC/workload.c"
echo "build EXIT=$?"
mkdir -p wl
./workload "$R/wl" > wl_expected.txt 2>&1
echo "dry-run EXIT=$?"
sed 's/^/  /' wl_expected.txt
echo "ground-truth op count: $(grep -c '^OP ' wl_expected.txt)"
rm -rf wl

sect "6. fs_usage DIFFERENTIAL"
mkdir -p wl
sudo fs_usage -w -f filesys workload > fsusage.txt 2>&1 &
FSU=$!
sleep 3
./workload "$R/wl" > wl_run1.txt 2>&1
echo "workload EXIT=$?"
sleep 2
sudo kill "$FSU" 2>/dev/null; wait "$FSU" 2>/dev/null
echo "fs_usage captured lines: $(wc -l < fsusage.txt)"
echo "--- fs_usage output (first 200 lines) ---"
head -200 fsusage.txt
echo "--- fs_usage: which ops appear at all? ---"
for S in rename renameat renamex_np unlink link clonefile chmod fchmod lchmod \
         utimensat futimens ftruncate truncate fchown setattrlist fsetattrlist \
         setattrlistat chflags setxattr mmap WrData PgOut open write; do
  printf '  %-16s %s\n' "$S" "$(grep -c "$S" fsusage.txt)"
done
rm -rf wl

sect "6b. fs_usage RAW-MODE / STRUCTURED OUTPUT?"
run fs_usage -h
echo "--- ktrace tool ---"
run which ktrace
run ktrace help
run ktrace machine

sect "7. DTRACE syscall-provider DIFFERENTIAL (same workload)"
cat > trace.d <<'EOD'
#pragma D option quiet
#pragma D option destructive
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
sudo dtrace -s trace.d -o dtrace_out.txt 2> dtrace_err.txt &
DTP=$!
sleep 5
./workload "$R/wl" > wl_run2.txt 2>&1
echo "workload EXIT=$?"
sleep 2
sudo kill -INT "$DTP" 2>/dev/null; wait "$DTP" 2>/dev/null
echo "--- dtrace stderr (the channel that hid the truncation bug) ---"
sed 's/^/  /' dtrace_err.txt
echo "--- dtrace captured ops, counted ---"
sort dtrace_out.txt | uniq -c | sort -rn
rm -rf wl

sect "8. ENDPOINTSECURITY: SDK PRESENCE"
SDK=$(xcrun --show-sdk-path)
echo "SDK=$SDK"
ESINC="$SDK/usr/include/EndpointSecurity"
run ls -la "$ESINC"
run ls -la /System/Library/Extensions/EndpointSecurity.kext
run ls -la /usr/libexec/endpointsecurityd
echo "--- es_new_client_result_t values in the header ---"
sed -n '/es_new_client_result_t/,/}/p' "$ESINC/ESTypes.h" 2>/dev/null | head -40
echo "--- count of ES_EVENT_TYPE_NOTIFY_* the SDK declares ---"
grep -c 'ES_EVENT_TYPE_NOTIFY_' "$ESINC/ESTypes.h" 2>/dev/null
echo "--- every NOTIFY event the SDK declares ---"
grep -oE 'ES_EVENT_TYPE_NOTIFY_[A-Z_0-9]+' "$ESINC/ESTypes.h" 2>/dev/null | sort -u | tr '\n' ' '
echo
echo "--- ES message version / seq fields (loss accounting) ---"
grep -nE 'seq_num|global_seq_num|version' "$ESINC/ESMessage.h" 2>/dev/null | head -20

sect "9. ENDPOINTSECURITY: es_new_client() UNDER FOUR SIGNING STATES"
clang -O0 -g -o es_probe "$SRC/es_probe.c" -lEndpointSecurity -lbsm 2> es_build.txt
echo "es_probe build EXIT=$?"; sed 's/^/  /' es_build.txt

cat > es.entitlements <<'EOD'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.developer.endpoint-security.client</key><true/>
</dict></plist>
EOD

echo "--- 9a. UNSIGNED (as built), run as root ---"
sudo ./es_probe 0 > es_a.txt 2>&1; echo "EXIT=$?"; sed 's/^/  /' es_a.txt

echo "--- 9b. AD-HOC SIGNED, NO entitlement, run as root ---"
cp es_probe es_probe_b && codesign --force -s - es_probe_b 2>&1 | sed 's/^/  /'
sudo ./es_probe_b 0 > es_b.txt 2>&1; echo "EXIT=$?"; sed 's/^/  /' es_b.txt

echo "--- 9c. AD-HOC SIGNED **WITH** the ES entitlement, run as root  <<< THE VERDICT ---"
cp es_probe es_probe_c && codesign --force -s - --entitlements es.entitlements es_probe_c 2>&1 | sed 's/^/  /'
run codesign -d --entitlements - es_probe_c
sudo ./es_probe_c 0 > es_c.txt 2>&1; echo "EXIT=$?"; sed 's/^/  /' es_c.txt

echo "--- 9d. entitled + also given com.apple.private.tcc.allow (TCC bypass attempt) ---"
cat > es2.entitlements <<'EOD'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.developer.endpoint-security.client</key><true/>
  <key>com.apple.private.tcc.allow</key><array><string>kTCCServiceSystemPolicyAllFiles</string></array>
</dict></plist>
EOD
cp es_probe es_probe_d && codesign --force -s - --entitlements es2.entitlements es_probe_d 2>&1 | sed 's/^/  /'
sudo ./es_probe_d 0 > es_d.txt 2>&1; echo "EXIT=$?"; sed 's/^/  /' es_d.txt

echo "--- 9e. TCC state: is the runner's DB writable / is FDA grantable? ---"
run ls -la /Library/Application\ Support/com.apple.TCC/TCC.db
run sudo sqlite3 /Library/Application\ Support/com.apple.TCC/TCC.db ".tables"
run sudo tccutil reset SystemPolicyAllFiles

sect "10. ENDPOINTSECURITY DIFFERENTIAL (only meaningful if 9c/9d succeeded)"
mkdir -p wl
sudo ./es_probe_d 25 workload > es_events.txt 2>&1 &
ESP=$!
sleep 6
./workload "$R/wl" > wl_run3.txt 2>&1
echo "workload EXIT=$?"
sleep 3
sudo kill "$ESP" 2>/dev/null; wait "$ESP" 2>/dev/null
echo "ES lines captured: $(wc -l < es_events.txt)"
head -200 es_events.txt
echo "--- ES event-type tally ---"
awk '/^ES /{print $2}' es_events.txt | sort | uniq -c | sort -rn
rm -rf wl

sect "11. /dev/fsevents"
run ls -la /dev/fsevents
sudo node -e '
  const fs=require("fs");
  try { const fd=fs.openSync("/dev/fsevents","r"); console.log("open OK fd="+fd); fs.closeSync(fd); }
  catch(e){ console.log("open FAILED: "+e.message); }
' 2>&1 | sed 's/^/  /'
run which fseventsd
run ls -la /System/Library/Frameworks/CoreServices.framework/Frameworks/FSEvents.framework 2>/dev/null

sect "12. kauth / kext / sysext feasibility"
run kmutil showloaded --show all
echo "(head only)"
run systemextensionsctl list
echo "--- kext dev mode ---"
run systemextensionsctl developer on
run sudo spctl --status
run sudo spctl kext-consent status

sect "13. DTRACE LOSS / DROP ACCOUNTING"
cat > drop.d <<'EOD'
#pragma D option quiet
#pragma D option bufsize=4k
#pragma D option bufpolicy=fill
syscall:::entry { printf("%s %d %d %d\n", probefunc, arg0, arg1, arg2); }
EOD
sudo dtrace -s drop.d -o drop_out.txt 2> drop_err.txt &
DP=$!
sleep 4
sudo kill -INT "$DP" 2>/dev/null; wait "$DP" 2>/dev/null
echo "--- dtrace drop stderr (does it report drops, and how?) ---"
sed 's/^/  /' drop_err.txt
echo "captured lines: $(wc -l < drop_out.txt)"
echo "--- dtrace aggregation/dynvar drop options available ---"
sudo dtrace -h -o /dev/null 2>&1 | head -5
dtrace -V

sect "14. DONE"
echo "artifacts in $R"
ls -la "$R"
