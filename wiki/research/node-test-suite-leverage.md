---
**Status:** v2, 2026-08-22. §1–§6 are the 2026-05 design survey; §8 carries the measured Node 26.7.0 numbers from `tests/cross-runtime/`.
**Question:** How should Nub leverage Node's own test suite as the load-bearing compatibility-validation surface, and what should the harness, vendoring strategy, and published metric look like?
**Headline answer:** Mirror Deno's structure: vendor a frozen Node-test mirror behind a git submodule, drive it from a Rust harness with a JSONC allowlist (`ignore` + `flaky` + platform flags), run every test twice (`nub --node` passthrough — target ≥99.5%; augmented `nub` — target ≥95% with documented divergences), publish a per-category pass-rate plus the diff against Node-on-Node. Cap the corpus at the executable-level categories — `parallel/`, `sequential/`, `es-module/`, `async-hooks/`, `message/`, `module-hooks/`, `test-runner/`, `pseudo-tty/`, `abort/` — and explicitly exclude `cctest/`, `addons/`, `js-native-api/`, `node-api/`, `internet/`, `pummel/`, `v8-updates/`, `code-cache/`, `wpt/`, `embedding/`.
**Informs:** the implementation plan's test-harness phase (new sub-phase 9.B), and the published compatibility claim, which a number replaces.
---

# Leveraging Node's own test suite for Nub compat validation

Node's own suite as Nub's compatibility metric: which categories are portable, how Deno and Bun run them, and a dual-mode harness targeting ≥99.5% under `nub --node` and ≥95% augmented.

## 1. TL;DR

Five findings: the corpus is heterogeneous, Deno's harness is the design to copy, Bun ports per module instead of corpus-wide, dual-mode reporting is the right metric, and `test/common` is the landmine.

