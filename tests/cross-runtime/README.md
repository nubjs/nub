# Cross-runtime Node-compatibility benchmark

This harness runs **Deno's own** Node-compatibility corpus — not a list we curated — identically against `node`, `nub`, `bun`, and `deno`, and reports a non-cherry-picked pass rate per runtime. The point is that nobody can accuse us of picking the tests: the corpus is the one Deno maintains and measures itself against, pinned to an immutable commit you can clone and re-run.

## What's pinned (so it reproduces forever)

- **Corpus:** [`colinhacks/node_test`](https://github.com/colinhacks/node_test) tag **`node-25.8.1`** — a fork of [`denoland/node_test`](https://github.com/denoland/node_test) at commit `c5baef08`, which is the corpus state vendoring **Node v25.8.1**'s test suite. (MIT.)
- **Skip list + per-test expected-failure config:** [`config.jsonc`](./config.jsonc), vendored verbatim from `denoland/deno` (`tests/node_compat/config.jsonc`) at the same point. It lives in Deno's *main* repo, not the corpus repo, so we vendor it here to keep the benchmark self-contained. (MIT.)
- **The runner:** [`run.mjs`](./run.mjs) — a faithful reimplementation of Deno's Rust runner (`tests/node_compat/runner/mod.rs`): same eligible-file set, same `IGNORED_TEST_DIRS` + `config.jsonc` skips, same pass criterion (child exit 0 = pass; an expected-failure entry passes only when it fails in exactly the configured way), same env (`NODE_TEST_KNOWN_GLOBALS=0`, `NODE_SKIP_FLAG_CHECK=1`, `NO_COLOR=1`, per-test `NODE_OPTIONS` from the `// Flags:` directive), same cwd/path model.

## Runtime versions we measured

Match these to reproduce our numbers. **Critically, point both `node` and `nub` at Node 25.8.1** — the corpus targets 25.8.1, so running a different Node introduces version skew that has nothing to do with nub. nub augments whatever Node you give it, so this is just a matter of putting 25.8.1 first on `PATH`.

| Runtime | Version |
|---------|---------|
| node    | v25.8.1 |
| nub     | (augmented, default mode) on Node v25.8.1 |
| bun     | 1.3.14 |
| deno    | 2.8.1 |

## Reproduce it yourself

```sh
# 1. Get Node 25.8.1 (the corpus version) and put it first on PATH.
#    (any method — nvm, the nodejs.org tarball, or `nub`'s own provisioning)
curl -sL https://nodejs.org/dist/v25.8.1/node-v25.8.1-$(uname -s | tr A-Z a-z)-arm64.tar.xz | tar -xJ
export PATH="$PWD/node-v25.8.1-$(uname -s | tr A-Z a-z)-arm64/bin:$PATH"
node --version   # -> v25.8.1

# 2. Clone the pinned corpus.
git clone --depth 1 --branch node-25.8.1 https://github.com/colinhacks/node_test /tmp/node_test

# 3. Run the corpus across all four runtimes (nub here is the built binary).
node tests/cross-runtime/run.mjs --corpus /tmp/node_test
```

Results land in [`results.json`](./results.json). Useful flags: `--runtimes node,nub` to compare a subset, `--limit N` for a quick smoke slice.

## How to read the results

`results.json` carries:

- `perRuntime` — pass / fail / timeout / pct for each runtime on the identical corpus. Use these for the competitive comparison (node + nub at the top, deno and bun below).
- `nubVsNode` — **the honest compat number.** `nubRegressions` lists, *by filename*, every test that real Node 25.8.1 passes and nub fails. That count is nub's only genuine-incompatibility figure — read it verbatim. (`nubFixesVsNode` is the inverse: tests nub passes that this Node fails.) Because both node and nub run on the *same* 25.8.1 binary, this delta is immune to corpus-version skew.
- `fails` — every runtime's failures by filename, including nub's. Publishing our own failures by name is the anti-cherry-pick proof.

Don't report a single headline percentage as "nub's compatibility" — the raw pass% is capped by corpus-vs-binary version alignment and invites denominator games. The defensible statement is the delta: *"nub passes every test real Node passes, minus `nubRegressions.length` documented deltas,"* shown next to the four-runtime competitive bars.

## Attribution

The corpus (`node_test`) and the skip/config file (`config.jsonc`) are Deno's work, redistributed here under the MIT license for reproducible measurement. The competitive figures cross-check against Deno's own published head-to-head in the [Deno v2.8 release notes](https://deno.com/blog/v2.8#nodejs-api-compatibility).
