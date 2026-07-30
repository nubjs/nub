#!/bin/bash
# Does macOS permit a genuinely rerooted "fresh disk" (chroot) with SIP ENABLED?
#
# The gate under test is AMFI's execve hook, not chroot(2) itself: XNU's chroot needs only
# suser(), but AMFI carries the string "hardened runtime not allowed in chroot" plus an
# undocumented escape entitlement com.apple.security.cs.allow-in-chroot. Apple platform
# binaries run with CS_RUNTIME *implied* (codesign reports flags=0x0 on disk), so they land
# in AMFI's kill set. A second, independent wall — launch constraints — SIGKILLs a plain
# copy of a system binary even outside any chroot. `codesign -f -s -` is hypothesized to
# defeat both at once by stripping the Apple identity and CS_RUNTIME together.
#
# Sections 0-2 answer the yes/no with a full differential. Sections 3+ only run if 2 passes.
# Nothing here fails the job: every stage records its verdict and the summary is the output.

set -u
export LC_ALL=C

JAIL=/tmp/mjail
WORK=/tmp/mprobe
mkdir -p "$WORK"

# Verdict ledger — appended by note(), dumped at the end. The summary IS the deliverable.
LEDGER="$WORK/ledger.txt"
: > "$LEDGER"
note() { printf '%s\n' "$*" >> "$LEDGER"; }

hdr() { printf '\n\n========== %s ==========\n' "$*"; }
sub() { printf '\n----- %s -----\n' "$*"; }

# Runs a command, prints its exit code verbatim, returns that code. Never aborts the script.
run() { echo "\$ $*"; "$@"; local rc=$?; echo "[exit $rc]"; return $rc; }

hdr "0. ENVIRONMENT — is this a valid test bed?"

sub "0.1 macOS version / hardware"
run sw_vers
run uname -a
run sysctl -n machdep.cpu.brand_string
run sysctl -n kern.osversion

sub "0.2 SIP — a SIP-OFF runner would invalidate this entire probe"
run csrutil status
SIP_RAW="$(csrutil status 2>&1)"
run csrutil authenticated-root status
SSV_RAW="$(csrutil authenticated-root status 2>&1)"
case "$SIP_RAW" in
  *"enabled"*) note "SIP: ENABLED  ($SIP_RAW)" ;;
  *)           note "SIP: *** NOT CLEANLY ENABLED *** ($SIP_RAW) — RESULTS SUSPECT" ;;
esac
note "SSV(authenticated-root): $SSV_RAW"

sub "0.3 AMFI boot-args (amfi_get_out_of_my_way etc. would also invalidate the probe)"
run nvram boot-args
run sysctl -a 2>/dev/null | grep -iE 'amfi|cs_enforce|vm.cs' | head -40

sub "0.4 sudo availability"
run sudo -n true
note "sudo -n: rc=$?"

sub "0.5 Does THIS kernel carry the AMFI chroot gate? (if absent, a pass proves nothing about 26.5)"
KC=""
for cand in \
  /System/Library/KernelCollections/BootKernelExtensions.kc \
  /System/Volumes/Preboot/*/boot/*/System/Library/KernelCollections/BootKernelExtensions.kc ; do
  [ -f "$cand" ] && KC="$cand" && break
done
echo "kernel collection: ${KC:-<not found>}"
if [ -n "$KC" ]; then
  ls -l "$KC"
  for s in "hardened runtime not allowed in chroot" "com.apple.security.cs.allow-in-chroot" "not allowed in chroot"; do
    if strings -a "$KC" 2>/dev/null | grep -qF "$s"; then
      echo "PRESENT : $s"; note "kernel string PRESENT: '$s'"
    else
      echo "ABSENT  : $s"; note "kernel string ABSENT : '$s'"
    fi
  done
else
  note "kernel collection NOT FOUND — could not confirm the gate exists on this build"
fi

sub "0.6 Volume layout — where does the sealed system snapshot live?"
run mount
run df -P / /usr /bin /System /Users /private /usr/local 2>/dev/null
run diskutil apfs list
run diskutil apfs listSnapshots /

hdr "1. STEP 1 — bare chroot / with an untouched Apple binary (expect SIGKILL 137)"

