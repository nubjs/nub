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
    #[error(
        "cannot check the publish age of {}@{} — the registry served no publish time for any matching version",
        .0.name, .0.range
    )]
    ReleaseAgeMissingTime(Box<UndatedDetails>),
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
        "in {}: `\"{}\": \"{}\"` names workspace package `{}`, which is not in this workspace",
        .0.importer, .0.dep_name, .0.spec, .0.target
    )]
    WorkspacePkgNotFound(Box<WorkspacePkgNotFoundDetails>),
    #[error(
        "in {}: `\"{}\": \"{}\"` wants `{}` at `{}`, but this workspace has {}@{}",
        .0.importer, .0.dep_name, .0.spec, .0.target, .0.range, .0.target, .0.local_version
    )]
    WorkspaceVersionMismatch(Box<WorkspaceVersionMismatchDetails>),
    #[error(
        "peer-context fixed-point did not converge after {0} iterations; lockfile would be incomplete"
    )]
    PeerContextDivergence(usize),
}

/// Context attached to a `WorkspacePkgNotFound` error.
///
/// `dep_name` is the key as written in the manifest and `target` is the
/// member the spec asks for. They differ only for the alias form
/// (`"card": "workspace:components-card@*"`), and keeping both is what
/// lets the message point at the name the user actually has to fix.
#[derive(Debug)]
pub struct WorkspacePkgNotFoundDetails {
    pub dep_name: String,
    pub spec: String,
    pub target: String,
    pub importer: String,
    /// Every member name in the workspace, for a did-you-mean list.
    /// The formatter caps the rendered list; empty means the workspace
    /// has no members at all.
    pub known: Vec<String>,
}

/// Context attached to a `WorkspaceVersionMismatch` error — an aliased
/// `workspace:<target>@<range>` whose range the local copy of `target`
/// does not satisfy.
#[derive(Debug)]
pub struct WorkspaceVersionMismatchDetails {
    pub dep_name: String,
    pub spec: String,
    pub target: String,
    pub range: String,
    pub local_version: String,
    pub importer: String,
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
    /// The identity `minimumReleaseAgeExclude` actually matches on —
    /// `ResolveTask::registry_name()`, i.e. the real name for an
    /// `npm:`-aliased dep and `name` for everything else.
    ///
    /// Kept separate from `name` because the two differ exactly where it
    /// matters: for `"foo": "npm:real-pkg@^1"` the human reads `foo`, but an
    /// exclude entry naming `foo` matches nothing (the exempt closure in
    /// `resolve::driver` binds the registry name). Printing `name` in an
    /// exclude remedy hands the user an entry clap accepts and the matcher
    /// then silently ignores, so the remedies below use THIS field and the
    /// prose keeps `name`.
    pub registry_name: String,
    pub range: String,
    pub minutes: u64,
    pub importer: String,
    pub ancestors: Vec<(String, String)>,
    /// Version strings that satisfied the range, carried a publish time,
    /// and were blocked for being newer than the cutoff — sorted
    /// newest-first. Undated versions are excluded: they were blocked for
    /// a different reason and listing them here would claim evidence the
    /// registry never served.
    pub gated: Vec<String>,
}

