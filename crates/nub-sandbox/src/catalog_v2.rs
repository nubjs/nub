//! The v2 build-jail catalog: one table, keyed by package name, whose grants say what a
//! package's lifecycle scripts may reach beyond the base profile.
//!
//! WHAT THE BASE PROFILE ALREADY GRANTS, and therefore what is NOT expressible here: read
//! and write on the package's own directory (node-gyp builds into its own `build/`), READ
//! on its declared dependencies (that is how `require` works — deny it and nothing
//! installs), a private writable `$HOME`, and the project root directory node so `getcwd`
//! succeeds. A grant only ever widens beyond that.
//!
//! THE SHAPE IS TWO UNIONS, AND BOTH ARE DELIBERATE. `read` and `write` each carry either a
//! SET of narrow scopes or the single escalation `"disk"`. The narrow scopes compose because
//! none nests inside another — `deps` sits under `project` when a package is force-
//! materialized and under `userHome` when it is symlinked into the global store, measured in
//! one install — so no combination may be validated away as implied. `disk` is the only
//! dominance relation, which is why it is a separate arm: `disk` alongside a narrow scope is
//! unrepresentable rather than merely rejected.
//!
//! `write` IMPLIES read at its own scope, so a `read` naming a scope its `write` already
//! covers is rejected as redundant rather than silently honoured.
//!
//! VERSIONS ARE `default` PLUS `<`-BOUNDED BANDS, and NOTHING MERGES ACROSS THEM. A package's
//! entry carries one `default` — generated from `latest`, so it covers today's release and
//! every future one — beside optional bands whose keys are all `<X`. Bands therefore reach
//! DOWNWARD without limit, which is what makes coverage total: a band catches every old
//! release including the ones too unpopular to probe, and `default` catches the rest. Because
//! all keys are `<`, any two bands either nest or are disjoint, so resolution is NARROWEST
//! BOUND WINS with no ordering rule and no dependence on JSON key order.
//!
//! A version resolves to EXACTLY ONE grant, complete in itself. `default` is not a base a band
//! extends, and a band is not a patch on `default` — reading one grant tells you the whole
//! answer for the versions it covers. This is deliberately UNLIKE the per-OS overlays, which
//! DO merge: a package is exactly one version, so bands are ALTERNATIVES, whereas an OS overlay
//! refines a grant that still applies. The two cannot share a rule.

use std::collections::BTreeMap;

/// Which filesystem scope a capability names. `Deps` is write-only: read on declared
/// dependencies is part of the base profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// The package's declared dependencies, resolved by FOLLOWING the package's own
    /// `node_modules` links — never by joining a name onto a directory. That is the whole
    /// security argument: the only reachable entries are ones the package can already
    /// `require`, so a separator in a name cannot escape because no name is ever joined.
    Deps,
    /// The user's project tree. Reachable by a lifecycle script only when nub materialized
    /// the package into the project or the script consults `INIT_CWD`; nub sets `INIT_CWD`
    /// to the project root and it is inherited into the jail, so this is never security by
    /// obscurity.
    Project,
    /// The REAL user home — where nub's store and package caches live. NOT `$HOME`, which
    /// the jail has already redirected to a private per-package directory.
    UserHome,
}

impl Scope {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "deps" => Some(Self::Deps),
            "project" => Some(Self::Project),
            "userHome" => Some(Self::UserHome),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deps => "deps",
            Self::Project => "project",
            Self::UserHome => "userHome",
        }
    }
}

/// A capability's reach: a set of narrow scopes, or the whole filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Reach {
    #[default]
    None,
    Scopes(Vec<Scope>),
    Disk,
}

impl Reach {
    pub fn covers(&self, scope: Scope) -> bool {
        match self {
            Self::None => false,
            Self::Disk => true,
            Self::Scopes(v) => v.contains(&scope),
        }
    }
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Platform {
    Macos,
    Linux,
    Windows,
}

impl Platform {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "macos" => Some(Self::Macos),
            "linux" => Some(Self::Linux),
            "windows" => Some(Self::Windows),
            _ => None,
        }
    }
    /// The platform this build targets, so a grant's `platforms` can be evaluated without
    /// the caller having to know how to spell the current OS.
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Macos
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self::Linux
        }
    }
}

