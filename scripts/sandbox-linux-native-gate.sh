#!/usr/bin/env bash
# Bounded real-kernel discriminator, not a full production acceptance matrix.
set -euo pipefail
SOURCE=${1:?source directory}
EVIDENCE=${2:?evidence directory}
COMMIT=${3:?source commit}
MODE=${4:-mounted}
case "$MODE" in mounted|source-only) ;; *) echo "unknown gate mode: $MODE" >&2; exit 2;; esac
mkdir -p "$EVIDENCE"
cd "$SOURCE"
export PATH="$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=1.95.0
export CARGO_TARGET_DIR="$HOME/native-vertical-target"
export CARGO_BUILD_JOBS=4
{
  id
  test "$(id -u)" != 0
  uname -a
  cat /etc/os-release
  grep -E '^(Uid|Gid|CapInh|CapPrm|CapEff|CapAmb|NoNewPrivs):' /proc/self/status
  ls -l /dev/fuse
  rustc -Vv
  cargo -Vv
} > "$EVIDENCE/environment.log"
printf '%s\n' "$COMMIT" > "$EVIDENCE/source-commit.txt"
find crates/nub-sandbox/src -type f -print0 | sort -z | xargs -0 sha256sum > "$EVIDENCE/source.sha256"
sha256sum Cargo.toml Cargo.lock crates/nub-sandbox/Cargo.toml > "$EVIDENCE/manifests.sha256"
sha256sum scripts/sandbox-linux-namespace-gate.sh scripts/sandbox-linux-native-gate.sh > "$EVIDENCE/namespace-instrument.txt"
printf 'BUILD_START %s\n' "$(date -u +%FT%TZ)"
cargo check --locked --profile fast -p nub-sandbox --lib > "$EVIDENCE/check.stdout" 2> "$EVIDENCE/check.stderr"
cargo test --locked --profile fast -p nub-sandbox --lib --no-run --message-format=json-render-diagnostics > "$EVIDENCE/compiler.jsonl" 2> "$EVIDENCE/compiler.stderr"
BIN=$(python3 - "$EVIDENCE/compiler.jsonl" <<'PY'
import json, sys
paths = []
for line in open(sys.argv[1]):
    row = json.loads(line)
    if (row.get('reason') == 'compiler-artifact'
        and row.get('target', {}).get('name') == 'nub_sandbox'
        and row.get('profile', {}).get('test') and row.get('executable')):
        paths.append(row['executable'])
assert len(paths) == 1, paths
print(paths[0])
PY
)
printf '%s\n' "$BIN" > "$EVIDENCE/test-binary.path"
sha256sum "$BIN" > "$EVIDENCE/test-binary.sha256"
file "$BIN" > "$EVIDENCE/test-binary.file"
gzip -c "$BIN" > "$EVIDENCE/test-binary.gz"
"$BIN" --list > "$EVIDENCE/tests.list"
for filter in \
  backend::linux_projection_mount_tests::mounted_projection_delivers_native_regular_files \
  backend::linux_projection_mount_tests::mounted_projection_enforces_paths_and_owns_commands \
  backend::linux_projection_mount_tests::mounted_projection_preserves_mapping_semantics \
  backend::linux_projection::session_tests::projected_session_real_lifecycle_and_cleanup \
  backend::linux_supervisor::lifecycle_tests::legacy_write_policy_is_refused_before_command_admission
do
  grep -F "$filter: test" "$EVIDENCE/tests.list"
done
grep -F 'backend::linux_supervisor::projected_open::tests::' "$EVIDENCE/tests.list"
result=0
run_test() {
  local label=$1 expected=$2
  shift 2
  printf 'TEST_START %s %s\n' "$label" "$(date -u +%FT%TZ)"
  set +e
  timeout --kill-after=10s 180s "$BIN" "$@" > "$EVIDENCE/$label.log" 2>&1
  local rc=$?
  set -e
  printf '%s\t%s\t%s\n' "$label" "$rc" "$expected" >> "$EVIDENCE/results.tsv"
  cat "$EVIDENCE/$label.log"
  if [ "$rc" -ne "$expected" ]; then result=1; fi
}
run_test provider 0 backend::linux_projection:: --nocapture --test-threads=1
run_test dispatcher 0 backend::linux_supervisor::projected_open::tests:: --nocapture --test-threads=1
run_test legacy-refusal 0 backend::linux_supervisor::lifecycle_tests::legacy_write_policy_is_refused_before_command_admission --exact --nocapture
run_test owner-unmount 0 backend::linux_projection_mount_tests::owner_unmount_classifies_only_connection_abort --exact --nocapture
if [ "$MODE" = source-only ]; then
  printf 'PROJECTION_SOURCE_GATE_EXIT=%s MOUNTED_ENFORCEMENT_NOT_RUN=1\n' "$result"
  exit "$result"
fi
run_test native-mounted 0 backend::linux_projection_mount_tests::mounted_projection_delivers_native_regular_files --exact --ignored --nocapture --test-threads=1
run_test original-mounted 0 backend::linux_projection_mount_tests::mounted_projection_enforces_paths_and_owns_commands --exact --ignored --nocapture --test-threads=1
run_test fuse-only-mapping-control 101 backend::linux_projection_mount_tests::mounted_projection_preserves_mapping_semantics --exact --ignored --nocapture --test-threads=1
if ! grep -Fq 'NATIVE_PROJECTION_ACCEPTANCE_OK' "$EVIDENCE/native-mounted.log"; then result=1; fi
for marker in NATIVE_QUEUE_SATURATION_OK NATIVE_QUEUED_CANCEL_OK NATIVE_SHARED_SERVICE_COMMANDS_OK NATIVE_SHARED_SERVICE_CANCELLATION_OK NATIVE_ADMISSION_CANCELLATION_OK NATIVE_STALE_NOTIFICATION_CANCEL_OK NATIVE_ACQUIRING_THREAD_EXIT_SURVIVED NATIVE_STALLED_SERVICE_SHUTDOWN_REAPED; do
  if ! grep -Fq "$marker" "$EVIDENCE/native-mounted.log"; then result=1; fi
done
if ! grep -Fq 'prefaulted mapped hardlink alias must observe ordinary writes' "$EVIDENCE/fuse-only-mapping-control.log"; then result=1; fi
printf 'NATIVE_VERTICAL_GATE_EXIT=%s\n' "$result"
exit "$result"
