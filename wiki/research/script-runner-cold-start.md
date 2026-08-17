---
**Status:** v1, 2026-05-16. Companion to [[research/cold-start]], which covers `node hello.js`. This doc covers the latency users actually feel: `pnpm run <script>` and friends. The levers overlap, but the dominant cost is different.
**Builds on:** [[research/cold-start]], [[research/rust-resolution-feasibility]], [[research/pnpm-specific-behavior]].
---

# Script-runner cold start: pnpm vs npm vs bun vs hypothetical Nub

Script-runner latency is what users feel, not `node hello.js`. Measured: `pnpm run` 194 ms, `npm run` 135 ms, `bun run` 36 ms. An in-process Nub runner projects to ~12–17 ms.

## Question

People rarely type `node hello.js` directly. They type `pnpm run dev`, `npm test`, `pnpm build`. Whatever startup overhead `node` has is multiplied by the package-manager wrapper above it, and that is the perceptual-latency surface that matters.

This doc:

1. Measures pnpm / npm / bun script-runner cold start.
2. Decomposes where the time goes.
3. Projects what a native-Rust Nub script-runner could realistically hit.
4. Asks whether `pacquet` (the official pnpm Rust rewrite) closes the gap, and on what timeline.

## Local measurements (Apple Silicon, macOS 15)

Hyperfine, `--shell=none`, 50 runs after a 10-run warmup, against this fixture (`$D/`):

```json
{ "name":"sb", "version":"0.0.0",
  "scripts": { "hello": "node hello.js" } }
```
```js
// hello.js
console.log("hi")
```

| Invocation | Mean | vs. `bun --version` |
|---|---|---|
| `bun --version`              |   4.3 ms | 1.0× |
| `bun  hello.js` (direct)     |  10.4 ms | 2.4× |
| `node hello.js` (direct)     |  27.6 ms | 6.4× |
| `bun  run hello`             |  36.4 ms | 8.4× |
| `npm  --version`             |  79.3 ms | 18.3× |
| `npm  run hello`             | 134.7 ms | 31.1× |
| `pnpm --version`             | 157.8 ms | 36.5× |
| `pnpm exec node hello.js`    | 191.1 ms | 44.2× |
| `pnpm run hello`             | **194.0 ms** | **44.8×** |

Versions: pnpm 10.15.1, npm 11.9.0, bun 1.3.9, node v24.14.0.

The headline: `pnpm run hello` takes 194 ms — 45× longer than Bun needs to print its own version, 7× longer than calling `node hello.js` directly, and 5× longer than `bun run hello` — on a tiny synthetic fixture with no `node_modules`, no workspaces, no pre/post scripts. Real-world projects are worse. And `pnpm --version` alone is 158 ms: before pnpm has done anything it cares about, it has burned more wall time than `node hello.js` cold-start does end-to-end.

## Cost decomposition: why is `pnpm run` 194 ms?

Reconstructed from the bench numbers and pnpm's architecture:

| Phase | Approx. cost | Source |
|---|---|---|
| pnpm binary launch (embedded Node + image load) | ~30 ms | matches `node -e ''` ~27 ms + a hair |
| pnpm's own JS bootstrap (commander, config, .npmrc parse, workspace probe) | ~120 ms | `pnpm --version` total − Node bootstrap |
| Script lookup: read `package.json`, resolve script, build env, prepend `node_modules/.bin` to `PATH` | ~5–10 ms | typical Node fs/object work |
| Spawn `/bin/sh -c "node hello.js"` | ~2 ms | shell fork+exec on macOS |
| Second Node cold start | ~27 ms | matches direct `node hello.js` |
| Wait + teardown | ~2 ms | hyperfine includes this |
| **Total** | **~186–193 ms** | matches measured 194 ms ✅ |

pnpm ships as a self-contained binary built with `@yao-pkg/pkg`, which embeds Node plus pnpm's JS, so step 1 is unavoidable Node startup. Steps 2–3 are pnpm's userland JS doing config and workspace probing it needs even for a trivial case; step 4 is the shell pnpm must invoke because scripts may contain `&&`, `||`, env-var expansion, or redirection; step 5 is the second full Node cold start.

The 158 ms `pnpm --version` baseline *is* steps 1+2, and it dominates: even with step 5 at zero, `pnpm run` would still cost ~160 ms.

For comparison, `bun run hello`:

