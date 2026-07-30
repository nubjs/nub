#!/bin/bash
# Build a real macOS chroot jail and test the three success criteria.
#
# WHY THIS IS VALID ON A SIP-OFF RUNNER. The AMFI chroot gate (can you ENTER) and the jail's
# viability (does anything WORK once inside) are separable questions. Criteria (a) realpath,
# (b) getpwuid leakage and (c) node-gyp emitting a loadable Mach-O are pure filesystem/Mach/
# toolchain questions with no AMFI involvement, so a SIP-off box answers them honestly.
# Only the entry gate needs a SIP-on host — that is gate.sh's job.
#
# Run-1 harness bugs fixed here: macOS ships openrsync, which rejects --info=stats1 (every
# rsync failed, so the tree was empty and every later stage failed for the wrong reason);
# and the dyld cache dir holds arm64e + x86_64 + Rosetta-AOT caches, of which only arm64e
# is needed.

set -u
export LC_ALL=C
JAIL=/tmp/mjail
W=/tmp/jailwork; mkdir -p "$W"
run() { echo "\$ $*"; "$@"; local rc=$?; echo "[exit $rc]"; return $rc; }
sub() { printf '\n----- %s -----\n' "$*"; }
hdr() { printf '\n\n========== %s ==========\n' "$*"; }
LED="$W/ledger"; : > "$LED"
note() { printf '%s\n' "$*" >> "$LED"; }

hdr "0 environment"
run sw_vers; run uname -m
echo "SIP: $(csrutil status 2>&1)"
run df -h /

hdr "1 skeleton"
sudo rm -rf "$JAIL"
sudo mkdir -p "$JAIL"/{bin,sbin,usr/bin,usr/sbin,usr/lib,usr/libexec,usr/share,private/etc,private/tmp,private/var/run,dev,System/Library/dyld,System/Library/CoreServices,home/build,work,opt,AppleInternal/XBS}
sudo ln -sfn private/etc "$JAIL/etc"
sudo ln -sfn private/tmp "$JAIL/tmp"
sudo ln -sfn private/var "$JAIL/var"
sudo chmod 1777 "$JAIL/private/tmp"
# darwin-jail plants this marker; it is Apple's own build-system "I am in a chroot" flag.
sudo touch "$JAIL/AppleInternal/XBS/.isChrooted"

hdr "2 dyld shared cache — arm64e only"
CS=""
for d in /System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld /System/Library/dyld; do
  ls "$d"/dyld_shared_cache_arm64e* >/dev/null 2>&1 && CS="$d" && break
done
echo "cache source: ${CS:-<none>}"
T0=$(date +%s)
if [ -n "$CS" ]; then
  # arm64e only: x86_64 (Rosetta target) and aot_shared_cache (Rosetta AOT) are dead weight.
  sudo cp -p "$CS"/dyld_shared_cache_arm64e* "$JAIL/System/Library/dyld/"
  sudo mkdir -p "$JAIL/System/Volumes/Preboot/Cryptexes/OS/System/Library"
  sudo ln -sfn /System/Library/dyld "$JAIL/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld"
fi
T1=$(date +%s)
run sudo du -sh "$JAIL/System/Library/dyld"
note "dyld cache (arm64e only): $(sudo du -sh "$JAIL/System/Library/dyld" | cut -f1) in $((T1-T0))s"

hdr "3 rsync the system tree"
T0=$(date +%s)
for d in /bin /sbin /usr/bin /usr/sbin /usr/libexec /usr/lib /usr/share /System/Library/Frameworks /System/Library/PrivateFrameworks; do
  [ -e "$d" ] || continue
  sudo rsync -a "$d/" "$JAIL$d/" 2>/dev/null
  echo "  synced $d"
done
sudo cp /System/Library/CoreServices/SystemVersion.plist "$JAIL/System/Library/CoreServices/" 2>/dev/null
T1=$(date +%s)
run sudo du -sh "$JAIL"
note "system tree rsync: $((T1-T0))s"

hdr "4 NO RE-SIGNING — plain copies (runs 2-4 showed re-signing IS the killer)"
# Reversal. The hypothesis was that `codesign -f -s -` strips CS_RUNTIME and frees the tree.
# Measured: re-signing is what causes the SIGKILL. It demotes a platform binary to
# third-party, and every macOS system executable is arm64e-only, an ABI AMFI rejects for
# third-party code on 14/15 (`AMFI: ... is adhoc signed. AMFI: code signature validation
# failed.`). A plain copy runs fine in a real reroot. So: copy, do not sign.
echo "skipping the re-sign sweep by design; tree keeps its Apple signatures"