sub "1.0 control: the same binary OUTSIDE any chroot"
run /bin/echo hello-outside-plain-apple
note "S1 control /bin/echo outside chroot: rc=$?"

sub "1.1 sudo chroot / /bin/echo  (chroot / is a no-op reroot that still sets FD_CHROOT)"
run sudo chroot / /bin/echo hello-inside-chroot-apple
S1=$?
note "S1 sudo chroot / /bin/echo: rc=$S1  (137=SIGKILL by AMFI, 0=no gate on this build)"

sub "1.2 kernel log for the kill reason"
run sudo log show --last 3m --style compact --predicate 'eventMessage CONTAINS "chroot"' 2>&1 | tail -40
run sudo log show --last 3m --style compact --predicate 'senderImagePath CONTAINS "AppleMobileFileIntegrity"' 2>&1 | tail -40
run sudo dmesg 2>/dev/null | tail -30

hdr "2. STEP 2 — THE BALLGAME: does ad-hoc re-signing defeat the gate?"

# 2x2 differential: {plain copy, ad-hoc re-signed} x {outside chroot, inside chroot}.
# Only the signature and the chroot state vary; same source bytes in every cell.
cp /bin/echo "$WORK/echo_plain"
cp /bin/echo "$WORK/echo_adhoc"
codesign -f -s - "$WORK/echo_adhoc" 2>&1
echo "--- codesign -dvvv on each ---"
codesign -dvvv "$WORK/echo_plain" 2>&1 | sed 's/^/plain: /'
codesign -dvvv "$WORK/echo_adhoc" 2>&1 | sed 's/^/adhoc: /'
codesign -dvvv /bin/echo          2>&1 | sed 's/^/orig : /'

sub "2.1 plain copy, OUTSIDE chroot  (isolates the launch-constraint wall)"
run "$WORK/echo_plain" cell-plain-outside
A=$?; note "S2 [plain  | outside] rc=$A  (137 => launch constraints kill a bare copy)"

sub "2.2 ad-hoc re-signed, OUTSIDE chroot  (does re-signing clear launch constraints?)"
run "$WORK/echo_adhoc" cell-adhoc-outside
B=$?; note "S2 [adhoc  | outside] rc=$B"

sub "2.3 plain copy, INSIDE chroot"
run sudo chroot / "$WORK/echo_plain" cell-plain-inside
C=$?; note "S2 [plain  | inside ] rc=$C"

sub "2.4 *** THE BALLGAME *** ad-hoc re-signed, INSIDE chroot"
run sudo chroot / "$WORK/echo_adhoc" cell-adhoc-inside
D=$?; note "S2 [adhoc  | inside ] rc=$D   <<< THE BALLGAME"

if [ "$D" -eq 0 ]; then
  note "STEP 2 VERDICT: PASS — ad-hoc re-signing defeats the AMFI chroot gate."
else
  note "STEP 2 VERDICT: FAIL — a second unconditional gate remains (rc=$D)."
fi

sub "2.5 log for the step-2 attempts"
run sudo log show --last 2m --style compact --predicate 'eventMessage CONTAINS "chroot" OR eventMessage CONTAINS "echo_adhoc" OR eventMessage CONTAINS "echo_plain"' 2>&1 | tail -50

hdr "2b. Isolate the mechanism with OUR OWN binaries (not Apple's — no launch constraint at all)"

# A locally-compiled binary has no Apple identity, so launch constraints cannot apply to it.
# Varying ONLY --options runtime therefore isolates CS_RUNTIME as the chroot gate.
cat > "$WORK/hello.c" <<'EOF'
#include <stdio.h>
int main(int argc, char **argv) { printf("hello from %s\n", argv[0]); return 0; }
EOF
run clang -o "$WORK/hello_default" "$WORK/hello.c"
cp "$WORK/hello_default" "$WORK/hello_adhoc"
cp "$WORK/hello_default" "$WORK/hello_hard"
cp "$WORK/hello_default" "$WORK/hello_hard_ent"
cat > "$WORK/chroot.entitlements" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.cs.allow-in-chroot</key>
  <true/>