| Phase | Approx. cost |
|---|---|
| bun binary launch | ~4 ms |
| bun's script-runner JS (read package.json etc.) | ~6 ms |
| Spawn second bun for `node hello.js` (Bun aliases `node` ⇒ self) | ~26 ms |
| **Total** | **~36 ms** |

Bun saves against pnpm twice over: its own startup is 4 ms vs pnpm's 158 ms (40×), and its second-process spawn lands a fresh Bun (~10 ms) rather than a fresh Node (~27 ms). It still pays the second-process tax, hence 36 ms vs 10 ms direct.

## Why pnpm is structurally locked into this

pnpm can't kill the 158 ms wrapper tax without a rewrite — it is Node's startup plus a decade of npm-ecosystem JS (lifecycle hooks, dep-graph algorithms, workspace logic, registry quirks) that isn't portable in a quarter.

It can't easily kill the shell spawn because script bodies are arbitrary shell, and it can't kill the second Node process because pnpm-the-runner is not a JS runtime — it has no V8 to evaluate the target file in.

The remaining pnpm-side optimization is reducing the ~120 ms userland JS bootstrap, which pnpm has been working on gradually (lazy module loading, deferred config parsing). The floor is Node's own startup: no lower than ~30 ms without leaving Node, which is what `pacquet` is for.

## What a native Nub script-runner could hit

Assuming Nub is the runtime rather than a separate binary on top of one, `nub run hello` flows as:

1. Nub binary starts (~10–15 ms at the v1 target from [[research/cold-start]]).
2. Read and parse `package.json` (~0.5 ms in Rust pre-V8).
3. Look up the `scripts.hello` field, parse the command string.
4. **Decision point:** is the script a simple command Nub can execute in-process?
   - `node X`, `tsx X`, `ts-node X`, `nub X` → drop the shell and the second process. Resolve `X` (via Rust resolver — see [[research/rust-resolution-feasibility]]) and run it in the already-booted Nub runtime. **Zero additional spawns.**
   - `eslint .`, `vitest`, `next dev` (a bin in `node_modules/.bin`) → resolve the bin's `package.json#bin` target. If it's a JS file, as almost all are: same path, run in-process. **Still zero additional spawns.**
   - `&&`, `||`, `$(…)`, redirections → fall back to `sh -c`. Only here is the second-process tax paid.
5. For the in-process path, total cost is Nub startup plus ~1 ms of script-runner work.

Estimated `nub run hello` cost:

| Phase | Cost |
|---|---|
| Nub binary launch | 10–15 ms (v1 target per cold-start.md) |
| Read package.json | 0.5 ms |
| Parse script, identify "simple node X" pattern | 0.1 ms |
| Resolve `hello.js` via Rust resolver, populate cache | 0.5 ms |
| Hand off to V8 (already booted) and run | ~1 ms (compile + exec) |
| **Total** | **~12–17 ms** |

That is ~12× faster than pnpm, ~3× faster than `bun run`, and roughly matches direct `bun hello.js`. The leverage comes from being the runtime AND the runner in one binary. pnpm can't do this because pnpm isn't a runtime; Bun does it partially but still spawns a fresh process for the script body, because its runner is JS-level and can't trivially graft the script into the running isolate.

A more aggressive variant — when the whole script is a JS file Nub could execute, skip the package.json round-trip and resolve directly — saves ~0.5 ms against the table above, not worth diverging from "always check package.json first."

### Caveats — when in-process doesn't work

Cases the in-process path does not cover cleanly: native `.bin` binaries and an exotic `script-shell` still need an exec, and runner-injected env vars, nested package-manager calls, and pre/post scripts need extra handling.

- **Native binaries in `.bin`** (e.g. `esbuild` on macOS, which in some package shapes has both a JS wrapper and a native binary) → must exec.
- **Scripts depending on `INIT_CWD`, `npm_lifecycle_event`, `npm_package_*` env vars set by the runner** → Nub must replicate the full env-injection surface in-process, including for an in-process child. Doable, costs a few ms of fixup.
- **Scripts that re-invoke the package manager** (`pnpm run other` inside a `dev` script) → still works, but the inner `pnpm run` pays pnpm's 194 ms tax. Nub-on-Nub stays fast.
- **Pre/post scripts** (`prehello`, `posthello`) → run in-process serially with the main script, probably in separate isolates for safety; still cheaper than spawning a fresh process because the dyld/OpenSSL/snapshot cost is skipped. Estimated ~5 ms per extra step instead of ~30 ms.
- **`script-shell` set to something exotic** (zsh, fish, pwsh) → honor it; users who set this are explicitly opting into a shell.

