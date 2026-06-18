//! Node binary discovery: pin-file walk-up, PATH probe, nvm scan.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use camino::Utf8PathBuf;
use thiserror::Error;

use super::version::{NodeVersion, VersionPin};

/// A resolved Node binary: its path on disk and its parsed version.
/// `pin_source` records where the version was pinned (e.g. `.nvmrc`,
/// `.node-version`) when discovery walked up and found a pin file; it
/// is `None` when the version came from the shell PATH alone, so the
/// hard-error message can reference the pin source cleanly when it's
/// known.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub path: Utf8PathBuf,
    pub version: NodeVersion,
    pub pin_source: Option<String>,
}

impl ResolvedNode {
    pub fn fallback() -> Self {
        Self {
            path: Utf8PathBuf::from("node"),
            version: NodeVersion::new(22, 15, 0),
            pin_source: None,
        }
    }
}

#[derive(Error, Debug)]
pub enum DiscoveryError {
    #[error(
        "pinned Node version {pin} not found\n\
         \x20\x20Active shell Node: {shell_version} (does not satisfy pin)\n\
         \x20\x20Provision it with: nub node install {pin}\n\
         \x20\x20(nub auto-provisions the pinned Node when you run a file; `nub run` / `nubx` use what's already installed.)"
    )]
    PinnedNotFound { pin: String, shell_version: String },

    #[error(
        "no Node binary found on PATH\n\
         \x20\x20Install one with: nub node install (or `nub node install <version>` to pick a version)\n\
         \x20\x20(nub augments your Node — it doesn't bundle one; a pin in .node-version / .nvmrc / engines.node is provisioned automatically.)"
    )]
    NoNodeOnPath,

    /// The discovered Node is older than `NodeVersion::MIN_SUPPORTED`
    /// (18.19.0). No hook API exists below this floor that can carry
    /// Nub's feature surface, so Nub refuses to run. Canonical wording
    /// per `wiki/research/supported-node-versions.md` line 52.
    /// Replaces the prior `TooOld` variant, which gated on the 22.15
    /// fast-path floor — that boundary is now a tier classifier
    /// (sync vs. async hook registration), not an error.
    #[error("{}", format_unsupported(.version, .pin_source.as_deref()))]
    Unsupported {
        version: NodeVersion,
        pin_source: Option<String>,
    },

    /// `package.json#devEngines.runtime` declares only non-Node runtimes
    /// (bun/deno/workerd/…) and the governing entry's effective `onFail` is the
    /// default `error` (or `download` — nub can't download a non-Node runtime).
    /// nub's environment IS Node, so it refuses rather than silently running a
    /// project on a runtime it asked not to be run on. An explicit
    /// `onFail: "warn"`/`"ignore"` falls through to the next pin source instead.
    #[error(
        "this project declares \"{runtime}\" as its runtime (devEngines.runtime) — nub runs Node\n\
         \x20\x20Add a node entry to devEngines.runtime, or set onFail: \"warn\" or \"ignore\" on the entry to let nub continue."
    )]
    RuntimeNotNode { runtime: String },

    #[error("failed to detect Node version: {0}")]
    VersionDetection(String),

    /// A pinned version wasn't on PATH / in nub's store / in nvm, and the
    /// download+install from nodejs.org failed. Names the version + pin source +
    /// the underlying reason so the user can act (network/proxy, or pre-install).
    #[error(
        "ERR_NUB_NODE_PROVISION_FAILED: failed to provision Node {version} (pinned via {pin_source}): {reason}\n\
         \x20\x20Check your network / proxy, or pre-install Node {version} so it's on PATH."
    )]
    ProvisionFailed {
        version: String,
        pin_source: String,
        reason: String,
    },
}

/// Format the `Unsupported` error text. Centralized so the canonical
/// wording (per `wiki/research/supported-node-versions.md` line 52)
/// lives in one place; tests pin to the output of this function.
fn format_unsupported(version: &NodeVersion, pin_source: Option<&str>) -> String {
    match pin_source {
        Some(src) => format!(
            "Nub requires Node 18.19 or newer for runtime augmentation. \
             This project pins Node {version} via {src}. \
             To run it: update the pin to 18.19+ (Nub will run it in compatibility mode), \
             or run plain `node` directly for this project."
        ),
        None => "Nub requires Node 18.19 or newer for runtime augmentation. \
             To run it: upgrade Node to 18.19+ (Nub will run it in compatibility mode), \
             or run plain `node` directly for this project."
            .to_string(),
    }
}

/// Discover the Node binary to use, following the resolution order in
/// `wiki/runtime/node-version-management.md`.
///
/// 1. Resolve the pin chain: `package.json#devEngines.runtime` (#1, may refuse
///    when the declared runtime isn't Node) → `.node-version` (#2) → `.nvmrc`
///    (#3) → `package.json#engines.node` (#4, a resolution range).
///    `devEngines.runtime` `onFail: "warn"` notices print here (once per
///    invocation), then resolution falls through.
/// 2. If no pin: use `node` on PATH.
/// 3. If pinned: PATH node satisfies → nub's own download store
///    (`~/.cache/nub/node/<version>/`) → nvm scan → error. (The download +
///    install step that populates the store, replacing the error, is
///    [`discover_or_provision_node`].)
///
/// The hard floor (Node 18.19) is **not** enforced here — call
/// [`check_min_version`] afterwards. Discovery deliberately stays
/// floor-agnostic so callers like `nub --version` (which only need
/// the binary path) don't trip the version gate.
pub fn discover_node(cwd: &Path) -> Result<ResolvedNode, DiscoveryError> {
    // NODE_EXECUTABLE — the sole version-management override surface
    // (node-version-management.md). An absolute path bypasses pin-file reading,
    // cache, nvm, and download: use that binary directly. Its version is still
    // detected, so the floor check + tier dispatch apply (a Node-16 NODE_EXECUTABLE
    // hard-errors exactly like a Node-16 pin). Brand-compliant: Node doesn't claim
    // the NODE_EXECUTABLE name, so piggybacking on NODE_* is the prescribed hatch.
    if let Some(node) = node_executable_override()? {
        return Ok(node);
    }

    let chain = resolve_pin_chain(cwd)?;
    for warning in &chain.warnings {
        eprintln!("{warning}");
    }

    match chain.pin {
        None => {
            // No pin file — use whatever node is on PATH.
            shell_path_node(None)
        }
        Some((pin_str, parsed_pin, pin_source)) => {
            // Try shell PATH first (covers fnm/Volta/mise auto-switch).
            if let Ok(node) = shell_path_node(Some(pin_source.clone())) {
                if node.version.satisfies(&parsed_pin) {
                    return Ok(node);
                }
                // PATH node doesn't satisfy — try nub's own download store, then nvm.
                if let Some(mut node) = nub_store_node(&parsed_pin) {
                    node.pin_source = Some(pin_source.clone());
                    return Ok(node);
                }
                if let Some(mut node) = scan_nvm(&parsed_pin) {
                    node.pin_source = Some(pin_source);
                    return Ok(node);
                }
                return Err(DiscoveryError::PinnedNotFound {
                    pin: pin_str,
                    shell_version: format!("v{}", node.version),
                });
            }
            // No node on PATH at all — try nub's own store, then nvm.
            if let Some(mut node) = nub_store_node(&parsed_pin) {
                node.pin_source = Some(pin_source.clone());
                return Ok(node);
            }
            if let Some(mut node) = scan_nvm(&parsed_pin) {
                node.pin_source = Some(pin_source);
                return Ok(node);
            }
            Err(DiscoveryError::NoNodeOnPath)
        }
    }
}

