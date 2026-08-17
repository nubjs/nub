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
//!
//! PER-OS OVERLAYS OVERRIDE FIELD BY FIELD. A grant carries the capability fields directly —
//! that is its answer everywhere — plus optional `macos` / `linux` / `win` blocks, each holding
//! the same fields and nothing else. A block REPLACES only the fields it NAMES and leaves the
//! rest of the outer grant standing, so `{write:"disk", macos:{write:{project:true}}}` is
//! `disk` on Linux and Windows and `project` on macOS with one line, rather than three grants
//! that must be kept in step. This is what the retired `platforms` FILTER could not express:
//! a filter could only make a whole grant apply or not, so a package whose needs merely DIFFER
//! by OS had to be written as several mutually-exclusive entries or, in practice, as one grant
//! carrying the widest answer everywhere. 44 of 581 package/versions measured on two or more
//! operating systems diverge in the expensive direction — `write:"disk"` on one OS and narrow
//! on another — so the filter was over-granting 7.6% of the corpus by construction.
//!
//! `null` IS HOW A BLOCK SAYS "NOT NEEDED HERE", and it is the only spelling of nothing at any
//! level. `{write:"disk", macos:{write:null}}` grants no write on macOS. Without it the
//! narrowing direction is inexpressible whenever the outer grant is the union — the author
//! would have to invert the entry so the outer is the INTERSECTION and every OS widens, which
//! is a different document for the same policy and one a generator gets wrong silently, in the
//! over-granting direction. `false` is not an alternative spelling: `network` still admits only
//! `true` or `null`, so there is exactly one way to write each answer.
//!
//! NESTING IS EXACTLY ONE LEVEL. A block may not contain `macos`/`linux`/`win`; that is a hard
//! parse error, not a silently-ignored key. There is no second OS to refine, so the only thing
//! a nested block could express is a contradiction.
//!
//! EVERY VALIDATION RUNS ON THE EFFECTIVE GRANT, ONCE PER OS — not on the outer grant and the
//! blocks in isolation. `write` implying read is the case that forces it: `{write:"disk",
//! macos:{read:{project:true}}}` is redundant on macOS and on no other OS, and neither half is
//! redundant read alone.

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
    /// Every OS an overlay can name. Iterated by the parser, which validates the EFFECTIVE
    /// grant on each rather than the outer grant and its blocks in isolation.
    pub const ALL: [Platform; 3] = [Self::Macos, Self::Linux, Self::Windows];

    /// The catalog's key for this OS. `win`, not `windows` — the schema's spelling.
    pub fn key(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Windows => "win",
        }
    }

    fn parse_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.key() == s)
    }

    /// The platform this build targets, so a caller can resolve a grant's overlays without
    /// having to know how to spell the current OS.
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

/// What a grant says on ONE operating system, with no overlay left to apply — the schema's
/// `Omit<Grant, "win"|"linux"|"macos">`.
///
/// THIS IS THE ONLY TYPE A CONSUMER SHOULD ACT ON. [`Grant`]'s own fields are private for
/// exactly that reason: reading a capability straight off the grant would answer from the
/// outer default and silently ignore an overlay that narrows it, which is the wrong-answer
/// this shape exists to make unrepresentable. Go through [`Grant::on`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Caps {
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

impl Caps {
    /// Does this widen anything at all beyond the base profile?
    pub fn widens_nothing(&self) -> bool {
        self.read.is_none() && self.write.is_none() && !self.network && self.write_paths.is_empty()
    }
}

/// Home-relative subpaths the BASELINE promotes — the cache and config directories that install
/// scripts write and that a fresh install genuinely needs to keep.
///
/// ⛔ THESE ARE PROMOTION TARGETS, NOT A LIVE GRANT ON THE REAL HOME, which is the whole reason a
/// baseline can afford them. The jail redirects `$HOME` to a throwaway, the script writes there, and
/// nub copies only these subpaths out once the scripts finish. A script therefore never holds a
/// handle on the real `$HOME`, so this cannot be used to plant a shell profile (`.bashrc`, `.zshrc`,
/// `.profile`) or a git hook — the persistence vector that rules a broad `userHome` WRITE out of any
/// baseline, and which the secret-path deny list does not cover because those files are not secrets.
///
/// Derived from what the corpus measured packages actually asking for, most-wanted first:
/// `.electron` (15), `.config/configstore` (11), `.backport` (11), `.config/netlify` (9),
/// `.amplify` (7), `.cache/Cypress` (7), `.cache/prisma` (5), `.cache/puppeteer` (4),
/// `.cache/electron` (3), `.cache/esbuild` (2). The prefixes below cover those and their siblings
/// without enumerating vendors, so a new tool that caches conventionally works with no catalog entry.
pub const BASELINE_WRITE_PATHS: &[&str] = &[
    ".cache",
    ".config",
    ".npm",
    ".electron",
    // Windows and macOS spell the same idea differently, and a baseline that named only the POSIX
    // form would silently be narrower on two of three platforms.
    "AppData/Local",
    "Library/Caches",
    "Library/Application Support",
];