/// One grant: what a package's scripts may reach beyond the base profile, at the versions its
/// position in the entry covers. The version range is NOT here — it is the KEY of the entry's
/// `versions` map, so one grant can never carry two answers about its own scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Empty means every platform.
    pub platforms: Vec<Platform>,
    pub read: Reach,
    pub write: Reach,
    pub network: bool,
    /// Subpaths of the package's PRIVATE `$HOME` that nub moves into the REAL home once the
    /// lifecycle scripts finish. Relative to that home — `.cache/puppeteer`, not an absolute
    /// path.
    ///
    /// THE NAME IS HONEST: nub copies whatever lands in these directories into the real home, so
    /// the script effectively HOLDS that write. Calling it anything softer would understate the
    /// authority.
    ///
    /// WHY A COPY RATHER THAN A DIRECT GRANT. The jail redirects `$HOME` to a throwaway directory,
    /// so a package caching under `~/.cache/<vendor>` installs cleanly and its artefact is
    /// discarded: puppeteer's browser was 355 of 359 written paths, with none under the real
    /// `~/.cache/puppeteer`. The obvious fix is to grant write on the real home, but that hands
    /// a dependency script a live handle on `$HOME` for the whole run. Promotion never does:
    /// the script writes to the throwaway, and NUB moves the declared subpaths afterwards. Same
    /// end state, and the script never touches the user's home.
    ///
    /// DECLARED, never "everything in the private home" — that would let any package land
    /// arbitrary files in a real `$HOME`. The entries are derived from a run record's
    /// `ephemeralPaths`, so they are measured rather than authored.
    ///
    /// The move is a rename: `jail-home` and the real cache are on one filesystem (verified),
    /// so a 300 MB browser costs nothing.
    pub write_paths: Vec<String>,
    /// Free text, optional, unvalidated. A CATCHALL — the earlier `evidence` enum was
    /// validated and then discarded by every consumer, and a minimum-length rule on this
    /// field was the same mistake in cheaper clothing: it rejected real catalogs written by
    /// a harness that had nothing interesting to say.
    pub notes: String,
}

impl Grant {
    /// Does this grant apply on `platform`? Version selection is [`Entry::grant_for`]'s; this
    /// is the second, independent matcher the caller applies to whatever that returned.
    pub fn matches_platform(&self, platform: Platform) -> bool {
        self.platforms.is_empty() || self.platforms.contains(&platform)
    }

    /// Does this grant widen anything at all beyond the base profile?
    fn widens_nothing(&self) -> bool {
        self.read.is_none() && self.write.is_none() && !self.network && self.write_paths.is_empty()
    }
}

/// One `<`-bounded version band: the grant for every release below its bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// The range exactly as written — `<0.28.1` — so it can be handed to the shared range
    /// matcher rather than reconstructed from the parsed bound.
    pub range: String,
    /// The bound, parsed at LOAD time. It is what orders bands against each other, and
    /// ordering by a parsed bound rather than by the map's iteration is what makes
    /// [`Entry::grant_for`] independent of the order the catalog happens to be written in.
    /// Private because nothing outside this module has a reason to construct a band.
    bound: semver::Version,
    pub grant: Grant,
}

/// One package's entry: the grant for current and future releases, plus the bands that cover
/// older ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// GENERATED FROM `latest`. Every band key is a `<` bound, so old versions are always
    /// caught by the lowest band and `default` only ever applies from the highest bound upward
    /// — today's releases and tomorrow's, which is exactly what a measurement at `latest`
    /// predicts.
    pub default: Grant,
    /// The `<`-bounded bands, in no meaningful order. [`Entry::grant_for`] resolves by bound,
    /// so nothing here depends on how they were written.
    pub versions: Vec<Band>,
}