/// Context for `ReleaseAgeMissingTime`: the gate could not be evaluated at
/// all because the registry dated none of the candidates (#581). A separate
/// error from `AgeGate` because the remedies are disjoint — no window would
/// ever have admitted these versions, so the ordinary "loosen
/// `minimumReleaseAge`" advice is wrong here.
#[derive(Debug)]
pub struct UndatedDetails {
    pub name: String,
    /// The exclude-matching identity — see [`AgeGateDetails::registry_name`].
    pub registry_name: String,
    pub range: String,
    pub importer: String,
    pub ancestors: Vec<(String, String)>,
    /// Range-satisfying versions the packument carries no publish time for,
    /// sorted newest-first. Shown so the reader can see the range itself
    /// matched.
    pub undated: Vec<String>,
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
            Self::ReleaseAgeMissingTime(_) => ERR_AUBE_RELEASE_AGE_MISSING_TIME,
            Self::Registry(_, _) => ERR_AUBE_REGISTRY_ERROR,
            Self::UnknownCatalog(_) => ERR_AUBE_UNKNOWN_CATALOG,
            Self::UnknownCatalogEntry(_) => ERR_AUBE_UNKNOWN_CATALOG_ENTRY,
            Self::BlockedExoticSubdep(_) => ERR_AUBE_BLOCKED_EXOTIC_SUBDEP,
            Self::TrustDowngrade(_) => ERR_AUBE_TRUST_DOWNGRADE,
            Self::TrustCheckMissingTime(_) => ERR_AUBE_TRUST_MISSING_TIME,
            Self::WorkspacePkgNotFound(_) => ERR_AUBE_WORKSPACE_PKG_NOT_FOUND,
            Self::WorkspaceVersionMismatch(_) => ERR_AUBE_NO_MATCHING_VERSION,
            Self::PeerContextDivergence(_) => ERR_AUBE_PEER_CONTEXT_NOT_CONVERGED,
        }))
    }

    fn help<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        match self {
            Self::NoMatch(d) => Some(Box::new(format_no_match_help(d))),
            Self::AgeGate(d) => Some(Box::new(format_age_gate_help(d))),
            Self::ReleaseAgeMissingTime(d) => Some(Box::new(format_undated_help(d))),
            Self::Registry(name, msg) => Some(Box::new(format_registry_help(name, msg))),
            Self::UnknownCatalog(d) => Some(Box::new(format_unknown_catalog_help(d))),
            Self::UnknownCatalogEntry(d) => Some(Box::new(format_unknown_catalog_entry_help(d))),
            Self::BlockedExoticSubdep(d) => Some(Box::new(format_exotic_subdep_help(d))),
            Self::TrustDowngrade(d) => Some(Box::new(format_trust_downgrade_help(d))),
            Self::TrustCheckMissingTime(d) => Some(Box::new(format_trust_missing_time_help(d))),
            Self::WorkspacePkgNotFound(d) => Some(Box::new(format_workspace_pkg_not_found_help(d))),
            Self::WorkspaceVersionMismatch(d) => Some(Box::new(format!(
                "the `workspace:` protocol only resolves against this workspace, so this does \
                 not fall back to the registry. Either widen the range (`workspace:{target}@*` \
                 tracks whatever the local copy is) or bump `{target}` to a version that \
                 satisfies `{range}`",
                target = d.target,
                range = d.range,
            ))),
            Self::PeerContextDivergence(_) => None,
        }
    }
}

fn format_workspace_pkg_not_found_help(d: &WorkspacePkgNotFoundDetails) -> String {
    let mut help = if d.dep_name == d.target {
        format!(
            "the `workspace:` protocol only resolves against this workspace's own packages, \
             so `{}` never falls back to the registry",
            d.target
        )
    } else {
        // The alias form. Naming both halves matters: the key is what
        // lands in `node_modules/`, the target is what has to exist.
        format!(
            "`workspace:{target}@<range>` aliases the local package `{target}` under the name \
             `{key}`. `{key}` is the directory name you get in `node_modules/`; `{target}` is \
             the `name` field some package in this workspace must declare",
            target = d.target,
            key = d.dep_name,
        )
    };
    if d.known.is_empty() {
        help.push_str(". This workspace has no packages");
        return help;
    }
    // A big monorepo would bury the message under its own member list.
    const SHOWN: usize = 10;
    help.push_str(". Packages in this workspace: ");
    help.push_str(&d.known[..d.known.len().min(SHOWN)].join(", "));
    if d.known.len() > SHOWN {
        help.push_str(&format!(" (+{} more)", d.known.len() - SHOWN));
    }
    help
}