/// What a package with NO catalog entry may do — the HOT-SWAPPABLE baseline.
///
/// ⛔⛔ THIS FUNCTION IS THE ENTIRE UNCATALOGUED POLICY. It exists as ONE named profile, expressed in
/// the same [`Caps`] vocabulary a catalog entry uses, so that widening or narrowing the baseline is
/// an edit here plus a test update — never a hunt through backend conditionals. It deliberately goes
/// through the same `apply_v2_grant` lowering as a catalog grant: a baseline with its own code path
/// is a second policy engine, and the two would drift.
///
/// WHY A BASELINE AT ALL, rather than denying everything an entry does not name. npm publishes
/// continuously and the catalog is compiled into the binary, so a package published after a release
/// is uncatalogued BY CONSTRUCTION and no amount of corpus growth closes that. Denying by default
/// therefore does not mean "secure", it means "install scripts break for a growing tail forever".
/// Measured over 2,028 packages with a known minimum grant: denying everything satisfies 54.2%,
/// while this baseline satisfies 96.4%.
///
/// WHAT IT DELIBERATELY WITHHOLDS, and why each is not a compatibility problem worth its risk:
/// - **No `read` beyond the base profile.** This is the load-bearing one. Credential FILES stay
///   unreadable, which is what breaks the read-then-exfiltrate pair: verified on linux, win32 and
///   macOS that a package granted `network` reads 0 of 5 planted credential decoys and receives 0 of
///   3 credential env vars while its socket still connects.
/// - **No `write` on the real `$HOME`.** See [`BASELINE_WRITE_PATHS`] — promotion covers the real
///   need (143 of 2,028 packages want any home write, and only 63 of those want it broadly) without
///   handing out a live handle.
/// - **No `write` on the project.** Granting it would let an unknown package rewrite the consuming
///   project's source and lockfile, which is how a supply-chain worm propagates. Costs ~0.4%.
/// - **No whole-disk anything.** On Windows a full-disk grant makes the backend decline the LowBox
///   token, and egress is an AppContainer capability, so fs AND network confinement are lost
///   together — a whole-disk baseline would mean no confinement at all on that platform.
///
/// Egress IS granted, and that is a real concession rather than an oversight: 90.1% of packages need
/// it and nothing narrower is expressible today (the proxy enforces per-host policy, but the grant
/// vocabulary's network axis is a boolean). The filesystem denials above are what keep it from being
/// an exfiltration channel.
pub fn baseline_caps() -> Caps {
    Caps {
        read: Reach::None,
        write: Reach::Scopes(vec![Scope::Deps]),
        network: true,
        write_paths: BASELINE_WRITE_PATHS.iter().map(|s| (*s).to_string()).collect(),
        notes: String::from("baseline profile for a package with no catalog entry"),
    }
}

/// A per-OS override: the same fields as [`Caps`], each `Some` exactly when the block NAMED it.
///
/// A named field replaces the outer one WHOLE — there is no merging within a field, so
/// `macos: {write: {project: true}}` under an outer `write: "disk"` is `project` on macOS and
/// not `project` unioned with anything. `null` names a field as nothing, which is the only way
/// to narrow toward the base profile; see the module doc.
///
/// This type doubles as the parse result for the OUTER grant's own fields, which is what keeps
/// one field parser honest for both positions — an absent outer field and an absent overlay
/// field mean different things (default vs. inherit), but they PARSE identically.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Overlay {
    pub read: Option<Reach>,
    pub write: Option<Reach>,
    pub network: Option<bool>,
    pub write_paths: Option<Vec<String>>,
    pub notes: Option<String>,
}