</dict>
</plist>
EOF
codesign -f -s - "$WORK/hello_adhoc" 2>&1
codesign -f -s - --options runtime "$WORK/hello_hard" 2>&1
codesign -f -s - --options runtime --entitlements "$WORK/chroot.entitlements" "$WORK/hello_hard_ent" 2>&1
for b in hello_default hello_adhoc hello_hard hello_hard_ent; do
  echo "--- $b ---"; codesign -dvvv --entitlements - "$WORK/$b" 2>&1 | grep -iE 'flags|Ident|chroot|runtime' | head
done

for b in hello_default hello_adhoc hello_hard hello_hard_ent; do
  sub "2b outside chroot: $b"
  run "$WORK/$b"; o=$?
  sub "2b INSIDE  chroot: $b"
  run sudo chroot / "$WORK/$b"; i=$?
  note "S2b $b: outside rc=$o | inside rc=$i"
done

# ---------------------------------------------------------------------------
if [ "$D" -ne 0 ]; then
  hdr "STOPPING — step 2 failed; macOS rerooting is closed. Skipping jail construction."
  hdr "LEDGER"; cat "$LEDGER"
  exit 0
fi
# ---------------------------------------------------------------------------

hdr "3. BUILD THE JAIL"

sub "3.0 skeleton"
run sudo rm -rf "$JAIL"
run sudo mkdir -p "$JAIL"/{bin,sbin,usr/bin,usr/sbin,usr/lib,usr/libexec,usr/share,private/etc,private/tmp,private/var,dev,System/Library/dyld,home/build,work,opt}
run sudo ln -sfn private/etc "$JAIL/etc"
run sudo ln -sfn private/tmp "$JAIL/tmp"
run sudo ln -sfn private/var "$JAIL/var"
run sudo chmod 1777 "$JAIL/private/tmp"

sub "3.1 dyld shared cache — the hard dependency (every system dylib lives only in here)"
CACHE_SRC=""
for d in \
  /System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld \
  /System/Library/dyld ; do
  if ls "$d"/dyld_shared_cache_* >/dev/null 2>&1; then CACHE_SRC="$d"; break; fi
done
echo "cache source: ${CACHE_SRC:-<none found>}"
[ -n "$CACHE_SRC" ] && run du -sh "$CACHE_SRC"
if [ -n "$CACHE_SRC" ]; then
  # Legacy path is what dyld falls back to inside a reroot; copy rather than mount so the
  # jail has no dependency on a second APFS mount surviving.
  run sudo ditto "$CACHE_SRC" "$JAIL/System/Library/dyld"
  run sudo mkdir -p "$JAIL/System/Volumes/Preboot/Cryptexes/OS/System/Library"
  run sudo ln -sfn /System/Library/dyld "$JAIL/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld"
fi
run ls -la "$JAIL/System/Library/dyld"

sub "3.2 rsync the executable tree"
for d in /bin /sbin /usr/bin /usr/sbin /usr/libexec /usr/lib /usr/share/zoneinfo; do
  [ -e "$d" ] || continue
  echo ">>> $d"
  sudo rsync -a --info=stats1 "$d/" "$JAIL$d/" 2>&1 | tail -5
done
run sudo du -sh "$JAIL"

sub "3.3 ad-hoc re-sign every Mach-O in the jail (this is the step that defeats both walls)"
COUNT=$(sudo find "$JAIL/bin" "$JAIL/sbin" "$JAIL/usr/bin" "$JAIL/usr/sbin" "$JAIL/usr/libexec" -type f -perm -u+x 2>/dev/null | wc -l | tr -d ' ')
echo "executables to re-sign: $COUNT"
T0=$(date +%s)
sudo find "$JAIL/bin" "$JAIL/sbin" "$JAIL/usr/bin" "$JAIL/usr/sbin" "$JAIL/usr/libexec" "$JAIL/usr/lib" \
  -type f \( -perm -u+x -o -name '*.dylib' \) -print0 2>/dev/null \
  | sudo xargs -0 -P 8 -n 40 codesign -f -s - >/dev/null 2>&1