/// A NON-SPAWNING, NON-NETWORKING, NON-PROVISIONING variant of [`discover_node`]
/// for latency-critical informational paths — chiefly `nub --version`, which must
/// be near-instant and must never block, spawn a Node subprocess, hit the network,
/// or provision. It resolves the SAME pin chain (cheap file reads) but learns a
/// candidate Node's version only for FREE:
///
/// - a PATH node's version comes from the mtime-valid discovery cache only — never
///   by spawning `node --version` (the multi-second hang `nub --version` exhibited
///   when the box's `node` startup was slow, e.g. a heavy `NODE_OPTIONS`, a
///   network-mounted node, or AV scanning);
/// - a store / nvm node's version comes from its directory name (the name IS the
///   concrete version), so those resolve with no spawn at all.
///
/// Returns `None` whenever the version can't be learned cheaply (PATH node present
/// but uncached, no node found, discovery would error) — the caller then omits its
/// informational line rather than paying for resolution. NEVER use this on a run
/// path: it deliberately under-reports rather than spawn.
pub fn discover_node_cached(cwd: &Path) -> Option<ResolvedNode> {
    // Honor the same NODE_EXECUTABLE override surface, but only when its version
    // is already cached (no spawn).
    if let Some(raw) = env::var_os("NODE_EXECUTABLE") {
        if !raw.is_empty() {
            let path = PathBuf::from(&raw);
            let version = read_version_cache(&path)?;
            let utf8_path = Utf8PathBuf::try_from(path).ok()?;
            return Some(ResolvedNode {
                path: utf8_path,
                version,
                pin_source: Some("NODE_EXECUTABLE".to_string()),
            });
        }
    }

    // resolve_pin_chain can error (RuntimeNotNode); a version query never fails on
    // that — treat any chain error as "nothing to report".
    let chain = resolve_pin_chain(cwd).ok()?;

    match chain.pin {
        None => shell_path_node_cached(None),
        Some((_, parsed_pin, pin_source)) => {
            // PATH node, version from cache only — and only if it satisfies the pin.
            if let Some(node) = shell_path_node_cached(Some(pin_source.clone())) {
                if node.version.satisfies(&parsed_pin) {
                    return Some(node);
                }
            }
            // Store / nvm: version is the directory name, free to read.
            if let Some(mut node) = nub_store_node(&parsed_pin) {
                node.pin_source = Some(pin_source.clone());
                return Some(node);
            }
            if let Some(mut node) = scan_nvm(&parsed_pin) {
                node.pin_source = Some(pin_source);
                return Some(node);
            }
            None
        }
    }
}

/// PATH-node resolution whose version comes ONLY from the mtime-valid discovery
/// cache — never by spawning `node --version`. Returns `None` on a cache miss so
/// the latency-critical caller stays spawn-free. Companion to [`shell_path_node`].
fn shell_path_node_cached(pin_source: Option<String>) -> Option<ResolvedNode> {
    let node_path = which_node().ok()?;
    let version = read_version_cache(&node_path)?;
    let utf8_path = Utf8PathBuf::try_from(node_path).ok()?;
    Some(ResolvedNode {
        path: utf8_path,
        version,
        pin_source,
    })
}

/// [`discover_node`], but when a pinned version can't be satisfied from PATH /
/// nub's store / nvm, DOWNLOAD + install it from nodejs.org (uv-style, silent)
/// and use it. This is the provisioning fire point — call it ONLY from
/// `nub <file>` and the hijack-descendant `node` handler, never from
/// `nub run` / `nub exec` (which keep plain [`discover_node`]), per
/// `wiki/runtime/node-version-management.md` §"Where the version logic fires".
///
/// Exact pins provision the named version directly; range pins (`22`, `22.13`)
/// and aliases (`latest`, `lts`, `lts/<codename>`) resolve to a concrete version
/// against nodejs.org's `index.json` (cached) first. (`rc/<major>` lives on a
/// different mirror — not yet resolved; it surfaces a clear ProvisionFailed.)
pub fn discover_or_provision_node(cwd: &Path) -> Result<ResolvedNode, DiscoveryError> {
    // Fast path: PATH / nub's store / nvm already satisfy the pin (or there's no
    // pin). Aliases never satisfy a concrete check, so they always fall through.
    let discover_err = match discover_node(cwd) {
        Ok(node) => return Ok(node),
        Err(e @ (DiscoveryError::PinnedNotFound { .. } | DiscoveryError::NoNodeOnPath)) => e,
        Err(other) => return Err(other),
    };
    // Re-resolve the chain for the pin to provision. Warnings are deliberately
    // not re-printed — the discover_node call above already emitted them; a
    // refusal can't reach here (discover_node returned it as `other`).
    let Some((raw, pin, pin_source)) = resolve_pin_chain(cwd)?.pin else {
        return Err(discover_err); // no pin → nothing to provision
    };

    let fail = |reason: String| DiscoveryError::ProvisionFailed {
        version: raw.clone(),
        pin_source: pin_source.clone(),
        reason,
    };
    let host = crate::version_management::HostTarget::detect()
        .ok_or_else(|| fail("this host is not a platform nodejs.org publishes".to_string()))?;
    let store_root = cache_dir().ok_or_else(|| {
        fail("could not locate a cache directory (no $HOME / $XDG_CACHE_HOME)".to_string())
    })?;

    // Resolve to a concrete version. Exact is already concrete; everything else
    // resolves against the (cached) dist index.
    let concrete = match &pin {
        VersionPin::Exact(version) => version.clone(),
        _ => {
            let mirror = crate::version_management::resolve_mirror_base(&host);
            let index = crate::version_management::node_index::load_index(&store_root, &mirror)
                .map_err(|e| fail(format!("could not fetch the Node release index: {e:#}")))?;
            match &pin {
                // A devEngines.runtime / engines.node semver range resolves to
                // the newest published version satisfying it
                // (node-version-management.md §Resolution order).
                VersionPin::Range(alternatives) => {
                    crate::version_management::node_index::resolve_range(alternatives, &index)
                }
                _ => crate::version_management::node_index::resolve_spec(&raw, &index),
            }
            .ok_or_else(|| fail("no published Node version matches this pin".to_string()))?
        }
    };

    // The resolved concrete may already be on PATH or in nub's store (e.g. an
    // alias that resolved to the active version) — use it without downloading.
    let concrete_pin = VersionPin::Exact(concrete.clone());
    if let Some(mut node) = nub_store_node(&concrete_pin) {
        node.pin_source = Some(pin_source);
        return Ok(node);
    }
    if let Ok(node) = shell_path_node(Some(pin_source.clone())) {
        if node.version == concrete {
            return Ok(node);
        }
    }

    // Download + install it. Provenance names the pin source; the resolved
    // version is on the same `Using` line, so the raw pin text isn't repeated.
    let version_dir =
        crate::version_management::provision_node(&concrete, &host, &store_root, Some(&pin_source))
            .map_err(|e| fail(format!("{e:#}")))?;
    let bin = store_node_binary(&version_dir).ok_or_else(|| {
        fail("installed, but no node binary was found in the extracted tree".to_string())
    })?;
    Ok(ResolvedNode {
        path: bin,
        version: concrete,
        pin_source: Some(pin_source),
    })
}