impl Overlay {
    /// Read as an OUTER grant's fields: an absent field is the default, not an inherit.
    fn into_caps(self) -> Caps {
        Caps {
            read: self.read.unwrap_or_default(),
            write: self.write.unwrap_or_default(),
            network: self.network.unwrap_or_default(),
            write_paths: self.write_paths.unwrap_or_default(),
            notes: self.notes.unwrap_or_default(),
        }
    }

    fn names_nothing(&self) -> bool {
        self == &Self::default()
    }
}

/// One grant: what a package's scripts may reach beyond the base profile, at the versions its
/// position in the entry covers. The version range is NOT here — it is the KEY of the entry's
/// `versions` map, so one grant can never carry two answers about its own scope.
///
/// The capability fields are the answer on every OS the overlays do not speak for; resolve one
/// OS's answer with [`Grant::on`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grant {
    base: Caps,
    macos: Option<Overlay>,
    linux: Option<Overlay>,
    windows: Option<Overlay>,
}

impl Grant {
    /// The ONE effective answer on `platform`: the outer fields with this OS's block laid over
    /// them field by field. Borrowed when no block names this OS, which is the common case.
    pub fn on(&self, platform: Platform) -> std::borrow::Cow<'_, Caps> {
        match self.overlay(platform) {
            None => std::borrow::Cow::Borrowed(&self.base),
            Some(o) => std::borrow::Cow::Owned(Caps {
                read: o.read.clone().unwrap_or_else(|| self.base.read.clone()),
                write: o.write.clone().unwrap_or_else(|| self.base.write.clone()),
                network: o.network.unwrap_or(self.base.network),
                write_paths: o
                    .write_paths
                    .clone()
                    .unwrap_or_else(|| self.base.write_paths.clone()),
                notes: o.notes.clone().unwrap_or_else(|| self.base.notes.clone()),
            }),
        }
    }

    fn overlay(&self, platform: Platform) -> Option<&Overlay> {
        match platform {
            Platform::Macos => self.macos.as_ref(),
            Platform::Linux => self.linux.as_ref(),
            Platform::Windows => self.windows.as_ref(),
        }
    }

    fn overlay_mut(&mut self, platform: Platform) -> &mut Option<Overlay> {
        match platform {
            Platform::Macos => &mut self.macos,
            Platform::Linux => &mut self.linux,
            Platform::Windows => &mut self.windows,
        }
    }

    /// Does this grant widen nothing on ANY operating system? An entry-level check, because an
    /// outer grant that widens nothing is legitimate the moment a block widens something —
    /// `@ffmpeg-installer/linux-x64` needs nothing on macOS and `disk` on Windows, which is
    /// exactly an empty outer plus one `win` block.
    fn widens_nothing(&self) -> bool {
        Platform::ALL.iter().all(|p| self.on(*p).widens_nothing())
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

    check_grant_keys(obj, at, /* nested = */ false)?;

    let mut grant = Grant {
        base: parse_fields(obj, at)?.into_caps(),
        ..Grant::default()
    };

    for platform in Platform::ALL {
        let Some(v) = obj.get(platform.key()) else {
            continue;
        };
        let at = format!("{at}.{}", platform.key());
        let block = v
            .as_object()
            .ok_or_else(|| format!("{at}: a per-OS override must be an object"))?;
        check_grant_keys(block, &at, /* nested = */ true)?;
        let overlay = parse_fields(block, &at)?;
        // AN OVERLAY THAT NAMES NOTHING OVERRIDES NOTHING, so it is the same defect as an empty
        // `writePaths` or an empty `read`: a line whose author believed it was doing something.
        // Note this is NOT the "needs nothing here" case — that one NAMES its fields as `null`.
        if overlay.names_nothing() {
            return Err(format!(
                "{at}: names no field, so it overrides nothing; omit it, or name the fields as \
                 `null` to say this OS needs them not at all"
            ));
        }
        *grant.overlay_mut(platform) = Some(overlay);
    }

    // VALIDATE THE EFFECTIVE GRANT, ONCE PER OS. Checking the outer fields and each block in
    // isolation would miss the case the overlays create: `{write:"disk", macos:{read:{project}}}`
    // is redundant only once the two are laid together, and neither half is wrong alone. Where a
    // grant has no overlay at all this reduces to exactly the old single check, which is why the
    // pre-overlay rejections keep their wording.
    for platform in Platform::ALL {
        let where_ = match grant.overlay(platform) {
            None => at.to_string(),
            Some(_) => format!("{at} (on {})", platform.key()),
        };
        check_write_implies_read(&grant.on(platform), &where_)?;
    }

    // NO "grants nothing" CHECK HERE. An empty grant is a positive statement under this shape —
    // an empty `default` says "latest passes ungranted", which is what makes a `<` band below it
    // meaningful. The emptiness that IS a defect is an entry with nothing anywhere, and
    // [`parse_entry`] owns that because only it can see the whole entry.
    Ok(grant)
}