fn format_trust_downgrade_help(d: &TrustDowngradeDetails) -> String {
    format!(
        "this is a supply-chain trust failure, not an ordinary version-resolution error. \
         An earlier release carried {prior_evidence}, but {name}@{ver} carries {current_evidence}.\n\
         \n\
         This can signal a compromised publisher or tampered release. It can also be benign \
         release-process drift: a maintainer manually published, backported outside the trusted \
         workflow, skipped provenance for convenience, or used a registry that stripped metadata.\n\
         \n\
         Before bypassing:\n\
         1. Inspect the package's npm release, source tag/commit, publisher identity, and tarball; \
         compare the metadata with npmjs.org, and confirm the change is expected and nothing \
         appears tampered with.\n\
         2. Report inconsistent evidence to the relevant upstream owner. Package-release drift \
         belongs with the maintainer; metadata present on npmjs.org but missing from a proxy or \
         mirror belongs with that registry operator.\n\
         3. Only after review, pin a version that retains evidence or add the narrow \
         `{name}@{ver}` exception to `trustPolicyExclude`. A bare `{name}` exempts every version; \
         `trustPolicy = off` disables this protection for the entire install.\n\
         \n\
         Details and known built-in exceptions: https://aube.jdx.dev/trust-policy-exceptions",
        prior_evidence = d.prior_evidence.label(),
        current_evidence = d
            .current_evidence
            .map_or("no trust evidence", |e| e.label()),
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
        ancestors: task.ancestors.to_vec(),
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

/// Range-satisfying versions, newest-first, partitioned by whether the
/// packument dates them. `pick_version` has already decided which of the two
/// age failures happened; this recovers the version lists for the message.
/// Recomputed from the packument rather than threaded out of the pick because
/// both age-gate paths are uncommon and the recompute is dwarfed by resolution.
///
/// Mirrors `pick_version`'s dist-tag handling: a `task.range` that is a tag
/// name (`"latest"`, `"next"`) resolves to its concrete version before
/// parsing. Without it the semver parse fails silently and the diagnostic
/// drops its version list entirely.
fn satisfying_versions(task: &ResolveTask, packument: &Packument) -> (Vec<String>, Vec<String>) {
    let effective = resolve_dist_tag_range(packument, &task.range);
    let Ok(range) = node_semver::Range::parse(&effective) else {
        return (Vec::new(), Vec::new());
    };
    let mut matching: Vec<(node_semver::Version, String)> = Vec::new();
    for ver in packument.versions.keys() {
        let Ok(v) = node_semver::Version::parse(ver) else {
            continue;
        };
        if v.satisfies(&range) {
            matching.push((v, ver.clone()));
        }
    }
    matching.sort_by(|a, b| b.0.cmp(&a.0));
    matching
        .into_iter()
        .map(|(_, s)| s)
        .partition(|ver| packument.time.contains_key(ver))
}

pub(crate) fn build_age_gate(
    task: &ResolveTask,
    packument: &Packument,
    minutes: u64,
) -> AgeGateDetails {
    let (dated, _) = satisfying_versions(task, packument);
    AgeGateDetails {
        name: task.name.clone(),
        registry_name: task.registry_name().to_string(),
        range: task.range.clone(),
        minutes,
        importer: task.importer.clone(),
        ancestors: task.ancestors.to_vec(),
        gated: dated,
    }
}

pub(crate) fn build_release_age_missing_time(
    task: &ResolveTask,
    packument: &Packument,
) -> UndatedDetails {
    let (_, undated) = satisfying_versions(task, packument);
    UndatedDetails {
        name: task.name.clone(),
        registry_name: task.registry_name().to_string(),
        range: task.range.clone(),
        importer: task.importer.clone(),
        ancestors: task.ancestors.to_vec(),
        undated,
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
    // Lead with the one-shot flags — this error most often interrupts a single
    // command (a dlx of a just-published tool), where editing config to get
    // through it once is the wrong shape of remedy. The persistent config
    // remedies follow for the case where the exemption should stick.
    //
    // Every remedy named here must be one nub actually accepts, and every one is
    // about the WINDOW, never its strictness: under nub the gate is enforced, so
    // the way out is a shorter window or an exemption, not a window that is
    // quietly ignored. (`minimumReleaseAgeStrict=false` is deliberately absent
    // for that reason — it remains a settable key, but recommending it would
    // point users at a posture nub does not stand behind.)
    // Offer SHORTENING first and `0` as its limit: the flag takes a duration,
    // so pointing every blocked user straight at "switch the gate off" is a
    // heavier remedy than the situation usually needs.
    s.push_str(
        "to bypass for this run: `--minimum-release-age=<duration>` to shorten the window \
         (`0` turns it off), or `--minimum-release-age-exclude=",
    );
    // The exclude remedies print `registry_name`, NOT `name` — see the field
    // docs. For an `npm:`-aliased dep they differ, and an entry naming the alias
    // is silently ignored by the matcher.
    s.push_str(&d.registry_name);
    s.push_str("` to exempt just this package\n");
    s.push_str("to bypass persistently: shorten `minimumReleaseAge` in .npmrc (`0` turns it off), or add `");
    s.push_str(&d.registry_name);
    s.push_str("` to `minimumReleaseAgeExclude`");
    s
}

/// Deliberately omits `minimumReleaseAgeStrict=false`. Loosening strictness
/// here does not produce a mature pick — it produces the newest matching
/// version with no age evidence at all, which is the silent bypass #581
/// closed. The remedies offered are the ones that leave the gate meaningful.
fn format_undated_help(d: &UndatedDetails) -> String {
    let mut s = String::new();
    push_importer(&mut s, &d.importer);
    push_chain(&mut s, &d.ancestors, &d.name);
    if !d.undated.is_empty() {
        s.push_str(&format!(
            "undated matching versions: {}\n",
            d.undated
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    s.push_str(
        "the minimumReleaseAge window is checked against the registry's `time` metadata, which \
         omits these versions — no window would admit them\n",
    );
    s.push_str(
        "to proceed: unset `registry-supports-time-field` if it is on (it suppresses the \
         full-packument fetch that carries `time`), check the registry config in .npmrc, add `",
    );
    // `registry_name`, not `name`: an exclude entry naming an `npm:` alias
    // matches nothing. (Shortening is NOT offered here — unlike the ordinary
    // age gate, no window admits an undated version, so only an exemption or
    // turning the window off can help.)
    s.push_str(&d.registry_name);
    s.push_str(
        "` to `minimumReleaseAgeExclude`, or set `minimumReleaseAge=0` to turn the window off \
         for this project (`--minimum-release-age=0` for this run alone)",
    );
    s
}

pub(crate) fn format_registry_help(name: &str, msg: &str) -> String {
    format_registry_help_for(name, msg, aube_util::agent_sandbox::detect())
}

/// `sandbox` is the coding agent's sandbox around this process, injected so
/// tests can pin the help without touching the environment. It changes ONE
/// arm: a request the registry client classified as a network deny names the
/// sandbox as the cause, because "check auth" is advice an agent may act on
/// by poking at credentials. Every other failure inside a sandbox — auth,
/// integrity, a 5xx from an allowlisted registry — keeps its own help;
/// sandbox presence alone says nothing about why a request failed.
pub(crate) fn format_registry_help_for(
    name: &str,
    msg: &str,
    sandbox: Option<aube_util::agent_sandbox::AgentSandbox>,
) -> String {
    let kind = classify_registry_error(msg);
    let mut s = String::new();
    if !name.is_empty() && name != "(resolver)" {
        s.push_str(&format!("package: {name}\n"));
    }
    let help: &str = match kind {
        RegistryErrorKind::NetworkDenied => {
            s.push_str(&aube_util::agent_sandbox::network_denied_help_for(sandbox));
            return s;
        }
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
    };
    s.push_str(help);
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
    /// The registry client's own verdict (`aube_registry::Error::NetworkDenied`):
    /// the socket was refused by policy, so no retry and no credential fixes it.
    NetworkDenied,
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
    } else if lower.contains("network access denied") {
        // Before the tarball/fetch arms: a denied tarball download embeds
        // the word "tarball" and the URL, and both would steal it.
        RegistryErrorKind::NetworkDenied
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::TrustEvidence;

    #[test]
    fn trust_downgrade_help_prioritizes_investigation_and_upstream_reporting() {
        let help = format_trust_downgrade_help(&TrustDowngradeDetails {
            name: "@scope/pkg".into(),
            picked_version: "2.0.0".into(),
            current_evidence: None,
            prior_evidence: TrustEvidence::TrustedPublisher,
            prior_version: "1.9.0".into(),
        });

        assert!(help.contains("not an ordinary version-resolution error"));
        assert!(help.contains("carries no trust evidence"));
        assert!(help.contains("confirm the change is expected and nothing appears tampered with"));
        assert!(help.contains("Report inconsistent evidence to the relevant upstream owner"));
        assert!(help.contains("belongs with that registry operator"));
        assert!(help.contains("`@scope/pkg@2.0.0` exception"));
        assert!(help.contains("A bare `@scope/pkg` exempts every version"));
        assert!(help.contains("https://aube.jdx.dev/trust-policy-exceptions"));
    }
}
