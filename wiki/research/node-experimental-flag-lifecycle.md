# Node experimental-flag lifecycle

Node deletes some experimental flags once the feature they gate becomes default-on, and injecting a deleted flag aborts startup before any user code runs. So the inject decision has to be made per binary, pre-spawn.

## Question

Nub injects `--experimental-*` flags to unflag features early, through the feature-matrix `Unflag` bands — `--experimental-import-text` on `[24.19.0, 25.0.0) ∪ [26.5.0, ∞)`, for instance.

Several bands are **open-ended** (`hi = None`). If Node eventually **removes** a flag once its feature is default-on, does injecting it crash Node, and if so, how can a launcher stay robust without paying node-invocation latency on every run?

## Findings (empirically verified)

Probed against installed Node 18.20.4 / 22.15 / 23.x / 24.x / 26.2 / 26.5 and Node's own `src/node_options.cc`.

### 1. An unknown / removed flag is a hard startup abort

`node --experimental-does-not-exist -e 1` → `node: bad option: …`, **exit 9**, before any user code runs. Same for a removed flag in `NODE_OPTIONS` ("not allowed in NODE_OPTIONS").

### 2. Node does NOT keep every unflagged flag — the behavior is per-flag

Two outcomes exist and the split is arbitrary per flag: some spellings survive indefinitely as accepted no-ops, others are deleted outright when the feature stabilizes.

- **Kept as an accepted no-op (safe forever):** `--experimental-fetch`, `--experimental-modules`, `--experimental-global-webcrypto`, `--experimental-repl-await`, `--experimental-abortcontroller`, `--experimental-json-modules`, `--experimental-worker`, `--experimental-report`, `--experimental-wasi-unstable-preview1`, … → exit 0 on Node 26. In `node_options.cc` these are `AddOption("--x", "", NoOp{}, kAllowedInEnvvar)`: accepted, does nothing. ~15 such no-ops, some a decade old.
- **Hard-removed (crash):** `--experimental-policy`, `--experimental-network-imports` → `bad option` on Node 22 / 24 / 26, with no no-op shim left behind.
- **The decisive precedent — `--experimental-permission`:** accepted through Node **23.11.0** (exit 0, with an ExperimentalWarning), then **`bad option` at Node 24.1**, the feature having stabilized to `--permission` with the experimental spelling DELETED. That is the trajectory an open-ended `Unflag` band walks into: it would inject `--experimental-permission` into every Node 24+ and abort startup.

**Conclusion:** whether an unflagged flag survives as a no-op is unpredictable per-flag, so an open-ended `Unflag [lo, ∞)` band is a latent startup crash on whatever future Node removes that flag. Runtime feature-detection cannot save it — `process.allowedNodeEnvironmentFlags` runs *inside* Node, after flag parsing, and a bad flag aborts before any preload loads. The inject/skip decision must be made pre-spawn.

### 3. The accepted-flag set is extractable both ways; the invocation is authoritative

Two ways to read a binary's accepted flags were measured. Scanning the binary's string literals needs no spawn but a match is not proof the option is live; asking Node itself costs one spawn and is ground truth.

- **Byte-scan the binary (no spawn):** option names are string literals in the binary (`experimental-import-text` present in the 26.5 binary, absent in 24.17). Targeted scan ≈ 57 ms. But a string match is not proof the option is a *live accepted* flag, and a false positive is the exact crash this guards against. Rejected as the primary mechanism.
- **`node -e process.allowedNodeEnvironmentFlags` (one spawn):** ≈ 30–42 ms, authoritative (291 flags on 26.5). This is Node's ground truth and a **static property of a binary**, so probe once and cache by (path, mtime).

## Decision / mitigation (implemented)

Option A — **probe and cache the binary's accepted-flag set; inject = (version-band wants it) ∩ (binary accepts it).** Self-correcting: a flag the running Node removed is dropped with no Nub release needed.

- `discovery::accepted_env_flags(node_path) -> Option<BTreeSet<String>>` invokes the `-e` probe once and caches per (path, mtime) in `~/.cache/nub/node-env-flags.json`, alongside the existing `node-discovery.json` version cache. Amortized cost ≈ 0.
- `flags::compute_inject_flags` gained a Stage-4 intersection against that set, name-based, so `--disable-warning=…` matches the bare `--disable-warning`. `None` (probe unavailable) means pure version-band behavior, no regression.
- The universe is sound because Nub injects flags via argv AND propagates them to children via `NODE_OPTIONS`, so every Nub-injected flag is already envvar-allowed — exactly what `allowedNodeEnvironmentFlags` enumerates. Guarded by the `host_node_accepts_every_injected_flag` test: the intersection must drop nothing on a current, supported Node.

## Changelog

Dated revisions, newest first. The 2026-08 entry records a second failure direction the original guard cannot cover.

- 2026-08-07 — The band system has a SECOND failure direction, found by #688. The guard
  above covers "the binary rejects a flag the band wants"; the converse is "the binary
  ACCEPTS a flag the band does not want", which the intersection cannot fix because
  Stage 4 only subtracts. It bites whenever runtime behavior is feature-detected off
  `allowedNodeEnvironmentFlags` while injection is version-banded: Node backported
  `--experimental-import-text` to 24.19.0, so preload-common.cjs's `NATIVE_IMPORT_TEXT`
  went true and stepped aside to a native translator nub had not enabled
  (ERR_UNKNOWN_FILE_EXTENSION on every `with { type: "text" }` import). A backport is
  SEMVER-MINOR on an LTS line and lands with no warning, so any flag read that way needs
  its bands to cover every release that KNOWS the flag, not just the release that
  introduced it. Guarded by `host_node_that_knows_import_text_gets_it_injected`.
- 2026-07-09 — Initial write-up. Triggered by PR #395 (open-ended `--experimental-import-text`
  band). Established that Node hard-removes some experimental flags (`policy`,
  `network-imports`, `permission`→`--permission` at 24.0); implemented the probe-and-intersect
  guard.
