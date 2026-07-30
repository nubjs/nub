# Node experimental-flag lifecycle

## Question

nub injects `--experimental-*` flags to unflag features early (the feature-matrix
`Unflag` bands, e.g. `--experimental-import-text` on `[26.5.0, ∞)`). Several bands are
**open-ended** (`hi = None`). If Node eventually **removes** a flag once its feature is
default-on, does injecting it crash Node — and if so, how can a launcher stay robust without
paying node-invocation latency on every run?

## Findings (empirically verified)

Probed against installed Node 18.20.4 / 22.15 / 23.x / 24.x / 26.2 / 26.5 and Node's own
`src/node_options.cc`.

### 1. An unknown / removed flag is a hard startup abort
`node --experimental-does-not-exist -e 1` → `node: bad option: …`, **exit 9**, before
any user code runs. Same for a removed flag in `NODE_OPTIONS` ("not allowed in
NODE_OPTIONS").

### 2. Node does NOT keep every unflagged flag — the behavior is per-flag
- **Kept as an accepted no-op (safe forever):** `--experimental-fetch`,
  `--experimental-modules`, `--experimental-global-webcrypto`, `--experimental-repl-await`,
  `--experimental-abortcontroller`, `--experimental-json-modules`, `--experimental-worker`,
  `--experimental-report`, `--experimental-wasi-unstable-preview1`, … → exit 0 on Node 26.
  In `node_options.cc` these are `AddOption("--x", "", NoOp{}, kAllowedInEnvvar)` — accepted,
  does nothing. ~15 such no-ops, some a decade old.
- **Hard-removed (crash):** `--experimental-policy`, `--experimental-network-imports` →
  `bad option` on Node 22 / 24 / 26. No no-op shim was left behind.
- **The decisive precedent — `--experimental-permission`:** accepted through Node **23.11.0**
  (exit 0, with an ExperimentalWarning), then **`bad option` at Node 24.1** — the feature
  stabilized to `--permission` and the experimental spelling was DELETED. This is exactly the
  trajectory an open-ended nub `Unflag` band walks into: it would inject
  `--experimental-permission` into every Node 24+ and abort startup.

**Conclusion:** whether an unflagged flag survives as a no-op is unpredictable per-flag. An
open-ended `Unflag [lo, ∞)` band is a latent startup-crash on whatever future Node removes
that flag. Runtime feature-detection can't save it — `process.allowedNodeEnvironmentFlags`
runs *inside* Node, after flag parsing; a bad flag aborts before any preload loads. The
inject/skip decision must be made pre-spawn.

### 3. The accepted-flag set is extractable both ways; the invocation is authoritative
- **Byte-scan the binary (no spawn):** option names are string literals in the binary
  (`experimental-import-text` present in the 26.5 binary, absent in 24.17). Targeted scan
  ≈ 57 ms. BUT a string match is not proof the option is a *live accepted* flag — a false
  positive is the exact crash we guard against. Rejected as the primary mechanism.
- **`node -e process.allowedNodeEnvironmentFlags` (one spawn):** ≈ 30–42 ms, authoritative
  (291 flags on 26.5). This IS Node's ground truth, and it's a **static property of a
  binary** → probe once, cache by (path, mtime).

## Decision / mitigation (implemented)

Option A — **probe + cache the binary's accepted-flag set; inject = (version-band wants it)
∩ (binary accepts it).** Self-correcting: a flag the running Node removed is dropped, no nub
release needed.

- `discovery::accepted_env_flags(node_path) -> Option<BTreeSet<String>>` invokes the `-e`
  probe once and caches per (path, mtime) in `~/.cache/nub/node-env-flags.json` (sibling to
  the existing `node-discovery.json` version cache). Amortized cost ≈ 0.
- `flags::compute_inject_flags` gained a Stage-4 intersection against that set (name-based,
  so `--disable-warning=…` matches the bare `--disable-warning`). `None` (probe unavailable)
  = pure version-band behavior, no regression.
- The universe is sound because nub injects flags via argv AND propagates them to children
  via `NODE_OPTIONS`, so every nub-injected flag is already envvar-allowed — exactly what
  `allowedNodeEnvironmentFlags` enumerates. Guarded by the `host_node_accepts_every_injected_flag`
  test (the intersection must drop nothing on a current, supported Node).

## Changelog

- 2026-07-09 — Initial write-up. Triggered by PR #395 (open-ended `--experimental-import-text`
  band). Established that Node hard-removes some experimental flags (`policy`,
  `network-imports`, `permission`→`--permission` at 24.0); implemented the probe-and-intersect
  guard.