/// Which keys a grant object may carry. `nested` is a per-OS block, whose extra rejection —
/// another per-OS key — is the whole one-level rule.
fn check_grant_keys(
    obj: &serde_json::Map<String, serde_json::Value>,
    at: &str,
    nested: bool,
) -> Result<(), String> {
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
        // A `platforms` FILTER IS NOT AN OVERRIDE, and accepting it as an unknown key would be
        // the worst of both: the grant would apply on EVERY OS, including the ones the author
        // wrote the field to exclude. Name the replacement rather than reporting a typo.
        if key == "platforms" {
            return Err(format!(
                "{at}: `platforms` was a FILTER and is retired; a grant now applies everywhere \
                 and a `macos`/`linux`/`win` block overrides it field by field, with `null` for \
                 a field this OS does not need"
            ));
        }
        if nested && Platform::parse_key(key).is_some() {
            return Err(format!(
                "{at}: a per-OS override may not contain `{key}` — overrides nest exactly one \
                 level, and there is no second OS for this block to refine"
            ));
        }
        let known = matches!(
            key.as_str(),
            "read" | "write" | "network" | "writePaths" | "notes"
        ) || (!nested && Platform::parse_key(key).is_some());
        if !known {
            return Err(format!("{at}: unknown field `{key}`"));
        }
    }
    Ok(())
}

/// The five capability fields, each `Some` exactly when the object NAMED it. `null` names a
/// field as nothing, which is what an overlay narrowing toward the base profile needs and what
/// an outer grant may spell instead of omitting the field.
fn parse_fields(
    obj: &serde_json::Map<String, serde_json::Value>,
    at: &str,
) -> Result<Overlay, String> {
    /// `Some(None)` — present and `null`, i.e. named as nothing. `None` — absent.
    fn named<'a>(
        obj: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<Option<&'a serde_json::Value>> {
        obj.get(key)
            .map(|v| if v.is_null() { None } else { Some(v) })
    }

    let read = match named(obj, "read") {
        None => None,
        Some(None) => Some(Reach::None),
        Some(Some(v)) => Some(parse_reach(
            v,
            at,
            "read",
            &[Scope::Project, Scope::UserHome],
        )?),
    };
    let write = match named(obj, "write") {
        None => None,
        Some(None) => Some(Reach::None),
        Some(Some(v)) => Some(parse_reach(
            v,
            at,
            "write",
            &[Scope::Deps, Scope::Project, Scope::UserHome],
        )?),
    };

    let network = match named(obj, "network") {
        None => None,
        Some(None) => Some(false),
        Some(Some(serde_json::Value::Bool(true))) => Some(true),
        // `false` IS REFUSED SO THAT NOTHING HAS TWO SPELLINGS. Every other field says "not
        // needed here" with `null`; letting `network` also say it with `false` would put one
        // answer in two dialects and leave a reader wondering whether they differ.
        Some(Some(_)) => {
            return Err(format!(
                "{at}: `network` may only be `true` or `null`; omit it to grant no egress, or \
                 write `null` to withdraw an outer grant on this OS"
            ));
        }
    };

    let write_paths = match named(obj, "writePaths") {
        None => None,
        Some(None) => Some(Vec::new()),
        Some(Some(v)) => Some(parse_write_paths(v, at)?),
    };

    let notes = match named(obj, "notes") {
        None => None,
        Some(None) => Some(String::new()),
        // Unvalidated free text, and a non-string is silently ignored rather than rejected —
        // preserved from the pre-overlay parser, which every generated catalog was written
        // against.
        Some(Some(v)) => Some(v.as_str().unwrap_or_default().trim().to_string()),
    };

    Ok(Overlay {
        read,
        write,
        network,
        write_paths,
        notes,
    })
}

