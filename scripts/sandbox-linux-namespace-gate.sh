#!/usr/bin/env bash
set -euo pipefail
SOURCE=${1:?source directory}
EVIDENCE=${2:?evidence directory}
COMMIT=${3:?source commit}
BASE_RUNNER=${4:?native vertical runner}
base_rc=0
bash "$BASE_RUNNER" "$SOURCE" "$EVIDENCE" "$COMMIT" || base_rc=$?
test -f "$EVIDENCE/test-binary.path" || exit "$base_rc"
BIN=$(cat "$EVIDENCE/test-binary.path")
FILTER=backend::linux_projection_mount_tests::namespace_tests::mounted_projection_namespace_operations
grep -Fx "$FILTER: test" "$EVIDENCE/tests.list"
PAIR_FILTER=backend::linux_projection::namespace_tests::namespace_pair_reenters_after_bootstrap_exit_from_multithreaded_parent
grep -Fx "$PAIR_FILTER: test" "$EVIDENCE/tests.list"
sha256sum "$0" "$BASE_RUNNER" > "$EVIDENCE/namespace-instrument.txt"
pair_rc=0
timeout --kill-after=10s 30s "$BIN" "$PAIR_FILTER" --exact --ignored --nocapture --test-threads=1 > "$EVIDENCE/namespace-pair.log" 2>&1 || pair_rc=$?
cat "$EVIDENCE/namespace-pair.log"
printf 'namespace-pair\t%s\t0\n' "$pair_rc" >> "$EVIDENCE/results.tsv"
rc=0
timeout --kill-after=10s 180s "$BIN" "$FILTER" --exact --ignored --nocapture --test-threads=1 > "$EVIDENCE/namespace-mounted.log" 2>&1 || rc=$?
cat "$EVIDENCE/namespace-mounted.log"
printf 'namespace-mounted\t%s\t0\n' "$rc" >> "$EVIDENCE/results.tsv"
result=0
if [ "$base_rc" -ne 0 ] || [ "$rc" -ne 0 ] || [ "$pair_rc" -ne 0 ]; then result=1; fi
for marker in PROJECTED_PREPARED_READY_STDIO_REAP_OK NAMESPACE_NATIVE_PROVIDER_NORMAL_UNMOUNT_OK 'NAMESPACE_CASE directory_exchange projected=true' 'NAMESPACE_CASE metadata projected=true'; do
  if ! grep -Fq "$marker" "$EVIDENCE/namespace-mounted.log"; then result=1; fi
done
printf 'NAMESPACE_VERTICAL_GATE_EXIT=%s BASE_EXIT=%s NAMESPACE_EXIT=%s PAIR_EXIT=%s\n' "$result" "$base_rc" "$rc" "$pair_rc"
exit "$result"
