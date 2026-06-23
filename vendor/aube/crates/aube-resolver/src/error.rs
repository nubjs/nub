use crate::ResolveTask;
use crate::semver_util::highest_stable_version;
use crate::trust::{MissingTimeDetails, TrustDowngradeDetails};
use aube_codes::errors::*;
use aube_registry::Packument;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no version of {} matches range `{}`", .0.name, .0.range)]
    NoMatch(Box<NoMatchDetails>),
    #[error(
        "no version of {} matching {} is older than {} minute(s) (minimumReleaseAgeStrict=true)",
        .0.name, .0.range, .0.minutes
    )]
    AgeGate(Box<AgeGateDetails>),
    #[error("registry error for {0}: {1}")]
    Registry(String, String),
    #[error(
        "{}: catalog reference `{}` does not resolve — catalog `{}` is not defined (add it to `catalog:` / `catalogs.{}:` in pnpm-workspace.yaml, or under `workspaces.catalog` / `pnpm.catalog` in package.json)",
        .0.name, .0.spec, .0.catalog, .0.catalog
    )]
    UnknownCatalog(Box<CatalogDetails>),
    #[error(
        "{}: catalog reference `{}` does not resolve — catalog `{}` has no entry for `{}`",
        .0.name, .0.spec, .0.catalog, .0.name
    )]
    UnknownCatalogEntry(Box<CatalogDetails>),
    #[error(
        "blocked exotic transitive dependency {}@{} from {} (blockExoticSubdeps=true; set blockExoticSubdeps=false to allow trusted git/file/tarball subdeps)",
        .0.name, .0.spec, .0.parent
    )]
    BlockedExoticSubdep(Box<ExoticSubdepDetails>),
    #[error(
        "trust downgrade for {}@{} (trustPolicy=no-downgrade): earlier published version {} had {} but this version has {}",
        .0.name, .0.picked_version, .0.prior_version, .0.prior_evidence.label(),
        .0.current_evidence.map_or("no trust evidence", |e| e.label())
    )]
    TrustDowngrade(Box<TrustDowngradeDetails>),
    #[error(
        "trust check failed for {}@{} (trustPolicy=no-downgrade): registry packument has no `time` entry for the picked version",
        .0.name, .0.version
    )]
    TrustCheckMissingTime(Box<MissingTimeDetails>),
    #[error(
        "peer-context fixed-point did not converge after {0} iterations. mutually recursive peers, lockfile would be incomplete"
    )]
    PeerContextDivergence(usize),
}

/// Context attached to a `NoMatch` error so the miette `help()` output can
/// show importer path, parent chain, and what versions the packument
/// actually contains. Boxed into the enum variant to keep `Error`'s size
/// under `clippy::result_large_err`.
#[derive(Debug)]
pub struct NoMatchDetails {
    pub name: String,
    pub range: String,
    pub importer: String,
    pub ancestors: Vec<(String, String)>,
    pub original_spec: Option<String>,
    /// Up to 5 most-recent version strings from the packument. Stable
    /// versions are preferred; when the packument contains only
    /// prereleases we fall back to showing those so the diagnostic
    /// doesn't misreport the packument as empty.
    pub available: Vec<String>,
    /// Total number of versions in the packument, including prereleases
    /// and unparseable keys. Used by the help text to distinguish a
    /// genuinely empty packument (wrong registry, missing package) from
    /// one that only publishes prereleases.
    pub total_versions: usize,
    /// True when every shown entry in `available` is a prerelease — the
    /// user asked for a stable range but the registry only has alpha /
    /// beta / rc builds. Help text steers them toward `name@next` or a
    /// prerelease range.
    pub only_prereleases: bool,
}

#[derive(Debug)]
pub struct AgeGateDetails {
    pub name: String,
    pub range: String,
    pub minutes: u64,
    pub importer: String,
    pub ancestors: Vec<(String, String)>,
    /// Version strings that satisfied the range but were blocked by
    /// the age gate, sorted newest-first. Empty when the cutoff was
    /// tighter than every published version.
    pub gated: Vec<String>,
}