impl Entry {
    /// The ONE grant that applies at `version`.
    ///
    /// NARROWEST BOUND WINS. All bands are `<X`, so any two either nest or are disjoint, and
    /// among those matching `version` the smallest bound is the most specific answer —
    /// `0.12.0` matching both `<0.13.0` and `<1.0.0` takes `<0.13.0`. Selecting by bound rather
    /// than by position is what removes the first-match footgun the `Grant[]` shape had, where
    /// a matcher-less grant written early silently shadowed every grant after it.
    ///
    /// NOTHING MERGES. The returned grant is complete on its own; `default` is not a base the
    /// bands extend.
    ///
    /// No band matching — including an absent, non-semver, or PRERELEASE version, which the
    /// shared range matcher deliberately refuses — falls to `default`. That fallback is
    /// strictly better than the withhold-everything the matcher's refusal used to mean: a
    /// prerelease now gets `latest`'s answer instead of no grant at all.
    pub fn grant_for(&self, version: Option<&str>) -> &Grant {
        self.versions
            .iter()
            .filter(|b| crate::compiler::version_scope::applies(Some(&b.range), version))
            .min_by(|a, b| a.bound.cmp(&b.bound))
            .map_or(&self.default, |b| &b.grant)
    }
}

/// One path every jailed script gets, regardless of package.
///
/// THE BASELINE LIVES IN THE CATALOG so it can be iterated without a rebuild. The alternative —
/// baking it into the binary — makes every "does this package work if we also allow X?"
/// experiment a 3-minute compile, which is what made the shape of this allowlist guesswork for
/// so long. A path discovered here (a toolchain's cache written beside its own sources, say)
/// can be added and re-measured in seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselinePath {
    /// A symbolic path pattern in the compiler's own vocabulary — `$cache/...`, `~/...`.
    pub path: String,
    /// WRITE IS THE EXCEPTION AND MUST BE ARGUED. The baseline already grants write on four
    /// things (the private `$HOME`, `$tmp`, the package's own store entry, its own package
    /// dir); everything added here should be READ unless a measurement shows a build genuinely
    /// fails without the write — not merely that the write was attempted and denied. A denied
    /// cache write is usually harmless: Python's `__pycache__` is refused today and every
    /// native build still succeeds.
    pub write: bool,
    pub notes: String,
}

/// One environment variable set for EVERY jailed script.
///
/// Here for the same reason as [`BaselinePath`] — so the jail's shape is data, not a rebuild.
/// The motivating case: `PYTHONDONTWRITEBYTECODE=1`. node-gyp ships `gyp/pylib`, CPython tries
/// to write `__pycache__/*.pyc` beside those sources, the jail refuses (correctly — that is
/// another package's store entry), and the build succeeds anyway because bytecode is a cache.
/// Setting the variable means Python never ATTEMPTS the write: no refused syscall, and no
/// phantom "side effect" for a grant search to trip over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEnv {
    pub name: String,
    pub value: String,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Catalog {
    /// Package name -> its entry. Resolution within an entry is [`Entry::grant_for`].
    pub packages: BTreeMap<String, Entry>,
    /// Paths granted to EVERY jailed script, before any package grant applies. Empty means the
    /// compiled-in baseline stands.
    pub baseline: Vec<BaselinePath>,
    /// Variables set for every jailed script, after the credential scrub.
    pub env: Vec<BaselineEnv>,
}

/// Parse and validate. Every rejection names the offending path so a contributor sees which
/// entry to fix, whether it surfaced from `cargo build` or from a dev override at startup.
pub fn parse(text: &str) -> Result<Catalog, String> {
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let obj = root
        .get("packages")
        .ok_or_else(|| "catalog has no `packages` table".to_string())?
        .as_object()
        .ok_or_else(|| "`packages` must be an object keyed by package name".to_string())?;

    let mut packages = BTreeMap::new();
    for (name, value) in obj {
        packages.insert(name.clone(), parse_entry(value, name)?);
    }
    Ok(Catalog {
        packages,
        baseline: parse_baseline(&root)?,
        env: parse_env(&root)?,
    })
}

