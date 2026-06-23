use aube_lockfile::LocalSource;
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Hook invoked once per resolved package, right after its version has
/// been picked from the packument and before its dependency set is
/// enqueued. Implementations may mutate `dependencies`,
/// `optionalDependencies`, `peerDependencies`, and
/// `peerDependenciesMeta`; every other field is ignored on the way
/// back, matching how pnpm's `readPackage` hook is used in the wild.
///
/// The trait is deliberately shaped to let a single long-lived node
/// subprocess implement it — `&mut self` so the impl can own stdin /
/// stdout halves of the child without interior mutability, and a boxed
/// future because `async fn` in dyn-compatible traits still requires
/// third-party crates we haven't pulled in.
pub trait ReadPackageHook: Send {
    fn read_package<'a>(
        &'a mut self,
        pkg: aube_registry::VersionMetadata,
    ) -> Pin<Box<dyn Future<Output = Result<aube_registry::VersionMetadata, String>> + Send + 'a>>;
}

/// Supply-chain mitigation: forbid versions younger than `min_age` for
/// every package whose name isn't in `exclude`. Mirrors pnpm's
/// `minimumReleaseAge` / `minimumReleaseAgeExclude` /
/// `minimumReleaseAgeStrict` triplet. Constructed by the install
/// command, threaded into [`Resolver::with_minimum_release_age`].
#[derive(Debug, Clone, Default)]
pub struct MinimumReleaseAge {
    /// Minutes a version must have aged in the registry. `0` disables.
    pub minutes: u64,
    /// Package names skipped by the cutoff filter entirely.
    pub exclude: HashSet<String>,
    /// When true, fail the install if no version satisfies the range
    /// without violating the cutoff. When false (the pnpm default), the
    /// resolver falls back to the lowest satisfying version, ignoring
    /// the cutoff for that pick only.
    pub strict: bool,
}

#[derive(Debug, Clone)]
pub struct DependencyPolicy {
    pub package_extensions: Vec<PackageExtension>,
    pub allowed_deprecated_versions: BTreeMap<String, String>,
    pub trust_policy: TrustPolicy,
    pub trust_policy_exclude: crate::trust::TrustExcludeRules,
    pub trust_policy_ignore_after: Option<u64>,
    pub block_exotic_subdeps: bool,
}

