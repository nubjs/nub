//! `engines` field validation.
//!
//! Checks each package's declared `engines.{node,aube}` constraints:
//!
//! - `engines.node` runs against the host's Node version (or the
//!   `node-version` `.npmrc` override). Checked on the root manifest,
//!   every workspace-project manifest, and every resolved transitive
//!   dependency.
//! - `engines.aube` runs against aube's own version. Checked on the
//!   root manifest and workspace-project manifests only.
//!
//! Mismatches are surfaced as warnings by default; when `engine-strict`
//! is set in `.npmrc` (or on the root package.json), they hard-fail the
//! install.
//!
//! `engines.pnpm` is intentionally ignored. aube is a pnpm-compatible
//! drop-in but its own version (`1.x`) does not live in pnpm's version
//! namespace (`10.x+`), so honoring a package's `engines.pnpm` against
//! aube's version would emit spurious mismatches on every install of a
//! project that pins a pnpm version. Projects that genuinely want to
//! gate on the running tool can use `engines.aube`.
//!
//! Other engine fields (`npm`, `yarn`, `vscode`, etc.) are ignored.

use aube_lockfile::LockfileGraph;
use aube_lockfile::dep_path_filename::dep_path_to_filename;
use aube_store::PackageIndex;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

/// The version reported for `engines.<self>` checks. Reads the active
/// embedder's `version` so guardrails here match the version used elsewhere.
/// Standalone aube's profile sets `version = env!("CARGO_PKG_VERSION")`, so
/// this is byte-identical to the running binary's package version.
pub fn aube_version() -> &'static str {
    aube_util::embedder().version
}

/// Which `engines.<key>` field a mismatch was found on. Carried on
/// `Mismatch` so the warning printer can label the failure correctly
/// without reparsing the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    Node,
    Aube,
}

impl Engine {
    /// The key as it appears under `engines.<key>` in package.json. The
    /// tool's own key is the active embedder's manifest namespace
    /// (standalone aube → `"aube"`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Aube => aube_util::embedder().manifest_namespace,
        }
    }
}

/// Outcome of checking a single package's `engines.<key>` field against
/// the active version for that engine.
#[derive(Debug)]
pub struct Mismatch {
    pub engine: Engine,
    pub package: String,
    pub declared: String,
    pub current: String,
}

/// Resolve the Node version to check against:
///
/// 1. `node-version` override from `.npmrc` if present;
/// 2. otherwise `node --version` (stripping the leading `v`);
/// 3. otherwise `None` (we silently skip the check — a user on a machine
///    without Node installed shouldn't be blocked from installing).
pub fn resolve_node_version(override_: Option<&str>) -> Option<String> {
    if let Some(v) = override_ {
        return Some(v.trim().trim_start_matches('v').to_string());
    }
    // Memoize the `node --version` probe. Spawning a process is cheap
    // individually but this is called once per install and may end up
    // called again by future callers in the same process (workspace
    // installs, `aube add` chaining into `install`, tests). OnceLock
    // gives us zero-cost lookups after the first probe.
    static PROBED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PROBED.get_or_init(probe_node_version).clone()
}

/// The Node version aube should report for engines checks once
/// runtime switching is in play:
///
/// 1. an explicit `node-version` `.npmrc` override always wins
///    (validation-only knob, pnpm semantics);
/// 2. otherwise the resolved runtime context's version — engines must
///    validate the node scripts will actually run on, not whatever
///    PATH happened to carry;
/// 3. otherwise the memoized `node --version` probe.
pub fn effective_node_version(override_: Option<&str>) -> Option<String> {
    if override_.is_some() {
        return resolve_node_version(override_);
    }
    if let Some(rt) = crate::runtime::current()
        && let Some(v) = &rt.version
    {
        return Some(v.clone());
    }
    resolve_node_version(None)
}

fn probe_node_version() -> Option<String> {
    let output = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    Some(s.trim().trim_start_matches('v').to_string())
}