hdr "5 /private/etc + devfs + DNS"
sudo tee "$JAIL/private/etc/passwd" >/dev/null <<'EOF'
root:*:0:0:System Administrator:/var/root:/bin/sh
build:*:501:20:Build:/home/build:/bin/sh
nobody:*:-2:-2:Unprivileged:/var/empty:/usr/bin/false
EOF
printf 'wheel:*:0:root\nstaff:*:20:\nnobody:*:-2:\n' | sudo tee "$JAIL/private/etc/group" >/dev/null
printf 'nameserver 1.1.1.1\n' | sudo tee "$JAIL/private/etc/resolv.conf" >/dev/null
printf '127.0.0.1 localhost\n' | sudo tee "$JAIL/private/etc/hosts" >/dev/null
sudo mkdir -p "$JAIL/private/etc/ssl" && sudo cp /etc/ssl/cert.pem "$JAIL/private/etc/ssl/" 2>/dev/null
run sudo mount -t devfs devfs "$JAIL/dev"
# mDNSResponder's socket must be HARDLINKED in (a copy is not a socket) for DNS to resolve.
sudo ln /var/run/mDNSResponder "$JAIL/private/var/run/mDNSResponder" 2>&1 || echo "mDNSResponder link failed"

hdr "6 FIRST REROOT"

# A bare "Killed: 9" carries no reason, so the crash report is the only thing that names it
# (Library not loaded / Code Signature Invalid / dyld cache missing are indistinguishable
# from the shell's exit status alone).
crashreason() {
  sleep 3
  for d in /Library/Logs/DiagnosticReports "$HOME/Library/Logs/DiagnosticReports"; do
    for f in $(sudo ls -t "$d" 2>/dev/null | head -4); do
      echo "--- $d/$f ---"
      sudo head -c 1200 "$d/$f" 2>/dev/null | grep -iE 'termination|reason|exception|signature|dyld|namespace|Library not loaded' | head -8
    done
  done
}

sub "6.1 bisect: does the kill survive a MINIMAL jail? (isolates the tree from the mechanism)"
# Same dyld cache + interpreter, one binary, nothing else. If this runs and the full tree
# does not, the fault is something the rsync brought in, not the reroot itself.
MIN=/tmp/minjail
sudo rm -rf "$MIN"; sudo mkdir -p "$MIN"/{bin,usr/lib,System/Library/dyld,dev}
# Plain copy, deliberately unsigned-by-us: re-signing is the thing that gets killed.
sudo cp /bin/echo "$MIN/bin/echo"
sudo cp -p /usr/lib/dyld "$MIN/usr/lib/dyld"
sudo cp -p "$CS"/dyld_shared_cache_arm64e* "$MIN/System/Library/dyld/" 2>/dev/null
sudo mkdir -p "$MIN/System/Volumes/Preboot/Cryptexes/OS/System/Library"
sudo ln -sfn /System/Library/dyld "$MIN/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld"
run sudo chroot "$MIN" /bin/echo minimal-jail-ok; M=$?
note "MINIMAL jail /bin/echo: rc=$M"
[ "$M" -ne 0 ] && crashreason

sub "6.2 the full jail"
run sudo chroot "$JAIL" /bin/echo hello-from-real-jail; J=$?
note "FULL jail /bin/echo: rc=$J"
run sudo chroot "$JAIL" /bin/sh -c 'echo sh-ok; /bin/pwd; /bin/ls /'
if [ "$J" -ne 0 ]; then
  crashreason
  sub "6.3 dyld diagnostics"
  run sudo DYLD_PRINT_LIBRARIES=1 chroot "$JAIL" /bin/echo x 2>&1 | tail -8
  run sudo DYLD_PRINT_SEARCHING=1 chroot "$JAIL" /bin/echo x 2>&1 | tail -8
  # Is the cache even visible at the path dyld will look for it?
  run sudo ls -la "$JAIL/System/Library/dyld/"
  run sudo ls -la "$JAIL/System/Volumes/Preboot/Cryptexes/OS/System/Library/"
  run sudo ls -la "$JAIL/usr/lib/dyld"
  sudo log show --last 2m --style compact --predicate \
    'eventMessage CONTAINS "mjail" OR eventMessage CONTAINS "chroot" OR eventMessage CONTAINS "Invalid"' 2>&1 \
    | grep -viE 'log run noninteractively|use_watchroot' | tail -15
