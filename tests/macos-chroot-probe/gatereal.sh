#!/bin/bash
# Does the AMFI chroot gate fire with SIP OFF? If it does, a SIP-off runner is a valid test
# bed for the gate and the only residual is an ADDITIONAL SIP-armed check on top.
#
# The prior evidence for that was one uncontrolled cell: node as-shipped (hardened) died in
# the jail while a re-signed copy ran. Nobody ever ran node as-shipped OUTSIDE the chroot on
# that runner, so "hardened dies in a chroot" and "this binary dies anywhere" were not
# separated. This script separates them.
#
# Locally-compiled binaries are arm64, NOT arm64e, so the arm64e third-party rejection that
# confounds Apple binaries cannot apply here. The only variable across a row is the location;
# the only variable down a column is the signature.

set -u
export LC_ALL=C
W=/tmp/gr; MIN=/tmp/grjail
sudo rm -rf "$MIN" "$W"; mkdir -p "$W"
run() { echo "\$ $*"; "$@"; local rc=$?; echo "[exit $rc]"; return $rc; }
sub() { printf '\n----- %s -----\n' "$*"; }
R=""; cell() { local n="$1"; shift; "$@" >/dev/null 2>&1; local rc=$?; R="$R$(printf '\n  %-34s rc=%s' "$n" "$rc")"; }

sub "test bed"
run sw_vers; echo "SIP: $(csrutil status 2>&1)"; run uname -m

sub "build subjects (arm64, not arm64e — no confound)"
printf '#include <stdio.h>\nint main(){puts("ran");return 0;}\n' > "$W/h.c"
clang -o "$W/h_adhoc" "$W/h.c"; codesign -f -s - "$W/h_adhoc" 2>&1
cp "$W/h_adhoc" "$W/h_hard";     codesign -f -s - --options runtime "$W/h_hard" 2>&1
cat > "$W/ent.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.cs.allow-in-chroot</key><true/></dict></plist>
EOF
cp "$W/h_adhoc" "$W/h_hard_ent"; codesign -f -s - --options runtime --entitlements "$W/ent.plist" "$W/h_hard_ent" 2>&1
run file "$W/h_hard"
for b in h_adhoc h_hard h_hard_ent; do
  echo "--- $b ---"; codesign -dvvv "$W/$b" 2>&1 | grep -E 'flags=|Runtime Version'
done

sub "official node — the subject whose uncontrolled result prompted this run"
NV=v24.18.0
curl -fsSL -o "$W/node.tar.xz" "https://nodejs.org/dist/$NV/node-$NV-darwin-arm64.tar.xz"
mkdir -p "$W/nodedist" && tar xJf "$W/node.tar.xz" -C "$W/nodedist" --strip-components=1
cp "$W/nodedist/bin/node" "$W/node_asis"
cp "$W/nodedist/bin/node" "$W/node_resigned"; codesign -f -s - "$W/node_resigned" 2>&1
run file "$W/node_asis"
echo "--- node as-shipped ---"; codesign -dvvv "$W/node_asis" 2>&1 | grep -E 'flags=|Authority=|Runtime Version'

sub "build a REAL minimal jail (chroot / does not arm the gate — a real reroot is required)"
CS=""
for d in /System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld /System/Library/dyld; do
  ls "$d"/dyld_shared_cache_arm64e* >/dev/null 2>&1 && CS="$d" && break
done
sudo mkdir -p "$MIN"/{bin,usr/lib,System/Library/dyld,System/Volumes/Preboot/Cryptexes/OS/System/Library}
sudo cp -p /usr/lib/dyld "$MIN/usr/lib/dyld"
sudo cp -p "$CS"/dyld_shared_cache_arm64e* "$MIN/System/Library/dyld/"
sudo ln -sfn /System/Library/dyld "$MIN/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld"
for b in h_adhoc h_hard h_hard_ent node_asis node_resigned; do sudo cp -p "$W/$b" "$MIN/bin/$b"; done
cp /bin/echo "$W/echo"; sudo cp -p "$W/echo" "$MIN/bin/echo"   # plain copy, known-good baseline
run sudo du -sh "$MIN"

MARK=$(date -u +%Y-%m-%d\ %H:%M:%S)
sub "THE MATRIX — location across, signature down"
for b in echo h_adhoc h_hard h_hard_ent; do
  cell "$b | outside"       "$W/$b"
  cell "$b | chroot /"      sudo chroot /     "$MIN/bin/$b"
  cell "$b | REAL reroot"   sudo chroot "$MIN" "/bin/$b"
done
cell "node_asis | outside"      "$W/node_asis" -e ''
cell "node_asis | REAL reroot"  sudo chroot "$MIN" /bin/node_asis -e ''
cell "node_resigned | outside"     "$W/node_resigned" -e ''
cell "node_resigned | REAL reroot" sudo chroot "$MIN" /bin/node_resigned -e ''
echo "$R"

sub "verbatim: the decisive pair"
run "$W/node_asis" -e 'console.log("node as-shipped OUTSIDE chroot:", process.version)'
run sudo chroot "$MIN" /bin/node_asis -e 'console.log("node as-shipped IN REAL REROOT:", process.version)'
run "$W/h_hard"
run sudo chroot "$MIN" /bin/h_hard

sub "AMFI log since $MARK"
sudo log show --start "$MARK" --style compact \
  --predicate 'senderImagePath CONTAINS "AppleMobileFileIntegrity" OR eventMessage CONTAINS "chroot" OR eventMessage CONTAINS "not allowed"' 2>&1 \
  | grep -viE 'log run noninteractively|use_watchroot' | tail -25

sub "does the undocumented escape entitlement change anything?"
run sudo chroot "$MIN" /bin/h_hard_ent
exit 0