fn parse_entry(value: &serde_json::Value, name: &str) -> Result<Entry, String> {
    let at = format!("packages[{name}]");
    let obj = value.as_object().ok_or_else(|| {
        format!("{at}: must be an OBJECT of the form {{default, versions?}} — a bare ARRAY is the retired first-match-wins shape")
    })?;
    for key in obj.keys() {
        if !matches!(key.as_str(), "default" | "versions") {
            return Err(format!("{at}: unknown field `{key}`"));
        }
    }

    let default = parse_grant(
        obj.get("default")
            .ok_or_else(|| format!("{at}: `default` is required — it is what every version not caught by a band resolves to"))?,
        &format!("{at}.default"),
    )?;

    let mut versions = Vec::new();
    if let Some(v) = obj.get("versions") {
        let map = v
            .as_object()
            .ok_or_else(|| format!("{at}.versions: must be an object keyed by a `<` bound"))?;
        for (range, grant) in map {
            let at = format!("{at}.versions[{range}]");
            // EVERY band key is `<X`, and that is load-bearing rather than a style rule: it is
            // what makes any two bands nest or be disjoint, which is what makes NARROWEST WINS
            // an unambiguous rule instead of an ordering convention. A key in any other dialect
            // is a generator bug, and parsing it would resolve versions by a rule nobody wrote.
            let bound = range.strip_prefix('<').ok_or_else(|| {
                format!(
                    "{at}: a band key must be a `<` bound (e.g. `<0.28.1`); bands nest by \
                     construction, which is what makes narrowest-wins unambiguous"
                )
            })?;
            let bound = semver::Version::parse(bound.trim()).map_err(|e| {
                format!("{at}: `{bound}` is not a complete semver version, so this band cannot be ordered against the others: {e}")
            })?;
            versions.push(Band {
                range: range.clone(),
                bound,
                grant: parse_grant(grant, &at)?,
            });
        }
    }

    // AN ENTRY MUST WIDEN SOMETHING. An empty `default` is a real statement — "latest passes
    // ungranted", which is exactly what esbuild and bcrypt measured — but only when a band
    // hangs off it. With no band the entry is byte-for-byte the base profile, i.e. what the
    // package gets by being absent from the catalog entirely, and an entry that LOOKS present
    // while doing nothing is the failure mode this parser exists to catch.
    if default.widens_nothing() && versions.is_empty() {
        return Err(format!(
            "{at}: `default` widens nothing and there are no version bands, so the entry grants \
             exactly the base profile; drop it"
        ));
    }

    Ok(Entry { default, versions })
}

fn parse_env(root: &serde_json::Value) -> Result<Vec<BaselineEnv>, String> {
    let Some(value) = root.get("env") else {
        return Ok(Vec::new());
    };
    let arr = value
        .as_array()
        .ok_or_else(|| "`env` must be an array of {name, value, notes}".to_string())?;
    let mut out: Vec<BaselineEnv> = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let at = format!("env[{i}]");
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("{at}: must be an object"))?;
        for key in obj.keys() {
            if !matches!(key.as_str(), "name" | "value" | "notes") {
                return Err(format!("{at}: unknown field `{key}`"));
            }
        }
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{at}: `name` is required and must be a string"))?
            .to_string();
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(format!("{at}: `{name}` is not a usable variable name"));
        }
        // The scrub exists to keep credentials out of a dependency's script. A catalog entry
        // that re-introduced one would quietly undo it, and the catalog is the LEAST reviewed
        // place that could — so refuse the shapes outright rather than trusting authorship.
        let upper = name.to_ascii_uppercase();
        const SECRETY: &[&str] = &[
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "CREDENTIAL",
            "APIKEY",
            "AUTH",
        ];
        if SECRETY.iter().any(|s| upper.contains(s)) || upper.ends_with("_KEY") {
            return Err(format!(
                "{at}: `{name}` looks like a credential; the jail scrubs those deliberately and \
                 the catalog must not put one back"
            ));
        }
        let value = obj
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{at}: `value` is required and must be a string"))?
            .to_string();
        let notes = obj
            .get("notes")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if out.iter().any(|e| e.name == name) {
            return Err(format!("{at}: `{name}` is set twice"));
        }
        out.push(BaselineEnv { name, value, notes });
    }
    Ok(out)
}