fi

hdr "7 node in the jail"
NV=v24.18.0
run curl -fsSL -o "$W/node.tar.xz" "https://nodejs.org/dist/$NV/node-$NV-darwin-arm64.tar.xz"
sudo mkdir -p "$JAIL/opt/node"
run sudo tar xJf "$W/node.tar.xz" -C "$JAIL/opt/node" --strip-components=1
# Official Node is arm64 (not arm64e) and ships hardened-runtime + a Node Foundation
# identity, so BOTH variants are worth a cell: as-shipped tests whether hardened runtime is
# tolerated, re-signed tests whether stripping it is safe for a non-Apple binary.
sudo cp -p "$JAIL/opt/node/bin/node" "$JAIL/opt/node/bin/node_resigned"
run sudo codesign -f -s - "$JAIL/opt/node/bin/node_resigned"
run file "$JAIL/opt/node/bin/node"
run sudo chroot "$JAIL" /opt/node/bin/node -e 'console.log("node as-shipped in jail:", process.version)'
NODE_ASIS=$?
run sudo chroot "$JAIL" /opt/node/bin/node_resigned -e 'console.log("node re-signed in jail:", process.version)'
note "node in jail: as-shipped rc=$NODE_ASIS re-signed rc=$?"
# Everything downstream needs a working node; prefer whichever variant ran.
[ "$NODE_ASIS" -ne 0 ] && sudo cp -p "$JAIL/opt/node/bin/node_resigned" "$JAIL/opt/node/bin/node"

hdr "8 CRITERION (a) — realpath / ancestor walk, the whole point of rerooting"
run sudo chroot "$JAIL" /opt/node/bin/node -e '
const fs=require("fs");
for (const p of ["/","/usr","/usr/bin","/usr/bin/clang","/bin/echo","/opt/node/bin/node","/work"]) {
  try { console.log("OK   realpath", p, "=>", fs.realpathSync(p)); }
  catch (e) { console.log("FAIL realpath", p, "=>", e.code, e.message); }
}
try { fs.lstatSync("/"); console.log("OK   lstat /"); } catch(e){ console.log("FAIL lstat /", e.code); }
'
note "criterion(a) realpath: rc=$?"

hdr "9 CRITERION (b) — does getpwuid still leak the real host identity?"
run sudo chroot "$JAIL" /opt/node/bin/node -e '
const os=require("os"), fs=require("fs");
console.log("env.HOME      =", process.env.HOME);
console.log("os.homedir()  =", os.homedir());
try { console.log("os.userInfo() =", JSON.stringify(os.userInfo())); }
catch(e){ console.log("os.userInfo() ERR", e.message); }
for (const p of ["/Users","/Library","/System/Volumes/Data","/private/var/root"]) {
  try { fs.accessSync(p); console.log("REACHABLE", p, fs.readdirSync(p).length, "entries"); }
  catch(e){ console.log("absent   ", p, e.code); }
}
'

hdr "10 CRITERION (c) — node-gyp rebuild → loadable Mach-O arm64"
CLT=/Library/Developer/CommandLineTools
echo "xcode-select -p: $(xcode-select -p 2>&1)"
# Staging the toolchain is the single most expensive step; skip it outright when nothing can
# execute in the jail, rather than spending ~20 minutes to reach a foregone SIGKILL.
if [ "$J" -ne 0 ]; then
  echo "SKIPPED — nothing executes in the jail (section 6 rc=$J), so criterion (c) is unreachable"
  note "criterion(c): SKIPPED, jail not enterable"
elif [ -d "$CLT" ]; then
  T0=$(date +%s)
  sudo mkdir -p "$JAIL/Library/Developer"
  sudo rsync -a "$CLT/" "$JAIL$CLT/" 2>/dev/null
  sudo find "$JAIL$CLT" -type f \( -perm -u+x -o -name '*.dylib' \) -print0 2>/dev/null \
    | sudo xargs -0 -P 8 -n 30 codesign -f -s - >/dev/null 2>&1
  T1=$(date +%s); echo "CLT staged+signed in $((T1-T0))s ($(sudo du -sh "$JAIL$CLT" | cut -f1))"
  note "CLT stage: $((T1-T0))s"
