use aube_manifest::BundledDependencies;
use aube_registry::{Attestations, Dist, NpmUser, Packument, PeerDepMeta, VersionMetadata};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

#[path = "primer_schema.rs"]
mod primer_schema;

pub(crate) use primer_schema::Seed;
use primer_schema::{
    PrimerBundledDependencies, PrimerDist, PrimerPackument, PrimerPeerDepMeta,
    PrimerVersionMetadata,
};

const PRIMER_FORMAT: &str = "rkyv-v1";
const PRUNE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const AUTO_PRUNE_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
const AUTO_PRUNE_DENOMINATOR: u8 = 100;

include!(concat!(env!("OUT_DIR"), "/primer_index.rs"));

#[derive(Default)]
pub struct PruneStats {
    pub files: u64,
    pub bytes: u64,
}

impl Seed {
    pub(crate) fn packument(&self) -> Packument {
        self.packument.to_packument()
    }
}

impl PrimerPackument {
    fn to_packument(&self) -> Packument {
        let mut time = BTreeMap::new();
        let versions = self
            .versions
            .iter()
            .map(|v| {
                if let Some(published_at) = v.published_at.as_ref() {
                    time.insert(v.version.clone(), published_at.clone());
                }
                (
                    v.version.clone(),
                    v.metadata.to_version_metadata(&self.name, &v.version),
                )
            })
            .collect();
        Packument {
            name: self.name.clone(),
            modified: self.modified.clone(),
            versions,
            dist_tags: self.dist_tags.clone(),
            time,
        }
    }
}