/// Test whether `version` satisfies `range`. A version or range we can't
/// parse is treated as "no opinion" (returns `true`) — matches pnpm's
/// leniency and avoids failing installs over malformed `engines.<key>`
/// fields or unusual binaries that report non-standard version strings
/// (e.g. nightly builds, custom forks).
fn version_satisfies(version: &str, range: &str) -> bool {
    let Ok(v) = node_semver::Version::parse(version) else {
        return true;
    };
    let Ok(r) = node_semver::Range::parse(range) else {
        return true;
    };
    v.satisfies(&r)
}

/// Check a single field in an `engines` map. Returns the declared range
/// on mismatch, `None` when the field is absent or satisfied.
fn check_engine_field(
    engines: &BTreeMap<String, String>,
    field: &str,
    current_version: &str,
) -> Option<String> {
    let range = engines.get(field)?;
    if version_satisfies(current_version, range) {
        None
    } else {
        Some(range.clone())
    }
}

/// Check the `node` and `aube` engine fields on a single manifest,
/// labeling each mismatch with the originating engine and the
/// `package` label (project name or "(root)" for an unnamed root). This
/// runs against root + workspace-project manifests; transitive deps go
/// through `collect_dep_mismatches` (node-only).
///
/// `engines.pnpm` is intentionally not checked here — see the module
/// docs for the rationale.
///
/// `node_version` is `None` when no Node binary was probed and no
/// `node-version` override is set — in that case the `engines.node`
/// field is skipped (we have nothing to compare against), but
/// `engines.aube` still runs.
fn check_manifest_engines(
    manifest: &aube_manifest::PackageJson,
    label: &str,
    node_version: Option<&str>,
) -> Vec<Mismatch> {
    let aube_v = aube_version();
    let mut out = Vec::new();
    if let Some(node_v) = node_version
        && let Some(declared) = check_engine_field(&manifest.engines, "node", node_v)
    {
        out.push(Mismatch {
            engine: Engine::Node,
            package: label.to_string(),
            declared,
            current: node_v.to_string(),
        });
    }
    if let Some(declared) = check_engine_field(&manifest.engines, Engine::Aube.key(), aube_v) {
        out.push(Mismatch {
            engine: Engine::Aube,
            package: label.to_string(),
            declared,
            current: aube_v.to_string(),
        });
    }
    out
}