#[derive(Debug)]
pub struct CatalogDetails {
    pub name: String,
    pub spec: String,
    pub catalog: String,
    /// For `UnknownCatalog`: the catalog names that *are* defined.
    /// For `UnknownCatalogEntry`: the package names defined under
    /// `catalog`. Empty when the catalog map itself is empty, or
    /// when the error is a chained-catalog case (see `chained_value`).
    pub available: Vec<String>,
    /// Set only for the chained-catalog case: the entry exists, but
    /// its value is itself another `catalog:` reference. Carries the
    /// offending value (e.g. `catalog:other`) so the help text can
    /// explain the chain rule rather than pretending the entry is
    /// missing.
    pub chained_value: Option<String>,
}

#[derive(Debug)]
pub struct ExoticSubdepDetails {
    pub name: String,
    pub spec: String,
    pub parent: String,
    pub ancestors: Vec<(String, String)>,
    pub importer: String,
}

impl miette::Diagnostic for Error {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new(match self {
            Self::NoMatch(_) => ERR_AUBE_NO_MATCHING_VERSION,
            Self::AgeGate(_) => ERR_AUBE_NO_MATURE_MATCHING_VERSION,
            Self::Registry(_, _) => ERR_AUBE_REGISTRY_ERROR,
            Self::UnknownCatalog(_) => ERR_AUBE_UNKNOWN_CATALOG,
            Self::UnknownCatalogEntry(_) => ERR_AUBE_UNKNOWN_CATALOG_ENTRY,
            Self::BlockedExoticSubdep(_) => ERR_AUBE_BLOCKED_EXOTIC_SUBDEP,
            Self::TrustDowngrade(_) => ERR_AUBE_TRUST_DOWNGRADE,
            Self::TrustCheckMissingTime(_) => ERR_AUBE_TRUST_MISSING_TIME,
            Self::PeerContextDivergence(_) => ERR_AUBE_PEER_CONTEXT_NOT_CONVERGED,
        }))
    }

    fn help<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        match self {
            Self::NoMatch(d) => Some(Box::new(format_no_match_help(d))),
            Self::AgeGate(d) => Some(Box::new(format_age_gate_help(d))),
            Self::Registry(name, msg) => Some(Box::new(format_registry_help(name, msg))),
            Self::UnknownCatalog(d) => Some(Box::new(format_unknown_catalog_help(d))),
            Self::UnknownCatalogEntry(d) => Some(Box::new(format_unknown_catalog_entry_help(d))),
            Self::BlockedExoticSubdep(d) => Some(Box::new(format_exotic_subdep_help(d))),
            Self::TrustDowngrade(d) => Some(Box::new(format_trust_downgrade_help(d))),
            Self::TrustCheckMissingTime(d) => Some(Box::new(format_trust_missing_time_help(d))),
            Self::PeerContextDivergence(_) => None,
        }
    }
}

fn format_trust_downgrade_help(d: &TrustDowngradeDetails) -> String {
    format!(
        "a trust downgrade may indicate a supply-chain incident — the publisher's previous releases carried {evidence} but {name}@{ver} does not.\n\
         to bypass: pin a version that retains evidence, set `trustPolicy = off` in .npmrc / pnpm-workspace.yaml, \
         or add `{name}` (or `{name}@{ver}`) to `trustPolicyExclude`.",
        evidence = d.prior_evidence.label(),
        name = d.name,
        ver = d.picked_version,
    )
}

fn format_trust_missing_time_help(d: &MissingTimeDetails) -> String {
    format!(
        "trustPolicy=no-downgrade compares against per-version publish times in the packument. \
         The registry serving {name} omitted `time[{ver}]` — check the registry config in .npmrc, \
         or set `trustPolicy = off` to skip the check.",
        name = d.name,
        ver = d.version,
    )
}

