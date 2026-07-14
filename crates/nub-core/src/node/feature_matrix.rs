//! THE canonical feature × Node-version mitigation matrix.
//!
//! This is the single auditable place where nub records, for every user-facing
//! runtime feature, *how* it is made to "just work" across the supported Node
//! range (18.19+) — and that record is **load-bearing**, not parallel
//! documentation. The unflagging logic in [`super::flags`] is *derived* from
//! this table (it iterates [`FEATURES`]); the webstorage gating predicates are
//! derived from it too. There is no second copy of the bands to drift against.
//!
//! ## The three mitigation shapes
//!
//! A feature reaches the user through exactly one of these per Node version:
//!
//! - **`Native`** — the version ships it default-on; nub does nothing.
//! - **`Unflag(flag)`** — the version has it behind an experimental flag that
//!   both *exists* and is still *required*; nub injects `flag`. (Injecting where
//!   the flag does not exist is a hard "bad option" / "not allowed in
//!   NODE_OPTIONS" startup abort — so the bands are tuned to the exact range
//!   where the flag both exists and is needed.)
//! - **`StorageFile`** — webstorage-specific: the global is native (or unflagged)
//!   but still needs a runtime-computed `--localstorage-file=<path>` to
//!   materialize. The path is workspace-keyed, so it lives in `spawn.rs`; this
//!   row records the *floor* and *intent*, not the path.
//! - **`Polyfill { runtime_file, global }`** — no Node version ships it (or the
//!   floor doesn't), so nub installs a JS polyfill from `runtime/<file>`, gated
//!   at runtime by a `typeof <global>` feature-detect. The matrix records the
//!   intent + the floor; it does **not** version-gate the polyfill in Rust (the
//!   polyfill bows out on its own when the global is already present). A unit
//!   test asserts each named file exists and contains the named feature-detect.
//! - **`Absent`** — below the feature's floor nub does nothing and the feature is
//!   simply unavailable (the honest compat-tier outcome).
//!
//! ## Why this is THE place
//!
//! Per AGENTS.md: **"Version-gated claims must trace to code."** Any claim the
//! marketing site makes about "nub gives you `node:sqlite` / EventSource /
//! Temporal on Node X" must trace to a row here. A future auditor reads the whole
//! story — what mitigation, on which versions, and the changelog evidence — from
//! this one file. The per-row `evidence` field and the doc comments on each
//! [`Feature`] carry the citations (PR numbers, unflag versions); they were
//! audited for exactness and must be corrected in place, never silently dropped.
//! NOTE the site sync is MANUAL — nothing programmatic consumes this table for
//! copy, so an edit to a row requires a matching pass over site/content (the
//! Modern APIs tables) by hand.
//!
//! ## How to add a feature
//!
//! 1. Add a `Feature` row to [`FEATURES`] with its `name`, its `mitigations`
//!    (a sorted, non-overlapping list of `(VersionBand, Mitigation)`), and an
//!    `evidence` string citing the changelog/PR.
//! 2. If the mitigation is `Unflag`, that's all — `super::flags::compute_inject_flags`
//!    picks it up automatically by iterating the matrix.
//! 3. If it's a `Polyfill`, ship the `runtime/<file>` with a `typeof <global>`
//!    feature-detect; the `polyfill_rows_are_backed_by_runtime_files` test will
//!    enforce that the file and detect exist.
//! 4. Run `cargo test -p nub-core --lib` — the matrix-invariant tests check the
//!    new row is sorted, non-overlapping, has a `--`-prefixed flag string, and a
//!    unique feature name.

use super::version::NodeVersion;

/// A half-open `[lo, hi)` Node-version band: inclusive low, exclusive high.
/// `hi: None` is open-ended (from `lo` to infinity). A version is *in* the band
/// iff `lo <= v && (hi.is_none() || v < hi)`.
#[derive(Clone)]
pub struct VersionBand {
    pub lo: NodeVersion,
    pub hi: Option<NodeVersion>,
}

impl VersionBand {
    pub const fn new(lo: NodeVersion, hi: Option<NodeVersion>) -> Self {
        Self { lo, hi }
    }

    /// Whether `v` falls in this `[lo, hi)` band.
    pub fn contains(&self, v: &NodeVersion) -> bool {
        *v >= self.lo && self.hi.as_ref().is_none_or(|hi| v < hi)
    }
}