- **Node's corpus is large but heterogeneous.** `node/test/parallel/` has 4,401 entries; `sequential/` 121, `es-module/` 226, `addons/` 50, `pummel/` 67, `known_issues/` 25, `message/` 22, `abort/` 12, plus ~20 more dirs. Only the JS-executable subset (`parallel`, `sequential`, `es-module`, `async-hooks`, `message`, `module-hooks`, `test-runner`, `pseudo-tty`, `abort`) is portable to Nub; C++ (`cctest`), addons (`addons`, `js-native-api`, `node-api`), V8-updates, embedding, code-cache, internet, pummel are not. Per Node's `test/README.md` (accessed 2026-05-25).
- **Deno's harness is the reference design.** [`tests/node_compat/`](https://github.com/denoland/deno/blob/main/tests/node_compat/README.md): a git submodule (`runner/suite/` → [`denoland/node_test`](https://github.com/denoland/node_test)), a [`config.jsonc`](https://github.com/denoland/deno/blob/main/tests/node_compat/config.jsonc) allowlist with `ignore`, `flaky`, `windows`/`darwin`/`linux` and free-text `reason`, and `mod.rs` driving via `cargo test`. Results surface through [`denoland/node_test_viewer`](https://github.com/denoland/node_test_viewer) at [node-test-viewer.deno.deno.net](https://node-test-viewer.deno.deno.net/). Deno 2.8 reports 76.4% on 4,457 tests ([deno.com/blog/v2.8](https://deno.com/blog/v2.8), 2026-02-13).
- **Bun's harness is per-module ports, not corpus-wide.** [`test/js/node/`](https://github.com/oven-sh/bun/blob/main/test/js/node/harness.ts) holds adapted tests run via `bun:test` through a `harness.ts` shim re-implementing Node's `assert` on Jest matchers. Bun publishes per-module prose ("> 90%" / "100%") on [bun.com/docs/runtime/nodejs-compat](https://bun.com/docs/runtime/nodejs-compat); no rolled-up score. Deno's 2.8 head-to-head pegs Bun 1.3.14 at 40.6% on the same 4,457-test corpus.
- **Recommended: dual-mode + Deno-shape vendoring.** Two pass-rates per release. The `--node` number is the integrity audit ("our passthrough is byte-for-byte Node"); the augmented number is the additivity audit ("our augmentations don't break Node semantics"). Marketing headline is the augmented one; the `--node` number is the credibility floor.
- **The load-bearing landmine is `test/common`.** Every Node test starts with `require('../common')` which (a) leaks-checks globals, (b) parses a `// Flags: …` header and re-spawns the test under those flags, (c) provides ~30 helpers (`mustCall`, `PORT`, `tmpDir`, …). We vendor `common/` verbatim. Tests whose flags include `--expose-internals`, `--allow-natives-syntax`, `--expose-externalize-string`, or any internal-binding flag must auto-skip — Nub has no analogue and shouldn't pretend it does.

## 2. Node test suite anatomy

The `node/test/` tree, counted 2026-05-25 against `nodejs/node` HEAD:

| Dir | Files | CI | Portable to Nub? |
|---|---|---|---|
| `parallel/` | 4,401 | Yes | **Yes** — bulk of stdlib coverage |
| `sequential/` | 121 | Yes | **Yes** — forced-serial stdlib |
| `es-module/` | 226 | Yes | **Yes** — ESM loader, `module.register`, hooks |
| `module-hooks/`, `async-hooks/`, `test-runner/` | — | Yes | **Yes** — hooks API, async tracking, `node:test` |
| `message/` | 22 | Yes | **Conditional** — asserts exact stderr (informational) |
| `pseudo-tty/`, `abort/` | —, 12 | Yes | **Conditional** — pty / `--abort-on-uncaught-exception` |
| `known_issues/` | 25 | Yes | **No** — expected to fail upstream |
| `addons/`, `js-native-api/`, `node-api/` | 50, —, — | Yes | **No** — compiled C++ / N-API |
| `cctest/`, `embedding/` | — | Yes | **No** — `libnode` C++ tests |
| `internet/`, `pummel/`, `v8-updates/`, `code-cache/`, `tick-processor/` | — | No | **No** — network / load / V8 internals |
| `wpt/` | — | Yes | Defer — tracked under WPT proper |
| `fixtures/`, `common/`, `testpy/` | — | _N/A_ | Vendored alongside (infrastructure) |

**Runners.** Two coexist. `tools/test.py` is the Python runner inherited from V8 — parses `.status` files (e.g. `parallel.status`) and dispatches each `.js` to a fresh `node` subprocess. Increasingly Node also drives via `node --test`, particularly for `test-runner/`. The transport is always **"spawn a node binary on a single `.js`, check exit code + stderr."** That's what Nub wires up.

**The `test/common` module** ([test/common/README.md](https://github.com/nodejs/node/blob/main/test/common/README.md)) does four things: (1) snapshots `globalThis` and asserts no leaks at exit, (2) sets umask `0o022`, (3) parses `// Flags: <flags>` at the top of the test file and re-spawns the test under those flags if not present, (4) exports ~30 helpers. We vendor `common/` verbatim; Nub supplies it without patches.

**Flag-header taxonomy.** Grepping `// Flags:` over `test/parallel/`: ~60 use `--expose-internals` (skip), ~40 use `--allow-natives-syntax` (V8 D8 syntax — skip), ~30 use `--expose-gc` (forwardable), the rest use `--no-warnings`, `--unhandled-rejections=throw`, `--input-type=module`, etc. (forwardable). The harness parses this header and passes forwarded flags through to Nub.

## 3. Deno's existing harness

[`tests/node_compat/`](https://github.com/denoland/deno/tree/main/tests/node_compat) (accessed 2026-05-25):

```
tests/node_compat/
├── config.jsonc       # allowlist + per-test config
├── mod.rs             # Rust entrypoint (cargo test)
└── runner/suite/      # git submodule → denoland/node_test
```

The `config.jsonc` shape:

```jsonc
{
  "tests": {
    "parallel/test-foo.js": {},
    "parallel/test-bar.js": { "flaky": true },
    "abort/test-zlib-invalid-internals-usage.js": {
      "ignore": true,
      "reason": "Tests Node.js internal C++ binding (internalBinding('zlib').Zlib)"
    },
    "parallel/test-windows-only.js": {
      "darwin": false, "linux": false, "reason": "Win32 specifics"
    }
  }
}
```

The `mod.rs` entrypoint reads the config, walks `runner/suite/test/` recursively, and runs each `.js` through Deno or skips per the entry. CI fails when an expected-pass entry regresses, or an unlisted file passes and should be promoted. The viewer ([`denoland/node_test_viewer`](https://github.com/denoland/node_test_viewer), TS, last push 2026-05-14) ingests the daily JSON output and renders [node-test-viewer.deno.deno.net/results/latest](https://node-test-viewer.deno.deno.net/results/latest) — per-module table (`fs(358)`, `http(437)`, `stream(213)`, …).

Design choices to copy: separate-repo submodule, JSONC allowlist with reason strings, Rust harness driven by Cargo, out-of-band viewer cadence. **Do not copy** the framing: Deno's score measures whether their userland TS reimplementations of `node:fs`/`node:http`/… match upstream, whereas Nub's measures whether passthrough to actual Node holds plus whether augmentations break Node semantics — which is why our `--node`-mode number should be ~100% by construction.

## 4. Bun's existing harness

[`test/js/node/`](https://github.com/oven-sh/bun/tree/main/test/js/node) under `oven-sh/bun` (accessed 2026-05-25), two patterns:

1. **Per-module Node-ish ports.** Subdirs (`fs/`, `http/`, …) hold `.test.ts` files adapting selected Node tests into `bun:test` via [`harness.ts`](https://github.com/oven-sh/bun/blob/main/test/js/node/harness.ts) — a `createTest(path)` shim wrapping Node's `assert` on `Bun.jest(path)` matchers, hiding shim frames. Adaptation, not verbatim ingestion.
2. **Verbatim subset under `test/js/node/test/{parallel,sequential}/`.** Per Bun's CLAUDE.md (surfaced in PR #25844): "use `bun bd <file>` instead of `bun bd test <file>` since they expect exit code 0" — confirming unmodified Node tests run as plain scripts, success = exit 0.

Bun publishes per-module prose, not a rolled-up corpus number; Deno's 2.8 head-to-head pegged Bun 1.3.14 at 40.6%. Bun's adapted layer is too tied to `bun:test` to lift wholesale; the verbatim-subset approach (bare `.js`, exit-code semantics) is what Nub needs, and Deno already runs it at scale.

## 5. Recommended Nub strategy

The proposed shape: a shallow submodule of `nodejs/node`, a Deno-schema JSONC allowlist, a Rust dispatcher running each test in both modes, two published pass rates, and a static compat page.

### 5.1 Vendoring

**Current decision (2026-05-26):** shallow git submodule of `nodejs/node` directly, `--depth 1` pinned to the current LTS tag.

The blob fetch is one commit's worth (~50 MB), not the full history (~6 GB), so the size objection that previously motivated a separate mirror no longer applies. Cost: re-clone (not pull) when bumping the pinned tag — acceptable for an LTS-cadence bump. Benefit: zero infrastructure (no mirror repo, no update CI job, no second org to keep in sync).

Submodule path: `tests/node-suite/`. Pin to current Node LTS (24); bump on each LTS minor by re-pointing the submodule at the new tag.

**Previously recommended (superseded):** Nub-controlled mirror at `nubjs/node-test-mirror` holding only a `test/`-only snapshot, refreshed by a CI job (`git clone nodejs/node && cp -R node/test ./test && commit`). Rejected because the `--depth 1` shallow submodule reaches the same size budget with no second-repo overhead.

### 5.2 Allowlist (`tests/node-suite-config.jsonc`)

Copy Deno's schema directly. Entries are paths relative to the submodule's `test/` (`parallel/test-fs-readfile.js`). Per-entry: `ignore`, `flaky`, `windows`/`darwin`/`linux`, `reason`, plus two Nub-specific additions:
- `mode`: `"both"` (default) | `"compat-only"` | `"augmented-only"` — for tests that legitimately differ between modes (e.g. tests asserting `.ts` is _not_ executable: `compat-only`).
- `expect`: `"pass"` (default) | `"fail-known"` (replaces `known_issues/`-style). Documented divergences, not green CI signal.

### 5.3 Harness

A Rust crate `nub-test-suite` under `crates/`:

1. Parse the `// Flags:` header. Auto-skip if any flag is in the V8/Node-internal set (`--expose-internals`, `--allow-natives-syntax`, `--expose-externalize-string`, `--allow-sloppy`, …). Forward the rest as Nub execargs.
2. For each non-skipped entry, spawn two children: `nub --node <test>.js` (compat) and `nub <test>.js` (augmented). Per-test wall-clock budget: 60s default, override-able.
3. A test passes iff exit 0 and stderr matches the configured pattern (most: empty; `message/`: `*.out` snapshot match).
4. Categorize failures:
   - **Compat-mode regression** (`--node` failing) — passthrough bug, hard CI failure.
   - **Augmented-mode regression** (augmented failing where compat passes) — augmentation broke Node semantics. Fix or move to `expect: fail-known` with a `reason`.
   - **Both modes failing on a previously-passing test** — Node test or fixture changed; bump submodule.
   - **Flaky** — three retries before failure.
5. Emit JSON: `{ "category": "fs", "total": 358, "passed": 348, "failed": 10, "skipped": 12, "mode": "augmented" }` per category, plus a top-level rollup. Viewer reads this.

### 5.4 Dual-mode reporting

Two numbers per release:

- **Compat-mode pass rate.** Target ≥ 99.5%. The remaining ≤0.5% is tests that legitimately depend on a flag we don't propagate. Below 99.5% is a passthrough bug.
- **Augmented-mode pass rate.** Target ≥ 95%. Documented divergences (e.g. tests asserting `import './file'` resolves _without_ extension when our extensionless-resolver _does_ add one) are listed with `expect: fail-known` and counted alongside the score.

Marketing headline: **"Nub runs N% of Node's own test suite. Set `NODE_COMPAT=1` and the number is 99.X%."**

### 5.5 CI cadence

Four cadences: a fast per-PR subset, a release-blocking full corpus, a nightly full run on `main` that feeds the viewer, and a bump run on each Node LTS.

- **Per-PR fast subset** (~500 tests, 5 min) — keeps the harness working without blocking dev velocity.
- **Per-Nub-release full corpus** (both modes, all categories) — required green for release.
- **Nightly full corpus on `main`** — pushed to viewer. The "is Nub improving" signal.
- **Per-Node-LTS bump** — submodule moves; expect a wave of new failures matching new APIs we haven't augmented.

### 5.6 Public surface

A page like `nub.sh/compat` (static export from CI; no runtime infra needed). Per-category table mirroring Deno's viewer, two columns: compat-mode % and augmented-mode %.

Diff against Node-on-Node (~100% by construction; deviations indicate a flaky upstream or environment, a useful sanity floor). Published compatibility prose is replaced with the live number.

## 6. Implementation plan sketch

This becomes **Phase 9.B "Node test suite ingestion"** in the implementation plan, after the integration-test harness:

- [ ] **9.B.1** — Add `nodejs/node` as a shallow `--depth 1` git submodule at `tests/node-suite/`, pinned to the current LTS tag (`v24.x.y`). No separate mirror repo.
- [ ] **9.B.2** — Document the LTS-bump procedure (re-point submodule, regenerate allowlist diff, re-run dual-mode full corpus locally) in `tests/node-suite/README.md`.
- [ ] **9.B.3** — `crates/nub-test-suite/`: header parser, config reader, dispatcher.
- [ ] **9.B.4** — Auto-skip set (internal-only flags: `--expose-internals`, `--allow-natives-syntax`, `--expose-externalize-string`, any `--enable-internal-*` / internal-binding flag). Encoded in code; rationale lives in §1 above.
- [ ] **9.B.5** — Dual-mode runner: `nub --node` + `nub` per entry, JSON out.
- [ ] **9.B.6** — JSONC config seeded with `ignore: true` for non-portable categories (`addons/`, `cctest/`, `internet/`, `pummel/`, `v8-updates/`, `code-cache/`, `embedding/`, `js-native-api/`, `node-api/`, `tick-processor/`).
- [ ] **9.B.7** — CI integration: per-PR fast, nightly full, release-blocking full.
- [ ] **9.B.8** — Viewer: simple SSG (Vite/Astro), GitHub Pages or Vercel. JSON in, HTML out.
- [ ] **9.B.9** — Replace the published compatibility prose with a live-number reference.
- [ ] **9.B.10** — Backfill `expect: fail-known` for every documented augmentation divergence (TS-extension resolution, env-loading order, etc.), each with a `reason` naming the divergence.

## 7. Sources

Node's own test documentation, Deno's harness and viewer repositories, Deno's 2.8 release numbers, and Bun's harness and per-module prose.

- Node `test/README.md`, `test/common/README.md`, `tools/test.py` — directory taxonomy, `// Flags:` header, Python runner. Accessed 2026-05-25.
- [denoland/deno `tests/node_compat/`](https://github.com/denoland/deno/tree/main/tests/node_compat) — README + config.jsonc — Deno's harness layout and allowlist schema. Accessed 2026-05-25.
- [denoland/node_test](https://github.com/denoland/node_test) — Deno's vendored Node-test mirror.
- [denoland/node_test_viewer](https://github.com/denoland/node_test_viewer) — viewer source (last push 2026-05-14); served at [node-test-viewer.deno.deno.net/results/latest](https://node-test-viewer.deno.deno.net/results/latest).
- [Deno 2.8 release blog](https://deno.com/blog/v2.8) — 76.4% on 4,457; head-to-head Bun 1.3.14 at 40.6%. Published 2026-02-13.
- [denoland/deno discussion #26745](https://github.com/denoland/deno/discussions/26745) — historical "595 of 3681 ported" snapshot, ignored-directories list.
- [oven-sh/bun `test/js/node/harness.ts`](https://github.com/oven-sh/bun/blob/main/test/js/node/harness.ts), [CLAUDE.md](https://github.com/oven-sh/bun/blob/main/CLAUDE.md) — Bun's adapter shim and verbatim-subset convention. Accessed 2026-05-25.
- [bun.com/docs/runtime/nodejs-compat](https://bun.com/docs/runtime/nodejs-compat) — Bun's per-module pass-rate prose. Accessed 2026-05-25.
- Local repo `node/test/` — file counts. Accessed 2026-05-25.

## 8. Measured at Node 26.7.0 (2026-08-22)

The harness in `tests/cross-runtime/` runs the whole Node 26.7.0 test suite from a full checkout against node, nub, bun and deno under named scoring lenses; the Rust gate over `tests/node-compat-config.jsonc` covers 5,278 tests in six directories.

The headline facts, with the lens named each time because the lens is most of the number:

- Node-relative to Node 26.7.0, under Deno's own directory set and skip list: nub 98.1%, deno 74.2%, bun 68.1%; under the bun.com universe (`parallel/` + `sequential/`, nothing skipped): nub 97.9%, deno 71.8%, bun 69.8%; over every executable directory with nothing skipped, including `pseudo-tty/` under a real pty, `ffi/` with its fixture compiled and Node's `wpt/` wrappers: nub 97.1%, deno 68.1%, bun 63.8%. Dropping the 718-test engine-specific class (V8 internals, natives syntax, snapshots, inspector protocol, tracing, V8 error text — `tests/cross-runtime/engine-specific.txt`) moves bun and deno up by 4–7 points and nub by none.
- Node itself passes 99.1% of its own suite on the measuring host once the harness runs it the way Node's runner does (pseudo-terminal + `.out` comparison for `pseudo-tty/`, the compiled `ffi/` fixture, a full checkout so `doc/`, `deps/npm` and `benchmark/` resolve); the 50 residual failures are the system CA store, the network, reporter snapshots and load.
- Version skew is real and large: Node 25.9.0 passes 89.6% of what Node 26.7.0 passes on the 26.7.0 corpus (bun lens), but 94.7% on the 26.3.0 corpus. Any comparison across corpus versions carries that much noise, so the corpus version is part of every published figure.
- How the other two trackers count, stated for comparability: Deno's viewer scores passes over the collected tests minus its `ignore`/platform-`false` entries and counts a `flaky` retry as a pass; bun.com's tracker marks a test passing when a file of that name exists in Bun's repository and runs nothing. The harness's `denoExclusions` and `bunUniverse` lenses reproduce those two denominators on verbatim runs.
- Two harness defects fixed in this revision: bun received none of the tests' `// Flags:` (it ignores `NODE_OPTIONS` but accepts the flags as arguments), which cost it ~4 points; and the Rust gate let `test/common` re-spawn flagged tests under `process.execPath` — plain node — so augmentation was never measured on a flagged test there.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-05-25 — Initial write-up.
- 2026-05-26 — **REVERSAL on §5.1 (vendoring):** swap the recommended Nub-controlled mirror (`nubjs/node-test-mirror`) for a shallow `--depth 1` git submodule of `nodejs/node` directly. A `--depth 1` clone is ~50 MB (one commit's worth of blobs) rather than the full ~6 GB history, so the original size objection that drove the mirror-repo recommendation no longer applies. Prior conclusion preserved in §5.1 under "Previously recommended (superseded)." Also retargeted the cross-references onto the current implementation plan.
- 2026-08-20 — Delinked the `node-test-viewer.deno.dev` citations after that host began 404ing.
- 2026-08-21 — Relinked them: the viewer moved to `node-test-viewer.deno.deno.net` (verified live, serving per-module results). Corrected the viewer's last-push date to 2026-05-14.
- 2026-08-22 — Added §8: the harness now measures the Node 26.7.0 corpus under named lenses (Deno's, bun.com's, full, engine-specific-excluded) with a symmetric retry pass; bun now receives `// Flags:`; the Rust gate passes flags with `NODE_SKIP_FLAG_CHECK=1` and its config grew from 2,554 to 5,278 entries regenerated from the run; Node 25 vs 26 version skew measured (89.6% on 26.7.0, 94.7% on 26.3.0). Same day: the corpus moved from a bare `test/` tree to a full checkout, `pseudo-tty/` runs under a pty with `.out` judging, `ffi/` gets its compiled fixture and `wpt/` is enumerated — Node's own pass rate on the host went from 97.9% to 99.1%.