/// Build a `NoMatchDetails` snapshot from the task that failed and the
/// packument it was looked up against. Captures importer, parent chain,
/// the original package.json spec (if rewritten by catalog/override/
/// alias), and a sample of the highest non-prerelease versions so the
/// diagnostic can tell the user how close they were.
pub(crate) fn build_no_match(task: &ResolveTask, packument: &Packument) -> NoMatchDetails {
    let mut stable: Vec<(node_semver::Version, &str)> = Vec::new();
    let mut prerelease: Vec<(node_semver::Version, &str)> = Vec::new();
    for v in packument.versions.keys() {
        let Ok(parsed) = node_semver::Version::parse(v) else {
            continue;
        };
        if parsed.pre_release.is_empty() {
            stable.push((parsed, v.as_str()));
        } else {
            prerelease.push((parsed, v.as_str()));
        }
    }
    stable.sort_by(|a, b| b.0.cmp(&a.0));
    prerelease.sort_by(|a, b| b.0.cmp(&a.0));
    let (pool, only_prereleases) = if stable.is_empty() {
        (prerelease, true)
    } else {
        (stable, false)
    };
    let available = pool
        .into_iter()
        .take(5)
        .map(|(_, s)| s.to_string())
        .collect();
    NoMatchDetails {
        name: task.name.clone(),
        range: task.range.clone(),
        importer: task.importer.clone(),
        ancestors: task.ancestors.clone(),
        original_spec: task.original_specifier.clone(),
        available,
        total_versions: packument.versions.len(),
        only_prereleases,
    }
}

/// Build an `AgeGateDetails` snapshot: which versions actually
/// satisfied the range but were blocked by the cutoff. Recomputed from
/// the packument rather than threaded out of `pick_version` because
/// the age-gate path is uncommon and the recompute cost is dwarfed by
/// the resolution itself.
/// Resolve a `task.range` string that may be a dist-tag (`"latest"`,
/// `"next"`, …) to the concrete version it points at. Used by the
/// diagnostic builders where we need to parse the range for display
/// purposes after `pick_version` has already accepted or rejected it.
/// Falls back to the raw input when nothing matches — callers treat a
/// subsequent semver parse failure as "skip, best-effort".
fn resolve_dist_tag_range(packument: &Packument, range_str: &str) -> String {
    if let Some(tagged) = packument.dist_tags.get(range_str) {
        tagged.clone()
    } else if range_str == "latest"
        && let Some(v) = highest_stable_version(packument)
    {
        v
    } else {
        range_str.to_string()
    }
}

pub(crate) fn build_age_gate(
    task: &ResolveTask,
    packument: &Packument,
    minutes: u64,
) -> AgeGateDetails {
    // Mirror `pick_version`'s dist-tag handling: if `task.range` is a
    // tag name (e.g. `"latest"`, `"next"`), resolve it to the concrete
    // version string before parsing. Without this the semver parse
    // fails silently and the help text drops the "blocked by age gate"
    // line entirely, losing the most useful diagnostic.
    let effective = resolve_dist_tag_range(packument, &task.range);
    let range = node_semver::Range::parse(&effective).ok();
    let mut gated: Vec<(node_semver::Version, String)> = Vec::new();
    if let Some(r) = range {
        for ver in packument.versions.keys() {
            let Ok(v) = node_semver::Version::parse(ver) else {
                continue;
            };
            if !v.satisfies(&r) {
                continue;
            }
            gated.push((v, ver.clone()));
        }
    }
    gated.sort_by(|a, b| b.0.cmp(&a.0));
    AgeGateDetails {
        name: task.name.clone(),
        range: task.range.clone(),
        minutes,
        importer: task.importer.clone(),
        ancestors: task.ancestors.clone(),
        gated: gated.into_iter().map(|(_, s)| s).collect(),
    }
}

