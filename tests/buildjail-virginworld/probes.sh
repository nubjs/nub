#!/usr/bin/env bash
# Seven probes against the rerooted world. Every claim in the report must trace to a banner here
# plus its verbatim output; probes that assert a NEGATIVE carry their positive control inline.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WORLD="${WORLD:?set WORLD to the prepared rootfs}"
RUN="${RUN:-/tmp/vw-run}"
JAIL="$HERE/jail"
REALHOME="$HOME"

banner() { echo; echo "################ $* ################"; }
run() { echo "\$ $*"; "$@" 2>&1; echo "[rc=$?]"; }
runq() { echo "\$ $1"; bash -c "$1" 2>&1; echo "[rc=$?]"; }

# The one bind mount: project AND project-local CAS store under a single root (research doc §2b).
rm -rf "$RUN"; mkdir -p "$RUN/work/project" "$RUN/work/store" "$RUN/side"
cp -r "$HERE/fixture-addon" "$RUN/work/project/addon"
cp -r "$HERE/fixture-real" "$RUN/work/project/real"
echo "cas-blob" > "$RUN/work/store/blob"
echo "side-blob" > "$RUN/side/blob"

# --uid 1000: see P4a-control. A single-uid map plus uid 0 inside makes node-tar attempt chown,
# which EINVALs on every unmapped uid in the Node headers tarball and kills every node-gyp build.
J=("$JAIL" --root "$WORLD" --bind "$RUN/work:/work" --uid 1000 --cwd /work/project)

# P6 measures the privilege question properly. Everything before it needs a working user namespace,
# so grant it up front on a host where the distro restricts it — this ONE apparmor_parser call is
# the whole privilege cost, and P6 removes it again to show the failure it is papering over.
cat > /tmp/nub-jail.apparmor <<EOF
abi <abi/4.0>,
include <tunables/global>
profile nub-jail $JAIL flags=(unconfined) {
  userns,
}
EOF
if [ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)" = "1" ]; then
  sudo apparmor_parser -r -W /tmp/nub-jail.apparmor \
    && echo "PRELUDE: one-time root step applied — path-bound AppArmor 'userns' profile for $JAIL"
fi

banner "P0  host baseline"
run uname -a
runq "ldd --version | head -1"
runq "id"
runq "cat /etc/os-release | head -3"
runq "sysctl kernel.apparmor_restrict_unprivileged_userns 2>&1 || echo '(sysctl absent)'"
runq "cat /proc/sys/kernel/unprivileged_userns_clone 2>&1 || echo '(knob absent)'"
runq "node --version 2>&1; which node"
echo "HOST_ROOT_LISTING:"; ls -1 / | tr '\n' ' '; echo

banner "P1  it reroots"
run "${J[@]}" -- /bin/sh -c 'echo "IN-WORLD / :"; ls -1 /; echo; echo "hostname: $(hostname)"; echo "uid: $(id -u) ($(id -un))"'
echo "--- differential: the host root, same moment ---"
runq "ls -1 /"
echo "--- the world cannot see the host filesystem at all ---"
run "${J[@]}" -- /bin/sh -c 'for p in /Users /snap /home/runner /mnt /usr/share/dotnet /opt/hostedtoolcache; do [ -e "$p" ] && echo "PRESENT $p" || echo "ABSENT  $p"; done'