## Pacquet impact: does the pnpm Rust rewrite close this gap?

`pacquet` is the official pnpm rewrite in Rust ([pnpm/pacquet][pacquet], merged into the main [pnpm/pnpm][pnpm] monorepo at `/pacquet` on 2026-05-14 — a sign of imminent integration, not abandonment). Active development: 531 commits, 1.15 k stars, 52 contributors.

**Scope** (from the crate layout): a full pnpm-compatible CLI in Rust, staged — Phase 1 is fetch + link (install engine), Phase 2 is resolution. The `run` subcommand exists in `crates/cli/src/cli_args/run.rs` and is trivially implemented: read `package.json`, look up the script, shell out via `pacquet_executor::execute_shell(command)` to `/bin/sh -c <script>`. No lifecycle scripts, no pre/post, no workspace recursion, no `--filter`. A stub today.

**Projected `pacquet run hello`:**

| Phase | Cost |
|---|---|
| pacquet binary launch (pure Rust, static) | ~3–5 ms |
| Read package.json, look up script | ~1 ms |
| Spawn `/bin/sh -c "node hello.js"` | ~2 ms |
| Second Node cold start | ~27 ms |
| **Total** | **~33–35 ms** |

A 5–6× win over pnpm for the wrapper portion alone, roughly matching `bun run hello` (36 ms), eliminating pnpm's 158 ms userland tax entirely. It still spawns a second Node process for the script body: the 27 ms Node cold start is unavoidable for pacquet, which is not a JS runtime.

No public benchmarks for `pacquet run` exist. Realistic ETA to feature-parity stable: 12–18 months (no pre/post, no `--filter`, no workspace recursion, no env-injection parity yet). Nub should not assume pacquet closes the latency gap within the relevant competitive window — and even when it does, Nub's in-process approach still beats it by ~2× because pacquet can't avoid the second-process Node spawn.

### Cost comparison summary

For a trivial `package.json` script that runs `node hello.js`:

| Runner | Today | Realistic floor | Why floored there |
|---|---|---|---|
| `pnpm run` | 194 ms | ~150 ms (with continued JS-side lazification) | Node startup + pnpm's userland JS, both unavoidable in current architecture |
| `npm run`  | 135 ms | ~100 ms | Same shape as pnpm; less workspace machinery to bootstrap |
| `bun run`  | 36 ms  | ~30 ms | Bun's runner runs in JS too, so the script body still spawns a second Bun |
| `pacquet run` | n/a | ~33 ms | Native wrapper, but still spawns Node for the script |
| **`nub run`** | n/a  | **~12–17 ms** | **In-process execution; no second-process spawn for JS scripts** |

The relative ordering among the contenders barely changes; what changes with Nub is the absolute floor. Pacquet removes pnpm's 158 ms wrapper tax and brings it close to `bun run`. Nub, by being the runtime, unlocks another ~2× by collapsing the wrapper and the runtime into one process.

## Where this lever lives in the Nub plans

In-process script execution is a `nub run` concern. The resolver work that makes step 4 above feasible from Rust is covered in [[research/rust-resolution-feasibility]]. The script body's own Nub-startup floor is what [[research/cold-start]] is optimizing.

## Open questions

Four unsettled items: the lifecycle env-var surface, install-time scripts, workspace script orchestration, and whether the runner should accept a file path with no `package.json` present.

- **Lifecycle script edge cases.** Worth a separate audit of exactly which env vars npm/pnpm/yarn set and which user code reads them. `npm_package_config_*` and `npm_lifecycle_event` are the most-cited; the long tail may be small enough to handle once.
- **`postinstall` / install-time scripts.** Out of scope here (this doc is about `nub run`, not `nub install`), but the same in-process logic could apply. Tracked separately; deferred to v1.x.
- **Workspace scripts** (`pnpm -r run build`). The in-process trick still applies per-package, but orchestration (parallelism, topological order, output streaming) is a meaningful piece of work. Not a v1 target.
- **Should `nub run X` work outside a `package.json`?** Bun supports `bun run ./file.js` as a near-alias for `bun ./file.js`. Worth matching for ergonomics; trivial on top of the in-process path.

## Sources

Link definitions for the pacquet and pnpm repositories cited above.

[pacquet]: https://github.com/pnpm/pacquet
[pnpm]: https://github.com/pnpm/pnpm/tree/main/pacquet

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