/// Enforce the hard floor: Node 18.19.0. Below that, Nub cannot
/// deliver its feature surface (no hook API capable of carrying
/// it exists pre-18.19; see
/// `wiki/research/supported-node-versions.md`). At or above 18.19,
/// the spawn path proceeds and the JS preload picks the
/// hook-registration shape based on the version tier (sync
/// `registerHooks` at 22.15+, async `register()` at 18.19-22.14).
///
/// Name kept as `check_min_version` to minimize churn at call sites;
/// the semantics changed (floor moved from 22.15 to 18.19) but the
/// shape and signature did not.
pub fn check_min_version(node: &ResolvedNode) -> Result<(), DiscoveryError> {
    if node.version.is_supported() {
        Ok(())
    } else {
        Err(DiscoveryError::Unsupported {
            version: node.version.clone(),
            pin_source: node.pin_source.clone(),
        })
    }
}

/// Walk up from `cwd` looking for a pin file. Returns the raw pin string, parsed
/// pin, and the filename that produced it (`.node-version` or `.nvmrc`) for
/// user-facing messages. Bounded by $HOME, filesystem root, and 16 ancestors.
///
/// Precedence within a directory is `.node-version` BEFORE `.nvmrc`, per
/// `wiki/runtime/node-version-management.md` §"Resolution order" (#2 `.node-version`,
/// #3 `.nvmrc`). `.node-version` is the tool-agnostic standard, so it wins when a
/// project carries both. Precedence #1, `package.json#devEngines.runtime`, sits
/// ABOVE both and #4, `package.json#engines.node`, BELOW both —
/// [`resolve_pin_chain`] orders all four; this helper is only the pin-file
/// middle of the chain.
pub fn walk_up_for_pin(cwd: &Path) -> Option<(String, VersionPin, String)> {
    let home = dirs_next::home_dir();
    let mut dir = cwd.to_path_buf();
    let max_depth = 16;

    for _ in 0..max_depth {
        // A `.nvmrc`/`.node-version` shipped inside an installed dependency (under
        // `node_modules`) is that package's own CI pin, not the consumer's. Honoring
        // it would run e.g. a dependency's lifecycle script under the dep's pinned
        // Node instead of the project's — and inherited NODE_OPTIONS flags computed
        // for the project Node (e.g. `--experimental-webstorage`) then abort the
        // older one. npm/pnpm/nvm never let a dependency's bundled pin drive the
        // consumer. Skip pin files inside `node_modules`, but keep walking up so the
        // project pin above it still resolves.
        let in_node_modules = dir.components().any(|c| c.as_os_str() == "node_modules");
        if !in_node_modules {
            for filename in &[".node-version", ".nvmrc"] {
                let pin_path = dir.join(filename);
                if let Ok(content) = fs::read_to_string(&pin_path) {
                    // Strip a leading UTF-8 BOM (str::trim does not — U+FEFF is not
                    // whitespace) so a BOM-prefixed `.nvmrc`/`.node-version` (the
                    // default for many Windows editors) still parses instead of
                    // silently dropping the pin. The serde_json path
                    // (packageManager/devEngines) handles BOMs already.
                    let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
                    let trimmed = content.trim();
                    if !trimmed.is_empty() {
                        if let Ok(pin) = trimmed.parse::<VersionPin>() {
                            tracing::debug!(path = %pin_path.display(), pin = trimmed, "found pin file");
                            return Some((trimmed.to_string(), pin, (*filename).to_string()));
                        }
                        tracing::debug!(
                            path = %pin_path.display(),
                            content = trimmed,
                            "pin file found but unparseable — skipping"
                        );
                    }
                }
            }
        }

        // Stop at home dir or filesystem root.
        if home.as_deref() == Some(&dir) || !dir.pop() {
            break;
        }
    }

    None
}

/// Source label for the `devEngines.runtime` pin channel (precedence #1),
/// shaped like the `package.json#engines.node` label.
const DEV_ENGINES_RUNTIME_SOURCE: &str = "package.json#devEngines.runtime";

/// Source label for the `engines.node` pin channel (precedence #4).
const ENGINES_NODE_SOURCE: &str = "package.json#engines.node";

/// The governing `package.json` for `cwd`, parsed: the WORKSPACE ROOT's manifest
/// when one exists above `cwd`, else the nearest one. This is the one manifest
/// both `devEngines.runtime` (#1) and `engines.node` (#4) read from.
///
/// Workspace-root, not nearest, deliberately: a monorepo pins its Node once at
/// the root (pnpm — the field's model implementation — reads `devEngines.runtime`
/// at the workspace root), and the pin-file walk (`walk_up_for_pin`) already
/// climbs past a member to a root `.node-version`. A nearest-only read here
/// would invert the spec's precedence from a member dir — a root
/// `devEngines.runtime` (#1) invisible while a root `.node-version` (#2) wins.
/// Same scope rule as the PM side (`pm::resolve::root_manifest`); a member's own
/// manifest governs only when no workspace root exists above it.
fn project_manifest(cwd: &Path) -> Option<serde_json::Value> {
    let project = crate::workspace::detect::detect_project(cwd)?;
    match &project.workspace_root {
        Some(ws) if *ws != project.root => {
            let content = fs::read_to_string(ws.join("package.json")).ok()?;
            serde_json::from_str(&content).ok()
        }
        _ => Some(project.manifest),
    }
}

/// Read `package.json#engines.node` (precedence #4, a semver *range*) from the
/// governing manifest ([`project_manifest`]). Returns `(range, source_label)`,
/// or `None` when the manifest has no `engines.node`.
fn read_engines_node(cwd: &Path) -> Option<(String, String)> {
    let range = project_manifest(cwd)?
        .get("engines")?
        .get("node")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)?;
    Some((range, ENGINES_NODE_SOURCE.to_string()))
}

/// One entry of `devEngines.runtime` (the object form is a single entry).
/// Malformed entries (non-object, missing/empty `name`) parse to `None` and are
/// skipped — same conservative posture as an unparseable pin file.
struct RuntimeEntry {
    name: String,
    version: Option<String>,
    on_fail: Option<String>,
}

fn runtime_entry(value: &serde_json::Value) -> Option<RuntimeEntry> {
    let name = value.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let str_field = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(RuntimeEntry {
        name: name.to_string(),
        version: str_field("version"),
        on_fail: str_field("onFail"),
    })
}

/// What `devEngines.runtime` says about this project's runtime.
#[derive(Debug)]
enum RuntimeOutcome {
    /// A node-named entry with a parseable constraint — the top-precedence pin.
    Pin { raw: String, pin: VersionPin },
    /// No applicable constraint — continue down the chain. `warnings` carries
    /// `onFail: "warn"` notices for non-node runtimes (printed once by
    /// [`discover_node`]).
    FallThrough { warnings: Vec<String> },
    /// A non-node runtime whose effective `onFail` is `error`/`download` (or
    /// the default) — refuse to run.
    Refuse { runtime: String },
}