fn parse_baseline(root: &serde_json::Value) -> Result<Vec<BaselinePath>, String> {
    let Some(value) = root.get("baseline") else {
        return Ok(Vec::new());
    };
    let arr = value
        .as_array()
        .ok_or_else(|| "`baseline` must be an array of {path, write?, notes}".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let at = format!("baseline[{i}]");
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("{at}: must be an object"))?;
        for key in obj.keys() {
            if !matches!(key.as_str(), "path" | "write" | "notes") {
                return Err(format!("{at}: unknown field `{key}`"));
            }
        }
        let path = obj
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{at}: `path` is required and must be a string"))?
            .to_string();
        if path.trim().is_empty() {
            return Err(format!("{at}: `path` is empty"));
        }
        // A bare `/` (or a lone symbolic root) is `disk` wearing a baseline's clothes, and it
        // would apply to EVERY package with none of the per-package review a `disk` grant gets.
        if path == "/" || path == "~" || path == "$home" {
            return Err(format!(
                "{at}: `{path}` is the whole filesystem — that is the `disk` capability, and it \
                 belongs on a package, not on every script at once"
            ));
        }
        let write = match obj.get("write") {
            None => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(_) => return Err(format!("{at}: `write` must be a boolean")),
        };
        let notes = obj
            .get("notes")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if out.iter().any(|b: &BaselinePath| b.path == path) {
            return Err(format!("{at}: `{path}` is listed twice"));
        }
        out.push(BaselinePath { path, write, notes });
    }
    Ok(out)
}

fn parse_grant(value: &serde_json::Value, at: &str) -> Result<Grant, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("{at}: must be an object"))?;

    for key in obj.keys() {
        // A STALE `versions` FIELD MUST NOT PARSE. It was a grant's own semver range under the
        // retired first-match-wins shape; the range is now the KEY of the entry's `versions`
        // map. Accepting it as an unknown-but-ignorable field would silently drop the scope and
        // apply an old-version grant to every release — under-granting's mirror image, and
        // invisible in a hand-edited catalog until something over-reaches.
        if key == "versions" {
            return Err(format!(
                "{at}: `versions` is no longer a grant field — the range is the KEY of the \
                 entry's `versions` map, so this grant's scope would be silently lost"
            ));
        }
        if !matches!(
            key.as_str(),
            "platforms" | "read" | "write" | "network" | "writePaths" | "notes"
        ) {
            return Err(format!("{at}: unknown field `{key}`"));
        }
    }

    let notes = obj
        .get("notes")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    let mut platforms = Vec::new();
    if let Some(v) = obj.get("platforms") {
        let arr = v
            .as_array()
            .ok_or_else(|| format!("{at}: `platforms` must be an array"))?;
        if arr.is_empty() {
            return Err(format!(
                "{at}: `platforms` is empty, so this grant can never match; omit it to mean every platform"
            ));
        }
        for p in arr {
            let s = p
                .as_str()
                .ok_or_else(|| format!("{at}: `platforms` entries must be strings"))?;
            let parsed = Platform::parse(s).ok_or_else(|| {
                format!("{at}: unknown platform `{s}`; expected macos, linux or windows")
            })?;
            if platforms.contains(&parsed) {
                return Err(format!("{at}: platform `{s}` is listed twice"));
            }
            platforms.push(parsed);
        }
    }

    let read = parse_reach(
        obj.get("read"),
        at,
        "read",
        &[Scope::Project, Scope::UserHome],
    )?;
    let write = parse_reach(
        obj.get("write"),
        at,
        "write",
        &[Scope::Deps, Scope::Project, Scope::UserHome],
    )?;

    // `write` implies read at its own scope, so a `read` naming a scope the `write` already
    // covers means nothing. Reject it rather than honour it silently — a grant whose author
    // believed it was doing something is worse than one that fails the build.
    // Order matters: `Reach::Disk` covers every scope, so the per-scope check below would
    // otherwise fire first and report the vaguer message for the disk case.
    if matches!(write, Reach::Disk) && !read.is_none() {
        return Err(format!(
            "{at}: `write: \"disk\"` already grants every read; remove `read`"
        ));
    }
    if let Reach::Scopes(rs) = &read {
        for s in rs {
            if write.covers(*s) {
                return Err(format!(
                    "{at}: `read.{}` is already implied by `write`; remove it",
                    s.as_str()
                ));
            }
        }
    }

    let network = match obj.get("network") {
        None => false,
        Some(serde_json::Value::Bool(true)) => true,
        Some(_) => {
            return Err(format!(
                "{at}: `network` may only be `true`; omit it to grant no egress"
            ));
        }
    };

    let mut promote = Vec::new();
    if let Some(v) = obj.get("writePaths") {
        let arr = v
            .as_array()
            .ok_or_else(|| format!("{at}: `writePaths` must be an array of home-relative paths"))?;
        for entry in arr {
            let rel = entry
                .as_str()
                .ok_or_else(|| format!("{at}: `writePaths` entries must be strings"))?
                .trim()
                .to_string();
            // ABSOLUTE PATHS AND TRAVERSAL ARE REFUSED. A promote entry names a destination in
            // the user's REAL home; anything that escapes the private home would let a catalog
            // line write outside it, which is exactly the authority promotion exists to avoid.
            if rel.is_empty() || rel == "." {
                return Err(format!("{at}: `writePaths` entry is empty"));
            }
            if rel.starts_with('/') || rel.starts_with('~') || rel.contains("..") {
                return Err(format!(
                    "{at}: `writePaths` entry `{rel}` must be RELATIVE to the package's home and \
                     must not traverse out of it"
                ));
            }
            if promote.contains(&rel) {
                return Err(format!("{at}: `writePaths` entry `{rel}` is listed twice"));
            }
            promote.push(rel);
        }
        if promote.is_empty() {
            return Err(format!("{at}: `writePaths` is empty; omit it instead"));
        }
    }

    // NO "grants nothing" CHECK HERE. An empty grant is a positive statement under this shape —
    // an empty `default` says "latest passes ungranted", which is what makes a `<` band below it
    // meaningful. The emptiness that IS a defect is an entry with nothing anywhere, and
    // [`parse_entry`] owns that because only it can see the whole entry.
    Ok(Grant {
        platforms,
        read,
        write,
        network,
        write_paths: promote,
        notes,
    })
}