fn format_no_match_help(d: &NoMatchDetails) -> String {
    let mut s = String::new();
    push_importer(&mut s, &d.importer);
    push_chain(&mut s, &d.ancestors, &d.name);
    if let Some(orig) = &d.original_spec
        && orig != &d.range
    {
        s.push_str(&format!(
            "original spec: `{orig}` (rewritten to `{}`)\n",
            d.range
        ));
    }
    if d.available.is_empty() {
        if d.total_versions == 0 {
            s.push_str("packument has no versions — check that the package exists on the configured registry");
        } else {
            s.push_str(&format!(
                "packument has {} unparseable version(s) — check registry for non-semver tags",
                d.total_versions
            ));
        }
    } else if d.only_prereleases {
        s.push_str(&format!(
            "no stable versions published; only prereleases available: {}\nhint: request a prerelease explicitly (e.g. `{}@{}`) or via the `next` dist-tag",
            d.available.join(", "),
            d.name,
            d.available.first().map(String::as_str).unwrap_or("next"),
        ));
    } else {
        s.push_str(&format!("available versions: {}", d.available.join(", ")));
    }
    s
}

fn format_age_gate_help(d: &AgeGateDetails) -> String {
    let mut s = String::new();
    push_importer(&mut s, &d.importer);
    push_chain(&mut s, &d.ancestors, &d.name);
    if !d.gated.is_empty() {
        s.push_str(&format!(
            "blocked by age gate: {}\n",
            d.gated
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    s.push_str("to bypass: loosen `minimumReleaseAge` in .npmrc, set `minimumReleaseAgeStrict=false` to fall back to the lowest satisfying version, or add `");
    s.push_str(&d.name);
    s.push_str("` to `minimumReleaseAgeExclude`");
    s
}

pub(crate) fn format_registry_help(name: &str, msg: &str) -> String {
    let kind = classify_registry_error(msg);
    let mut s = String::new();
    if !name.is_empty() && name != "(resolver)" {
        s.push_str(&format!("package: {name}\n"));
    }
    s.push_str(match kind {
        RegistryErrorKind::Tarball => {
            "tarball download or integrity check failed — try `aube store prune` to clear the cache; if the lockfile references a tarball that moved, delete the lockfile entry for this package and re-resolve"
        }
        RegistryErrorKind::Fetch => {
            "packument fetch failed — verify the registry URL in .npmrc, check auth (`npm login` / `NPM_TOKEN`), and confirm network connectivity"
        }
        RegistryErrorKind::Git => {
            "git dep failed to resolve — confirm the ref exists, that credentials are configured for the host, and that the URL form is supported"
        }
        RegistryErrorKind::LocalSpec => {
            "unparseable local specifier — `file:`/`link:`/`workspace:` paths must be relative to the importer, and `http(s):` URLs must end in `.tgz`"
        }
        RegistryErrorKind::Hook => {
            "pnpmfile `readPackage` hook returned an error — check the hook's stack trace above for the underlying cause"
        }
        RegistryErrorKind::ResolverBug => {
            "internal resolver invariant violated — please report at https://github.com/jdx/aube/discussions with the lockfile and command that reproduced this"
        }
        RegistryErrorKind::Generic => {
            "registry operation failed — see the message above for the underlying cause"
        }
    });
    s
}

fn format_unknown_catalog_help(d: &CatalogDetails) -> String {
    let mut s = String::new();
    if d.available.is_empty() {
        s.push_str("no catalogs are defined in this workspace; add a `catalog:` block to `pnpm-workspace.yaml` or a `workspaces.catalog` entry in root `package.json`");
    } else {
        s.push_str(&format!("defined catalogs: {}", d.available.join(", ")));
    }
    s
}

fn format_unknown_catalog_entry_help(d: &CatalogDetails) -> String {
    if let Some(chained) = &d.chained_value {
        return format!(
            "catalogs cannot chain — replace `{}` with a concrete semver range (e.g. `^1.0.0`) under the catalog entry",
            chained
        );
    }
    let mut s = String::new();
    if d.available.is_empty() {
        s.push_str(&format!(
            "catalog `{}` is empty; add `{}: <version>` under `catalogs.{}` in pnpm-workspace.yaml",
            d.catalog, d.name, d.catalog
        ));
    } else {
        let suggestion = suggest_similar(&d.name, &d.available);
        if let Some(best) = suggestion {
            s.push_str(&format!(
                "catalog `{}` defines: {} — did you mean `{}`?",
                d.catalog,
                truncate_list(&d.available, 8),
                best
            ));
        } else {
            s.push_str(&format!(
                "catalog `{}` defines: {}",
                d.catalog,
                truncate_list(&d.available, 8)
            ));
        }
    }
    s
}

fn format_exotic_subdep_help(d: &ExoticSubdepDetails) -> String {
    let mut s = String::new();
    push_importer(&mut s, &d.importer);
    push_chain(&mut s, &d.ancestors, &d.name);
    s.push_str(&format!(
        "to allow: either pin `{}` in your root package.json (moves the exotic spec out of the transitive graph), or set `blockExoticSubdeps=false` in .npmrc / settings.toml to trust every transitive git/file/tarball dep",
        d.name
    ));
    s
}

fn push_importer(s: &mut String, importer: &str) {
    if !importer.is_empty() && importer != "." {
        s.push_str(&format!("importer: {importer}\n"));
    }
}

fn push_chain(s: &mut String, ancestors: &[(String, String)], leaf: &str) {
    if ancestors.is_empty() {
        return;
    }
    s.push_str("chain: ");
    for (i, (n, v)) in ancestors.iter().enumerate() {
        if i > 0 {
            s.push_str(" > ");
        }
        s.push_str(&format!("{n}@{v}"));
    }
    s.push_str(&format!(" > {leaf}\n"));
}

fn truncate_list(items: &[String], max: usize) -> String {
    if items.len() <= max {
        items.join(", ")
    } else {
        let (head, tail) = items.split_at(max);
        format!("{} (+{} more)", head.join(", "), tail.len())
    }
}

/// Suggest the closest string in `choices` to `needle` using a simple
/// case-insensitive prefix/substring match, falling back to first-char
/// equality. Returns `None` when nothing plausibly matches. This is a
/// deliberately cheap heuristic — good enough for catalog typos,
/// nothing more.
fn suggest_similar<'a>(needle: &str, choices: &'a [String]) -> Option<&'a str> {
    let lower = needle.to_ascii_lowercase();
    choices
        .iter()
        .map(String::as_str)
        .find(|c| {
            c.to_ascii_lowercase().contains(&lower) || lower.contains(&c.to_ascii_lowercase())
        })
        .or_else(|| {
            choices
                .iter()
                .map(String::as_str)
                .find(|c| c.chars().next() == needle.chars().next())
        })
}

pub(crate) enum RegistryErrorKind {
    Tarball,
    Fetch,
    Git,
    LocalSpec,
    Hook,
    ResolverBug,
    Generic,
}

/// Coarse classification by substring match. Registry errors carry
/// free-form `format!` strings from helper functions that already embed
/// intent ("fetch ", "tarball ", "git ", "readPackage", etc.), so a
/// lightweight match on those prefixes lets us pick a targeted help
/// message without plumbing a new enum through every call site.
pub(crate) fn classify_registry_error(msg: &str) -> RegistryErrorKind {
    let lower = msg.to_ascii_lowercase();
    // Specific-prefix branches (git, hook, local-spec) must run before
    // the generic `http` / `tarball` substring checks: each of those
    // error payloads can itself embed an https:// URL or a tarball
    // path, so a bare substring match on later arms would steal them.
    if lower.starts_with("git resolve ")
        || lower.starts_with("git dep ")
        || lower.starts_with("git task ")
        || lower.contains("git+")
    {
        RegistryErrorKind::Git
    } else if lower.starts_with("readpackage ") || lower.contains("readpackage hook") {
        RegistryErrorKind::Hook
    } else if lower.starts_with("unparseable local specifier") || lower.contains("workspace:") {
        RegistryErrorKind::LocalSpec
    } else if lower.contains("tarball") || lower.contains("integrity") {
        RegistryErrorKind::Tarball
    } else if lower.starts_with("fetch ") || lower.contains("packument") || lower.contains("http") {
        RegistryErrorKind::Fetch
    } else if lower.contains("deferred") || lower.contains("invariant") {
        RegistryErrorKind::ResolverBug
    } else {
        RegistryErrorKind::Generic
    }
}
