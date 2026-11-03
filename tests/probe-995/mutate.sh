#!/usr/bin/env bash
# Runs PR #995's unit tests, then breaks each half of the fix and reruns them.
set -u
cd vendor/aube
S=crates/aube-store/src/lib.rs; P=crates/aube/src/commands/store.rs
run() { # label want test-args...
  local label=$1 want=$2; shift 2
  cargo test "$@" > /tmp/t.log 2>&1; local rc=$?
  local ok=FAIL; { [ "$want" = 0 ] && [ $rc = 0 ]; } || { [ "$want" != 0 ] && [ $rc != 0 ]; } && ok=OK
  echo "[$ok] $label rc=$rc (want $want)"; grep -E '^test .* (ok|FAILED)$|^test result' /tmp/t.log | head -12
}
run "linker tests, fix" 0 -p aube-linker --lib clonedir_
run "prune tests, fix" 0 -p aube --lib extracted_tree_prune_tests
cp $S /tmp/s.bak; cp $P /tmp/p.bak
perl -0pi -e 's/    let digest = index_content_fingerprint\(index\);\n    format!\(\n        "\{coordinate\}\{TREE_KEY_CONTENT_SEPARATOR\}\{\}",\n        &digest\[\.\.TREE_KEY_CONTENT_HEX_LEN\]\n    \)/    let _ = index_content_fingerprint(index);\n    coordinate.to_string()/' $S
cmp -s $S /tmp/s.bak && echo "!! tree_key mutation did not apply"
run "linker test, tree key = coordinate only" 101 -p aube-linker --lib clonedir_never_reuses
cp /tmp/s.bak $S
perl -pi -e 's/let live = reachable\.contains\(coordinate\) && \(recent \|\| newest_variant\);/let live = reachable.contains(coordinate) \&\& (recent || newest_variant || true);/' $P
cmp -s $P /tmp/p.bak && echo "!! prune mutation 1 did not apply"
run "prune tests, keep every variant of a live coordinate" 101 -p aube --lib extracted_tree_prune_tests
cp /tmp/p.bak $P
perl -pi -e "s/if digested && reachable\.contains\(coordinate\) \{/if reachable.contains(coordinate) {/; s/let newest_variant = digested && newest/let newest_variant = newest/" $P
cmp -s $P /tmp/p.bak && echo "!! prune mutation 2 did not apply"
run "prune tests, protect undigested trees as newest" 101 -p aube --lib extracted_tree_prune_tests
cp /tmp/p.bak $P
perl -pi -e 's/let live = reachable\.contains\(coordinate\) && \(recent \|\| newest_variant\);/let live = reachable.contains(coordinate) \&\& recent;/' $P
cmp -s $P /tmp/p.bak && echo "!! prune mutation 3 did not apply"
run "prune tests, drop newest-variant protection" 101 -p aube --lib extracted_tree_prune_tests
cp /tmp/p.bak $P
L=crates/aube-linker/src/materialize.rs; cp $L /tmp/l.bak
perl -pi -e 's/let _ = std::fs::File::open\(tree_src\)\.and_then\(\|f\| f\.set_modified\(now\)\);/let _ = now;/' $L
cmp -s $L /tmp/l.bak && echo "!! refresh mutation did not apply"
run "linker test, no use-stamp refresh" 101 -p aube-linker --lib clonedir_reuse_refreshes
cp /tmp/l.bak $L
cp $L /tmp/l.bak
perl -pi -e 's/let tree_coordinate = subdir\.clone\(\);/let tree_coordinate = self.virtual_store_subdir(dep_path);/' $L
cmp -s $L /tmp/l.bak && echo "!! coordinate mutation did not apply"
run "linker test, per-project tree named by hashed coordinate" 101 -p aube-linker --lib clonedir_never_reuses
cp /tmp/l.bak $L