/// Read each locked package's `package.json` and collect any
/// `engines.node` mismatches. Runs in parallel via rayon — each
/// read is small and independent.
///
/// Reads `package.json` from the materialized location at
/// `node_modules/.aube/<escaped dep_path>/node_modules/<name>/package.json`,
/// not from `indices[dep_path]`. The fetch phase's `AlreadyLinked`
/// fast path skips `load_index` for packages whose virtual-store
/// entry already exists (which is every package on a warm
/// re-install), so the `indices` map is sparse and a lookup-through
/// pattern would silently drop every already-linked package. That's
/// dangerous for the engine-strict use case: switching Node
/// versions (e.g. via nvm) and running `aube install` would
/// *appear* to succeed while missing every `engines.node` check
/// except the root. Reading the hardlinked file avoids that trap —
/// same bytes the CAS would point us at, with zero dependency on
/// the sparse indices map.
///
/// `indices` is still plumbed through because the CAS-pathed read
/// is a viable fallback for packages whose virtual-store entry is
/// missing or in the middle of being materialized (e.g. under
/// `aube install --lockfile-only`, where the linker never runs).
///
/// Error policy: `NotFound` on the materialized read falls through
/// to the CAS fallback; `NotFound` on the CAS fallback means we
/// have no `package.json` to check and we skip the dep. Any other
/// I/O error (permission denied, disk corruption, partial read) on
/// **either** path propagates as `miette::Error` so the user sees
/// the real problem — swallowing it would silently turn
/// `engine-strict` into a no-op on the affected package, which is
/// exactly the trap `run_dep_lifecycle_scripts` and
/// `read_materialized_pkg_json` already closed elsewhere in the
/// PR.
///
/// Only `engines.node` is read here. `engines.aube` is not checked on
/// transitive deps — that field is for the project's own toolchain
/// requirement, not its dependencies'. Workspace-project authors who
/// care about `engines.aube` get it via `check_manifest_engines` on
/// their own importer manifests. `engines.pnpm` is ignored everywhere;
/// see the module docs.
pub fn collect_dep_mismatches(
    aube_dir: &Path,
    graph: &LockfileGraph,
    indices: &BTreeMap<String, PackageIndex>,
    node_version: &str,
    virtual_store_dir_max_length: usize,
) -> miette::Result<Vec<Mismatch>> {
    use miette::miette;

    // Rayon: collect into `Result<Vec<Option<Mismatch>>>` so the
    // first real I/O error short-circuits the whole scan. The
    // `Option` captures "checked cleanly, no mismatch" vs "no
    // package.json available at all (skipped)".
    let per_pkg: miette::Result<Vec<Option<Mismatch>>> = graph
        .packages
        .par_iter()
        .map(|(dep_path, pkg)| -> miette::Result<Option<Mismatch>> {
            // Primary read path: materialized `package.json`. The
            // `virtual_store_dir_max_length` must match the value
            // the linker was built with — see `install::run` for
            // the single source of truth — or long `dep_path`s that
            // trip `dep_path_to_filename`'s truncate-and-hash
            // fallback will encode to a different filename than the
            // linker wrote and we'll silently miss the check.
            // `aube_dir` is the resolved `virtualStoreDir` — the
            // install driver threads it in via
            // `commands::resolve_virtual_store_dir` so custom
            // overrides land on the same path the linker wrote to.
            let materialized = aube_dir
                .join(dep_path_to_filename(dep_path, virtual_store_dir_max_length))
                .join("node_modules")
                .join(&pkg.name)
                .join("package.json");
            let content = match std::fs::read_to_string(&materialized) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Fallback: CAS read via the package index, which
                    // is still populated for packages whose
                    // virtual-store entry hadn't been created at
                    // fetch time (e.g. `--lockfile-only`).
                    let Some(stored) = indices
                        .get(dep_path)
                        .and_then(|idx| idx.get("package.json"))
                    else {
                        // No materialized file *and* no CAS entry —
                        // nothing to check against, skip the dep.
                        // This happens legitimately for `link:` deps
                        // and for packages that ship without a
                        // top-level `package.json`.
                        return Ok(None);
                    };
                    match std::fs::read_to_string(&stored.store_path) {
                        Ok(s) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                        Err(e) => {
                            return Err(miette!(
                                "failed to read CAS `package.json` for {}@{} at {}: {e}",
                                pkg.name,
                                pkg.version,
                                stored.store_path.display()
                            ));
                        }
                    }
                }
                Err(e) => {
                    return Err(miette!(
                        "failed to read materialized `package.json` for {}@{} at {}: {e}",
                        pkg.name,
                        pkg.version,
                        materialized.display()
                    ));
                }
            };
            // Parse errors propagate. A truncated or corrupted
            // `package.json` is the kind of thing a user genuinely
            // wants to see, not an excuse to skip the check.
            let parsed: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
                miette!(
                    "failed to parse `package.json` for {}@{}: {e}",
                    pkg.name,
                    pkg.version
                )
            })?;
            let Some(engines) = parsed.get("engines").and_then(|v| v.as_object()) else {
                return Ok(None);
            };
            let Some(node_range) = engines.get("node").and_then(|v| v.as_str()) else {
                return Ok(None);
            };
            if version_satisfies(node_version, node_range) {
                Ok(None)
            } else {
                Ok(Some(Mismatch {
                    engine: Engine::Node,
                    package: pkg.spec_key(),
                    declared: node_range.to_string(),
                    current: node_version.to_string(),
                }))
            }
        })
        .collect();

    Ok(per_pkg?.into_iter().flatten().collect())
}