/// How a feature is made to work on a given Node-version band. See the module
/// doc for the full semantics of each shape.
#[derive(Clone, Copy)]
pub enum Mitigation {
    /// The version ships it default-on; nub injects nothing.
    Native,
    /// nub injects this experimental flag (it exists here and is still required).
    Unflag(&'static str),
    /// Webstorage: the global is native/unflagged but still needs a
    /// runtime-computed `--localstorage-file=<path>` (handled in spawn.rs).
    StorageFile,
    /// nub installs a JS polyfill from `runtime/<runtime_file>`, gated at runtime
    /// by a `typeof <global>` feature-detect. The matrix records intent + floor;
    /// it does NOT version-gate the polyfill in Rust.
    Polyfill {
        runtime_file: &'static str,
        global: &'static str,
    },
    /// Below the feature's floor: nub does nothing, the feature is unavailable.
    Absent,
}

/// One user-facing runtime feature and its per-version mitigation.
pub struct Feature {
    /// Stable, human-readable feature name (unique across the table).
    pub name: &'static str,
    /// The per-version mitigation bands, sorted ascending and non-overlapping.
    /// A version's mitigation is the band it falls into (or "nothing" if none).
    pub mitigations: &'static [(VersionBand, Mitigation)],
    /// Changelog / PR citation for the whole feature (the bands' evidence).
    pub evidence: &'static str,
}

impl Feature {
    /// The mitigation that applies to `v` — the band it falls into, if any.
    pub fn mitigation_for(&self, v: &NodeVersion) -> Option<Mitigation> {
        self.mitigations
            .iter()
            .find(|(band, _)| band.contains(v))
            .map(|(_, m)| *m)
    }
}

const fn band(lo: (u32, u32, u32), hi: Option<(u32, u32, u32)>) -> VersionBand {
    let lo = NodeVersion::new(lo.0, lo.1, lo.2);
    let hi = match hi {
        Some((a, b, c)) => Some(NodeVersion::new(a, b, c)),
        None => None,
    };
    VersionBand::new(lo, hi)
}

/// THE matrix. One flat, const-friendly table; every row is a user-facing
/// feature with its per-version mitigation and changelog evidence. Everything
/// version-keyed in [`super::flags`] and the webstorage predicates is derived
/// from this — do not add a parallel table elsewhere.
pub static FEATURES: &[Feature] = &[
    // ── vm.Module / vm.SourceTextModule ────────────────────────────────────
    // Flag added in Node 9.6.0 (#14253) and NEVER unflagged through Node 26 —
    // `vm.Module` stays experimental and the flag is always required. So inject
    // across the ENTIRE supported floor (18.19+). (Previously a min:22.15.0 gate
    // left vm.Module broken on 18.19–22.14.)
    Feature {
        name: "vm-modules",
        mitigations: &[(
            band((18, 19, 0), None),
            Mitigation::Unflag("--experimental-vm-modules"),
        )],
        evidence: "flag added Node 9.6.0 (#14253); never unflagged through Node 26",
    },
    // ── ShadowRealm — DELIBERATELY NOT AUTO-UNFLAGGED (the harmony-flag policy) ──
    // POLICY (load-bearing — enforced by `no_v8_harmony_flag_in_unflag_set` and
    // listed in V8_HARMONY_UNFLAG_DENYLIST below): nub NEVER auto-injects a V8
    // *harmony* / startup-snapshot-affecting flag into the tree-wide NODE_OPTIONS.
    // `--experimental-shadow-realm` implies V8's `--harmony-shadow-realm`, which is
    // part of the isolate's V8-flag hash. NODE_OPTIONS is inherited by the WHOLE
    // process subtree, including EMBEDDED Node runtimes that boot from a V8 context
    // SNAPSHOT (Electron). When the runtime V8-flag hash ≠ the snapshot's, V8's
    // `Context::FromSnapshot` returns empty and `node::CreateEnvironment` aborts on a
    // failed CHECK — an EXC_BREAKPOINT/SIGTRAP crash BEFORE any preload JS runs, so
    // no in-process self-disable can catch it (issue #246: Electron 42.5.1 / Node
    // 24.17 / V8 14.8 crashed deterministically on this flag alone; Electron 37 /
    // V8 13.8 tolerated it — the breakage is V8-snapshot-version-dependent, which is
    // exactly why this is a categorical ban, not a per-version carve-out). Node
    // itself rejects the same flag in Worker execArgv (ERR_WORKER_INVALID_EXEC_ARGV);
    // runtime/worker-polyfill.mjs already strips it for the same harmony reason.
    // Only snapshot-NEUTRAL Node feature flags (vm-modules, eventsource, sqlite, …)
    // are safe to auto-unflag. The cost of NOT injecting: `globalThis.ShadowRealm`
    // (TC39 Stage 3) is no longer auto-provided — deferred until Node ships it
    // default-on, at which point no flag is needed and the snapshot hazard is gone.
    // Full rationale + evidence: wiki/runtime/harmony-flag-policy.md.
    //
    // ── EventSource global ──────────────────────────────────────────────────
    // #51575 ("add EventSource Client"). Landed on the 22.x line at 22.3.0 and was
    // backported to the 20.x LTS line at 20.18.0. The 21.x line was already EOL
    // when it landed, so the flag NEVER existed there — injecting it on any 21.x
    // is a "bad option" startup crash. Never unflagged through 26. Injection set:
    // [20.18.0, 21.0.0) ∪ [22.3.0, ∞). Below 20.18 it is Absent.
    Feature {
        name: "eventsource",
        mitigations: &[
            (
                band((20, 18, 0), Some((21, 0, 0))),
                Mitigation::Unflag("--experimental-eventsource"),
            ),
            (
                band((22, 3, 0), None),
                Mitigation::Unflag("--experimental-eventsource"),
            ),
        ],
        evidence: "#51575; landed 22.3.0, backported 20.18.0; never on 21.x (EOL); never unflagged through 26",
    },
    // ── node:sqlite ─────────────────────────────────────────────────────────
    // Flag added in Node 22.5.0 (#53752). Module unflagged (default import, no
    // flag needed) at 22.13.0 on the 22.x line and at 23.4.0 on the 23.x line. The
    // flag survives as a default-true bool after unflagging, so over-injecting in
    // the unflagged range would be a harmless no-op — but it doesn't EXIST below
    // 22.5.0, so injecting there crashes. Inject only where the flag both exists
    // and is still required: [22.5.0, 22.13.0) ∪ [23.0.0, 23.4.0).
    Feature {
        name: "sqlite",
        mitigations: &[
            (
                band((22, 5, 0), Some((22, 13, 0))),
                Mitigation::Unflag("--experimental-sqlite"),
            ),
            (
                band((23, 0, 0), Some((23, 4, 0))),
                Mitigation::Unflag("--experimental-sqlite"),
            ),
        ],
        evidence: "flag added Node 22.5.0 (#53752); unflagged 22.13.0 (22.x) and 23.4.0 (23.x)",
    },
    // ── Wasm ES-module imports (import of `.wasm`) ──────────────────────────
    // `--experimental-wasm-modules` gates `import` of `.wasm` files (instance-
    // phase `import * as M from './x.wasm'` and source-phase `import source M
    // from './x.wasm'`). The flag has existed since Node 12 — far below nub's
    // 18.19 floor — so it both EXISTS and is REQUIRED across the whole compat
    // range below the default-on cutover. It became default-on (flag → NoOp) at
    // 24.5.0 on the 24.x line and was backported to 22.19.0 on the 22.x line
    // (PR #57038); the 23.x line was EOL before the backport, so it never went
    // default-on there and stays flagged through the end of the 23.x line.
    // Inject only where the flag is still required:
    //   [18.19.0, 22.19.0) ∪ [23.0.0, 24.5.0).
    // (Injecting in the default-on range would be a harmless no-op — the flag
    // survives as a NoOp — but the bands are kept tight, matching sqlite.)
    Feature {
        name: "wasm-modules",
        mitigations: &[
            (
                band((18, 19, 0), Some((22, 19, 0))),
                Mitigation::Unflag("--experimental-wasm-modules"),
            ),
            (
                band((23, 0, 0), Some((24, 5, 0))),
                Mitigation::Unflag("--experimental-wasm-modules"),
            ),
        ],
        evidence: "flag since Node 12; default-on 24.5.0 (24.x) and 22.19.0 (22.x) via #57038; never default-on on 23.x (EOL)",
    },
    // ── addon-modules (ESM import of native .node addons) ────────────────────
    // `--experimental-addon-modules` makes the ESM resolver return format
    // "addon" for a `.node` URL, so `import x from './foo.node'` loads the native
    // addon directly. Flag added on the 23.x line at 23.6.0 and backported to the
    // 22.x LTS line at 22.20.0 (the same dual-line split as node:sqlite). It is
    // Stability 1.0 (Early development) and NEVER default-on through Node 27
    // nightly, so the Unflag bands are open-ended on the high side — there is no
    // native cutover. The flag does not exist below 22.20.0 or on [23.0, 23.6)
    // (the 23.x line before the backport landed), where injecting it is a "bad
    // option" startup abort. Injection set: [22.20.0, 23.0.0) ∪ [23.6.0, ∞).
    Feature {
        name: "addon-modules",
        mitigations: &[
            (
                band((22, 20, 0), Some((23, 0, 0))),
                Mitigation::Unflag("--experimental-addon-modules"),
            ),
            (
                band((23, 6, 0), None),
                Mitigation::Unflag("--experimental-addon-modules"),
            ),
        ],
        evidence: "flag added 23.6.0 (23.x) / 22.20.0 (22.x backport); Stability 1.0; never default-on through Node 27",
    },
    // ── import-text (importing source as text via import attributes) ─────────
    // `import txt from './x.txt' with { type: 'text' }` — the module's default export
    // is the file's string contents. Node gained this behind `--experimental-import-text`
    // on the 26.x line at 26.5.0 (SEMVER-MINOR, #62300); the flag does not exist below
    // 26.5.0, where injecting it is a "bad option" startup abort, and is still flag-gated
    // (never default-on) through Node 27 nightly — so this open-ended Unflag band injects
    // it on [26.5.0, ∞).
    //
    // This row is ONLY the native side. nub ALSO provides import-text on EVERY version
    // that can parse the `with` syntax (Node 18.20+) via a loader polyfill —
    // `loadTextImport` in runtime/transform-core.mjs, dispatched by the load hooks on
    // `importAttributes.type === "text"`. Per the additive contract, the fast-tier hook
    // (preload-common.cjs) feature-detects native support (`--experimental-import-text`
    // in `process.allowedNodeEnvironmentFlags`, i.e. Node 26.5+) and STEPS ASIDE to
    // Node's own textStrategy there — this injected flag is what makes that native path
    // work; below 26.5 the polyfill owns it. The polyfill is a load-hook, not a
    // typeof-global, so it does not fit the `Polyfill` mitigation shape and lives in the
    // runtime rather than as a band here.
    Feature {
        name: "import-text",
        mitigations: &[(
            band((26, 5, 0), None),
            Mitigation::Unflag("--experimental-import-text"),
        )],
        evidence: "flag added Node 26.5.0 (#62300); still flag-gated through Node 27 nightly; nub loader-polyfills below via runtime/transform-core.mjs loadTextImport",
    },
    // ── Module syntax detection (ambiguous ESM `.js`) ────────────────────────
    // `--experimental-detect-module` makes Node parse an ambiguous file — ES-module
    // syntax, a `.js` extension, and a `package.json` with no `"type"` field — and run it
    // as ESM when module syntax is found. Bare Node below the default-on cutover refuses
    // such a file ("To load an ES module, set type: module", exit 1). Introduced on the
    // 21.x line at 21.1.0 (#50096) and backported to the 20.x LTS line at 20.10.0; the
    // 21.0.0 release predates the flag, so injecting it there is a "bad option" startup
    // abort (the same EOL-line hole as eventsource). It became default-on (flag →
    // default-true, no longer required) at 20.19.0 on the 20.x line and 22.7.0 on the 22.x
    // line (#53619, "unflag detect-module"). Inject only where the flag both EXISTS and is
    // still REQUIRED: [20.10.0, 20.19.0) ∪ [21.1.0, 22.7.0). Empirically confirmed —
    // 20.18.0 needs the flag, 20.19.0 detects by default, 21.0.0 rejects the flag, 21.1.0
    // needs it, 22.7.0 detects by default.
    Feature {
        name: "detect-module",
        mitigations: &[
            (
                band((20, 10, 0), Some((20, 19, 0))),
                Mitigation::Unflag("--experimental-detect-module"),
            ),
            (
                band((21, 1, 0), Some((22, 7, 0))),
                Mitigation::Unflag("--experimental-detect-module"),
            ),
        ],
        evidence: "intro 21.1.0 (#50096), backported 20.10.0; default-on 20.19.0 & 22.7.0 (#53619); flag absent on 21.0.0",
    },
    // ── node:ffi (foreign function interface) ────────────────────────────────
    // `--experimental-ffi` gates the `node:ffi` module (loading dynamic libraries and
    // calling native symbols). Added on the 26.x line at 26.1.0 (#62072); the flag does
    // not exist on 26.0.0 or below, where injecting it is a "bad option" startup abort.
    // Snapshot-neutral (a module gate, not a V8 harmony flag — unlike ShadowRealm above),
    // so it is safe to auto-unflag; enabling the flag only makes the module importable,
    // and node:ffi is unsafe solely if user code actively misuses it. Stability 1.0 and
    // still flag-gated (default-off — `import('node:ffi')` throws
    // ERR_UNKNOWN_BUILTIN_MODULE without the flag) through Node 26.5 / 27 nightly, so this
    // open-ended band injects it on [26.1.0, ∞). (node:ffi additionally wants --allow-ffi
    // under the Permission Model; nub does not inject permission flags.)
    Feature {
        name: "ffi",
        mitigations: &[(
            band((26, 1, 0), None),
            Mitigation::Unflag("--experimental-ffi"),
        )],
        evidence: "node:ffi added Node 26.1.0 (#62072); absent 26.0.0; default-off through Node 27 nightly",
    },
    // ── node:vfs (virtual file system) ───────────────────────────────────────
    // `--experimental-vfs` gates the minimal `node:vfs` subsystem. Added on the 26.x line
    // at 26.4.0 (#63115); absent on 26.3.0 and below (injecting there is a "bad option"
    // abort). Snapshot-neutral. Still flag-gated (default-off — `import('node:vfs')` throws
    // ERR_UNKNOWN_BUILTIN_MODULE without the flag) through Node 26.5 / 27 nightly, so this
    // open-ended band injects it on [26.4.0, ∞).
    Feature {
        name: "vfs",
        mitigations: &[(
            band((26, 4, 0), None),
            Mitigation::Unflag("--experimental-vfs"),
        )],
        evidence: "node:vfs added Node 26.4.0 (#63115); absent 26.3.0; default-off through Node 27 nightly",
    },
    // ── node:stream/iter (async-iterator stream adapters) ────────────────────
    // `--experimental-stream-iter` gates the `node:stream/iter` module. Added on the 25.x
    // line at 25.9.0 (#62066); absent on 25.8.x and below (injecting there is a "bad
    // option" abort). Snapshot-neutral. Still flag-gated (default-off —
    // `import('node:stream/iter')` throws ERR_UNKNOWN_BUILTIN_MODULE without the flag)
    // through Node 26.5 / 27 nightly, so this open-ended band injects it on [25.9.0, ∞).
    Feature {
        name: "stream-iter",
        mitigations: &[(
            band((25, 9, 0), None),
            Mitigation::Unflag("--experimental-stream-iter"),
        )],
        evidence: "node:stream/iter added Node 25.9.0 (#62066); absent 25.8.1; default-off through Node 27 nightly",
    },
    // ── WebSocket global ────────────────────────────────────────────────────
    // Flag-gated on [20.10.0, 22.0.0) — the global exists on 20.10+ and all of the
    // 21.x line behind `--experimental-websocket`, then becomes default-on at
    // 22.0.0 (the flag persists as a default-true bool but is no longer required;
    // below 20.10.0 it doesn't exist and is a "bad option"). The experimental
    // warning emitted on 22.0–22.3 is already silenced by nub's
    // `--disable-warning=ExperimentalWarning` (injected ≥20.11). Injection set:
    // [20.10.0, 22.0.0).
    Feature {
        name: "websocket",
        mitigations: &[(
            band((20, 10, 0), Some((22, 0, 0))),
            Mitigation::Unflag("--experimental-websocket"),
        )],
        evidence: "flag-gated [20.10.0, 22.0.0); default-on at 22.0.0",
    },
    // ── Web Storage (localStorage / sessionStorage) ─────────────────────────
    // `--experimental-webstorage` + `--localstorage-file` landed in Node 22.4.0;
    // below it both are "bad option" (compat tier 18.19–22.3 runs without
    // webstorage → Absent). The experimental flag was unflagged (defaults on) in
    // Node 25.0.0 (PR #57666). So:
    //   • [22.4.0, 25.0.0) → Unflag("--experimental-webstorage") + a storage file.
    //   • [25.0.0, ∞)      → StorageFile only (global is native; the
    //                        `--localstorage-file` is STILL required for the global
    //                        to materialize — without it Node leaves localStorage
    //                        undefined and throws on access).
    // The `--localstorage-file=<path>` itself is workspace-keyed and runtime-
    // computed, so it lives in spawn.rs (`compute_localstorage_path`); this row
    // records the floor + the per-band flag intent. The webstorage predicates in
    // `super::flags` are derived from these two bands.
    Feature {
        name: "webstorage",
        mitigations: &[
            (
                band((22, 4, 0), Some((25, 0, 0))),
                // + StorageFile: the --localstorage-file is injected on this band
                // too (the file is required on the whole >=22.4 range — see
                // super::flags::webstorage_supported). The single-valued
                // Mitigation enum records the flag intent; the file rides along.
                Mitigation::Unflag("--experimental-webstorage"),
            ),
            (band((25, 0, 0), None), Mitigation::StorageFile),
        ],
        evidence: "flags added Node 22.4.0; --experimental-webstorage unflagged 25.0.0 (#57666); --localstorage-file always required",
    },
    // ── reportError ─────────────────────────────────────────────────────────
    // WinterTC minimum-common-API global, not shipped by ANY Node version, so it
    // is polyfilled across the whole floor. Installed non-enumerable (invisible to
    // `Object.keys(globalThis)`) to honor the additive contract.
    Feature {
        name: "reportError",
        mitigations: &[(
            band((18, 19, 0), None),
            Mitigation::Polyfill {
                runtime_file: "polyfills.cjs",
                global: "globalThis.reportError",
            },
        )],
        evidence: "WinterTC min-common-API; not in any Node through 26",
    },
    // ── URLPattern ──────────────────────────────────────────────────────────
    // Native on Node 24+; absent on the 22.x floor, where nub polyfills it from
    // the vendored `urlpattern` package. The polyfill feature-detects and bows out
    // when the native global is present, so no Rust version gate is needed.
    Feature {
        name: "URLPattern",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "globalThis.URLPattern",
                },
            ),
            (band((24, 0, 0), None), Mitigation::Native),
        ],
        evidence: "native on Node 24+; polyfilled on the 22.x floor",
    },
    // ── RegExp.escape ───────────────────────────────────────────────────────
    // TC39 proposal; native on Node 24+, polyfilled (spec-faithful port) on 22.x.
    Feature {
        name: "RegExp.escape",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "RegExp.escape",
                },
            ),
            (band((24, 0, 0), None), Mitigation::Native),
        ],
        evidence: "TC39 proposal; native on Node 24+",
    },
    // ── Error.isError ───────────────────────────────────────────────────────
    // Native on Node 24+; polyfilled (~95% fidelity — cross-realm internal-slot
    // unreachable) on 22.x.
    Feature {
        name: "Error.isError",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "Error.isError",
                },
            ),
            (band((24, 0, 0), None), Mitigation::Native),
        ],
        evidence: "native on Node 24+",
    },
    // ── Promise.try ─────────────────────────────────────────────────────────
    // Native on Node 24+; polyfilled on 22.x.
    Feature {
        name: "Promise.try",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "Promise.try",
                },
            ),
            (band((24, 0, 0), None), Mitigation::Native),
        ],
        evidence: "native on Node 24+",
    },
    // ── Float16Array ────────────────────────────────────────────────────────
    // TC39 Stage 4; native on Node 24+, absent on the 22.x floor, polyfilled from
    // the vendored `@petamoriken/float16` package. See
    // wiki/runtime/float16array-polyfill.md.
    Feature {
        name: "Float16Array",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "globalThis.Float16Array",
                },
            ),
            (band((24, 0, 0), None), Mitigation::Native),
        ],
        evidence: "TC39 Stage 4; native on Node 24+",
    },
    // ── Uint8Array base64/hex ───────────────────────────────────────────────
    // TC39 Stage 3 (Uint8Array to/from base64/hex). Native on Node 25+; absent on
    // the whole 18.19–24.x range below, where nub installs a spec-faithful port of
    // the proposal's reference polyfill (verified byte-for-byte against Node native).
    // The toBase64 prototype method is the feature-detect anchor for the whole
    // family (toBase64/fromBase64/setFromBase64/toHex/fromHex/setFromHex).
    Feature {
        name: "Uint8Array.toBase64",
        mitigations: &[
            (
                band((18, 19, 0), Some((25, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "Uint8Array.prototype.toBase64",
                },
            ),
            (band((25, 0, 0), None), Mitigation::Native),
        ],
        evidence: "TC39 Stage 3; native on Node 25+ (V8 13.x)",
    },
    // ── DisposableStack / AsyncDisposableStack / SuppressedError ─────────────
    // TC39 Stage 4 (Explicit Resource Management). The `using`/`await using` SYNTAX
    // is down-leveled by nub's transpiler; these runtime CLASSES are native on Node
    // 24+ and absent below, where nub polyfills them (spec LIFO disposal + the
    // SuppressedError aggregation chain, verified byte-for-byte against native).
    // The well-known symbols Symbol.dispose / Symbol.asyncDispose are present on
    // every supported Node, so only the classes + SuppressedError need polyfilling.
    Feature {
        name: "DisposableStack",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "globalThis.DisposableStack",
                },
            ),
            (band((24, 0, 0), None), Mitigation::Native),
        ],
        evidence: "TC39 Stage 4 (Explicit Resource Management); native on Node 24+",
    },
    Feature {
        name: "AsyncDisposableStack",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "globalThis.AsyncDisposableStack",
                },
            ),
            (band((24, 0, 0), None), Mitigation::Native),
        ],
        evidence: "TC39 Stage 4 (Explicit Resource Management); native on Node 24+",
    },
    Feature {
        name: "SuppressedError",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "polyfills.cjs",
                    global: "globalThis.SuppressedError",
                },
            ),
            (band((24, 0, 0), None), Mitigation::Native),
        ],
        evidence: "TC39 Stage 4 (Explicit Resource Management); native on Node 24+",
    },
    // ── Temporal ────────────────────────────────────────────────────────────
    // Not shipped by ANY Node version, so polyfilled across the whole floor — but
    // installed as a LAZY global by the preload entry (A37: the polyfill is ~18ms
    // to load, so it is deferred behind a getter), NOT eagerly in polyfills.cjs.
    // The feature-detect lives in preload-common.cjs (`installTemporalLazyGlobal`).
    Feature {
        name: "Temporal",
        mitigations: &[(
            band((18, 19, 0), None),
            Mitigation::Polyfill {
                runtime_file: "preload-common.cjs",
                global: "globalThis.Temporal",
            },
        )],
        evidence: "TC39 proposal; not in any Node through 26; lazy global (A37)",
    },
    // ── Worker (browser-shape global) ───────────────────────────────────────
    // The browser-shape `Worker` global is not shipped by any Node version (Node
    // has `node:worker_threads.Worker`, not the browser global), so nub polyfills
    // it across the floor, wrapping `worker_threads.Worker` with EventTarget.
    Feature {
        name: "Worker",
        mitigations: &[(
            band((18, 19, 0), None),
            Mitigation::Polyfill {
                runtime_file: "worker-polyfill.mjs",
                global: "globalThis.Worker",
            },
        )],
        evidence: "browser-shape Worker not in any Node; wraps node:worker_threads",
    },
    // ── navigator global ────────────────────────────────────────────────────
    // The `navigator` object (hardwareConcurrency / userAgent / language / languages
    // / platform, and from 24.5 `locks`) was added in Node 21.0.0 (#47769). Below 21
    // it is wholly absent, so any API hosted ON navigator — chiefly navigator.locks —
    // has no object to attach to. nub backfills the navigator object from
    // navigator-shim.mjs so the surface (and locks, below) reaches the 18.19 floor.
    // The userAgent stays `Node.js/<major>` — never `Nub/…` (brand boundary).
    Feature {
        name: "navigator",
        mitigations: &[
            (
                band((18, 19, 0), Some((21, 0, 0))),
                Mitigation::Polyfill {
                    runtime_file: "navigator-shim.mjs",
                    global: "globalThis.navigator",
                },
            ),
            (band((21, 0, 0), None), Mitigation::Native),
        ],
        evidence: "navigator global added Node 21.0.0 (#47769); backfilled below the 21.0 floor",
    },
    // ── navigator.locks (Web Locks API) ─────────────────────────────────────
    // Native on Node 24.5+ (worker: add web locks api, #58666 — NOT 24.0: the
    // global is undefined on 24.0–24.4); below that nub installs a Web Locks
    // polyfill onto `navigator.locks` (typeof-gated, so the 24.0–24.4 gap is
    // covered either way). On Node < 21 the polyfill rides on the navigator object
    // the `navigator` row backfills (navigator-shim.mjs runs first).
    Feature {
        name: "navigator.locks",
        mitigations: &[
            (
                band((18, 19, 0), Some((24, 5, 0))),
                Mitigation::Polyfill {
                    runtime_file: "navigator-locks.mjs",
                    global: "globalThis.navigator.locks",
                },
            ),
            (band((24, 5, 0), None), Mitigation::Native),
        ],
        evidence: "native on Node 24.5+ (#58666); polyfilled below",
    },
];