T1=$(date +%s)
echo "re-sign wall clock: $((T1-T0))s for $COUNT executables"
note "S3 re-sign: $COUNT executables in $((T1-T0))s"

sub "3.4 synthesize /private/etc"
sudo tee "$JAIL/private/etc/passwd" >/dev/null <<'EOF'
root:*:0:0:System Administrator:/var/root:/bin/sh
build:*:501:20:Build:/home/build:/bin/sh
nobody:*:-2:-2:Unprivileged:/var/empty:/usr/bin/false
EOF
sudo tee "$JAIL/private/etc/group" >/dev/null <<'EOF'
wheel:*:0:root
staff:*:20:
nobody:*:-2:
EOF
echo 'nameserver 1.1.1.1' | sudo tee "$JAIL/private/etc/resolv.conf" >/dev/null
printf '127.0.0.1 localhost\n' | sudo tee "$JAIL/private/etc/hosts" >/dev/null
[ -f /etc/ssl/cert.pem ] && sudo mkdir -p "$JAIL/private/etc/ssl" && sudo cp /etc/ssl/cert.pem "$JAIL/private/etc/ssl/" 2>/dev/null
run ls -la "$JAIL/private/etc"

sub "3.5 devfs"
run sudo mount -t devfs devfs "$JAIL/dev"
run ls "$JAIL/dev" 2>&1 | head -20

sub "3.6 FIRST REROOT — a real chroot into the jail, not chroot /"
run sudo chroot "$JAIL" /bin/echo hello-from-real-jail
J1=$?; note "S3 real chroot /bin/echo: rc=$J1"
run sudo chroot "$JAIL" /bin/sh -c 'echo sh-works; /bin/pwd; /bin/ls / '
J2=$?; note "S3 real chroot /bin/sh: rc=$J2"
if [ "$J1" -ne 0 ]; then
  sub "3.6b diagnosis"
  run sudo log show --last 2m --style compact --predicate 'eventMessage CONTAINS "chroot" OR eventMessage CONTAINS "dyld"' 2>&1 | tail -40
  run sudo chroot "$JAIL" /usr/bin/true
  run sudo DYLD_PRINT_LIBRARIES=1 chroot "$JAIL" /bin/echo x 2>&1 | head -20
fi

hdr "4. NODE IN THE JAIL + THE SUCCESS CRITERIA"

sub "4.0 stage node"
NODE_BIN="$(command -v node || true)"
echo "host node: $NODE_BIN"
if [ -n "$NODE_BIN" ]; then
  NODE_REAL="$(readlink -f "$NODE_BIN" 2>/dev/null || python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$NODE_BIN")"
  echo "resolved : $NODE_REAL"
  run sudo mkdir -p "$JAIL/opt/node/bin"
  run sudo cp "$NODE_REAL" "$JAIL/opt/node/bin/node"
  run sudo codesign -f -s - "$JAIL/opt/node/bin/node"
  run sudo ln -sf /opt/node/bin/node "$JAIL/usr/bin/node"
fi
run sudo chroot "$JAIL" /opt/node/bin/node -e 'console.log("node runs in the jail:", process.version)'
N1=$?; note "S4 node in jail: rc=$N1"

sub "4.1 CRITERION (a) — fs.realpathSync on a deep path, the ancestor walk this effort exists for"
run sudo chroot "$JAIL" /opt/node/bin/node -e '
const fs=require("fs");
for (const p of ["/usr/bin","/usr/bin/clang","/bin/echo","/opt/node/bin/node","/work"]) {
  try { console.log("realpath", p, "=>", fs.realpathSync(p)); }
  catch (e) { console.log("realpath", p, "=> ERROR", e.code, e.message); }
}
try { console.log("lstat / =>", JSON.stringify(fs.lstatSync("/").isDirectory())); }
catch (e) { console.log("lstat / => ERROR", e.code); }
'
note "S4 criterion(a) realpath: rc=$?"

