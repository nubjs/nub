#!/bin/bash
# FOCUSED survey probe. The broad v1 run already settled provider enumeration, per-syscall
# probe existence, and the fs_usage differential on macos-15/arm64 (run 31120085947).
# What is left is exactly three questions, so this script asks only those and finishes fast.
#
#   §A  the fsinfo probe INVENTORY  — which VFS operations the provider actually exposes
#   §B  the fsinfo DIFFERENTIAL     — what fsinfo reports for our 36-op ground truth
#   §C  the ES ENTITLEMENT VERDICT  — es_new_client() under four signing states
#
# HARD RULES learned from v1/v2, both of which hung past their own `timeout-minutes`:
#   * every command gets </dev/null (a tool blocking on stdin costs a whole runner slot)
#   * every background process redirects stdout AND stderr to a FILE, never the step's pipe
#     (bash exits but the Actions step waits for the inherited fd to close)
#   * no `wait` on a `sudo` child — SIGINT to sudo does not reliably reap the real process
#   * each dtrace capture is bounded by `#pragma D option ... ` + an explicit `timeout` probe

R="${RUNNER_TEMP:-/tmp}/observe-survey"
rm -rf "$R"; mkdir -p "$R"; cd "$R" || exit 1
SRC="${GITHUB_WORKSPACE:-$(pwd)}/tests/macos-observe-survey"

sect() { echo; echo "########## $* ##########  [t+$SECONDS s]"; }

sect "0. ENVIRONMENT"
{ sw_vers; uname -m; csrutil status; sudo nvram boot-args; sudo sysctl -n security.mac.amfi.force_policy; } </dev/null 2>&1 | sed 's/^/  /'

sect "A. fsinfo PROBE INVENTORY"
sudo dtrace -l -P fsinfo </dev/null > fsinfo_l.txt 2>&1
echo "EXIT=$?  lines=$(wc -l < fsinfo_l.txt)"
echo "--- probe names ---"
awk 'NR>1 {print $5}' fsinfo_l.txt | sort -u | tr '\n' ' '; echo
echo "--- verbatim (first 100) ---"
head -100 fsinfo_l.txt
echo "--- argument type of args[0], per dtrace itself ---"
sudo dtrace -lv -n 'fsinfo:::rename' </dev/null 2>&1 | head -30

sect "B. BUILD GROUND TRUTH"
clang -O0 -g -o workload "$SRC/workload.c" </dev/null 2>&1 | sed 's/^/  /'
echo "build EXIT=$?"

sect "B1. fsinfo DIFFERENTIAL — what does the VFS layer report for 36 known ops?"
# `timeout` probe bounds the run without any signalling, so nothing can outlive the script.
cat > fsi.d <<'EOD'
#pragma D option quiet
#pragma D option bufsize=32m
fsinfo:::
/execname == "workload"/
{ printf("FSI %s | name=%s | dir=%s | path=%s\n", probename,
         args[0]->fi_name, args[0]->fi_dirname, args[0]->fi_pathname); }
dtrace:::ERROR { printf("FSI_DTRACE_ERROR\n"); }
profile:::tick-25sec { exit(0); }
EOD
mkdir -p wl
sudo dtrace -s fsi.d -o fsinfo_out.txt </dev/null > fsinfo_stdout.txt 2> fsinfo_err.txt &
command sleep 8
./workload "$R/wl" </dev/null > wl_run.txt 2>&1
echo "workload EXIT=$?  ops=$(grep -c '^OP ' wl_run.txt)"
command sleep 20
echo "--- dtrace stderr ---"; sed 's/^/  /' fsinfo_err.txt
echo "--- fsinfo captured: $(wc -l < fsinfo_out.txt) lines ---"
head -120 fsinfo_out.txt
echo "--- fsinfo probe tally ---"
awk '/^FSI /{print $2}' fsinfo_out.txt | sort | uniq -c | sort -rn
echo "--- did the RENAME probe carry a usable destination? (expect: source DIR only) ---"
grep -E '^FSI (rename|renamex|clonefile|link|create|remove) ' fsinfo_out.txt | head -20
rm -rf wl

sect "C. ENDPOINTSECURITY — es_new_client() UNDER FOUR SIGNING STATES  <<< THE VERDICT"
clang -O0 -g -o es_probe "$SRC/es_probe.c" -lEndpointSecurity -lbsm </dev/null > es_build.txt 2>&1
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

echo "--- C1. UNSIGNED as built, run as ROOT ---"
sudo ./es_probe 0 </dev/null 2>&1 | sed 's/^/  /'
echo "--- C2. AD-HOC SIGNED, NO entitlement, ROOT ---"
cp es_probe es_b && codesign --force -s - es_b </dev/null 2>&1 | sed 's/^/  /'
sudo ./es_b 0 </dev/null 2>&1 | sed 's/^/  /'
echo "--- C3. AD-HOC SIGNED WITH the ES entitlement, ROOT   <<< the contested case ---"
cp es_probe es_c && codesign --force -s - --entitlements es.entitlements es_c </dev/null 2>&1 | sed 's/^/  /'
codesign -d --entitlements - es_c </dev/null 2>&1 | sed 's/^/  /'
sudo ./es_c 0 </dev/null 2>&1 | sed 's/^/  /'
echo "--- C4. entitled + com.apple.private.tcc.allow, ROOT ---"
cp es_probe es_d && codesign --force -s - --entitlements es2.entitlements es_d </dev/null 2>&1 | sed 's/^/  /'
sudo ./es_d 0 </dev/null 2>&1 | sed 's/^/  /'
echo "--- C5. entitled, NON-root (isolates the root requirement) ---"
./es_c 0 </dev/null 2>&1 | sed 's/^/  /'

sect "D. ES DIFFERENTIAL (runs only if some C variant returned SUCCESS)"
BEST=""
for c in es_d es_c es_b es_probe; do
  if sudo ./"$c" 0 </dev/null 2>&1 | grep -q 'ES_NEW_CLIENT_RESULT=0'; then BEST="$c"; break; fi
done
echo "usable ES binary: ${BEST:-NONE}"
if [ -n "$BEST" ]; then
  mkdir -p wl
  sudo ./"$BEST" 20 workload </dev/null > es_events.txt 2>&1 &
  command sleep 6
  ./workload "$R/wl" </dev/null > /dev/null 2>&1
  echo "workload EXIT=$?"
  command sleep 18
  echo "ES lines: $(wc -l < es_events.txt)"
  head -160 es_events.txt
  echo "--- ES event tally ---"
  awk '/^ES /{print $2}' es_events.txt | sort | uniq -c | sort -rn
  rm -rf wl
else
  echo "SKIPPED — no signing state produced a usable client on this host."
fi

sect "E. kdebug/ktrace as a STRUCTURED consumption point"
sudo ktrace trace --ndjson -T 3 -f 'C0x01' -c /usr/bin/true </dev/null > kt.txt 2>&1
echo "ktrace trace EXIT=$?  lines=$(wc -l < kt.txt)"
head -12 kt.txt | sed 's/^/  /'

sect "F. DONE  elapsed ${SECONDS}s"
