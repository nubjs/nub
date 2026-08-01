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

/// One grant. The matchers select it; the capabilities say what it widens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// A cargo semver range, or `None` for every version.
    pub versions: Option<String>,
    /// Empty means every platform.
    pub platforms: Vec<Platform>,
    pub read: Reach,
    pub write: Reach,
    pub network: bool,
    /// Free text. NOT machine-read — the earlier `evidence` enum was validated and then
    /// discarded by every consumer, so structuring this bought nothing.
    pub notes: String,
}

impl Grant {
    /// Does this grant apply on `platform`? Version matching is the caller's, because it
    /// needs the semver parser the compiler already links.
    pub fn matches_platform(&self, platform: Platform) -> bool {
        self.platforms.is_empty() || self.platforms.contains(&platform)
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
    /// Package name -> its grants, in written order. FIRST MATCH WINS.
    pub packages: BTreeMap<String, Vec<Grant>>,
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
        let list = value
            .as_array()
            .ok_or_else(|| format!("packages[{name}]: must be an ARRAY of grants"))?;
        if list.is_empty() {
            return Err(format!("packages[{name}]: has no grants; drop the entry"));
        }
        let mut grants = Vec::with_capacity(list.len());
        for (i, g) in list.iter().enumerate() {
            grants.push(parse_grant(g, &format!("packages[{name}][{i}]"))?);
        }
        reject_unreachable(&grants, name)?;
        packages.insert(name.clone(), grants);
    }
    Ok(Catalog {
        packages,
        baseline: parse_baseline(&root)?,
        env: parse_env(&root)?,
    })
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
        if notes.len() < 12 {
            return Err(format!(
                "{at}: `notes` must say why every jailed script needs this variable"
            ));
        }
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
        if notes.len() < 12 {
            return Err(format!(
                "{at}: `notes` must say why every jailed script needs this path"
            ));
        }
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
        if !matches!(
            key.as_str(),
            "versions" | "platforms" | "read" | "write" | "network" | "notes"
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
    if notes.len() < 12 {
        return Err(format!(
            "{at}: `notes` must say why this grant exists; it is the only thing a later reader gets"
        ));
    }

    let versions = match obj.get("versions") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| format!("{at}: `versions` must be a string semver range"))?
                .to_string(),
        ),
    };

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

    if read.is_none() && write.is_none() && !network {
        return Err(format!(
            "{at}: grants nothing; the base profile is what a package gets without an entry"
        ));
    }

    Ok(Grant {
        versions,
        platforms,
        read,
        write,
        network,
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

/// A grant nothing can select is the failure mode that hurts: it LOOKS present in the file
/// and never fires. `projectReads: ["."]` compiled to nothing for most of a measurement
/// campaign and every result taken through it was worthless, so these are build errors.
fn reject_unreachable(grants: &[Grant], name: &str) -> Result<(), String> {
    for (i, g) in grants.iter().enumerate() {
        let matches_everything = g.versions.is_none() && g.platforms.is_empty();
        if matches_everything && i + 1 < grants.len() {
            return Err(format!(
                "packages[{name}][{i}]: matches every version and platform but is not last, \
                 so the {} grant(s) after it can never be reached",
                grants.len() - i - 1
            ));
        }
        for (j, earlier) in grants.iter().enumerate().take(i) {
            if selects_superset(earlier, g) {
                return Err(format!(
                    "packages[{name}][{i}]: is unreachable — grant {j} already matches \
                     everything it does, and the FIRST match wins"
                ));
            }
        }
    }
    Ok(())
}

/// Does `a` match every input `b` does? Version ranges are compared textually: proving
/// containment needs a semver solver this crate does not link, so an EQUAL range counts and
/// anything else is treated as distinct. That errs toward accepting, which is the right
/// direction — this check exists to catch the obvious duplicate, not to be a decision
/// procedure.
fn selects_superset(a: &Grant, b: &Grant) -> bool {
    let versions = a.versions.is_none() || a.versions == b.versions;
    let platforms = a.platforms.is_empty()
        || (!b.platforms.is_empty() && b.platforms.iter().all(|p| a.platforms.contains(p)));
    versions && platforms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(body: &str) -> Result<Catalog, String> {
        parse(&format!(r#"{{"packages":{{"p":[{body}]}}}}"#))
    }
    const NOTES: &str = r#""notes":"measured on macOS by the grant search""#;

    #[test]
    fn a_narrow_read_and_write_compose() {
        let c = one(&format!(
            r#"{{"read":{{"userHome":true}},"write":{{"deps":true,"project":true}},{NOTES}}}"#
        ))
        .expect("valid");
        let g = &c.packages["p"][0];
        assert_eq!(g.read, Reach::Scopes(vec![Scope::UserHome]));
        assert_eq!(g.write, Reach::Scopes(vec![Scope::Deps, Scope::Project]));
        assert!(!g.network);
    }

    #[test]
    fn disk_is_a_separate_arm_not_another_scope() {
        assert_eq!(
            one(&format!(r#"{{"write":"disk",{NOTES}}}"#))
                .expect("valid")
                .packages["p"][0]
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

    #[test]
    fn a_grant_that_widens_nothing_is_rejected() {
        let err = one(&format!(r#"{{{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("grants nothing"), "{err}");
    }

    #[test]
    fn notes_are_required_because_they_are_all_a_later_reader_gets() {
        let err = one(r#"{"network":true,"notes":"why"}"#).unwrap_err();
        assert!(err.contains("`notes` must say why"), "{err}");
    }

    #[test]
    fn an_empty_platform_list_can_never_match() {
        let err = one(&format!(r#"{{"platforms":[],"network":true,{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("can never match"), "{err}");
    }

    #[test]
    fn a_match_everything_grant_must_be_last_or_it_shadows_the_rest() {
        let err = parse(&format!(
            r#"{{"packages":{{"p":[
                 {{"network":true,{NOTES}}},
                 {{"versions":"<1.0.0","write":"disk",{NOTES}}}]}}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("can never be reached"), "{err}");
    }

    #[test]
    fn a_grant_an_earlier_one_already_selects_is_unreachable() {
        let err = parse(&format!(
            r#"{{"packages":{{"p":[
                 {{"platforms":["macos","linux"],"network":true,{NOTES}}},
                 {{"platforms":["macos"],"write":"disk",{NOTES}}}]}}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("unreachable"), "{err}");
    }

    #[test]
    fn ordering_by_narrowness_is_accepted() {
        // The reachable spelling of the same intent: narrowest first, catch-all last.
        parse(&format!(
            r#"{{"packages":{{"p":[
                 {{"versions":"<0.13.0","write":"disk",{NOTES}}},
                 {{"network":true,{NOTES}}}]}}}}"#
        ))
        .expect("narrow-then-general must be accepted");
    }

    #[test]
    fn an_unknown_field_is_a_typo_not_a_forward_compatible_key() {
        let err = one(&format!(r#"{{"netwrok":true,{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("unknown field `netwrok`"), "{err}");
    }

    #[test]
    fn platform_matching_selects_the_current_os() {
        let g = one(&format!(
            r#"{{"platforms":["macos"],"network":true,{NOTES}}}"#
        ))
        .expect("valid")
        .packages["p"][0]
            .clone();
        assert!(g.matches_platform(Platform::Macos));
        assert!(!g.matches_platform(Platform::Linux));
    }
}