/// Check the root manifest's `engines.{node,aube}` constraints.
/// The package label is the manifest's `name`, falling back to
/// `(root)` for unnamed manifests.
pub fn check_root(
    manifest: &aube_manifest::PackageJson,
    node_version: Option<&str>,
) -> Vec<Mismatch> {
    let label = manifest.name.as_deref().unwrap_or("(root)");
    check_manifest_engines(manifest, label, node_version)
}

/// Check every workspace-project manifest the resolver loaded. Each
/// importer is keyed by its workspace-relative path (e.g. `packages/a`),
/// which is what the user already sees in lockfile importer keys and
/// `aube install --filter` output, so the same string makes the most
/// natural mismatch label.
///
/// `manifests` is the same `Vec<(rel_path, PackageJson)>` `install::run`
/// builds during workspace expansion. The root entry (`"."`) is
/// already covered by `check_root` and is skipped here so it doesn't
/// get reported twice.
pub fn check_workspace_importers(
    manifests: &[(String, aube_manifest::PackageJson)],
    node_version: Option<&str>,
) -> Vec<Mismatch> {
    let mut out = Vec::new();
    for (rel_path, manifest) in manifests {
        if rel_path == "." || rel_path.is_empty() {
            continue;
        }
        let label = manifest.name.as_deref().unwrap_or(rel_path.as_str());
        out.extend(check_manifest_engines(manifest, label, node_version));
    }
    out
}