/// Evaluate a `devEngines.runtime` value (object or array) per
/// `wiki/runtime/node-version-management.md` §"Resolution order":
///
/// - The entry whose `name` is `node` is the pin, regardless of array position;
///   non-node entries are then skipped entirely. Its `onFail` is not consulted
///   (`download` is simply nub's native provisioning behavior). A node entry
///   with no `version` (or an unparseable one) is "field present, no
///   constraint" — fall through to the next pin source.
/// - With no node entry, the declared runtimes govern: per entry, effective
///   `onFail` = the explicit value, else the spec's array default (`ignore` for
///   earlier elements, `error` for the last / the object form). `ignore` skips
///   silently, `warn` collects a notice and continues, anything else
///   (`error`, `download`, unrecognized) refuses naming that runtime.
fn evaluate_dev_engines_runtime(field: &serde_json::Value) -> RuntimeOutcome {
    let entries: Vec<RuntimeEntry> = match field {
        serde_json::Value::Array(items) => items.iter().filter_map(runtime_entry).collect(),
        obj @ serde_json::Value::Object(_) => runtime_entry(obj).into_iter().collect(),
        _ => Vec::new(),
    };

    if let Some(node) = entries.iter().find(|e| e.name == "node") {
        return match &node.version {
            Some(raw) => match VersionPin::parse_allowing_ranges(raw) {
                Ok(pin) => RuntimeOutcome::Pin {
                    raw: raw.clone(),
                    pin,
                },
                // Present but unusable — same loud posture as an unusable
                // `packageManager` spec: one stderr warning naming the field and
                // the raw spec, never a silent fall-through (the project stated a
                // pin; ignoring it without a word would be the worst option).
                Err(_) => RuntimeOutcome::FallThrough {
                    warnings: vec![format!(
                        "Warning: ignoring devEngines.runtime version \"{raw}\" — not a version \
                         or range nub can model; continuing with the next version source."
                    )],
                },
            },
            None => RuntimeOutcome::FallThrough {
                warnings: Vec::new(),
            },
        };
    }

    let mut warnings = Vec::new();
    let last = entries.len().saturating_sub(1);
    for (i, entry) in entries.iter().enumerate() {
        let effective =
            entry
                .on_fail
                .as_deref()
                .unwrap_or(if i == last { "error" } else { "ignore" });
        match effective {
            "ignore" => {}
            "warn" => warnings.push(format!(
                "Warning: this project declares \"{}\" as its runtime (devEngines.runtime, \
                 onFail: \"warn\") — nub runs Node; continuing with the next version source.",
                entry.name
            )),
            // "error", "download", or anything unrecognized: the field's default.
            _ => {
                return RuntimeOutcome::Refuse {
                    runtime: entry.name.clone(),
                };
            }
        }
    }
    RuntimeOutcome::FallThrough { warnings }
}

/// Result of the full pin-source chain. Public so the user-facing verbs that
/// must report or act on the SAME resolution the run path uses — `nub node`
/// status / `nub node which` (`resolution_source`) and bare `nub node install`
/// (`manage::install_from_pin`) — go through this chain rather than a private
/// re-derivation that could drift from it.
#[derive(Debug)]
pub struct PinChain {
    /// `(raw, parsed, source_label)` from the winning source, or `None` when no
    /// source pins a version (PATH node applies).
    pub pin: Option<(String, VersionPin, String)>,
    /// Notices collected during resolution (`devEngines.runtime`
    /// `onFail: "warn"`, present-but-unusable version specs) — printed once per
    /// invocation by the entry point ([`discover_node`], or the `nub node`
    /// verbs when they resolve the chain themselves).
    pub warnings: Vec<String>,
}

/// The pin-source chain in spec precedence order
/// (`wiki/runtime/node-version-management.md` §"Resolution order"):
/// `package.json#devEngines.runtime` (#1) → `.node-version` (#2) → `.nvmrc`
/// (#3) → `package.json#engines.node` (#4, a resolution range). Errs with
/// [`DiscoveryError::RuntimeNotNode`] when `devEngines.runtime` declares a
/// non-Node runtime that refuses (its default).
pub fn resolve_pin_chain(cwd: &Path) -> Result<PinChain, DiscoveryError> {
    let mut warnings = Vec::new();
    let manifest = project_manifest(cwd);
    if let Some(field) = manifest
        .as_ref()
        .and_then(|manifest| manifest.get("devEngines"))
        .and_then(|dev| dev.get("runtime"))
    {
        match evaluate_dev_engines_runtime(field) {
            RuntimeOutcome::Pin { raw, pin } => {
                return Ok(PinChain {
                    pin: Some((raw, pin, DEV_ENGINES_RUNTIME_SOURCE.to_string())),
                    warnings,
                });
            }
            RuntimeOutcome::Refuse { runtime } => {
                return Err(DiscoveryError::RuntimeNotNode { runtime });
            }
            RuntimeOutcome::FallThrough { warnings: w } => warnings = w,
        }
    }
    if let Some(pin) = walk_up_for_pin(cwd) {
        return Ok(PinChain {
            pin: Some(pin),
            warnings,
        });
    }
    // #4: engines.node — a resolution *range* ("resolve to the newest available
    // version satisfying the range"). A PATH node inside the range satisfies it
    // like any range pin; provisioning resolves newest-satisfying.
    if let Some((range, source)) = read_engines_node(cwd) {
        match VersionPin::parse_allowing_ranges(&range) {
            Ok(pin) => {
                return Ok(PinChain {
                    pin: Some((range, pin, source)),
                    warnings,
                });
            }
            Err(_) => warnings.push(format!(
                "Warning: ignoring {source} \"{range}\" — not a version or range nub can model; \
                 using node on PATH."
            )),
        }
    }
    Ok(PinChain {
        pin: None,
        warnings,
    })
}

/// Warn when pin sources disagree — a project misconfiguration the user should
/// see (`wiki/runtime/node-version-management.md`: "If sources disagree
/// (`devEngines.runtime` vs pin file, pin file vs `engines.node`), warn"). Two
/// checks, joined with a newline when both fire:
///
/// - when `devEngines.runtime` won, the resolved version vs the pin file
///   (`.node-version`/`.nvmrc`) it overrode;
/// - the resolved version (whatever source won) vs `package.json#engines.node`.
///
/// Returns `None` when nothing was pinned, there's nothing to compare against,
/// the losing spec can't be modeled concretely (alias pin, unparseable range —
/// be conservative, don't cry wolf), or the sources agree.
///
/// `node` is the already-resolved result of [`discover_node`]; its `version` IS
/// the pinned version when `pin_source` is set, so no re-resolution is needed.
pub fn engines_disagreement_warning(cwd: &Path, node: &ResolvedNode) -> Option<String> {
    // Only a pinned resolution can "disagree" — an engines-only project has
    // nothing to contradict.
    let pin_source = node.pin_source.as_deref()?;
    let mut warnings = Vec::new();

    // devEngines.runtime (winner, #1) vs the pin file (#2/#3) it overrode.
    if pin_source == DEV_ENGINES_RUNTIME_SOURCE {
        if let Some((raw, file_pin, file_source)) = walk_up_for_pin(cwd) {
            // An alias pin can't be compared without resolving it first.
            if !matches!(file_pin, VersionPin::Alias(_)) && !node.version.satisfies(&file_pin) {
                warnings.push(format!(
                    "Warning: Node {} is pinned via {pin_source}, but {file_source} pins \
                     \"{raw}\". devEngines.runtime wins; update one so they agree.",
                    node.version
                ));
            }
        }
    }

    // The winning pin vs package.json#engines.node (#4) — unless engines.node
    // IS the winning source (it can't disagree with itself).
    if pin_source != ENGINES_NODE_SOURCE {
        if let Some((range, engines_source)) = read_engines_node(cwd) {
            // Same grammar as the chain (operator-space, `||`, hyphen) — a range
            // the chain could honor must not be silently un-comparable here.
            if let Ok(pin) = VersionPin::parse_allowing_ranges(&range) {
                if !matches!(pin, VersionPin::Alias(_)) && !node.version.satisfies(&pin) {
                    warnings.push(format!(
                        "Warning: Node {} is pinned via {pin_source}, but {engines_source} requires \
                         \"{range}\". The pin wins; update the pin or the engines range so they agree.",
                        node.version
                    ));
                }
            }
        }
    }

    if warnings.is_empty() {
        None
    } else {
        Some(warnings.join("\n"))
    }
}