fn parse_reach(
    value: Option<&serde_json::Value>,
    at: &str,
    field: &str,
    allowed: &[Scope],
) -> Result<Reach, String> {
    let Some(value) = value else {
        return Ok(Reach::None);
    };
    if let Some(s) = value.as_str() {
        if s == "disk" {
            return Ok(Reach::Disk);
        }
        return Err(format!(
            "{at}: `{field}` as a string may only be \"disk\"; narrow scopes go in an object"
        ));
    }
    let obj = value.as_object().ok_or_else(|| {
        format!("{at}: `{field}` must be an object of scopes or the string \"disk\"")
    })?;
    if obj.is_empty() {
        return Err(format!("{at}: `{field}` is empty; omit it instead"));
    }
    let mut scopes = Vec::new();
    for (k, v) in obj {
        let scope = Scope::parse(k)
            .filter(|s| allowed.contains(s))
            .ok_or_else(|| {
                let names: Vec<_> = allowed.iter().map(|s| s.as_str()).collect();
                format!("{at}: `{field}.{k}` is not a scope {field} accepts ({names:?})")
            })?;
        if v != &serde_json::Value::Bool(true) {
            return Err(format!(
                "{at}: `{field}.{k}` may only be `true`; omit it to grant nothing"
            ));
        }
        scopes.push(scope);
    }
    scopes.sort();
    Ok(Reach::Scopes(scopes))
}

// AN UNREACHABLE-GRANT CHECK IS NO LONGER NEEDED, and that is the point of the shape rather
// than an omission. Under `Grant[]` a matcher-less grant written early silently shadowed every
// grant after it, so the parser had to reject the orderings that could not fire —
// `projectReads: ["."]` compiling to nothing cost a whole measurement campaign, and that class
// of defect is what those rejections guarded. `{default, versions}` removes the class by
// construction: `default` cannot shadow a band because bands are consulted first, and two bands
// cannot shadow each other because narrowest-wins picks between them by bound.

#[cfg(test)]
mod tests {
    use super::*;

    const NOTES: &str = r#""notes":"measured on macOS by the grant search""#;

    /// One package whose entry is a bare `default`, which is the shape most of the corpus takes.
    fn one(default: &str) -> Result<Catalog, String> {
        parse(&format!(
            r#"{{"packages":{{"p":{{"default":{default}}}}}}}"#
        ))
    }