else
  echo "NO CommandLineTools at $CLT — criterion (c) cannot run"
fi
run sudo chroot "$JAIL" /usr/bin/clang --version
run sudo chroot "$JAIL" /usr/bin/python3 --version
run sudo chroot "$JAIL" /usr/bin/make --version

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
printf '{ "targets": [ { "target_name": "hello", "sources": [ "hello.c" ] } ] }\n' | sudo tee "$JAIL/work/addon/binding.gyp" >/dev/null
printf '{ "name":"hello-addon","version":"1.0.0","private":true }\n' | sudo tee "$JAIL/work/addon/package.json" >/dev/null
# Stage headers on the host so the jailed build needs no network.
run curl -fsSL -o "$W/hdr.tar.gz" "https://nodejs.org/dist/$NV/node-$NV-headers.tar.gz"
sudo mkdir -p "$JAIL/work/nodedir"
sudo tar xzf "$W/hdr.tar.gz" -C "$JAIL/work/nodedir" --strip-components=1
sudo cp "$W/node.tar.xz" /dev/null 2>/dev/null || true

run sudo chroot "$JAIL" /bin/sh -c '
  export PATH=/opt/node/bin:/usr/bin:/bin:/usr/sbin:/sbin
  export HOME=/home/build
  # The per-user temp dir comes from a Mach service that chroot cannot scope, so confstr()
  # fails and DARWIN_USER_TEMP_DIR is empty; give the toolchain a plain TMPDIR instead.
  export TMPDIR=/tmp
  # node-gyp probes for python by spawning bare "python3"/"python" and reads the path back;
  # that handshake returns empty in the jail, so name the interpreter outright.
  export PYTHON=/usr/bin/python3
  cd /work/addon
  /opt/node/bin/node /opt/node/lib/node_modules/npm/node_modules/node-gyp/bin/node-gyp.js rebuild \
     --nodedir=/work/nodedir --python=/usr/bin/python3 2>&1 | tail -30
'
G=$?; note "node-gyp in jail: rc=$G"

sub "the artifact"
run sudo file "$JAIL/work/addon/build/Release/hello.node"
run sudo chroot "$JAIL" /opt/node/bin/node -e 'console.log("in-jail load:", require("/work/addon/build/Release/hello.node").hello())'
# The real criterion: it must load on the UNJAILED host.
run sudo /opt/node/bin/node -e "console.log('HOST load:', require('$JAIL/work/addon/build/Release/hello.node').hello())" 2>&1 \
  || node -e "console.log('HOST load:', require('$JAIL/work/addon/build/Release/hello.node').hello())"
note "HOST load of jail-built .node: rc=$?"

hdr "11 PRIVILEGE SHAPE — one-time root, or root on every spawn?"
cat > "$W/tryroot.c" <<'EOF'
#include <stdio.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
int main(int argc, char **argv) {
  printf("uid=%d euid=%d\n", (int)getuid(), (int)geteuid());
  if (chroot(argv[1]) != 0) { printf("chroot FAILED errno=%d %s\n", errno, strerror(errno)); return 1; }
  if (chdir("/") != 0) { printf("chdir failed\n"); return 1; }
  execl("/bin/echo", "echo", "chroot-worked", (char*)0);
  printf("exec failed: %s\n", strerror(errno)); return 1;
}
EOF
clang -o "$W/tryroot" "$W/tryroot.c"; codesign -f -s - "$W/tryroot" 2>&1
sub "11.1 as the ordinary user (expect EPERM — chroot(2) requires suser())"
run "$W/tryroot" "$JAIL"; note "unprivileged chroot(): rc=$?"
sub "11.2 via a setuid-root helper installed once (the one-time-root shape)"
sudo cp "$W/tryroot" "$W/tryroot_suid"
sudo chown root:wheel "$W/tryroot_suid"; sudo chmod 4755 "$W/tryroot_suid"
run ls -l "$W/tryroot_suid"
run "$W/tryroot_suid" "$JAIL"; note "setuid-root helper chroot(): rc=$?"
sub "11.3 do the mounts persist?"
mount | grep -F "$JAIL"

hdr "LEDGER"
cat "$LED"
echo "macOS tested: $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m); SIP: $(csrutil status 2>&1)"
exit 0