/// Resolve `node` from the shell PATH and detect its version.
/// `pin_source` is threaded through so the resulting `ResolvedNode`
/// carries the pin filename when one was found by the walk-up.
fn shell_path_node(pin_source: Option<String>) -> Result<ResolvedNode, DiscoveryError> {
    let node_path = which_node()?;
    let version = detect_version(&node_path)?;
    let utf8_path = Utf8PathBuf::try_from(node_path)
        .map_err(|e| DiscoveryError::VersionDetection(e.to_string()))?;
    Ok(ResolvedNode {
        path: utf8_path,
        version,
        pin_source,
    })
}

/// Find `node` on PATH, skipping nub's own PATH shim directories.
fn which_node() -> Result<PathBuf, DiscoveryError> {
    let path_var = env::var_os("PATH").unwrap_or_default();

    for dir in env::split_paths(&path_var) {
        // Skip our own PATH shim directories.
        if let Some(name) = dir.file_name() {
            if name.to_string_lossy().starts_with("nub-node-shim-") {
                continue;
            }
        }

        let candidate = dir.join("node");
        if candidate.is_file() {
            return Ok(candidate);
        }
        #[cfg(windows)]
        {
            let candidate_exe = dir.join("node.exe");
            if candidate_exe.is_file() {
                return Ok(candidate_exe);
            }
        }
    }
    Err(DiscoveryError::NoNodeOnPath)
}

/// Run `node --version` and parse the output, with a disk cache
/// keyed on the binary's path + mtime to avoid spawning on repeat calls.
fn detect_version(node_path: &Path) -> Result<NodeVersion, DiscoveryError> {
    if let Some(cached) = read_version_cache(node_path) {
        return Ok(cached);
    }

    let output = Command::new(node_path)
        .arg("--version")
        .output()
        .map_err(|e| DiscoveryError::VersionDetection(format!("{node_path:?}: {e}")))?;

    if !output.status.success() {
        return Err(DiscoveryError::VersionDetection(format!(
            "{node_path:?} --version exited with {}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout
        .trim()
        .parse::<NodeVersion>()
        .map_err(|e| DiscoveryError::VersionDetection(e.to_string()))?;

    write_version_cache(node_path, &version);
    Ok(version)
}

/// Resolve the `NODE_EXECUTABLE` override, if set. Split from the env read so the
/// resolution is unit-testable without mutating the process environment.
fn node_executable_from(
    raw: Option<std::ffi::OsString>,
) -> Result<Option<ResolvedNode>, DiscoveryError> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(raw);
    // Detect the version (spawns `<path> --version`, mtime-cached). A bad path /
    // non-Node binary surfaces a clear VersionDetection error.
    let version = detect_version(&path)?;
    let utf8_path =
        Utf8PathBuf::try_from(path).map_err(|e| DiscoveryError::VersionDetection(e.to_string()))?;
    Ok(Some(ResolvedNode {
        path: utf8_path,
        version,
        // Name the override as the source so the floor error attributes it.
        pin_source: Some("NODE_EXECUTABLE".to_string()),
    }))
}

fn node_executable_override() -> Result<Option<ResolvedNode>, DiscoveryError> {
    node_executable_from(env::var_os("NODE_EXECUTABLE"))
}

/// nub's cache root (`$XDG_CACHE_HOME/nub` or `~/.cache/nub`). Public so the
/// `nub node` command group can locate the store + index cache without
/// reimplementing the path logic.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs_next::home_dir().map(|h| h.join(".cache")))?;
    Some(base.join("nub"))
}

/// nub's own Node download store (`<cache_dir>/node/`), where each subdirectory
/// name IS the concrete installed version. Public for the `nub node` command
/// group (`ls`/`uninstall`/`install` all key off this dir).
pub fn node_store_dir() -> Option<PathBuf> {
    Some(cache_dir()?.join("node"))
}

fn read_version_cache(node_path: &Path) -> Option<NodeVersion> {
    let cache = cache_dir()?.join("node-discovery.json");
    let content = fs::read_to_string(&cache).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;
    let key = node_path.to_string_lossy();
    let entry = data.get(key.as_ref())?;
    let cached_mtime = entry.get("mtime")?.as_u64()?;
    let cached_version = entry.get("version")?.as_str()?;

    let actual_mtime = fs::metadata(node_path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    if cached_mtime == actual_mtime {
        cached_version.parse().ok()
    } else {
        None
    }
}

fn write_version_cache(node_path: &Path, version: &NodeVersion) {
    let Some(dir) = cache_dir() else { return };
    let _ = fs::create_dir_all(&dir);
    let cache = dir.join("node-discovery.json");

    let mut data: serde_json::Value = fs::read_to_string(&cache)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let mtime = fs::metadata(node_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let key = node_path.to_string_lossy().to_string();
    data[key] = serde_json::json!({
        "version": version.to_string(),
        "mtime": mtime,
    });

    let _ = fs::write(
        &cache,
        serde_json::to_string_pretty(&data).unwrap_or_default(),
    );
}

/// Scan the nvm install directory for a version matching the pin.
fn scan_nvm(pin: &VersionPin) -> Option<ResolvedNode> {
    let nvm_dir = nvm_dir()?;
    let versions_dir = nvm_dir.join("versions").join("node");

    let entries = fs::read_dir(&versions_dir).ok()?;
    let mut candidates: Vec<(NodeVersion, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name_str = name.to_str()?;
            let version = name_str.parse::<NodeVersion>().ok()?;
            let bin = entry.path().join("bin").join("node");
            if bin.is_file() {
                Some((version, bin))
            } else {
                None
            }
        })
        .filter(|(v, _)| v.satisfies(pin))
        .collect();

    // Pick the highest matching version.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0.clone()));

    candidates.into_iter().next().and_then(|(version, path)| {
        let utf8_path = Utf8PathBuf::try_from(path).ok()?;
        Some(ResolvedNode {
            path: utf8_path,
            version,
            // Caller (`discover_node`) overwrites this with the pin
            // filename when it had one; left `None` here so this
            // helper stays usable in isolation.
            pin_source: None,
        })
    })
}

/// The `node` binary inside one of nub's stock-dist version directories:
/// `bin/node` on unix, `node.exe` at the dir root on Windows (the layout
/// `nodejs.org/dist` tarballs extract to).
fn store_node_binary(version_dir: &Path) -> Option<Utf8PathBuf> {
    [
        version_dir.join("bin").join("node"),
        version_dir.join("node.exe"),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .and_then(|p| Utf8PathBuf::try_from(p).ok())
}

/// Look up a Node satisfying `pin` in nub's own download store
/// (`~/.cache/nub/node/<version>/`, where the directory name IS the concrete
/// version — `wiki/runtime/node-version-management.md` §"State 1: Cache hit").
/// On a hit the spawn is silent (no notice). Returns the highest cached version
/// satisfying the pin. Parameterized over `store` so it's testable without
/// mutating the process env (XDG_CACHE_HOME); `nub_store_node` is the wrapper.
fn nub_store_node_in(store: &Path, pin: &VersionPin) -> Option<ResolvedNode> {
    let mut candidates: Vec<(NodeVersion, Utf8PathBuf)> = fs::read_dir(store)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let version = entry.file_name().to_str()?.parse::<NodeVersion>().ok()?;
            let bin = store_node_binary(&entry.path())?;
            Some((version, bin))
        })
        .filter(|(v, _)| v.satisfies(pin))
        .collect();

    // Highest matching version wins (mirrors scan_nvm).
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0.clone()));
    candidates
        .into_iter()
        .next()
        .map(|(version, path)| ResolvedNode {
            path,
            version,
            // Caller overwrites with the pin filename; left None for isolation.
            pin_source: None,
        })
}