/// `write` implies read at its own scope, so a `read` naming a scope the `write` already
/// covers means nothing. Reject it rather than honour it silently — a grant whose author
/// believed it was doing something is worse than one that fails the build.
fn check_write_implies_read(caps: &Caps, at: &str) -> Result<(), String> {
    // Order matters: `Reach::Disk` covers every scope, so the per-scope check below would
    // otherwise fire first and report the vaguer message for the disk case.
    if matches!(caps.write, Reach::Disk) && !caps.read.is_none() {
        return Err(format!(
            "{at}: `write: \"disk\"` already grants every read; remove `read`"
        ));
    }
    if let Reach::Scopes(rs) = &caps.read {
        for s in rs {
            if caps.write.covers(*s) {
                return Err(format!(
                    "{at}: `read.{}` is already implied by `write`; remove it",
                    s.as_str()
                ));
            }
        }
    }
    Ok(())
}

fn parse_write_paths(value: &serde_json::Value, at: &str) -> Result<Vec<String>, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| format!("{at}: `writePaths` must be an array of home-relative paths"))?;
    let mut promote: Vec<String> = Vec::with_capacity(arr.len());
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
    Ok(promote)
}

fn parse_reach(
    value: &serde_json::Value,
    at: &str,
    field: &str,
    allowed: &[Scope],
) -> Result<Reach, String> {
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

// ── emit ──────────────────────────────────────────────────────────────────────

/// Write a catalog back out. `parse(&emit(&c))` is `c` for every catalog this parser accepts,
/// which is the property the overlays needed: an overlay carries an OPTION per field, so a
/// serializer that flattened it — writing the effective value rather than `null` — would look
/// right, parse cleanly, and lose the narrowing on exactly the packages the overlays exist for.
///
/// Deterministic: `packages` is a `BTreeMap`, and every object below it is written in one fixed
/// field order, so two runs over one catalog produce identical bytes.
pub fn emit(catalog: &Catalog) -> String {
    use serde_json::{Map, Value, json};

    fn reach(r: &Reach) -> Value {
        match r {
            Reach::None => Value::Null,
            Reach::Disk => json!("disk"),
            Reach::Scopes(v) => {
                let mut m = Map::new();
                for s in v {
                    m.insert(s.as_str().to_string(), json!(true));
                }
                Value::Object(m)
            }
        }
    }

    /// The five capability fields. `omit_empty` is the OUTER grant, where an absent field and a
    /// `null` one mean the same thing and the shorter spelling is the honest one; inside an
    /// overlay `null` is load-bearing and must survive.
    fn fields(o: &Overlay, into: &mut Map<String, Value>, omit_empty: bool) {
        let mut put = |k: &str, v: Value| {
            if !(omit_empty && v.is_null()) {
                into.insert(k.to_string(), v);
            }
        };
        if let Some(r) = &o.read {
            put("read", reach(r));
        }
        if let Some(w) = &o.write {
            put("write", reach(w));
        }
        if let Some(n) = o.network {
            put("network", if n { json!(true) } else { Value::Null });
        }
        if let Some(p) = &o.write_paths {
            put(
                "writePaths",
                if p.is_empty() { Value::Null } else { json!(p) },
            );
        }
        if let Some(n) = &o.notes {
            put("notes", if n.is_empty() { Value::Null } else { json!(n) });
        }
    }

    fn grant(g: &Grant) -> Value {
        let mut m = Map::new();
        // The outer fields go through the same writer as an overlay, with every field named —
        // `omit_empty` then drops the ones that carry nothing, which is what makes an
        // all-defaults grant emit as `{}` rather than five nulls.
        fields(
            &Overlay {
                read: Some(g.base.read.clone()),
                write: Some(g.base.write.clone()),
                network: Some(g.base.network),
                write_paths: Some(g.base.write_paths.clone()),
                notes: Some(g.base.notes.clone()),
            },
            &mut m,
            true,
        );
        for platform in Platform::ALL {
            if let Some(o) = g.overlay(platform) {
                let mut block = Map::new();
                fields(o, &mut block, false);
                m.insert(platform.key().to_string(), Value::Object(block));
            }
        }
        Value::Object(m)
    }

    let mut packages = Map::new();
    for (name, entry) in &catalog.packages {
        let mut e = Map::new();
        e.insert("default".to_string(), grant(&entry.default));
        if !entry.versions.is_empty() {
            // Emitted widest-bound-first, matching the generator. Resolution is by bound, so
            // this is presentation only.
            let mut bands: Vec<&Band> = entry.versions.iter().collect();
            bands.sort_by(|a, b| b.bound.cmp(&a.bound));
            let mut v = Map::new();
            for b in bands {
                v.insert(b.range.clone(), grant(&b.grant));
            }
            e.insert("versions".to_string(), Value::Object(v));
        }
        packages.insert(name.clone(), Value::Object(e));
    }

    let baseline: Vec<Value> = catalog
        .baseline
        .iter()
        .map(|b| {
            let mut m = Map::new();
            m.insert("path".to_string(), json!(b.path));
            if b.write {
                m.insert("write".to_string(), json!(true));
            }
            if !b.notes.is_empty() {
                m.insert("notes".to_string(), json!(b.notes));
            }
            Value::Object(m)
        })
        .collect();
    let env: Vec<Value> = catalog
        .env
        .iter()
        .map(|e| {
            let mut m = Map::new();
            m.insert("name".to_string(), json!(e.name));
            m.insert("value".to_string(), json!(e.value));
            if !e.notes.is_empty() {
                m.insert("notes".to_string(), json!(e.notes));
            }
            Value::Object(m)
        })
        .collect();

    let mut root = Map::new();
    root.insert("packages".to_string(), Value::Object(packages));
    root.insert("baseline".to_string(), json!(baseline));
    root.insert("env".to_string(), json!(env));
    serde_json::to_string_pretty(&Value::Object(root)).expect("a catalog is always serializable")
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
        let g = &c.packages["p"].default.base;
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
                .base
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
            !c.packages["p"].default.base.network,
            "the empty default must stay empty"
        );
    }

    #[test]
    fn an_unknown_field_is_a_typo_not_a_forward_compatible_key() {
        let err = one(&format!(r#"{{"netwrok":true,{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("unknown field `netwrok`"), "{err}");
    }

    // ── per-OS overrides ──────────────────────────────────────────────────────

    /// `p`'s default grant as it applies on `platform` — the only shape a consumer ever acts on.
    fn on(catalog: &Catalog, platform: Platform) -> Caps {
        catalog.packages["p"].default.on(platform).into_owned()
    }

    /// The narrowing direction, which is the whole reason the shape changed: 44 of 581
    /// package/versions measured on two or more operating systems want `disk` on one and
    /// something narrow on another. A block replaces ONLY the field it names — `network`
    /// survives untouched, and a merge that unioned `write` instead of replacing it would leave
    /// macOS on `disk`.
    #[test]
    fn a_block_replaces_only_the_field_it_names() {
        let c = one(&format!(
            r#"{{"write":"disk","network":true,"macos":{{"write":{{"project":true}}}},{NOTES}}}"#
        ))
        .expect("valid");

        assert_eq!(
            on(&c, Platform::Macos).write,
            Reach::Scopes(vec![Scope::Project]),
            "the macos block must REPLACE `disk`, not union with it"
        );
        assert!(
            on(&c, Platform::Macos).network,
            "a field the block does not name must survive from the outer grant"
        );
        for p in [Platform::Linux, Platform::Windows] {
            assert_eq!(
                on(&c, p).write,
                Reach::Disk,
                "{p:?} has no block and must keep the outer answer verbatim"
            );
        }
    }

    /// The widening direction. `@opencode-ai/cli` reads `/proc/cpuinfo` on Linux and shells out
    /// to `sysctl` on macOS, so its needs genuinely differ; an outer grant plus one block that
    /// ADDS a capability is how that is written.
    #[test]
    fn a_block_may_widen_as_well_as_narrow() {
        let c = one(&format!(
            r#"{{"write":{{"project":true}},"linux":{{"write":"disk","network":true}},{NOTES}}}"#
        ))
        .expect("valid");
        assert_eq!(on(&c, Platform::Linux).write, Reach::Disk);
        assert!(on(&c, Platform::Linux).network);
        assert_eq!(
            on(&c, Platform::Macos).write,
            Reach::Scopes(vec![Scope::Project]),
            "widening one OS must not widen the others"
        );
        assert!(!on(&c, Platform::Macos).network);
    }

    /// THE NULL CASE. `null` is how a block says "not needed on this OS", and it is the only
    /// spelling — `network: false` is refused so one answer never has two dialects.
    #[test]
    fn null_withdraws_an_outer_grant_on_one_os() {
        let c = one(&format!(
            r#"{{"write":"disk","network":true,"writePaths":[".cache/x"],
                 "macos":{{"write":null,"network":null,"writePaths":null}},{NOTES}}}"#
        ))
        .expect("valid");
        let mac = on(&c, Platform::Macos);
        assert!(
            mac.widens_nothing(),
            "every field was withdrawn, so macOS must land on the base profile; got {mac:?}"
        );
        assert_eq!(
            on(&c, Platform::Windows).write,
            Reach::Disk,
            "the control: withdrawing on macOS must leave Windows alone"
        );

        let err = one(&format!(
            r#"{{"network":true,"macos":{{"network":false}},{NOTES}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("may only be `true` or `null`"), "{err}");
    }

    /// `@ffmpeg-installer/linux-x64@4.1.0`: nothing on macOS, `write:"disk"` on Windows. The
    /// entry-level "widens nothing" gate must look THROUGH the blocks, or this legitimate shape
    /// is rejected as an entry that grants exactly the base profile.
    #[test]
    fn an_empty_outer_grant_carrying_one_os_block_is_a_real_entry() {
        let c = one(&format!(r#"{{"win":{{"write":"disk"}},{NOTES}}}"#))
            .expect("an outer grant that widens nothing is legitimate beneath a block");
        assert_eq!(on(&c, Platform::Windows).write, Reach::Disk);
        assert!(
            on(&c, Platform::Macos).widens_nothing() && on(&c, Platform::Linux).widens_nothing(),
            "the OSes the entry says nothing about must get the base profile"
        );

        // The control: strip the block and the same entry IS just the base profile.
        let err = one(&format!(r#"{{{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("base profile"), "{err}");
    }

    /// One level, and no more. There is no second OS for a nested block to refine, so the only
    /// thing it could express is a contradiction — a hard error rather than an ignored key.
    #[test]
    fn a_block_may_not_nest_another_block() {
        for inner in ["macos", "linux", "win"] {
            let err = one(&format!(
                r#"{{"network":true,"macos":{{"{inner}":{{"write":"disk"}}}},{NOTES}}}"#
            ))
            .unwrap_err();
            assert!(err.contains("nest exactly one level"), "{inner}: {err}");
            assert!(
                err.contains("packages[p].default.macos"),
                "the rejection must name the offending path: {err}"
            );
        }
    }

    /// Strictness holds INSIDE a block too — the parser's whole posture is that an unknown key
    /// is a typo whose grant would silently go missing, and a nested block is the easiest place
    /// for that to hide.
    #[test]
    fn an_unknown_key_inside_a_block_is_rejected() {
        let err = one(&format!(
            r#"{{"network":true,"linux":{{"netwrok":true}},{NOTES}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("unknown field `netwrok`"), "{err}");
        assert!(err.contains("packages[p].default.linux"), "{err}");

        // `versions` keeps its dedicated message at every level, for the same reason it has one
        // at the top: accepting it drops the range silently.
        let err = one(&format!(
            r#"{{"network":true,"linux":{{"versions":"<1.0.0"}},{NOTES}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("no longer a grant field"), "{err}");
    }

    /// A block that names nothing overrides nothing, which is the same defect as an empty
    /// `writePaths` — a line whose author believed it was doing something. Distinct from the
    /// null case, which NAMES its fields.
    #[test]
    fn a_block_that_names_no_field_is_rejected() {
        let err = one(&format!(r#"{{"network":true,"win":{{}},{NOTES}}}"#)).unwrap_err();
        assert!(err.contains("overrides nothing"), "{err}");
    }

    /// The retired FILTER must not parse as an unknown-but-harmless key. Accepting it would be
    /// the worst outcome available: the grant applies on EVERY OS, including the ones the field
    /// was written to exclude.
    #[test]
    fn the_retired_platforms_filter_is_rejected_by_name() {
        let err = one(&format!(
            r#"{{"platforms":["macos"],"network":true,{NOTES}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("was a FILTER and is retired"), "{err}");
    }

    /// Validation runs on the EFFECTIVE grant, once per OS. Neither half here is redundant
    /// alone — the outer has no `read`, the block has no `write` — so a parser that checked the
    /// outer grant and the blocks separately accepts this and silently honours a `read` that
    /// `write: "disk"` already covers.
    #[test]
    fn write_implies_read_is_checked_after_the_block_is_laid_on() {
        let err = one(&format!(
            r#"{{"write":"disk","macos":{{"read":{{"project":true}}}},{NOTES}}}"#
        ))
        .unwrap_err();
        assert!(err.contains("already grants every read"), "{err}");
        assert!(
            err.contains("(on macos)"),
            "the rejection must name the OS it fires on, since it fires on no other: {err}"
        );

        // …and the mirror: a block that WITHDRAWS the write makes the same pair legitimate.
        let c = one(&format!(
            r#"{{"write":"disk","macos":{{"write":null,"read":{{"project":true}}}},{NOTES}}}"#
        ))
        .expect("no OS sees both, so nothing is redundant");
        assert_eq!(
            on(&c, Platform::Macos).read,
            Reach::Scopes(vec![Scope::Project])
        );
        assert_eq!(on(&c, Platform::Macos).write, Reach::None);
    }

    /// Overlays live on BANDS as well as on `default` — nothing about the version axis makes an
    /// old release's needs uniform across operating systems.
    #[test]
    fn a_version_band_carries_its_own_blocks() {
        let c = parse(&format!(
            r#"{{"packages":{{"p":{{
                 "default":{{"network":true,{NOTES}}},
                 "versions":{{"<1.0.0":{{"write":"disk","macos":{{"write":null}},{NOTES}}}}}}}}}}}"#
        ))
        .expect("valid");
        let band = c.packages["p"].grant_for(Some("0.9.0"));
        assert_eq!(band.on(Platform::Linux).write, Reach::Disk);
        assert_eq!(band.on(Platform::Macos).write, Reach::None);
    }

    // ── round trip ────────────────────────────────────────────────────────────

    /// `parse(emit(c)) == c`. The property that matters is the OPTION per overlay field: a
    /// serializer that wrote the EFFECTIVE value instead of `null` would look right, parse
    /// cleanly, and silently lose every narrowing — on exactly the packages the overlays exist
    /// for.
    #[test]
    fn emit_round_trips_including_a_withdrawn_field() {
        let text = format!(
            r#"{{"packages":{{
                 "p":{{
                   "default":{{"write":"disk","network":true,
                               "macos":{{"write":{{"project":true}},"network":null}},
                               "win":{{"writePaths":[".cache/x"]}},{NOTES}}},
                   "versions":{{"<1.0.0":{{"read":{{"userHome":true}},"linux":{{"read":null}},{NOTES}}}}}}},
                 "q":{{"default":{{"write":{{"deps":true}},{NOTES}}}}}}},
               "baseline":[{{"path":"$cache/x","write":true,"notes":"n"}}],
               "env":[{{"name":"PYTHONDONTWRITEBYTECODE","value":"1"}}]}}"#
        );
        let first = parse(&text).expect("fixture must parse");
        let emitted = emit(&first);
        let second = parse(&emitted)
            .unwrap_or_else(|e| panic!("emitted catalog must parse: {e}\n{emitted}"));
        assert_eq!(first, second, "emitted:\n{emitted}");
        assert_eq!(emit(&second), emitted, "emit must be deterministic");

        // The control: the withdrawal must actually be IN the emitted bytes. Without this the
        // assertion above passes for a serializer that dropped both the block and the field it
        // withdraws — `first == second` would still hold if `emit` lost information symmetrically
        // on a re-parse it never performs.
        assert!(
            emitted.contains("\"network\": null"),
            "a withdrawn field must survive as `null`:\n{emitted}"
        );
        assert!(
            !first.packages["p"].default.on(Platform::Macos).network,
            "fixture precondition: macOS must be the OS whose network was withdrawn"
        );
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
            resolve(&c, "0.5.0").base.write,
            Reach::None,
            "0.5.0 matches all three bands and must take <0.6.0, which grants no write"
        );
        assert!(resolve(&c, "0.5.0").base.network, "<0.6.0 grants network");
        assert_eq!(
            resolve(&c, "0.9.0").base.write,
            Reach::Scopes(vec![Scope::UserHome]),
            "0.9.0 is above <0.6.0, so the next-narrowest band <1.0.0 applies"
        );
        assert_eq!(
            resolve(&c, "5.0.0").base.write,
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
            resolve(&ascending, "0.5.0").base.network,
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
                !g.base.network && g.base.write == Reach::Scopes(vec![Scope::Project]),
                "{version:?} matches no band and must resolve to default, got {g:?}"
            );
        }
        assert!(
            entry.grant_for(Some("0.9.0")).base.network,
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