banner "P2  realpath and ancestor walks work naturally"
run "${J[@]}" -- /bin/sh -c '
set -e
mkdir -p /work/project/deep/a/b/c/d/e
echo hi > /work/project/deep/a/b/c/d/e/f.txt
ln -sfn /work/project/deep /work/project/link
echo "--- coreutils realpath through a symlink ---"
realpath /work/project/link/a/b/c/d/e/f.txt
echo "--- fs.realpathSync on the same path ---"
/opt/node/bin/node -e "console.log(require(\"fs\").realpathSync(\"/work/project/link/a/b/c/d/e/f.txt\"))"
echo "--- lstat EVERY ancestor up to / (the exact walk that EPERMs under an allowlist) ---"
/opt/node/bin/node -e "
const fs=require(\"fs\"),p=require(\"path\");
let cur=\"/work/project/link/a/b/c/d/e/f.txt\", n=0;
while(true){ fs.lstatSync(cur); n++; const up=p.dirname(cur); if(up===cur) break; cur=up; }
console.log(\"ancestors lstat OK:\", n, \"(walk terminated at /)\");
"
echo "--- resolveMainPath: run a script REACHED THROUGH A SYMLINK (Node realpaths the main module) ---"
echo "console.log(\"main module resolved via realpath:\", __filename)" > /work/project/deep/main.js
/opt/node/bin/node /work/project/link/main.js
echo "--- opendir/readdir the whole chain to / ---"
ls -d / /work /work/project /work/project/deep >/dev/null && echo "opendir chain OK"
'

banner "P3  \$HOME is ABSENT, not denied"
run "${J[@]}" -- /bin/sh -c '
echo "HOME=$HOME"
echo "entries in \$HOME: $(ls -A "$HOME" | wc -l)"
echo "/home listing: $(ls -A /home | tr "\n" " ")"
echo "/root: $([ -e /root ] && echo present || echo absent)"
for p in "$1" /Users; do [ -e "$p" ] && echo "PRESENT $p" || echo "ABSENT  $p"; done
echo "--- nothing credential-shaped exists anywhere in the world ---"
find / -xdev \( -name "id_*" -o -name ".npmrc" -o -name ".netrc" -o -name "*.pem" -o -name "credentials" \) 2>/dev/null | head -20
echo "(end of credential scan)"
echo "--- env is scrubbed ---"
env | sort
' _ "$REALHOME"
echo "--- differential: what a script would have seen on the host ---"
runq "ls -A $REALHOME | wc -l | sed 's/^/host home entries: /'"
runq "ls -A $REALHOME | head -22 | tr '\\n' ' '"

banner "P4  real npm install with a native addon compiled from source"
echo "--- 4a-CONTROL: the SAME build with the ONLY variable being uid 0 inside instead of 1000 ---"
runq "$JAIL --root $WORLD --bind $RUN/work:/work --uid 0 --cwd /work/project/addon -- /bin/sh -c 'export npm_config_cache=/home/build/.npm; /opt/node/bin/npm install --no-audit --no-fund --loglevel=error' 2>&1 | grep -m3 -E 'TAR_ENTRY_ERROR|fatal problem|gyp ERR' ; echo '(3 lines of many)'"
rm -rf "$RUN/work/project/addon/build" "$RUN/work/project/addon/node_modules"
echo "--- 4a: hand-written N-API addon via npm lifecycle -> node-gyp rebuild ---"
run /usr/bin/time -f 'ADDON_BUILD_WALL %es' "${J[@]}" --cwd /work/project/addon -- /bin/sh -c '
set -e
export npm_config_cache=/home/build/.npm npm_config_unsafe_perm=true
/opt/node/bin/node --version; /opt/node/bin/npm --version; cc --version | head -1
/opt/node/bin/npm install --no-audit --no-fund --loglevel=error
ls -l build/Release/hello.node
'
echo "--- 4b: better-sqlite3 + express, better-sqlite3 FORCED to compile from source ---"
run /usr/bin/time -f 'REAL_INSTALL_WALL %es' "${J[@]}" --cwd /work/project/real -- /bin/sh -c '
set -e
export npm_config_cache=/home/build/.npm npm_config_unsafe_perm=true npm_config_build_from_source=true
/opt/node/bin/npm install --no-audit --no-fund --build-from-source --loglevel=error
echo "packages: $(ls node_modules | wc -l)"
find node_modules -name "*.node" | head
'
echo "--- 4c: THE MONEY SHOT — do those artifacts load on the HOST, outside the jail? ---"
runq "file $RUN/work/project/addon/build/Release/hello.node"
runq "objdump -T $RUN/work/project/addon/build/Release/hello.node 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -3; echo '(max GLIBC version required by the addon)'"
runq "ldd --version | head -1; echo '(host glibc)'"
runq "node --version; node -e \"console.log('HOST LOAD OK:', require('$RUN/work/project/addon/build/Release/hello.node').hello())\""
if [ -x /tmp/node24/bin/node ]; then
  echo "  (second host Node, different major — N-API ABI stability control)"
  runq "/tmp/node24/bin/node --version; /tmp/node24/bin/node -e \"console.log('HOST LOAD OK on a different Node major:', require('$RUN/work/project/addon/build/Release/hello.node').hello())\""
fi
runq "file \$(find $RUN/work/project/real/node_modules -name 'better_sqlite3.node' | head -1)"
runq "cd $RUN/work/project/real && node -e \"const D=require('better-sqlite3'); const db=new D(':memory:'); db.exec('create table t(x)'); db.prepare('insert into t values (?)').run(42); console.log('HOST LOAD OK: better-sqlite3 ->', db.prepare('select x from t').get());\""

banner "P5  EXDEV — hardlinks need ONE mount (the topology constraint)"
echo "--- 5a: store and project under the SINGLE /work bind mount (the design) ---"
run "${J[@]}" -- /bin/sh -c '
grep -E " /work " /proc/self/mountinfo | awk "{print \$1, \$3, \$4, \$5}"
mkdir -p /work/project/node_modules/pkg
ln /work/store/blob /work/project/node_modules/pkg/linked && echo "HARDLINK OK links=$(stat -c %h /work/store/blob)" || echo "HARDLINK FAILED"
'
echo "--- 5b: CONTROL, one variable changed: the SAME store dir mounted a SECOND time at /store ---"
run "$JAIL" --root "$WORLD" --bind "$RUN/work:/work" --bind "$RUN/work/store:/store" --cwd /work -- /bin/sh -c '
grep -E " /work | /store " /proc/self/mountinfo | awk "{print \$1, \$3, \$4, \$5}"
stat -c "same inode? /work/store/blob=%i /store/blob=%i  same dev? %d" /work/store/blob >/dev/null
echo "/work/store/blob inode=$(stat -c %i /work/store/blob) dev=$(stat -c %d /work/store/blob)"
echo "/store/blob      inode=$(stat -c %i /store/blob) dev=$(stat -c %d /store/blob)"
mkdir -p /work/project/node_modules/pkg2
ln /store/blob /work/project/node_modules/pkg2/linked && echo "HARDLINK OK (unexpected)" || echo "HARDLINK FAILED as predicted"
'

banner "P6  privilege reality check — a one-variable ladder on ONE host"
cat > /tmp/nub-jail.apparmor <<EOF
abi <abi/4.0>,
include <tunables/global>
profile nub-jail $JAIL flags=(unconfined) {
  userns,
}
EOF
probe6() { echo "  \$ $JAIL ... -- /bin/true"; "${J[@]}" -- /bin/true; echo "  [rc=$?]  (0=world built, 72=uid_map EPERM)"; }

echo "--- 6a: the TRAP — util-linux exit codes lie about userns availability ---"
runq "id -u"
runq "unshare --user /bin/true; echo 'unshare -U (no map) rc='\$?"
runq "unshare --user --map-root-user /bin/true; echo 'unshare -U --map-root-user rc='\$?"

echo "--- 6b: RUNG 1 — restriction ON, no AppArmor profile, no root (the stock Ubuntu 24.04 state) ---"
runq "sudo apparmor_parser -R /tmp/nub-jail.apparmor 2>/dev/null; sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1"
probe6

echo "--- 6c: RUNG 2 — ONE variable changed: load a path-bound profile granting only 'userns' ---"
runq "sudo apparmor_parser -r -W /tmp/nub-jail.apparmor && echo profile_loaded"
runq "sysctl -n kernel.apparmor_restrict_unprivileged_userns"
probe6

echo "--- 6d: RUNG 3 — profile REMOVED again, restriction flipped off instead (the blunt step) ---"
runq "sudo apparmor_parser -R /tmp/nub-jail.apparmor; sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0"
probe6

echo "--- 6e: RUNG 4 — restriction back ON, no profile, but run the jail AS ROOT (no userns at all) ---"
runq "sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1"
runq "sudo $JAIL --root $WORLD --bind $RUN/work:/work --cwd /work -- /bin/sh -c 'echo AS_ROOT_OK uid=\$(id -u); ls / | tr \"\\n\" \" \"'"

echo "--- restore the host to how it was found (restriction on, profile loaded for the rest of the run) ---"
runq "sudo apparmor_parser -r -W /tmp/nub-jail.apparmor; sysctl -n kernel.apparmor_restrict_unprivileged_userns"

banner "P7  cost"
echo "--- 7a: per-run world setup (overlay lower + tmpfs upper + ns + pivot_root), 5 reps ---"
for i in 1 2 3 4 5; do "${J[@]}" --timings -- /bin/true 2>&1 >/dev/null | grep TIMINGS; done
echo "--- 7b: same, but WITHOUT overlayfs (cp -a the whole rootfs into tmpfs) — the fallback's cost ---"
for i in 1 2; do "$JAIL" --root "$WORLD" --bind "$RUN/work:/work" --cwd /work --no-overlay --timings -- /bin/true 2>&1 >/dev/null | grep TIMINGS; done
echo "--- 7c: install inside the world vs plain npm install on the host, same fixture, both cold-cache ---"
rm -rf "$RUN/ctl"; cp -r "$HERE/fixture-real" "$RUN/ctl"
rm -rf "$RUN/work/project/real/node_modules" "$RUN/work/project/real/package-lock.json"
echo "IN-WORLD:"
runq "/usr/bin/time -f 'WORLD_INSTALL_WALL %es' ${J[*]} --cwd /work/project/real -- /bin/sh -c 'export npm_config_cache=/home/build/.npm npm_config_build_from_source=true; /opt/node/bin/npm install --no-audit --no-fund --build-from-source --loglevel=error' 2>&1 | tail -3"
echo "HOST CONTROL (no jail, fresh npm cache, same --build-from-source):"
runq "cd $RUN/ctl && /usr/bin/time -f 'HOST_INSTALL_WALL %es' env npm_config_cache=$RUN/.npmctl npm install --no-audit --no-fund --build-from-source --loglevel=error 2>&1 | tail -3"

banner "P8  full reign inside — no allowlist, no denylist, and it does not leak out"
run "${J[@]}" -- /bin/sh -c '
set -e
echo pwned > /etc/passwd; echo pwned > /usr/lib/x; mkdir -p /var/lib/y && echo pwned > /var/lib/y/z
rm -rf /usr/share/doc 2>/dev/null || true
echo "wrote to /etc, /usr, /var and deleted /usr/share/doc — all permitted, zero policy"
cat /etc/passwd
'
echo "--- the host rootfs and the prepared lowerdir are untouched (overlay upper was tmpfs) ---"
runq "head -1 $WORLD/etc/passwd"
runq "ls $WORLD/usr/lib/x 2>&1 | head -1"
runq "head -2 /etc/passwd"

banner "DONE"
