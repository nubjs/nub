# Store-prune test harness — the virtual-store / trees garbage collector

This directory exercises `nub store prune`'s mark-and-sweep over the two directory tiers the content-addressable sweep cannot see: the global virtual store (`~/.cache/nub/pm/store`) and the extracted-tree tier (`<store>/v1/trees/`). The Rust unit tests in [`vendor/aube/crates/aube/src/commands/store.rs`](../../vendor/aube/crates/aube/src/commands/store.rs) cover the mark, sweep, and registry functions directly; this harness covers what they structurally cannot — the CLI wiring, a real `nub install` producing real store entries, and whether a project still *resolves* after its store has been pruned.

That last check is the point. A reviewer reads code and a unit test asserts on a set; neither can catch a prune that deletes a directory a live project is symlinked at, because the symptom is a module resolution failing at runtime.

## The loop

```sh
scripts/rust-build.sh build -p nub-cli --profile fast
bash tests/store-prune/run-prune-sweep.sh target/fast/nub
```

The script takes the binary path as its first argument (or `$NUB`), defaulting to `target/fast/nub`. It exits non-zero if any case fails.

**It reassigns `HOME`, `XDG_CACHE_HOME`, and `XDG_DATA_HOME` into a `mktemp` sandbox and unsets `CI`.** That isolation is load-bearing rather than tidiness: the sweep plants orphan entries and then deletes unreachable ones, so pointing it at a real store would collect that machine's real packages. It also needs `CI` unset, because the global virtual store is auto-disabled under CI and the whole feature would sit idle.

## What each case pins

| Case | Why it exists |
| --- | --- |
| Empty registry deletes nothing | An unmigrated store is indistinguishable from an unreferenced one, so the sweep must decline. |
| ...and the guard is what stopped it | A positive control. Without it the case above passes when prune returns early for an unrelated reason — an absent CAS root did exactly that, and the bug survived a full green run. |
| Orphan removed, live entries kept | The basic mark-and-sweep contract. |
| Projects still resolve after prune | Catches the silent wrong answer: a prune that breaks resolution reports success. |
| Deleted project deregistered | A registration must not pin its entries forever after the project is gone. |
| Re-prune is idempotent | A second pass must not keep finding work. |
| Registry survives the sweep | The registry lives inside the store it protects; sweeping it would disable pruning permanently. |
| A hoisted project registers | It owns extracted-tree entries under its own un-hashed names, and only its own registration protects them. Registration used to be gated on the shared store, so this case failed. |
| A warm install registers | The fast path returns before the link phase, so a project whose tree is already current — every project upgrading from a pre-registry version — would otherwise never register. Pinned by asserting the absence of `phase:link ` under `RUST_LOG=debug`; "Already up to date" is not a tell, since the slow path prints it too. |

## Known gaps

**This harness is manual, and it is the only coverage the warm-path registration has.** The Rust unit tests reach the mark, sweep, and registry functions but not the install pipeline, and neither this script nor those tests run in nub CI — the unit tests live under `vendor/aube/**`, which `--all-targets` never builds from the root (the aube-workspace gate). Run it by hand when touching `store.rs`, `aube-store`, or the install fast path.

The trees tier is only built on macOS + APFS, so its sweep is exercised structurally here but its population is not — a Linux run reports `0 entries` for that tier and that is correct, not a failure. Windows is uncovered: the harness is bash, and the registry's stale-entry handling has no Windows-specific path worth a separate probe today.