sub "4.2 CRITERION (b) — does os.homedir()/getpwuid still leak the real host identity?"
run sudo chroot "$JAIL" /opt/node/bin/node -e '
const os=require("os");
console.log("env.HOME          =", process.env.HOME);
console.log("os.homedir()      =", os.homedir());
try { console.log("os.userInfo()     =", JSON.stringify(os.userInfo())); }
catch(e){ console.log("os.userInfo() ERR =", e.message); }
const fs=require("fs");
try { console.log("/Users visible    =", fs.readdirSync("/Users").length, "entries"); }
catch(e){ console.log("/Users           =", e.code); }
'
note "S4 criterion(b) identity leak: rc=$?"

sub "4.3 what ELSE is reachable — the honest containment picture"
run sudo chroot "$JAIL" /opt/node/bin/node -e '
const fs=require("fs"), cp=require("child_process");
for (const p of ["/Users","/private/var/root","/Library","/System/Volumes/Data","/etc/passwd"]) {
  try { fs.accessSync(p); console.log("REACHABLE", p); } catch(e){ console.log("absent   ", p, e.code); }
}
try { console.log("network:", cp.execSync("/usr/bin/curl -s -m 8 -o /dev/null -w %{http_code} https://registry.npmjs.org/", {encoding:"utf8"})); }
catch(e){ console.log("network ERR", String(e.message).slice(0,200)); }
'

hdr "5. CRITERION (c) — node-gyp rebuild inside the jail"

sub "5.0 stage the toolchain (Command Line Tools + python3)"
CLT=/Library/Developer/CommandLineTools
if [ -d "$CLT" ]; then
  run du -sh "$CLT"
  run sudo mkdir -p "$JAIL/Library/Developer"
  T0=$(date +%s)
  sudo rsync -a "$CLT/" "$JAIL$CLT/" 2>&1 | tail -3
  T1=$(date +%s); echo "CLT rsync: $((T1-T0))s"
  sudo find "$JAIL$CLT" -type f \( -perm -u+x -o -name '*.dylib' \) -print0 2>/dev/null \
    | sudo xargs -0 -P 8 -n 40 codesign -f -s - >/dev/null 2>&1
  echo "CLT re-signed"
else
  echo "no CLT at $CLT"
fi
run sudo chroot "$JAIL" /usr/bin/clang --version
run sudo chroot "$JAIL" /usr/bin/python3 --version
run sudo chroot "$JAIL" /usr/bin/make --version

sub "5.1 fixture + npm"
NPM_CLI="$(node -e 'console.log(require.resolve("npm/bin/npm-cli.js"))' 2>/dev/null || true)"
echo "npm cli: ${NPM_CLI:-<not resolvable>}"
NPM_ROOT="$(dirname "$(dirname "${NPM_CLI:-/x/y}")")"
if [ -d "$NPM_ROOT" ]; then
  run sudo mkdir -p "$JAIL/opt/node/lib/node_modules"
  sudo rsync -a "$NPM_ROOT/" "$JAIL/opt/node/lib/node_modules/npm/" 2>&1 | tail -2
fi
sudo mkdir -p "$JAIL/work/addon"
sudo tee "$JAIL/work/addon/hello.c" >/dev/null <<'EOF'
#include <node_api.h>
static napi_value Hello(napi_env env, napi_callback_info info) {
  napi_value out; napi_create_string_utf8(env, "hello from native", NAPI_AUTO_LENGTH, &out); return out;
}
NAPI_MODULE_INIT() {
  napi_value fn; napi_create_function(env, NULL, 0, Hello, NULL, &fn);
  napi_set_named_property(env, exports, "hello", fn); return exports;
}
EOF
sudo tee "$JAIL/work/addon/binding.gyp" >/dev/null <<'EOF'
{ "targets": [ { "target_name": "hello", "sources": [ "hello.c" ] } ] }
EOF
sudo tee "$JAIL/work/addon/package.json" >/dev/null <<'EOF'
{ "name": "hello-addon", "version": "1.0.0", "private": true }
EOF
# node-gyp needs the node headers; fetch them on the host and place them where a jailed
# `--nodedir` can see them, so the build needs no network from inside.
NODE_VER="$(node -p 'process.version')"
run curl -fsSL -o "$WORK/node-headers.tar.gz" "https://nodejs.org/dist/$NODE_VER/node-$NODE_VER-headers.tar.gz"
sudo mkdir -p "$JAIL/work/nodedir"
run sudo tar xzf "$WORK/node-headers.tar.gz" -C "$JAIL/work/nodedir" --strip-components=1
run ls "$JAIL/work/nodedir/include/node" 2>&1 | head