    /// The grant `p` resolves to at `version`, for a catalog written as `{"default":…}` plus
    /// whatever `versions` map the test supplies.
    fn resolve<'a>(catalog: &'a Catalog, version: &str) -> &'a Grant {
        catalog.packages["p"].grant_for(Some(version))
    }

    #[test]
    fn a_narrow_read_and_write_compose() {
        let c = one(&format!(
            r#"{{"read":{{"userHome":true}},"write":{{"deps":true,"project":true}},{NOTES}}}"#
        ))
        .expect("valid");
        let g = &c.packages["p"].default;
        assert_eq!(g.read, Reach::Scopes(vec![Scope::UserHome]));
        assert_eq!(g.write, Reach::Scopes(vec![Scope::Deps, Scope::Project]));
        assert!(!g.network);
    }

    #[test]
    fn disk_is_a_separate_arm_not_another_scope() {
        assert_eq!(
            one(&format!(r#"{{"write":"disk",{NOTES}}}"#))
                .expect("valid")
                .packages["p"]
                .default
                .write,
            Reach::Disk
        );
        // "disk" alongside a narrow scope is not expressible: the union admits one or the
        // other, so the only way to write it is a scope literally named `disk`, which is not
        // a scope.
        let err = one(&format!(r#"{{"write":{{"disk":true}},{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("not a scope"), "{err}");
    }

    #[test]
    fn a_read_its_write_already_implies_is_rejected() {
        let err = one(&format!(
            r#"{{"read":{{"project":true}},"write":{{"project":true}},{NOTES}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("already implied by `write`"), "{err}");

        let err = one(&format!(
            r#"{{"read":{{"project":true}},"write":"disk",{NOTES}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("already grants every read"), "{err}");
    }

    #[test]
    fn read_and_write_of_different_scopes_is_legitimate() {
        // `project` and `userHome` never dominate each other — the project is not inside the
        // home in a container, where nub explicitly supports installs.
        one(&format!(
            r#"{{"read":{{"project":true}},"write":{{"userHome":true}},{NOTES}}}"#
        ))
        .expect("distinct scopes must compose");
    }

    #[test]
    fn deps_is_write_only_because_reading_them_is_the_base_profile() {
        let err = one(&format!(r#"{{"read":{{"deps":true}},{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("not a scope read accepts"), "{err}");
    }

    /// An empty `default` is legitimate ONLY beneath a band — it is how "latest passes
    /// ungranted" is spelled, and esbuild and bcrypt both measured exactly that. With no band
    /// the same entry is indistinguishable from the package being absent from the catalog.
    #[test]
    fn an_entry_that_widens_nothing_anywhere_is_rejected_but_an_empty_default_under_a_band_is_not()
    {
        let err = one(&format!(r#"{{{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("base profile"), "{err}");

        let c = parse(&format!(
            r#"{{"packages":{{"p":{{
                 "default":{{{NOTES}}},
                 "versions":{{"<6.0.0":{{"network":true,{NOTES}}}}}}}}}}}"#
        ))
        .expect("an empty default beneath a band states that latest passes ungranted");
        assert!(
            !c.packages["p"].default.network,
            "the empty default must stay empty"
        );
    }

    #[test]
    fn an_empty_platform_list_can_never_match() {
        let err = one(&format!(r#"{{"platforms":[],"network":true,{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("can never match"), "{err}");
    }

    #[test]
    fn an_unknown_field_is_a_typo_not_a_forward_compatible_key() {
        let err = one(&format!(r#"{{"netwrok":true,{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("unknown field `netwrok`"), "{err}");
    }

    #[test]
    fn platform_matching_selects_the_current_os() {
        let c = one(&format!(
            r#"{{"platforms":["macos"],"network":true,{NOTES}}}"#
        ))
        .expect("valid");
        let g = &c.packages["p"].default;
        assert!(g.matches_platform(Platform::Macos));
        assert!(!g.matches_platform(Platform::Linux));
    }

    // ── version resolution ────────────────────────────────────────────────────

    /// The rule that replaced first-match-wins. `0.5.0` sits inside all three bands, so the
    /// answer is decided by BOUND and nothing else — a resolver that stopped at the first
    /// match, or preferred the widest, returns a different grant here.
    #[test]
    fn among_the_bands_that_match_the_narrowest_bound_wins() {
        let c = parse(&format!(
            r#"{{"packages":{{"p":{{
                 "default":{{{NOTES}}},
                 "versions":{{
                   "<10.0.0":{{"write":"disk",{NOTES}}},
                   "<1.0.0":{{"write":{{"userHome":true}},{NOTES}}},
                   "<0.6.0":{{"network":true,{NOTES}}}}}}}}}}}"#
        ))
        .expect("valid");
        assert_eq!(
            resolve(&c, "0.5.0").write,
            Reach::None,
            "0.5.0 matches all three bands and must take <0.6.0, which grants no write"
        );
        assert!(resolve(&c, "0.5.0").network, "<0.6.0 grants network");
        assert_eq!(
            resolve(&c, "0.9.0").write,
            Reach::Scopes(vec![Scope::UserHome]),
            "0.9.0 is above <0.6.0, so the next-narrowest band <1.0.0 applies"
        );
        assert_eq!(
            resolve(&c, "5.0.0").write,
            Reach::Disk,
            "5.0.0 matches only <10.0.0"
        );
    }

    /// The invariant most likely to rot: resolution reads a JSON OBJECT, and any resolver that
    /// walked it in iteration order would pass every other test in this file while silently
    /// depending on how the generator happened to emit its keys.
    #[test]
    fn resolution_does_not_depend_on_the_order_the_bands_were_written_in() {
        let narrow = r#""<0.6.0":{"network":true,NOTES}"#;
        let wide = r#""<1.0.0":{"write":{"userHome":true},NOTES}"#;
        let build = |first: &str, second: &str| {
            let text = [
                r#"{"packages":{"p":{"default":{NOTES},"versions":{"#,
                first,
                ",",
                second,
                "}}}}",
            ]
            .concat()
            .replace("NOTES", NOTES);
            parse(&text).expect("valid")
        };
        let ascending = build(narrow, wide);
        let descending = build(wide, narrow);
        assert_eq!(
            resolve(&ascending, "0.5.0"),
            resolve(&descending, "0.5.0"),
            "the same bands in two key orders must resolve identically"
        );
        assert!(
            resolve(&ascending, "0.5.0").network,
            "and the answer must be the narrowest band, not merely a stable one"
        );
    }

    /// `default` is the answer for everything the bands do not reach — today's release, every
    /// future one, and anything the range matcher cannot judge (an absent version, a
    /// `workspace:` pin, a prerelease). The last is a strict improvement on the retired shape,
    /// where an unjudgeable version fell through to NO grant.
    #[test]
    fn a_version_no_band_matches_resolves_to_default() {
        let c = parse(&format!(
            r#"{{"packages":{{"p":{{
                 "default":{{"write":{{"project":true}},{NOTES}}},
                 "versions":{{"<1.0.0":{{"network":true,{NOTES}}}}}}}}}}}"#
        ))
        .expect("valid");
        let entry = &c.packages["p"];
        for version in [Some("2.0.0"), Some("0.9.0-rc.1"), Some("workspace:*"), None] {
            let g = entry.grant_for(version);
            assert!(
                !g.network && g.write == Reach::Scopes(vec![Scope::Project]),
                "{version:?} matches no band and must resolve to default, got {g:?}"
            );
        }
        assert!(
            entry.grant_for(Some("0.9.0")).network,
            "the control: a version the band DOES match must not resolve to default"
        );
    }

    // ── the two shapes a stale hand-written catalog takes ─────────────────────

    #[test]
    fn a_grant_carrying_its_own_versions_range_is_rejected() {
        let err = one(&format!(
            r#"{{"versions":"<1.0.0","network":true,{NOTES}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("no longer a grant field"), "{err}");
        assert!(
            err.contains("packages[p].default"),
            "the rejection must name the offending path: {err}"
        );
    }

    #[test]
    fn a_band_key_that_is_not_a_bound_is_rejected() {
        let err = parse(&format!(
            r#"{{"packages":{{"p":{{
                 "default":{{"network":true,{NOTES}}},
                 "versions":{{">=14.0.0, <19.0.0":{{"write":"disk",{NOTES}}}}}}}}}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("must be a `<` bound"), "{err}");

        // A `<` that is not a complete version cannot be ordered against the other bands, so it
        // is refused for the same reason rather than silently sorted by string.
        let err = parse(&format!(
            r#"{{"packages":{{"p":{{
                 "default":{{"network":true,{NOTES}}},
                 "versions":{{"<1.0":{{"write":"disk",{NOTES}}}}}}}}}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("not a complete semver version"), "{err}");
    }
}