impl Default for DependencyPolicy {
    fn default() -> Self {
        Self {
            package_extensions: Vec::new(),
            allowed_deprecated_versions: BTreeMap::new(),
            trust_policy: TrustPolicy::default(),
            trust_policy_exclude: crate::trust::TrustExcludeRules::default(),
            trust_policy_ignore_after: None,
            block_exotic_subdeps: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageExtension {
    pub selector: String,
    pub dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
    pub peer_dependencies: BTreeMap<String, String>,
    pub peer_dependencies_meta: BTreeMap<String, aube_registry::PeerDepMeta>,
}

/// Default is `NoDowngrade` to match the user-facing default in
/// `crates/aube-settings/settings.toml`. The install command overrides
/// this from the resolved settings anyway, but library consumers
/// constructing a `Resolver` via [`Resolver::new`] inherit the
/// documented default behavior without extra plumbing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TrustPolicy {
    #[default]
    NoDowngrade,
    Off,
}

impl MinimumReleaseAge {
    /// Compute the absolute ISO-8601 UTC cutoff string. Returns `None`
    /// when the feature is disabled (`minutes == 0`). Format matches
    /// the npm registry's `time` map so a lexicographic compare on the
    /// raw strings doubles as an instant compare.
    pub fn cutoff(&self) -> Option<String> {
        if self.minutes == 0 {
            return None;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        let cutoff_secs = now.saturating_sub(self.minutes * 60);
        Some(format_iso8601_utc(cutoff_secs))
    }
}

/// Format a Unix epoch second count as an ISO-8601 UTC `Z` string. The
/// resolver only ever compares these against npm registry timestamps,
/// which are emitted in this exact shape — so we can ship our own
/// formatter and skip pulling in `chrono`/`time`. Algorithm adapted
/// from the days-from-epoch trick used by `time` and `civil` crates.
///
/// `aube/src/commands/sbom.rs` carries a near-identical formatter
/// for the SPDX/CycloneDX writers; that one emits seconds-only
/// (`...:00Z`) since SBOM consumers don't expect millis. Don't merge
/// without checking which format each caller needs — the npm registry
/// `time` map always uses `.000Z`, lex compare relies on it.
pub(crate) fn format_iso8601_utc(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.000Z")
}

/// Convert a day count from the Unix epoch (1970-01-01) to a
/// proleptic Gregorian (year, month, day). Lifted from Howard Hinnant's
/// `civil_from_days` paper, which the `time` crate uses.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// A resolved package emitted during resolution, allowing the caller
/// to start fetching tarballs before resolution is fully complete.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub dep_path: String,
    pub name: String,
    pub version: String,
    pub integrity: Option<String>,
    /// Exact tarball URL reported by the packument's `dist.tarball`
    /// field, or preserved from an existing lockfile. Most npm
    /// packages can re-derive this from name + version, but JSR's
    /// npm-compatible registry uses opaque tarball paths, so fetchers
    /// must prefer this when it is available.
    pub tarball_url: Option<String>,
    /// Real registry name when this package is an npm-alias
    /// (`"h3-v2": "npm:h3@..."`). `name` is the alias (`h3-v2` — the
    /// folder in `node_modules/`), `alias_of` is what the streaming
    /// fetch client uses to derive the tarball URL and store-index
    /// key. `None` for non-aliased packages, in which case `name`
    /// already matches the registry.
    pub alias_of: Option<String>,
    /// Set for non-registry packages (`file:` / `link:`). Downstream
    /// fetchers short-circuit the tarball path and materialize from
    /// disk instead.
    pub local_source: Option<LocalSource>,
    /// npm `os`/`cpu`/`libc` arrays straight from the packument (or
    /// lockfile). The streaming fetch coordinator uses them to defer
    /// tarball downloads for optional natives that won't install on
    /// the host — a post-resolve catch-up pass after `filter_graph`
    /// fetches anything that survived the graph trim but got deferred,
    /// so required-platform-mismatched packages (which `filter_graph`
    /// doesn't drop) still get their tarball before link.
    pub os: aube_lockfile::PlatformList,
    pub cpu: aube_lockfile::PlatformList,
    pub libc: aube_lockfile::PlatformList,
    /// Deprecation message from the registry, carried forward so the
    /// install command can render user-facing warnings without a
    /// second packument fetch. Only populated on the fresh-resolve
    /// path; lockfile-reuse and `file:`/`link:` packages carry `None`
    /// because the packument wasn't consulted. `allowedDeprecatedVersions`
    /// suppression is applied upstream, so anything set here is meant
    /// to surface to the user.
    pub deprecated: Option<Arc<str>>,
    /// Best-effort install-size hint from the packument's
    /// `dist.unpackedSize`. Summed across the resolve stream to drive
    /// the `4.2 MB / ~13.8 MB` segment in the progress bar. `None`
    /// when the packument doesn't carry the field (older publishes,
    /// `file:`/`link:` deps, JSR packages without npm metadata).
    pub unpacked_size: Option<u64>,
    /// Resolver's view of remaining work at the moment this package
    /// was sent: the size of the BFS queue plus any packument fetches
    /// still in flight plus transitives deferred behind a time-based
    /// cutoff. `received_count + pending` is a non-strict lower bound
    /// on the final resolved-package count, used by the install
    /// progress UI to render a real bar during the resolving phase
    /// instead of an empty placeholder. Approximate by construction —
    /// a packument fetch completing without enqueueing children can
    /// transiently shrink the frontier, so the install side raises
    /// the displayed denominator with `fetch_max` semantics.
    pub pending: usize,
}

impl ResolvedPackage {
    /// Registry lookup name — `alias_of` when set, otherwise `name`.
    /// Every tarball URL + store index site routes through this
    /// accessor so aliased packages resolve to the real registry
    /// entry without leaking the alias-qualified name into network
    /// requests (where it would 404).
    pub fn registry_name(&self) -> &str {
        self.alias_of.as_deref().unwrap_or(&self.name)
    }
}

/// Which version-picking strategy the resolver uses for a workspace.
/// Mirrors pnpm's `resolution-mode` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolutionMode {
    /// Classic pnpm behavior: every dep resolves to the highest version
    /// satisfying its range.
    #[default]
    Highest,
    /// Pick the lowest version that satisfies each direct-dep range,
    /// then constrain transitive picks to versions published on or
    /// before a cutoff date derived from the max publish time of
    /// already-locked packages. Matches pnpm's `time-based` mode.
    TimeBased,
}