sub "5.2 node-gyp rebuild INSIDE the jail"
run sudo chroot "$JAIL" /bin/sh -c '
  export PATH=/opt/node/bin:/usr/bin:/bin:/usr/sbin:/sbin
  export HOME=/home/build
  cd /work/addon
  /opt/node/bin/node /opt/node/lib/node_modules/npm/node_modules/node-gyp/bin/node-gyp.js rebuild \
     --nodedir=/work/nodedir 2>&1 | tail -40
'
G=$?; note "S5 node-gyp rebuild in jail: rc=$G"

sub "5.3 is the artifact host-ABI-correct Mach-O arm64, and does it LOAD?"
run sudo file "$JAIL/work/addon/build/Release/hello.node"
run sudo chroot "$JAIL" /opt/node/bin/node -e 'console.log("in-jail load:", require("/work/addon/build/Release/hello.node").hello())'
note "S5 in-jail load: rc=$?"
# The real criterion: the artifact must load on the UNJAILED host, same as any build output.
run node -e "console.log('host load:', require('$JAIL/work/addon/build/Release/hello.node').hello())"
note "S5 HOST load of jail-built .node: rc=$?"

sub "5.4 positive control — same fixture built on the host, unjailed"
mkdir -p "$WORK/addon-host"
cp "$JAIL/work/addon/hello.c" "$WORK/addon-host/" 2>/dev/null || sudo cp "$JAIL/work/addon/hello.c" "$WORK/addon-host/"
sudo cp "$JAIL/work/addon/binding.gyp" "$JAIL/work/addon/package.json" "$WORK/addon-host/" 2>/dev/null
sudo chown -R "$(id -u):$(id -g)" "$WORK/addon-host"
run sh -c "cd $WORK/addon-host && npx --yes node-gyp rebuild 2>&1 | tail -10"
run file "$WORK/addon-host/build/Release/hello.node"
run node -e "console.log('host control load:', require('$WORK/addon-host/build/Release/hello.node').hello())"

hdr "6. PRIVILEGE SHAPE — is root needed once, or on every spawn?"

sub "6.1 chroot(2) as an unprivileged user (expect EPERM — this is the crux)"
cat > "$WORK/tryroot.c" <<'EOF'
#include <stdio.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
int main(int argc, char **argv) {
  if (chroot(argv[1]) != 0) { printf("chroot(%s) FAILED errno=%d %s\n", argv[1], errno, strerror(errno)); return 1; }
  printf("chroot(%s) OK as uid=%d\n", argv[1], (int)getuid());
  if (chdir("/") != 0) { printf("chdir failed\n"); return 1; }
  execl("/bin/echo", "echo", "unprivileged-chroot-worked", (char*)0);
  printf("exec failed: %s\n", strerror(errno)); return 1;
}
EOF
run clang -o "$WORK/tryroot" "$WORK/tryroot.c"
codesign -f -s - "$WORK/tryroot" 2>&1
run "$WORK/tryroot" "$JAIL"
note "S6 unprivileged chroot(): rc=$?  (nonzero+EPERM => a privileged helper is required per spawn)"

sub "6.2 does a setuid-root helper survive? (the one-time-install shape)"
run sudo cp "$WORK/tryroot" "$WORK/tryroot_suid"
run sudo chown root:wheel "$WORK/tryroot_suid"
run sudo chmod 4755 "$WORK/tryroot_suid"
run ls -l "$WORK/tryroot_suid"
run "$WORK/tryroot_suid" "$JAIL"
note "S6 setuid-root helper chroot(): rc=$?  (0 => one-time root install, then unprivileged spawns)"

sub "6.3 do the jail mounts survive, or must root remount each time?"
run mount | grep -F "$JAIL"

hdr "LEDGER — the summary"
cat "$LEDGER"
echo
echo "macOS tested: $(sw_vers -productVersion) ($(sw_vers -buildVersion)) / $(uname -m)"
exit 0