/// V8 *harmony* / startup-snapshot-affecting flags that must NEVER appear in the
/// auto-unflag set (the harmony-flag policy — see the ShadowRealm note in
/// [`FEATURES`] and wiki/runtime/harmony-flag-policy.md). These imply a V8
/// `--harmony-*` staging flag that changes the isolate's V8-flag hash; injected
/// into the tree-wide NODE_OPTIONS they crash any embedded Node that boots from a
/// V8 context snapshot (Electron) in `Context::FromSnapshot` → `CreateEnvironment`,
/// before any preload JS can intervene (#246). The
/// `no_v8_harmony_flag_in_unflag_set` test enforces this against the whole matrix.
pub const V8_HARMONY_UNFLAG_DENYLIST: &[&str] = &["--experimental-shadow-realm"];

/// Look up a feature row by name (used by the webstorage-predicate derivation in
/// [`super::flags`]). Panics if the name is absent — it is only ever called with
/// a literal that must exist in [`FEATURES`], so an absent name is a programming
/// error worth failing loudly.
pub fn feature(name: &str) -> &'static Feature {
    FEATURES
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("feature_matrix: no feature named {name:?}"))
}

/// Every experimental flag nub should inject for `node_version`, derived by
/// iterating the matrix: for each feature whose mitigation at this version is
/// `Unflag(flag)`, the flag is in the set. This is the single source of the
/// version-banded unflag logic — [`super::flags::compute_inject_flags`] calls it.
///
/// Order follows table order (vm-modules, eventsource, sqlite, websocket,
/// webstorage…) so the produced flag list is deterministic.
pub fn unflag_flags_for(node_version: &NodeVersion) -> Vec<&'static str> {
    FEATURES
        .iter()
        .filter_map(|f| match f.mitigation_for(node_version) {
            Some(Mitigation::Unflag(flag)) => Some(flag),
            _ => None,
        })
        .collect()
}

