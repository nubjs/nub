# Node-version matrix smoke

Augmentation smoke across a big Node-version matrix. nub augments the user's installed Node
through version-banded mechanisms that break **differently** on different Node versions:

- the **fast tier** (Node >= 22.15): sync resolve/load hooks via `module.registerHooks`;
- the **compat tier** (18.19 - 22.14): an async `module.register` loader worker;
- V8-flag and experimental-flag injection, gated per version.

The repo's main CI matrix exercises only a few Node versions and never set up the
**async-loader x sync-hooks collision** that shipped the Turbopack/Tailwind `resolveSync()`
crash (the `nextjs-build-compat` thread / PR #98). This matrix closes that gap.

## What it runs

`run.sh <nub-binary> [--collision-must-pass]` — the lightweight per-version smoke:

- **functional smoke** (every version): `hello.js`, `hello.ts` (TS transpile + a non-erasable
  enum), ESM-imports-CJS, `import.meta.resolve`, Worker threads.
- **async-loader collision** (`fixtures/async-loader-collision/`): the faithful, **bundler-free**
  reproduction of the resolveSync class. User code calls `module.register(<async loader>)`
  while nub's fast-tier sync `registerHooks` resolve hook is active; resolving the loader
  module's own specifier through the sync chain hits Node's `Hooks.resolveSync()` stub, which
  throws `ERR_METHOD_NOT_IMPLEMENTED` on Node-broken versions. (Reduced from `@tailwindcss/node`;
  verified to reproduce identically to the real Turbopack build through the actual nub binary.)

`real-apps.sh <nub-binary> [--require-pass]` — the heavy nightly real-app builds:

- **Next 16 + Tailwind v4 + Turbopack** — the in-the-wild manifestation of the crash.
- **Vite SSR** (`vite build --ssr` + executing the SSR bundle) — a small, dependency-light
  second real toolchain. Chosen over a dev-server/express scaffold for CI reproducibility.

## The Node-version bands (resolveSync crash)

Empirically bisected (see `.fray/nextjs-build-compat.findings/resolvesync-version-band.md`):

| Band | resolveSync stub present? |
|---|---|
| 18.19 - 22.14 (compat tier, no `registerHooks`) | no bug — different code path |
| 22.15 - 22.16+ (v22 LTS) | **BROKEN** (not backported) |
| 23.6 - 23.11 | **BROKEN** |
| 24.1 - 24.11.x | **BROKEN** |
| 24.12+ | fixed |
| 25.0 - 25.1 | **BROKEN** (regression) |
| 25.2+ | fixed |
| 26.x | fixed |

The collision guard is meaningful **only on a Node-broken version** — on a Node-fixed version
it is vacuously green. The matrix therefore runs it across the broken bands (and asserts it
must pass on the fixed ones).

## Important: the engines-redirect guard (nub must run the matrix-selected Node)

nub discovers its Node from `PATH`, but an `engines.node` / `.node-version` / `packageManager`
constraint the PATH-selected Node does NOT satisfy makes nub **reject** it and fall through to the
highest installed Node — so a leg labeled "Node 22.16" would silently run on 26 and **mask the
version-specific bug**. That is the exact false-positive class this matrix exists to prevent, so
the matrix is hardened against it two ways:

1. **Permissive fixtures.** Every fixture (the smoke `package.json`, the scaffolded Next + Vite
   apps) ships `engines.node: ">=18"` — low enough that no matrix leg is ever redirected, and it
   shadows the repo root's `engines.node >= 22.15` (which would otherwise be inherited by upward
   walk and redirect the lower-tier legs).
2. **A version-assertion guard in BOTH runners.** `run.sh` and `real-apps.sh` each probe nub's
   actual `process.version` (from a pin-free dir, so the probe isn't itself redirected) and FAIL
   the leg if it doesn't equal the matrix-selected version. So even a stray pin, a transitive
   `engines` constraint, or a future floor bump can't produce a silent false-green — the leg goes
   RED with a "version redirect" message instead.

## The collision is a hard must-pass on every version

nub recovers the sync-into-async `resolveSync()`/`loadSync()` hop on the Node-broken bands
(the Next.js/Turbopack/Tailwind fix — nub's fast-tier resolve+load hooks catch the
`ERR_METHOD_NOT_IMPLEMENTED` stub throw and fall back to the parent CommonJS resolver). So the
collision guard runs with `--collision-must-pass` on **every** leg: the previously-broken fast-tier
versions (22.15-22.16, 23.6-23.11, 24.1-24.11, 25.0-25.1) now go GREEN, and so do the
Node-fixed (24.12+, 25.2+, 26) and compat-tier (no `registerHooks`) versions. A **RED** collision
on any leg is a real finding — either nub regressed the recovery, or a new broken Node band
appeared that the recovery doesn't cover. Do not re-XFAIL it; surface it.

The fixture also self-guards against silent augmentation-absence: on any Node with
`registerHooks`, it asserts nub wrapped it (`__nubWrapped`) before judging, so a leg can't pass
the collision merely because nub's preload failed to load (it would hard-fail instead).

History: an earlier revision of the fix only patched the resolve hook and gated recovery on a
flag that was false at the loader's own resolution, so it did NOT recover the broken bands — the
matrix was authored XFAIL-on-broken to stay honest about that gap. The reworked fix recovers both
the resolve and load hooks across the whole band, validated on real broken-Node binaries, which is
why every leg is now must-pass.

## Reproduce locally

nub finds its `runtime/` (preload + addon) by walking up from the binary, so the binary must
have `runtime/` as a sibling. The repo-root `target/debug/nub` does (the repo `runtime/` is a
sibling) — but a binary built to an **out-of-tree** `CARGO_TARGET_DIR` (e.g. a worktree's
`/tmp/<slug>-target/fast/nub`) does NOT: running the matrix against it directly yields **zero
augmentation**, and the collision self-guard then hard-fails "augmentation NOT active". Bundle
the binary with `runtime/` as siblings first — exactly what the CI `build` job stages.

```sh
# Build the dev binary once (from the repo root). nub-native is its own workspace
# (excluded from the root one), so the addon builds from inside the crate; its
# .cargo/config.toml routes output back into the shared root target/.
cargo build -p nub-cli
(cd crates/nub-native && cargo build)
mkdir -p runtime/addons && cp target/debug/libnub_native.so runtime/addons/nub-native.node

# Drive nub onto a specific Node via PATH (nub augments the PATH node):
PATH="$HOME/.nvm/versions/node/v24.11.0/bin:$PATH" tests/node-matrix/run.sh target/debug/nub
PATH="$HOME/.nvm/versions/node/v26.2.0/bin:$PATH"  tests/node-matrix/run.sh target/debug/nub --collision-must-pass

# Heavy real-app builds:
PATH="$HOME/.nvm/versions/node/v24.11.0/bin:$PATH" tests/node-matrix/real-apps.sh target/debug/nub
```