/// Run the full engines check and either emit warnings or hard-fail the
/// install, depending on `strict`. A `None` `node_version` (e.g. no node
/// binary on PATH) skips `engines.node` checks but still validates
/// `engines.aube` — that one doesn't depend on Node.
#[allow(clippy::too_many_arguments)]
pub fn run_checks(
    aube_dir: &Path,
    manifest: &aube_manifest::PackageJson,
    workspace_manifests: &[(String, aube_manifest::PackageJson)],
    graph: &LockfileGraph,
    indices: &BTreeMap<String, PackageIndex>,
    node_version: Option<&str>,
    strict: bool,
    virtual_store_dir_max_length: usize,
    self_engines_check: bool,
) -> miette::Result<()> {
    let mut mismatches = Vec::new();

    // `node_version` is `None` when no Node binary was probed and no
    // `node-version` override is set. The manifest-engine helpers skip
    // the `engines.node` field in that case (`check_engine_field` has
    // nothing to compare against), but still validate `engines.aube`
    // against aube's own version.
    mismatches.extend(check_root(manifest, node_version));
    mismatches.extend(check_workspace_importers(workspace_manifests, node_version));

    // `Embedder::self_engines_check == false` drops the `engines.<self>`
    // validation (the `engines.node` check is unaffected). An embedder whose
    // version isn't in aube's version namespace turns this off to avoid
    // spurious mismatches.
    if !self_engines_check {
        mismatches.retain(|m| m.engine != Engine::Aube);
    }

    // Transitive deps only get checked when we have a real Node
    // version — that scan is `engines.node`-only and there's nothing
    // to compare against without it.
    if let Some(node_v) = node_version {
        mismatches.extend(collect_dep_mismatches(
            aube_dir,
            graph,
            indices,
            node_v,
            virtual_store_dir_max_length,
        )?);
    }

    if mismatches.is_empty() {
        return Ok(());
    }

    let header = if strict {
        "Unsupported engine (engine-strict is on)"
    } else {
        "Unsupported engine"
    };
    eprintln!("warn: {header}");
    for m in &mismatches {
        eprintln!(
            "warn:   {}: wanted {} {}, got {}",
            m.package,
            m.engine.key(),
            m.declared,
            m.current,
        );
    }

    if strict {
        return Err(miette::miette!(
            code = aube_codes::errors::ERR_AUBE_UNSUPPORTED_ENGINE,
            "engine-strict: {} package(s) declare incompatible engine constraints",
            mismatches.len(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_manifest() -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some("x".into()),
            version: None,
            dependencies: Default::default(),
            dev_dependencies: Default::default(),
            peer_dependencies: Default::default(),
            optional_dependencies: Default::default(),
            update_config: None,
            scripts: Default::default(),
            engines: Default::default(),
            dev_engines: None,
            workspaces: None,
            bundled_dependencies: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn range_satisfied_basic() {
        assert!(version_satisfies("18.0.0", ">=16"));
        assert!(!version_satisfies("14.0.0", ">=16"));
    }

    #[test]
    fn unparseable_range_is_permissive() {
        // Some real packages ship nonsense here; we don't want to block on it.
        assert!(version_satisfies("18.0.0", "this-is-not-a-range"));
    }

    #[test]
    fn check_root_skips_when_no_engines() {
        let m = empty_manifest();
        assert!(check_root(&m, Some("18.0.0")).is_empty());
    }

    #[test]
    fn check_root_flags_node_mismatch() {
        let mut m = empty_manifest();
        m.engines.insert("node".into(), ">=20".into());
        let v = check_root(&m, Some("18.0.0"));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].engine, Engine::Node);
        assert_eq!(v[0].declared, ">=20");
    }

    #[test]
    fn check_root_flags_aube_mismatch() {
        // engines.aube checks against aube's own version. Pin to
        // something no real aube release will satisfy so the test
        // doesn't churn at every version bump.
        let mut m = empty_manifest();
        m.engines.insert("aube".into(), ">=99999".into());
        let v = check_root(&m, Some("18.0.0"));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].engine, Engine::Aube);
        assert_eq!(v[0].declared, ">=99999");
        assert_eq!(v[0].current, aube_version());
    }

    #[test]
    fn check_root_ignores_pnpm_engine() {
        // engines.pnpm is intentionally not honored: aube's own
        // version (1.x) lives in a different namespace than pnpm's
        // (10.x+), so comparing them produces spurious warnings on
        // every install of a project that pins a pnpm version.
        // Discussion: github.com/jdx/aube/discussions/626.
        let mut m = empty_manifest();
        m.engines.insert("pnpm".into(), ">=99999".into());
        assert!(check_root(&m, Some("18.0.0")).is_empty());
    }

    #[test]
    fn check_root_aube_satisfied_when_range_matches() {
        // Pin to a range any aube release will satisfy. Regression
        // guard: a refactor that swaps current vs declared in
        // `version_satisfies` would flip this assertion.
        let mut m = empty_manifest();
        m.engines.insert("aube".into(), ">=0.0.1".into());
        assert!(check_root(&m, Some("18.0.0")).is_empty());
    }

    #[test]
    fn check_root_flags_node_and_aube_independently() {
        // Both honored fields, both flagged in one manifest —
        // confirms we don't short-circuit on the first hit. engines.pnpm
        // is set but ignored (see check_root_ignores_pnpm_engine).
        let mut m = empty_manifest();
        m.engines.insert("node".into(), ">=99999".into());
        m.engines.insert("aube".into(), ">=99999".into());
        m.engines.insert("pnpm".into(), ">=99999".into());
        let v = check_root(&m, Some("18.0.0"));
        assert_eq!(v.len(), 2);
        let engines: std::collections::HashSet<_> = v.iter().map(|m| m.engine).collect();
        assert!(engines.contains(&Engine::Node));
        assert!(engines.contains(&Engine::Aube));
    }

    #[test]
    fn check_root_skips_engines_node_when_no_node() {
        // Regression: `run_checks` previously passed a sentinel string
        // ("0.0.0-no-node") when no Node was probed. That sentinel is
        // legal SemVer (prerelease tag `no-node`) and parses cleanly,
        // so `>=N` ranges did NOT trigger the "no opinion" fallback —
        // they ran for real and reported a spurious mismatch on every
        // manifest declaring `engines.node`. Under engine-strict that
        // hard-failed installs on machines without Node. The fix
        // threads `Option<&str>` and skips the field entirely when
        // None; engines.aube still runs.
        let mut m = empty_manifest();
        m.engines.insert("node".into(), ">=20".into());
        m.engines.insert("aube".into(), ">=99999".into());
        let v = check_root(&m, None);
        assert_eq!(v.len(), 1, "engines.node must be skipped, got {v:?}");
        assert_eq!(v[0].engine, Engine::Aube);
    }

    #[test]
    fn check_workspace_importers_skips_root() {
        // The "." entry is the root manifest, already covered by
        // `check_root`. Re-checking it would duplicate every warning.
        let mut root = empty_manifest();
        root.engines.insert("node".into(), ">=99999".into());
        let manifests = vec![(".".to_string(), root)];
        assert!(check_workspace_importers(&manifests, Some("18.0.0")).is_empty());
    }

    #[test]
    fn check_workspace_importers_flags_per_project() {
        let mut a = empty_manifest();
        a.name = Some("project-a".into());
        a.engines.insert("node".into(), ">=99999".into());
        let mut b = empty_manifest();
        b.name = Some("project-b".into());
        b.engines.insert("aube".into(), ">=99999".into());
        let manifests = vec![
            (".".to_string(), empty_manifest()),
            ("packages/a".to_string(), a),
            ("packages/b".to_string(), b),
        ];
        let v = check_workspace_importers(&manifests, Some("18.0.0"));
        assert_eq!(v.len(), 2);
        assert!(
            v.iter()
                .any(|m| m.package == "project-a" && m.engine == Engine::Node)
        );
        assert!(
            v.iter()
                .any(|m| m.package == "project-b" && m.engine == Engine::Aube)
        );
    }

    #[test]
    fn check_workspace_importers_falls_back_to_rel_path_label() {
        // Unnamed workspace member — label by the rel_path so the
        // mismatch is still locatable.
        let mut unnamed = empty_manifest();
        unnamed.name = None;
        unnamed.engines.insert("node".into(), ">=99999".into());
        let manifests = vec![("packages/unnamed".to_string(), unnamed)];
        let v = check_workspace_importers(&manifests, Some("18.0.0"));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].package, "packages/unnamed");
    }

    #[test]
    fn collect_dep_mismatches_reads_materialized_pkg_json() {
        // Regression: `collect_dep_mismatches` used to look up every
        // dep through `indices.get(dep_path)?` and silently skip on
        // miss. Since `fetch_packages_with_root`'s `AlreadyLinked`
        // fast path omits entries from `package_indices` for every
        // warmly-installed package, that swallowed every engine check
        // on a warm re-install — so switching Node versions (nvm,
        // asdf, mise) and re-running `aube install --engine-strict`
        // would silently pass.
        //
        // The fix is to read each dep's `package.json` from its
        // materialized `.aube/<escaped>/node_modules/<name>/` path
        // first, and only fall back to the CAS via `indices` for
        // packages that aren't linked on disk yet. This test sets up
        // a materialized `package.json` with `engines.node: ">=99"`,
        // passes an **empty** indices map, and asserts the mismatch
        // is still found.
        use aube_lockfile::dep_path_filename::DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH;
        use aube_lockfile::{DepType, DirectDep, LockedPackage};
        use std::collections::BTreeMap;

        let tmp = tempfile::tempdir().unwrap();
        let dep_path = "pkg@1.0.0";
        let pkg_dir = tmp
            .path()
            .join("node_modules/.aube")
            .join(dep_path_to_filename(
                dep_path,
                DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            ))
            .join("node_modules/pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.0.0","engines":{"node":">=99"}}"#,
        )
        .unwrap();

        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            dep_path.into(),
            LockedPackage {
                name: "pkg".into(),
                version: "1.0.0".into(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".into(),
            vec![DirectDep {
                name: "pkg".into(),
                dep_path: dep_path.into(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        // Empty indices — the warm-install case after the
        // `AlreadyLinked` fast path omits everything.
        let indices: BTreeMap<String, PackageIndex> = BTreeMap::new();
        let aube_dir = tmp.path().join("node_modules/.aube");
        let mismatches = collect_dep_mismatches(
            &aube_dir,
            &graph,
            &indices,
            "18.0.0",
            DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
        )
        .unwrap();
        assert_eq!(mismatches.len(), 1, "engine mismatch should be surfaced");
        assert_eq!(mismatches[0].package, "pkg@1.0.0");
        assert_eq!(mismatches[0].declared, ">=99");
        assert_eq!(mismatches[0].engine, Engine::Node);
    }

    // Guard against regressing the error-propagation fix. Unix-only
    // because the test uses `chmod 000` to trigger a permission-denied
    // read, which has no direct Windows equivalent.
    #[cfg(unix)]
    #[test]
    fn collect_dep_mismatches_propagates_non_not_found_io_errors() {
        // Regression: an earlier version of `collect_dep_mismatches`
        // had two match arms with identical bodies — one guarded on
        // `ErrorKind::NotFound`, the fallthrough arm catching every
        // other I/O error — and both silently fell through to a
        // CAS-pathed `.ok()?` that also swallowed errors. The effect
        // was that a real I/O failure on any dep's `package.json`
        // (permission denied, disk corruption, short read) got
        // dropped on the floor and the engine check became a no-op
        // for that package. Under `--engine-strict` this could have
        // let an incompatible Node version through undetected. The
        // fix returns `miette::Result<..>` and propagates any
        // non-`NotFound` I/O error on either read path.
        use aube_lockfile::dep_path_filename::DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH;
        use aube_lockfile::{DepType, DirectDep, LockedPackage};
        use std::collections::BTreeMap;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dep_path = "pkg@1.0.0";
        let pkg_dir = tmp
            .path()
            .join("node_modules/.aube")
            .join(dep_path_to_filename(
                dep_path,
                DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            ))
            .join("node_modules/pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let pkg_json = pkg_dir.join("package.json");
        std::fs::write(&pkg_json, r#"{"name":"pkg","version":"1.0.0"}"#).unwrap();
        // Make the file unreadable. `read_to_string` will return
        // `ErrorKind::PermissionDenied`, which is *not* NotFound and
        // must propagate.
        let mut perms = std::fs::metadata(&pkg_json).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&pkg_json, perms).unwrap();

        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            dep_path.into(),
            LockedPackage {
                name: "pkg".into(),
                version: "1.0.0".into(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".into(),
            vec![DirectDep {
                name: "pkg".into(),
                dep_path: dep_path.into(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        let indices: BTreeMap<String, PackageIndex> = BTreeMap::new();
        let aube_dir = tmp.path().join("node_modules/.aube");
        let result = collect_dep_mismatches(
            &aube_dir,
            &graph,
            &indices,
            "18.0.0",
            DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
        );
        // Restore perms so the tempdir can clean up cleanly.
        let mut perms = std::fs::metadata(&pkg_json).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&pkg_json, perms).unwrap();
        // Docker CI defaults to uid 0, and `chmod 000` is a no-op for
        // root: `read_to_string` succeeds, the function returns Ok,
        // and the assertion below would panic. Skip the regression
        // check in that environment — the non-root path still runs
        // on every developer laptop and on every non-root CI runner.
        // SAFETY: libc::geteuid is a leaf syscall with no preconditions.
        let is_root = unsafe { libc::geteuid() } == 0;
        if is_root {
            eprintln!("skipping permission-error test under root");
            return;
        }
        assert!(
            result.is_err(),
            "permission-denied read must propagate, got {result:?}"
        );
    }

    #[test]
    fn resolve_node_version_strips_v_prefix() {
        // `node --version` always prints `v<semver>`; the override
        // path accepts both forms. Guard the strip.
        assert_eq!(
            resolve_node_version(Some("v18.17.1")).as_deref(),
            Some("18.17.1")
        );
        assert_eq!(
            resolve_node_version(Some("20.0.0")).as_deref(),
            Some("20.0.0")
        );
    }
}
