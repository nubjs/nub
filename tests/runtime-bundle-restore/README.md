# Restoring the dev runtime bundle in a fresh worktree

A fresh worktree checkout has no built native addon and no installed JS
dependencies — both are gitignored build artifacts, not tracked source. The
dev `nub` binary looks for `runtime/addons/nub-native.node` and walks up to 5
parent directories from its own location for `runtime/preload.mjs` (see
`find_public_preload` in `crates/nub-core/src/node/spawn.rs`), so without
these in place the augmentation preload can't engage: TS/JSX/worker/using/
watch tests fail, not because of a code regression, but because the
transpiler and runtime aren't there yet.

## Recipe

Run from the worktree root, with `CARGO_TARGET_DIR` unset (or pointed at
`<worktree>/target`) — the walk-up discovery above only reaches a `runtime/`
directory that's a couple of parents above the binary, which is where
`target/fast/nub` naturally sits relative to the worktree's own `runtime/`.
Pointing `CARGO_TARGET_DIR` outside the worktree breaks both the Makefile's
`target/<profile>/...` copy paths (see `addon`/`addon-fast` in the
`Makefile`) and this walk-up, so don't.

```sh
cd <worktree>
make addon-fast                        # builds crates/nub-native, copies the
                                        # dylib to runtime/addons/nub-native.node
cargo build -p nub-cli --profile fast  # -> target/fast/nub
pnpm install --frozen-lockfile         # installs @oxc-project/runtime, the
                                        # transpile-emit helper package
                                        # (usingCtx, decorate, etc.) — without
                                        # it, `using`/legacy-decorator fixtures
                                        # fail with ERR_MODULE_NOT_FOUND /
                                        # MODULE_NOT_FOUND even though the
                                        # addon itself is fine
```

`make verify` already checks for `node_modules/@oxc-project/runtime/package.json`
and fails with the `pnpm install --frozen-lockfile` instruction if it's
missing — this recipe is the same prerequisite, spelled out for someone
running the integration suite directly rather than through `make verify`.

Confirm augmentation is live before trusting any test run:

```sh
./target/fast/nub some-file.ts   # should transpile + execute, not error on
                                  # a placeholder/missing addon
```