/// `nub_store_node_in` against nub's real store at `~/.cache/nub/node/`.
fn nub_store_node(pin: &VersionPin) -> Option<ResolvedNode> {
    nub_store_node_in(&cache_dir()?.join("node"), pin)
}

/// Resolve the nvm install directory.
fn nvm_dir() -> Option<PathBuf> {
    // $NVM_DIR if set, otherwise ~/.nvm
    if let Some(dir) = env::var_os("NVM_DIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Some(path);
        }
    }
    let home = dirs_next::home_dir()?;
    let default = home.join(".nvm");
    if default.is_dir() {
        Some(default)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_node_finds_something() {
        // This test requires node on PATH. Skip gracefully if not present.
        match which_node() {
            Ok(path) => assert!(path.is_file()),
            Err(DiscoveryError::NoNodeOnPath) => {
                eprintln!("skipping: no node on PATH");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn detect_version_works() {
        if let Ok(path) = which_node() {
            let version = detect_version(&path).unwrap();
            assert!(version.major() >= 18, "expected Node 18+, got {version}");
        }
    }

    #[test]
    fn walk_up_returns_none_for_tmp() {
        // /tmp typically has no .nvmrc
        let pin = walk_up_for_pin(Path::new("/tmp"));
        assert!(pin.is_none());
    }

    /// A unique temp dir for resolution tests (no tempfile dev-dep). Created under
    /// the system temp dir, which is NOT under $HOME on macOS (/var/folders) or
    /// Linux (/tmp), so the walk-up can't reach a stray pin file up the tree — and
    /// the test files live directly in `dir`, found before any walk.
    fn resolution_tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-disc-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn node_version_file_wins_over_nvmrc() {
        // Spec precedence (node-version-management.md §"Resolution order"):
        // .node-version (#1) beats .nvmrc (#2) in the same directory.
        let dir = resolution_tmpdir("prec");
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        std::fs::write(dir.join(".nvmrc"), "18.19.0\n").unwrap();
        let (raw, _pin, source) = walk_up_for_pin(&dir).expect("a pin file");
        assert_eq!(
            source, ".node-version",
            ".node-version must win over .nvmrc"
        );
        assert_eq!(raw, "20.11.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pin_files_inside_node_modules_are_ignored() {
        // A dependency's bundled `.nvmrc`/`.node-version` (under `node_modules`) is
        // that package's own CI pin, not the consumer's. The walk must skip it and
        // keep climbing to the project pin above `node_modules`. Regression: a dep's
        // nested `.nvmrc` pinning an old Node ran the dep's lifecycle script under
        // that Node, which aborted on the inherited `--experimental-webstorage` in
        // NODE_OPTIONS (valid only on Node >= 22.4), failing the whole install.
        let root = resolution_tmpdir("nm-skip");
        std::fs::write(root.join(".node-version"), "24.3.0\n").unwrap();
        let dep = root.join("node_modules").join("tldjs");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join(".nvmrc"), "20\n").unwrap();
        let (raw, _pin, source) = walk_up_for_pin(&dep).expect("project pin above node_modules");
        assert_eq!(
            source, ".node-version",
            "the dep's nested pin must be skipped"
        );
        assert_eq!(raw, "24.3.0", "the project pin above node_modules must win");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bom_prefixed_pin_file_still_parses() {
        // Windows editors default to writing a UTF-8 BOM; str::trim does not
        // strip U+FEFF, so without the explicit BOM strip the pin would be
        // dropped silently. The parsed pin must match the BOM-free version.
        let dir = resolution_tmpdir("bom");
        std::fs::write(dir.join(".nvmrc"), "\u{FEFF}20.11.0\n").unwrap();
        let (raw, _pin, source) = walk_up_for_pin(&dir).expect("a BOM-prefixed pin file");
        assert_eq!(raw, "20.11.0", "the BOM must be stripped before parsing");
        assert_eq!(source, ".nvmrc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_engines_node_from_the_governing_manifest() {
        let dir = resolution_tmpdir("eng");
        std::fs::write(dir.join("package.json"), r#"{"engines":{"node":">=20"}}"#).unwrap();
        let (range, source) = read_engines_node(&dir).expect("engines.node range");
        assert_eq!(range, ">=20");
        assert!(
            source.contains("engines.node"),
            "source label names engines.node: {source}"
        );
        // A non-workspace package.json without engines.node is the project
        // boundary → None, not a walk into ancestors.
        let dir2 = resolution_tmpdir("noeng");
        std::fs::write(dir2.join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert!(read_engines_node(&dir2).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn engines_disagreement_warns_when_pin_violates_engines() {
        let dir = resolution_tmpdir("disagree");
        std::fs::write(dir.join("package.json"), r#"{"engines":{"node":">=20"}}"#).unwrap();
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/x/node"),
            version: NodeVersion::new(18, 19, 0),
            pin_source: Some(".nvmrc".to_string()),
        };
        let warning = engines_disagreement_warning(&dir, &node).expect("a disagreement warning");
        assert!(
            warning.contains("18.19.0") && warning.contains(".nvmrc") && warning.contains(">=20"),
            "warning must name the pinned version, the pin source, and the engines range: {warning}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_engines_exact_pin_wins_over_node_version_file() {
        // Spec precedence (node-version-management.md §"Resolution order"):
        // package.json#devEngines.runtime (#1) beats .node-version (#2).
        let dir = resolution_tmpdir("dev-exact");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"node","version":"22.13.0"}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        let chain = resolve_pin_chain(&dir).expect("a node entry never refuses");
        let (raw, pin, source) = chain.pin.expect("a pin");
        assert_eq!(
            source, DEV_ENGINES_RUNTIME_SOURCE,
            "devEngines.runtime must win over .node-version"
        );
        assert_eq!(raw, "22.13.0");
        assert_eq!(pin, VersionPin::Exact(NodeVersion::new(22, 13, 0)));
        assert!(chain.warnings.is_empty(), "no warn entries → no warnings");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_engines_range_becomes_a_constraining_range_pin() {
        // A semver range resolves like engines.node ranges: constrain here
        // (PATH-satisfies check), newest-satisfying at provision time (the
        // resolve_range test in node_index.rs covers that half). onFail:
        // "download" on a node entry is nub's native behavior — ignored.
        let dir = resolution_tmpdir("dev-range");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"node","version":">=20 <23","onFail":"download"}}}"#,
        )
        .unwrap();
        let chain = resolve_pin_chain(&dir).unwrap();
        let (raw, pin, source) = chain.pin.expect("a pin");
        assert_eq!(source, DEV_ENGINES_RUNTIME_SOURCE);
        assert_eq!(raw, ">=20 <23");
        assert!(
            NodeVersion::new(22, 14, 0).satisfies(&pin),
            "22.14 is inside >=20 <23"
        );
        assert!(
            !NodeVersion::new(23, 0, 0).satisfies(&pin),
            "23.0 is outside >=20 <23"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_dev_engines_runtime_governs_from_a_workspace_member() {
        // The monorepo precedence regression: from a member dir with its own
        // package.json, a root-level devEngines.runtime (#1) must beat a
        // root-level .node-version (#2) — the field reads at the workspace root
        // (matching the PM side's rule), not nearest-manifest-only.
        let root = resolution_tmpdir("ws-dev");
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces":["packages/*"],"devEngines":{"runtime":{"name":"node","version":"22.13.0"}}}"#,
        )
        .unwrap();
        std::fs::write(root.join(".node-version"), "20.11.0\n").unwrap();
        let member = root.join("packages").join("app");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(member.join("package.json"), r#"{"name":"@mono/app"}"#).unwrap();

        let chain = resolve_pin_chain(&member).unwrap();
        let (raw, _pin, source) = chain.pin.expect("a pin");
        assert_eq!(
            source, DEV_ENGINES_RUNTIME_SOURCE,
            "the root devEngines.runtime must govern a member, beating the root .node-version"
        );
        assert_eq!(raw, "22.13.0");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn engines_node_is_the_fourth_chain_source_below_pin_files() {
        // engines.node alone is a resolution range (#4) — including the legal
        // operator-space form (">= 20"), which must not silently degrade to
        // no-constraint.
        let dir = resolution_tmpdir("eng-chain");
        std::fs::write(dir.join("package.json"), r#"{"engines":{"node":">= 20"}}"#).unwrap();
        let chain = resolve_pin_chain(&dir).unwrap();
        let (raw, pin, source) = chain.pin.expect("engines.node pins as a range");
        assert_eq!(source, ENGINES_NODE_SOURCE);
        assert_eq!(raw, ">= 20");
        assert!(NodeVersion::new(22, 13, 0).satisfies(&pin));
        assert!(!NodeVersion::new(18, 19, 0).satisfies(&pin));

        // A pin file outranks it (#2 beats #4).
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        let chain = resolve_pin_chain(&dir).unwrap();
        let (_, _, source) = chain.pin.expect("a pin");
        assert_eq!(source, ".node-version", "a pin file must beat engines.node");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unusable_dev_engines_runtime_version_warns_and_falls_through() {
        // A present-but-unmodelable devEngines.runtime version (e.g. a dist-tag)
        // must warn on the chain — same posture as an unusable packageManager —
        // and fall through to the next source, never silently un-constrain.
        let dir = resolution_tmpdir("dev-bad-ver");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"node","version":"current"}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        let chain = resolve_pin_chain(&dir).unwrap();
        let (_, _, source) = chain.pin.expect("falls through to the pin file");
        assert_eq!(source, ".node-version");
        assert_eq!(chain.warnings.len(), 1, "exactly one unusable-spec warning");
        assert!(
            chain.warnings[0].contains("devEngines.runtime")
                && chain.warnings[0].contains("\"current\""),
            "the warning names the field and the raw spec: {}",
            chain.warnings[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_engines_bun_only_refuses_naming_the_runtime() {
        // A devEngines.runtime naming only non-node runtimes fails by default
        // (the field's onFail default is error) — even when a pin file exists
        // below it in the chain.
        let dir = resolution_tmpdir("dev-bun");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"bun","version":"^1.2.0"}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        match resolve_pin_chain(&dir) {
            Err(e @ DiscoveryError::RuntimeNotNode { .. }) => {
                let msg = e.to_string();
                assert!(msg.contains("\"bun\""), "names the declared runtime: {msg}");
                assert!(msg.contains("nub runs Node"), "states nub's runtime: {msg}");
            }
            other => panic!("expected RuntimeNotNode, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_engines_bun_with_on_fail_warn_falls_through_to_pin_file() {
        let dir = resolution_tmpdir("dev-warn");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"bun","onFail":"warn"}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        let chain = resolve_pin_chain(&dir).expect("warn must not refuse");
        let (raw, _pin, source) = chain.pin.expect("falls through to the pin file");
        assert_eq!(source, ".node-version", "next source in the chain wins");
        assert_eq!(raw, "20.11.0");
        assert_eq!(chain.warnings.len(), 1, "exactly one onFail:warn notice");
        assert!(
            chain.warnings[0].contains("\"bun\""),
            "warning names the runtime: {}",
            chain.warnings[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_engines_array_node_entry_wins_regardless_of_position() {
        // Spec array semantics: the node-named entry is the pin; earlier
        // non-node entries are skipped silently (default ignore).
        let dir = resolution_tmpdir("dev-array");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":[{"name":"bun","version":"^1.0.0"},{"name":"node","version":">=20"}]}}"#,
        )
        .unwrap();
        let chain = resolve_pin_chain(&dir).expect("the node entry must preempt bun's refusal");
        let (raw, pin, source) = chain.pin.expect("a pin");
        assert_eq!(source, DEV_ENGINES_RUNTIME_SOURCE);
        assert_eq!(raw, ">=20");
        assert!(NodeVersion::new(22, 13, 0).satisfies(&pin));
        assert!(
            chain.warnings.is_empty(),
            "skipped non-node entries are silent: {:?}",
            chain.warnings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_engines_evaluator_edge_semantics() {
        let eval = |s: &str| evaluate_dev_engines_runtime(&serde_json::from_str(s).unwrap());
        // A node entry with no version: field present, no constraint → fall
        // through to the next pin source (not a pin, not a refusal).
        assert!(matches!(
            eval(r#"{"name":"node"}"#),
            RuntimeOutcome::FallThrough { .. }
        ));
        // onFail:"ignore" on a non-node entry → silent fall-through.
        match eval(r#"{"name":"deno","onFail":"ignore"}"#) {
            RuntimeOutcome::FallThrough { warnings } => {
                assert!(warnings.is_empty(), "ignore must be silent: {warnings:?}")
            }
            other => panic!("expected FallThrough, got {other:?}"),
        }
        // Array with no node entry: earlier entries default to ignore, the
        // LAST defaults to error — [bun, deno] refuses naming deno.
        match eval(r#"[{"name":"bun"},{"name":"deno"}]"#) {
            RuntimeOutcome::Refuse { runtime } => assert_eq!(runtime, "deno"),
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn dev_engines_disagreement_with_pin_file_warns() {
        // devEngines.runtime (winner) and .node-version disagree → one warning
        // naming both sources and both versions.
        let dir = resolution_tmpdir("dev-disagree");
        std::fs::write(
            dir.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"node","version":"22.13.0"}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".node-version"), "20.11.0\n").unwrap();
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/x/node"),
            version: NodeVersion::new(22, 13, 0),
            pin_source: Some(DEV_ENGINES_RUNTIME_SOURCE.to_string()),
        };
        let warning = engines_disagreement_warning(&dir, &node).expect("a disagreement warning");
        assert!(
            warning.contains("22.13.0")
                && warning.contains(".node-version")
                && warning.contains("20.11.0")
                && warning.contains("devEngines.runtime"),
            "warning must name both sources and both versions: {warning}"
        );
        // Sources agreeing → silent.
        std::fs::write(dir.join(".node-version"), "22.13.0\n").unwrap();
        assert!(engines_disagreement_warning(&dir, &node).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_executable_override_uses_the_given_binary() {
        // Use whatever real Node is on PATH as the override target.
        let Ok(node_path) = which_node() else {
            eprintln!("skipping: no node on PATH");
            return;
        };
        let resolved = node_executable_from(Some(node_path.clone().into_os_string()))
            .unwrap()
            .expect("an explicit NODE_EXECUTABLE resolves to that binary");
        assert_eq!(resolved.pin_source.as_deref(), Some("NODE_EXECUTABLE"));
        assert_eq!(resolved.path.as_std_path(), node_path.as_path());
        assert!(resolved.version.major() >= 18);
        // Unset / empty → no override (falls through to normal resolution).
        assert!(node_executable_from(None).unwrap().is_none());
        assert!(
            node_executable_from(Some(std::ffi::OsString::new()))
                .unwrap()
                .is_none()
        );
        // A bad path is a clear error, not a silent fall-through.
        assert!(node_executable_from(Some("/no/such/node".into())).is_err());
    }

    #[test]
    fn nub_store_finds_highest_satisfying_cached_version() {
        // nub's store layout: ~/.cache/nub/node/<version>/bin/node (dir = version).
        let store = resolution_tmpdir("store");
        for v in ["20.11.0", "22.13.0", "22.15.0"] {
            let bin = store.join(v).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("node"), "").unwrap();
        }
        // Exact pin → that exact cached version.
        let exact = nub_store_node_in(&store, &"22.13.0".parse::<VersionPin>().unwrap())
            .expect("cached 22.13.0");
        assert_eq!(exact.version, NodeVersion::new(22, 13, 0));
        assert!(exact.path.as_str().contains("22.13.0"));
        // Range pin (major 22) → highest matching cached version.
        let major =
            nub_store_node_in(&store, &"22".parse::<VersionPin>().unwrap()).expect("a cached 22.x");
        assert_eq!(
            major.version,
            NodeVersion::new(22, 15, 0),
            "highest matching wins"
        );
        // Not cached → None (falls through to nvm / download).
        assert!(nub_store_node_in(&store, &"18.19.0".parse::<VersionPin>().unwrap()).is_none());
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn engines_disagreement_silent_when_satisfied_or_unpinned() {
        let dir = resolution_tmpdir("agree");
        std::fs::write(dir.join("package.json"), r#"{"engines":{"node":">=18"}}"#).unwrap();
        // Pin satisfies the range → no warning.
        let satisfied = ResolvedNode {
            path: Utf8PathBuf::from("/x/node"),
            version: NodeVersion::new(20, 11, 0),
            pin_source: Some(".node-version".to_string()),
        };
        assert!(engines_disagreement_warning(&dir, &satisfied).is_none());
        // No pin file (PATH-resolved) — engines alone has nothing to disagree with.
        let unpinned = ResolvedNode {
            path: Utf8PathBuf::from("/x/node"),
            version: NodeVersion::new(16, 0, 0),
            pin_source: None,
        };
        assert!(engines_disagreement_warning(&dir, &unpinned).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_error_with_pin_source_matches_canonical_wording() {
        // Canonical wording per the v0.1-anneal binding brief
        // (and wiki/research/supported-node-versions.md). Exact-string
        // assertion — any rewording must update this test deliberately.
        let err = DiscoveryError::Unsupported {
            version: NodeVersion::new(16, 10, 0),
            pin_source: Some(".nvmrc".to_string()),
        };
        let msg = format!("{err}");
        let expected = "Nub requires Node 18.19 or newer for runtime augmentation. \
                        This project pins Node 16.10.0 via .nvmrc. \
                        To run it: update the pin to 18.19+ (Nub will run it in compatibility mode), \
                        or run plain `node` directly for this project.";
        assert_eq!(msg, expected);
    }

    #[test]
    fn unsupported_error_without_pin_source_omits_project_clause() {
        // When deferring to whatever Node is on PATH (no pin file
        // discovered), the message must NOT claim the project is using
        // any particular Node — the project hasn't said anything about
        // Node version, so the message should just state the
        // requirement + the user's action. the maintainer 2026-05-29.
        let err = DiscoveryError::Unsupported {
            version: NodeVersion::new(18, 18, 2),
            pin_source: None,
        };
        let msg = format!("{err}");
        let expected = "Nub requires Node 18.19 or newer for runtime augmentation. \
                        To run it: upgrade Node to 18.19+ (Nub will run it in compatibility mode), \
                        or run plain `node` directly for this project.";
        assert_eq!(msg, expected);
        assert!(!msg.contains("This project"));
        assert!(!msg.contains(" via "));
    }

    #[test]
    fn provision_failed_error_names_version_source_reason_and_suggestion() {
        // The graceful-failure contract (Plumbing): a pin that can't be fetched
        // must name the version, the pin source, the underlying reason, and offer
        // a way forward.
        let err = DiscoveryError::ProvisionFailed {
            version: "22.99.99".to_string(),
            pin_source: ".node-version".to_string(),
            reason: "HTTP status client error (404 Not Found)".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("22.99.99"), "names the version: {msg}");
        assert!(msg.contains(".node-version"), "names the pin source: {msg}");
        assert!(msg.contains("404 Not Found"), "includes the reason: {msg}");
        assert!(msg.contains("pre-install"), "offers a way forward: {msg}");
        // A provisioning failure carries nub's stable, branded code so it surfaces
        // like the rest of the CLI's coded errors instead of a bare `Error:` line.
        assert!(
            msg.contains("ERR_NUB_NODE_PROVISION_FAILED"),
            "carries the branded error code: {msg}"
        );
    }

    #[test]
    fn no_node_on_path_offers_install_remedy() {
        // A user who installed nub before any Node must be told the way out —
        // nub augments Node, it doesn't bundle one — instead of a dead-end
        // "no Node binary found on PATH".
        let msg = DiscoveryError::NoNodeOnPath.to_string();
        assert!(
            msg.contains("nub node install"),
            "points at nub's own provisioning: {msg}"
        );
        assert!(
            msg.contains("doesn't bundle one"),
            "explains nub augments rather than bundles Node: {msg}"
        );
    }

    #[test]
    fn check_min_version_accepts_18_19() {
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/usr/bin/node"),
            version: NodeVersion::new(18, 19, 0),
            pin_source: None,
        };
        assert!(check_min_version(&node).is_ok());
    }

    #[test]
    fn check_min_version_accepts_22_14_compat_tier() {
        // 22.14 is below MIN_AUGMENTED but at/above MIN_SUPPORTED —
        // it runs in compatibility mode, not refused.
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/usr/bin/node"),
            version: NodeVersion::new(22, 14, 5),
            pin_source: None,
        };
        assert!(check_min_version(&node).is_ok());
    }

    #[test]
    fn check_min_version_rejects_18_18() {
        let node = ResolvedNode {
            path: Utf8PathBuf::from("/usr/bin/node"),
            version: NodeVersion::new(18, 18, 2),
            pin_source: Some(".nvmrc".to_string()),
        };
        match check_min_version(&node) {
            Err(DiscoveryError::Unsupported {
                version,
                pin_source,
            }) => {
                assert_eq!(version, NodeVersion::new(18, 18, 2));
                assert_eq!(pin_source.as_deref(), Some(".nvmrc"));
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn discover_node_returns_something() {
        // Basic smoke test — requires node on PATH.
        let cwd = env::current_dir().unwrap();
        match discover_node(&cwd) {
            Ok(node) => {
                assert!(!node.path.as_str().is_empty());
                assert!(node.version.major() >= 18);
            }
            Err(DiscoveryError::NoNodeOnPath) => {
                eprintln!("skipping: no node on PATH");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}