impl PrimerVersionMetadata {
    fn to_version_metadata(&self, name: &str, version: &str) -> VersionMetadata {
        VersionMetadata {
            name: name.to_owned(),
            version: version.to_owned(),
            dependencies: self.dependencies.clone(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: self.peer_dependencies.clone(),
            peer_dependencies_meta: self
                .peer_dependencies_meta
                .iter()
                .map(|(name, meta)| (name.clone(), meta.to_peer_dep_meta()))
                .collect(),
            optional_dependencies: self.optional_dependencies.clone(),
            bundled_dependencies: self
                .bundled_dependencies
                .as_ref()
                .map(PrimerBundledDependencies::to_bundled_dependencies),
            dist: self.dist.as_ref().map(|d| d.to_dist(name, version)),
            os: self.os.clone(),
            cpu: self.cpu.clone(),
            libc: self.libc.clone(),
            engines: self.engines.clone(),
            license: self.license.clone(),
            funding_url: self.funding_url.clone(),
            bin: self.bin.clone(),
            has_install_script: self.has_install_script,
            deprecated: self.deprecated.clone(),
            approver: None,
            npm_user: self.trusted_publisher.then(|| NpmUser {
                trusted_publisher: Some(serde_json::json!({"id": "npm-primer"})),
            }),
        }
    }
}

impl PrimerPeerDepMeta {
    fn to_peer_dep_meta(&self) -> PeerDepMeta {
        PeerDepMeta {
            optional: self.optional,
        }
    }
}

impl PrimerBundledDependencies {
    fn to_bundled_dependencies(&self) -> BundledDependencies {
        match self {
            Self::List(v) => BundledDependencies::List(v.clone()),
            Self::All(v) => BundledDependencies::All(*v),
        }
    }
}

impl PrimerDist {
    fn to_dist(&self, name: &str, version: &str) -> Dist {
        Dist {
            tarball: self
                .tarball
                .clone()
                .unwrap_or_else(|| deterministic_tarball_url(name, version)),
            integrity: self.integrity.clone(),
            shasum: None,
            unpacked_size: None,
            attestations: self.provenance.then(|| Attestations {
                provenance: Some(serde_json::json!({
                    "predicateType": "https://slsa.dev/provenance/v1"
                })),
            }),
        }
    }
}

/// Reconstruct the npmjs tarball URL when the primer omitted it
/// (the common case — see PrimerDist::tarball docs). Mirrors
/// `RegistryClient::tarball_url`'s format for `registry.npmjs.org`.
/// In force-metadata-primer mode the URL is rewritten to the active
/// registry by the resolver, so this default is only consulted on
/// the default-registry path.
fn deterministic_tarball_url(name: &str, version: &str) -> String {
    let unscoped = name
        .strip_prefix('@')
        .and_then(|rest| rest.split('/').nth(1))
        .unwrap_or(name);
    format!("https://registry.npmjs.org/{name}/-/{unscoped}-{version}.tgz")
}

static GENERATED_AT: OnceLock<Option<String>> = OnceLock::new();
static GENERATED_AT_SECS: OnceLock<Option<u64>> = OnceLock::new();
static AUTO_PRUNED: OnceLock<()> = OnceLock::new();

pub(crate) fn get(name: &str) -> Option<Seed> {
    let (_, offset, len) = PRIMER_INDEX
        .binary_search_by(|(candidate, _, _)| candidate.cmp(&name))
        .ok()
        .and_then(|idx| PRIMER_INDEX.get(idx))?;
    auto_prune_once();
    let end = offset.checked_add(*len)?;
    let compressed = PRIMER_BLOB.get(*offset..end)?;
    let archived = zstd::stream::decode_all(Cursor::new(compressed)).ok()?;
    rkyv::from_bytes::<Seed, rkyv::rancor::Error>(&archived).ok()
}

pub(crate) fn covers_cutoff(cutoff: &str) -> bool {
    generated_at().is_some_and(|generated_at| generated_at.as_str() >= cutoff)
}

/// Top-level primer-TTL gate: is the bundled primer young enough (relative to
/// its build date) to be consulted at all?
///
/// The per-pick *regime* logic — a FROZEN pick is served from the offline
/// primer, a live-frontier pick keeps the freshness refetch (see
/// `primer_pick_needs_refetch` + the `PickResult::Found` arm in `driver.rs`) —
/// is the always-on correctness layer beneath this gate. This function only
/// decides whether the primer is alive *at all*: while `now − generated_at <
/// TTL` (or the TTL is unlimited) the primer is consulted and the regime logic
/// runs; once the binary ages past a finite TTL the primer is fully disabled
/// and resolution goes all-network.
///
/// The effective TTL is the active embedder's [`primer_ttl`] default, overridden
/// by the `{config_env_prefix}_PRIMER_TTL` env var (`AUBE_PRIMER_TTL` /
/// `NUB_PRIMER_TTL`) when set to a recognized value (`0`/`unlimited`/… →
/// unlimited; `30d`/`720h`/… → finite). The default for both standalone aube and
/// nub is *unlimited* (frozen resolution data is immutable, so an aged binary's
/// frozen picks are still correct — "evergreen" is just an ∞ TTL). Read once and
/// memoized.
///
/// [`primer_ttl`]: aube_util::identity::Embedder::primer_ttl
pub(crate) fn primer_within_ttl() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let ttl = aube_util::env::parse_primer_ttl(
            aube_util::env::config_env("PRIMER_TTL")
                .as_deref()
                .and_then(|s| s.to_str()),
        )
        .unwrap_or(aube_util::embedder().primer_ttl);
        within_ttl(ttl, generated_at_secs(), now_secs())
    })
}

/// Pure TTL decision, split out so it's unit-testable without the process-global
/// `OnceLock` / env var / build clock. Unlimited TTL (`None`) → always consult.
/// A finite TTL consults only while `now − generated_at < ttl`; an unknown build
/// date (`generated_at = None`, e.g. an empty primer or a build without the
/// `AUBE_PRIMER_GENERATED_AT` stamp) is treated as *not expired* so a finite TTL
/// never silently disables a primer whose age can't be computed.
fn within_ttl(ttl: Option<Duration>, generated_at: Option<u64>, now: u64) -> bool {
    let Some(ttl) = ttl else { return true };
    let Some(built) = generated_at else {
        return true;
    };
    now.saturating_sub(built) < ttl.as_secs()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The names carried by the bundled primer, in index order. Used by the
/// resolver's tests to drive the primer code paths against real seeds.
#[cfg(test)]
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    PRIMER_INDEX.iter().map(|(name, _, _)| *name)
}