/// The lowest Node version at which `flag` EXISTS — the minimum `lo` across every
/// `Unflag(flag)` band in the matrix — or `None` if no band unflags `flag`.
///
/// Once Node ships an experimental flag it keeps ACCEPTING it (as a default-true
/// no-op bool) at every higher version, so the band where Node REJECTS the flag is
/// exactly `[0, floor)`. [`super::flags::strip_unsupported_node_options`] uses this
/// to snip a below-floor gated flag out of an inherited NODE_OPTIONS. Derived from
/// the same matrix the inject path reads — no parallel floor table.
pub fn unflag_floor(flag: &str) -> Option<NodeVersion> {
    FEATURES
        .iter()
        .flat_map(|f| f.mitigations.iter())
        .filter_map(|(band, m)| match m {
            Mitigation::Unflag(f) if *f == flag => Some(band.lo.clone()),
            _ => None,
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn v(major: u32, minor: u32, patch: u32) -> NodeVersion {
        NodeVersion::new(major, minor, patch)
    }

    /// The runtime/ directory ships alongside the crate (located at run time by
    /// `spawn::find_preload` walking up from the binary). For tests it sits at
    /// `<crate>/../../runtime`.
    fn runtime_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime")
    }

    #[test]
    fn no_duplicate_feature_names() {
        let mut seen = std::collections::HashSet::new();
        for f in FEATURES {
            assert!(
                seen.insert(f.name),
                "duplicate feature name in matrix: {:?}",
                f.name
            );
        }
    }

    #[test]
    fn bands_are_sorted_and_non_overlapping_per_feature() {
        for f in FEATURES {
            let bands: Vec<&VersionBand> = f.mitigations.iter().map(|(b, _)| b).collect();
            for pair in bands.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                // Sorted ascending by low bound.
                assert!(
                    a.lo <= b.lo,
                    "feature {:?}: bands not sorted by low bound ({:?} before {:?})",
                    f.name,
                    a.lo,
                    b.lo
                );
                // Non-overlapping: the earlier band must close (Some hi) at or
                // before the next band opens.
                match &a.hi {
                    Some(hi) => assert!(
                        *hi <= b.lo,
                        "feature {:?}: bands overlap (band ending {:?} overlaps band starting {:?})",
                        f.name,
                        hi,
                        b.lo
                    ),
                    None => panic!(
                        "feature {:?}: an open-ended band is followed by another band",
                        f.name
                    ),
                }
            }
        }
    }

    #[test]
    fn every_unflag_flag_starts_with_double_dash() {
        for f in FEATURES {
            for (_, m) in f.mitigations {
                if let Mitigation::Unflag(flag) = m {
                    assert!(
                        flag.starts_with("--"),
                        "feature {:?}: unflag string {:?} must start with '--'",
                        f.name,
                        flag
                    );
                }
            }
        }
    }

    #[test]
    fn polyfill_rows_are_backed_by_runtime_files() {
        // Each Polyfill row must name a runtime/ file that EXISTS and contains a
        // `typeof <global>` feature-detect for the named global — keeping the
        // matrix's polyfill claims honest against the actual shipped JS. (The
        // polyfills stay runtime-feature-detected; this only verifies the detect
        // is present, not that it version-gates.)
        let dir = runtime_dir();
        let mut checked = 0;
        for f in FEATURES {
            for (_, m) in f.mitigations {
                if let Mitigation::Polyfill {
                    runtime_file,
                    global,
                } = m
                {
                    let path = dir.join(runtime_file);
                    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                        panic!(
                            "feature {:?}: runtime file {} not readable: {e}",
                            f.name,
                            path.display()
                        )
                    });
                    let needle = format!("typeof {global}");
                    assert!(
                        src.contains(&needle),
                        "feature {:?}: {} does not contain feature-detect {:?}",
                        f.name,
                        runtime_file,
                        needle
                    );
                    checked += 1;
                }
            }
        }
        // Guard against the table silently losing all polyfill rows.
        assert!(
            checked >= 9,
            "expected >=9 polyfill rows, checked {checked}"
        );
    }

    #[test]
    fn unflag_set_matches_known_boundaries() {
        // Spot-check the derived unflag set at the load-bearing boundaries — this
        // is the same behavior the flags.rs band tests assert, anchored here at
        // the matrix layer they derive from.
        // vm-modules: whole floor.
        assert!(unflag_flags_for(&v(18, 19, 0)).contains(&"--experimental-vm-modules"));
        assert!(unflag_flags_for(&v(26, 2, 0)).contains(&"--experimental-vm-modules"));
        // shadow-realm: DELIBERATELY never injected (the harmony-flag policy —
        // it crashes embedded-Node/Electron via a snapshot-flag-hash mismatch, #246).
        assert!(!unflag_flags_for(&v(18, 19, 0)).contains(&"--experimental-shadow-realm"));
        assert!(!unflag_flags_for(&v(22, 19, 0)).contains(&"--experimental-shadow-realm"));
        assert!(!unflag_flags_for(&v(26, 0, 0)).contains(&"--experimental-shadow-realm"));
        // eventsource: the 21.x hole.
        assert!(!unflag_flags_for(&v(21, 0, 0)).contains(&"--experimental-eventsource"));
        assert!(unflag_flags_for(&v(20, 18, 0)).contains(&"--experimental-eventsource"));
        assert!(unflag_flags_for(&v(22, 3, 0)).contains(&"--experimental-eventsource"));
        // sqlite: two disjoint bands.
        assert!(unflag_flags_for(&v(22, 5, 0)).contains(&"--experimental-sqlite"));
        assert!(!unflag_flags_for(&v(22, 13, 0)).contains(&"--experimental-sqlite"));
        assert!(unflag_flags_for(&v(23, 3, 0)).contains(&"--experimental-sqlite"));
        assert!(!unflag_flags_for(&v(24, 0, 0)).contains(&"--experimental-sqlite"));
        // wasm-modules: two disjoint bands [18.19, 22.19) ∪ [23.0, 24.5).
        // 22.x line: flagged below 22.19, native at/after (backport).
        assert!(unflag_flags_for(&v(18, 19, 0)).contains(&"--experimental-wasm-modules"));
        assert!(unflag_flags_for(&v(20, 18, 0)).contains(&"--experimental-wasm-modules"));
        assert!(unflag_flags_for(&v(22, 13, 0)).contains(&"--experimental-wasm-modules"));
        assert!(unflag_flags_for(&v(22, 18, 0)).contains(&"--experimental-wasm-modules"));
        assert!(!unflag_flags_for(&v(22, 19, 0)).contains(&"--experimental-wasm-modules"));
        // 23.x line: flagged the whole line (never got the backport).
        assert!(unflag_flags_for(&v(23, 2, 0)).contains(&"--experimental-wasm-modules"));
        assert!(unflag_flags_for(&v(24, 4, 0)).contains(&"--experimental-wasm-modules"));
        // 24.5+: native (flag → NoOp).
        assert!(!unflag_flags_for(&v(24, 5, 0)).contains(&"--experimental-wasm-modules"));
        assert!(!unflag_flags_for(&v(26, 0, 0)).contains(&"--experimental-wasm-modules"));
        // addon-modules: [22.20, 23.0) ∪ [23.6, ∞); the [23.0, 23.6) hole and
        // the whole compat tier below 22.20 are excluded (flag doesn't exist).
        let addon = "--experimental-addon-modules";
        assert!(!unflag_flags_for(&v(22, 19, 0)).contains(&addon));
        assert!(unflag_flags_for(&v(22, 20, 0)).contains(&addon));
        assert!(!unflag_flags_for(&v(23, 0, 0)).contains(&addon));
        assert!(!unflag_flags_for(&v(23, 5, 0)).contains(&addon));
        assert!(unflag_flags_for(&v(23, 6, 0)).contains(&addon));
        assert!(unflag_flags_for(&v(26, 2, 0)).contains(&addon));
        // websocket: [20.10, 22.0).
        assert!(!unflag_flags_for(&v(20, 9, 0)).contains(&"--experimental-websocket"));
        assert!(unflag_flags_for(&v(20, 10, 0)).contains(&"--experimental-websocket"));
        assert!(!unflag_flags_for(&v(22, 0, 0)).contains(&"--experimental-websocket"));
        // webstorage flag: only on the 22.4–24.x Unflag band.
        assert!(unflag_flags_for(&v(22, 4, 0)).contains(&"--experimental-webstorage"));
        assert!(unflag_flags_for(&v(24, 99, 0)).contains(&"--experimental-webstorage"));
        assert!(!unflag_flags_for(&v(25, 0, 0)).contains(&"--experimental-webstorage"));
        // import-text: open-ended [26.5.0, ∞); the flag doesn't exist below 26.5.0.
        let it = "--experimental-import-text";
        assert!(!unflag_flags_for(&v(26, 4, 0)).contains(&it));
        assert!(unflag_flags_for(&v(26, 5, 0)).contains(&it));
        assert!(unflag_flags_for(&v(27, 0, 0)).contains(&it));
        assert_eq!(unflag_floor(it), Some(v(26, 5, 0)));
        // detect-module: [20.10.0, 20.19.0) ∪ [21.1.0, 22.7.0). Below the backport
        // floor and at each default-on cutover it is excluded; the 21.0.0 release
        // predates the flag (injecting it is a "bad option" crash — the eventsource hole).
        let dm = "--experimental-detect-module";
        assert!(!unflag_flags_for(&v(20, 9, 0)).contains(&dm));
        assert!(unflag_flags_for(&v(20, 10, 0)).contains(&dm));
        assert!(unflag_flags_for(&v(20, 18, 0)).contains(&dm));
        assert!(!unflag_flags_for(&v(20, 19, 0)).contains(&dm)); // default-on
        assert!(!unflag_flags_for(&v(21, 0, 0)).contains(&dm)); // flag absent (hole)
        assert!(unflag_flags_for(&v(21, 1, 0)).contains(&dm));
        assert!(unflag_flags_for(&v(22, 6, 0)).contains(&dm));
        assert!(!unflag_flags_for(&v(22, 7, 0)).contains(&dm)); // default-on
        assert_eq!(unflag_floor(dm), Some(v(20, 10, 0)));
        // ffi: open-ended [26.1.0, ∞); the flag doesn't exist on 26.0.0.
        let ffi = "--experimental-ffi";
        assert!(!unflag_flags_for(&v(26, 0, 0)).contains(&ffi));
        assert!(unflag_flags_for(&v(26, 1, 0)).contains(&ffi));
        assert!(unflag_flags_for(&v(26, 5, 0)).contains(&ffi));
        assert!(unflag_flags_for(&v(27, 0, 0)).contains(&ffi));
        assert_eq!(unflag_floor(ffi), Some(v(26, 1, 0)));
        // vfs: open-ended [26.4.0, ∞); the flag doesn't exist on 26.3.0.
        let vfs = "--experimental-vfs";
        assert!(!unflag_flags_for(&v(26, 3, 0)).contains(&vfs));
        assert!(unflag_flags_for(&v(26, 4, 0)).contains(&vfs));
        assert!(unflag_flags_for(&v(26, 5, 0)).contains(&vfs));
        assert_eq!(unflag_floor(vfs), Some(v(26, 4, 0)));
        // stream-iter: open-ended [25.9.0, ∞); the flag doesn't exist on 25.8.x.
        let si = "--experimental-stream-iter";
        assert!(!unflag_flags_for(&v(25, 8, 1)).contains(&si));
        assert!(unflag_flags_for(&v(25, 9, 0)).contains(&si));
        assert!(unflag_flags_for(&v(26, 5, 0)).contains(&si));
        assert_eq!(unflag_floor(si), Some(v(25, 9, 0)));
    }

    #[test]
    fn no_v8_harmony_flag_in_unflag_set() {
        // The harmony-flag policy, enforced (not just documented): no flag in
        // V8_HARMONY_UNFLAG_DENYLIST may ever appear in the auto-unflag set, at ANY
        // version. Such flags imply a V8 `--harmony-*` staging flag that changes the
        // isolate's V8-flag hash and crashes embedded Node booting from a context
        // snapshot (Electron) in CreateEnvironment, before any preload JS — #246.
        // Sweep the whole supported range plus the boundary majors so a future matrix
        // edit that re-adds shadow-realm (or any other harmony flag) trips here.
        let versions = [
            v(18, 19, 0),
            v(20, 18, 0),
            v(22, 15, 0),
            v(23, 6, 0),
            v(24, 17, 0),
            v(25, 0, 0),
            v(26, 2, 0),
        ];
        for ver in versions {
            let set = unflag_flags_for(&ver);
            for &banned in V8_HARMONY_UNFLAG_DENYLIST {
                assert!(
                    !set.contains(&banned),
                    "harmony-flag policy violated: {banned:?} is auto-unflagged at Node {ver:?} \
                     — it crashes embedded Node (Electron) via a snapshot-flag-hash mismatch (#246). \
                     See the ShadowRealm note in FEATURES + wiki/runtime/harmony-flag-policy.md."
                );
            }
        }
    }

    #[test]
    fn webstorage_bands_drive_the_predicates() {
        // The webstorage row is the source of `webstorage_supported` /
        // `webstorage_flag_needed`; verify the row's shape so a future edit that
        // breaks the predicates trips here too.
        let ws = feature("webstorage");
        // Below 22.4: no band → unsupported.
        assert!(ws.mitigation_for(&v(22, 3, 0)).is_none());
        // 22.4–24.x: Unflag band.
        assert!(matches!(
            ws.mitigation_for(&v(22, 4, 0)),
            Some(Mitigation::Unflag("--experimental-webstorage"))
        ));
        // 25+: StorageFile band (native global, file still required).
        assert!(matches!(
            ws.mitigation_for(&v(25, 0, 0)),
            Some(Mitigation::StorageFile)
        ));
        assert!(matches!(
            ws.mitigation_for(&v(26, 2, 0)),
            Some(Mitigation::StorageFile)
        ));
    }
}