fn generated_at() -> Option<&'static String> {
    GENERATED_AT
        .get_or_init(|| {
            let secs = generated_at_secs()?;
            Some(crate::types::format_iso8601_utc(secs))
        })
        .as_ref()
}

/// The primer's build date as epoch seconds (the mtime of the source primer at
/// compile time, stamped into `AUBE_PRIMER_GENERATED_AT` by `build.rs`). `None`
/// when the stamp is absent — an empty primer, or a build that didn't set it.
/// Used by the TTL gate; `generated_at()` formats the same value as ISO-8601.
fn generated_at_secs() -> Option<u64> {
    *GENERATED_AT_SECS.get_or_init(|| option_env!("AUBE_PRIMER_GENERATED_AT")?.parse().ok())
}

fn auto_prune_once() {
    AUTO_PRUNED.get_or_init(|| {
        if let Some(dir) = primer_cache_dir() {
            auto_prune(&dir);
        }
    });
}

fn auto_prune(dir: &Path) {
    if !random_byte().is_multiple_of(AUTO_PRUNE_DENOMINATOR) {
        return;
    }
    if let Err(e) = prune_old(dir, PRUNE_AGE, false, Some(AUTO_PRUNE_COOLDOWN)) {
        tracing::debug!("failed to prune old primer cache files: {e}");
    }
}

pub fn prune_cache(dry_run: bool, age: Duration) -> std::io::Result<PruneStats> {
    let Some(dir) = primer_cache_dir() else {
        return Ok(PruneStats::default());
    };
    prune_old(&dir, age, dry_run, None)
}

fn prune_old(
    dir: &Path,
    age: Duration,
    dry_run: bool,
    sentinel_cooldown: Option<Duration>,
) -> std::io::Result<PruneStats> {
    let mut stats = PruneStats::default();
    std::fs::create_dir_all(dir)?;
    let sentinel = dir.join(".auto_prune");
    if let Some(cooldown) = sentinel_cooldown
        && let Ok(modified) = sentinel.metadata().and_then(|m| m.modified())
        && modified.elapsed().unwrap_or_default() < cooldown
    {
        return Ok(stats);
    }
    if sentinel_cooldown.is_some() {
        touch(&sentinel)?;
    }
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_primer_cache_file(name) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.modified()?.elapsed().unwrap_or_default() > age {
            stats.files += 1;
            stats.bytes += metadata.len();
            if !dry_run {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(stats)
}

fn touch(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(b"\n")
}

fn is_primer_cache_file(name: &str) -> bool {
    name.starts_with(&format!("{PRIMER_FORMAT}-")) && name.ends_with(".rkyv")
}

fn random_byte() -> u8 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    (nanos as u8) ^ (std::process::id() as u8)
}

fn primer_cache_dir() -> Option<PathBuf> {
    // First-class config knob, read under the active embedder's brand
    // (`AUBE_CACHE_DIR` for standalone aube, `<BRAND>_CACHE_DIR` for an embedder
    // with its own `config_env_prefix`) via `config_env` — never the branded
    // `AUBE_*` form under such a host.
    if let Some(base) = aube_util::env::config_env("CACHE_DIR") {
        return Some(PathBuf::from(base).join("primer"));
    }
    // Active embedder's `cache_namespace` (standalone aube → "aube"), not a literal,
    // so the primer lands beside the packument cache in aube-store's `cache_dir`
    // rather than under an aube-named path in a host embedder's $XDG_CACHE.
    cache_base_dir().map(|p| p.join(aube_util::embedder().cache_namespace).join("primer"))
}

#[cfg(unix)]
fn cache_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
}

#[cfg(windows)]
fn cache_base_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pure TTL gate: unlimited always consults; a finite TTL consults only
    /// while the binary is younger than the TTL; an unknown build date is never
    /// treated as expired (so a finite TTL can't silently kill a primer whose
    /// age can't be computed).
    #[test]
    fn within_ttl_gates_on_age_relative_to_build_date() {
        let day = 86_400;
        let built = 1_000_000_000; // arbitrary fixed build epoch
        // Unlimited (None) → always consult, regardless of age.
        assert!(within_ttl(None, Some(built), built + 10 * 365 * day));
        // Finite 30d TTL: young binary consults, aged binary does not.
        let ttl = Some(Duration::from_secs(30 * day));
        assert!(within_ttl(ttl, Some(built), built + 29 * day)); // within
        assert!(within_ttl(ttl, Some(built), built)); // same instant
        assert!(!within_ttl(ttl, Some(built), built + 31 * day)); // expired
        assert!(!within_ttl(ttl, Some(built), built + 30 * day)); // boundary: not < ttl
        // Unknown build date → never expired even under a finite TTL.
        assert!(within_ttl(ttl, None, built + 10 * 365 * day));
        // Clock skew (now < built) saturates to 0 elapsed → still within.
        assert!(within_ttl(ttl, Some(built), built - day));
    }

    #[test]
    fn bundled_primer_loads() {
        let Some((name, _, _)) = PRIMER_INDEX.first() else {
            return;
        };
        assert!(super::get(name).is_some());
    }

    #[test]
    fn bundled_primer_synthesizes_tarball_urls() {
        // The generator omits the tarball URL when it matches the
        // deterministic `{registry}/{name}/-/{unscoped}-{version}.tgz`
        // pattern. Verify the runtime fills it in correctly: every
        // dist must surface a tarball URL whose path segments match
        // the package name + version we asked for, so a synthesis bug
        // that drops or swaps either field can't pass silently.
        let Some((name, _, _)) = PRIMER_INDEX.first() else {
            return;
        };
        let packument = super::get(name).expect("primer hit").packument();
        let (version, meta) = packument
            .versions
            .iter()
            .find(|(_, v)| v.dist.is_some())
            .expect("packument has at least one version with dist metadata");
        let dist = meta.dist.as_ref().unwrap();
        assert!(
            dist.tarball.starts_with("https://"),
            "tarball: {}",
            dist.tarball
        );
        assert!(dist.tarball.ends_with(".tgz"), "tarball: {}", dist.tarball);
        assert!(
            dist.tarball.contains(*name),
            "tarball {} missing package name {name}",
            dist.tarball,
        );
        assert!(
            dist.tarball.contains(version),
            "tarball {} missing version {version}",
            dist.tarball,
        );
    }

    #[test]
    fn deterministic_tarball_url_handles_scoped_names() {
        assert_eq!(
            deterministic_tarball_url("react", "18.2.0"),
            "https://registry.npmjs.org/react/-/react-18.2.0.tgz"
        );
        assert_eq!(
            deterministic_tarball_url("@types/node", "20.10.0"),
            "https://registry.npmjs.org/@types/node/-/node-20.10.0.tgz"
        );
    }

    #[test]
    fn primer_cache_file_match_is_narrow() {
        assert!(is_primer_cache_file("rkyv-v1-abc.rkyv"));
        assert!(!is_primer_cache_file(".auto_prune"));
        assert!(!is_primer_cache_file("rkyv-v1-abc.tmp"));
        assert!(!is_primer_cache_file("other-v1-abc.rkyv"));
    }

    #[test]
    fn prune_removes_old_extracted_primer_files() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("rkyv-v1-old-0-old.rkyv"), "{}").unwrap();
        std::fs::write(dir.join("packument.json"), "{}").unwrap();
        let stats = prune_old(dir, Duration::from_secs(0), false, None).unwrap();
        assert_eq!(stats.files, 1);
        assert!(!dir.join("rkyv-v1-old-0-old.rkyv").exists());
        assert!(dir.join("packument.json").exists());
    }

    #[test]
    fn prune_sentinel_uses_own_cooldown() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let primer_file = dir.join("rkyv-v1-old-0-old.rkyv");
        std::fs::write(&primer_file, "{}").unwrap();
        touch(&dir.join(".auto_prune")).unwrap();

        let stats = prune_old(
            dir,
            Duration::from_secs(0),
            false,
            Some(Duration::from_secs(60)),
        )
        .unwrap();

        assert_eq!(stats.files, 0);
        assert!(primer_file.exists());
    }
}
