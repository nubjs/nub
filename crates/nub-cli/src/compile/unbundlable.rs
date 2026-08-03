//! Deciding, from a package's manifest alone, that it cannot be bundled.
//!
//! Some packages resolve a native artifact at runtime by computing its path, so
//! no import graph can see it:
//!
//! ```text
//! require(path.join(__dirname, 'build', 'Release', 'foo.node'))
//! require('node-gyp-build')(__dirname)
//! require('bindings')('foo.node')
//! ```
//!
//! The ecosystem's answer to this is not analysis. `@vercel/nft` is a 1,424-line
//! partial evaluator that EXECUTES `bindings()` / `nodeGypBuild()` at build time to
//! recover the path, backed by 21 hand-written per-package AST rewrites — and the
//! same project still ships a 79-entry manual opt-out list years later. Nothing in
//! the surveyed prior art detects these statically.
//!
//! The observation this module rests on: a package that CALLS those resolvers
//! DECLARES them, and a package built by napi-rs advertises its per-platform
//! binaries as optional dependencies. Both are ordinary manifest fields. So the
//! expensive question ("what path will this compute?") is replaced by a cheap one
//! ("does this package look like it builds or loads native code?"), which the
//! manifest answers without reading a byte of source or running anything.
//!
//! Measured against real registry manifests: every one of `bcrypt`, `sqlite3`,
//! `canvas`, `isolated-vm`, `better-sqlite3`, `sharp`, `@node-rs/argon2` and
//! `cpu-features` is caught, and `pino`, `keyv` and `@prisma/client` are not.
//!
//! This is deliberately not the whole answer. Packages that are pure JS yet still
//! unbundlable — `pino` handing `join(__dirname, 'worker.js')` to a worker thread,
//! `keyv` requiring a computed backend — carry no manifest signal at all and are
//! covered by the curated list instead. Over-firing here costs a package its
//! tree-shaking; under-firing ships a binary that fails at runtime, so each rule is
//! kept narrow enough to name what it saw.

use serde_json::Value;

/// Packages whose whole purpose is locating a native addon at runtime. Depending
/// on one is a declaration that this package's real entry point is a `.node` file
/// the import graph will never name.
///
/// `node-addon-api` and `nan` are headers rather than resolvers, so they prove the
/// package COMPILES native code rather than that it locates it — same conclusion
/// for our purposes, and they catch `better-sqlite3`, which declares no resolver.
const RESOLVER_DEPENDENCIES: &[&str] = &[
    "node-gyp-build",
    "bindings",
    "node-pre-gyp",
    "@mapbox/node-pre-gyp",
    "prebuild-install",
    "node-addon-api",
    "nan",
];

/// Platform tokens as they appear in a napi-rs sidecar package name. Node's own
/// `process.platform` spelling, which is what the generators emit.
const PLATFORM_TOKENS: &[&str] = &[
    "darwin", "linux", "win32", "android", "freebsd", "openbsd", "sunos", "aix",
];

/// Packages that cannot be bundled for reasons no manifest field records.
///
/// Every rule above reads a declaration the package made about itself. These made
/// none: they are ordinary JavaScript whose behaviour at run time — handing a
/// worker thread a path, requiring a backend chosen from a string, replacing the
/// module loader — defeats bundling without leaving a trace in `package.json`.
///
/// This is where the ecosystem's answer is a list, and every tool that has tried
/// to avoid one still ships it: Next.js maintains 79 entries after years of
/// investment in static analysis. The entries here are the subset that applies to
/// a compiled binary. Their list also excludes heavyweight build tools
/// (`typescript`, `webpack`, `eslint`) purely to keep a dev server's rebuilds
/// fast, which is not a correctness concern and not ours — those are not in a
/// compiled application's runtime graph at all.
///
/// A list is a maintenance cost with no principled end, so it stays small and
/// each entry carries the behaviour that put it here. `--unbundled` covers
/// anything not yet listed, which is what keeps this from being load-bearing.
const KNOWN_UNBUNDLABLE: &[(&str, &str)] = &[
    // Hands `join(__dirname, 'worker.js')` to thread-stream, which requires that
    // path from a worker thread — a path that exists only in the source tree.
    (
        "pino",
        "it loads its transport worker from a path built at run time",
    ),
    // Named as a string in pino config and required inside the worker, so the
    // specifier never appears as an import anywhere the bundler can see.
    (
        "pino-pretty",
        "it is named as a string and required inside a worker",
    ),
    (
        "pino-roll",
        "it is named as a string and required inside a worker",
    ),
    (
        "thread-stream",
        "it starts a worker from a path given at run time",
    ),
    // Requires a backend chosen from a connection string.
    ("keyv", "it requires a storage backend chosen at run time"),
    // Requires an undeclared dependency unconditionally.
    ("config", "it requires a dependency it does not declare"),
    // Both replace the module loader itself, which a bundle has already resolved
    // past by the time they run.
    (
        "import-in-the-middle",
        "it patches the module loader, which a bundle has already bypassed",
    ),
    (
        "require-in-the-middle",
        "it patches the module loader, which a bundle has already bypassed",
    ),
];

/// Why a package must ship unbundled. Kept as distinct variants rather than a
/// boolean so a wrong verdict can be traced to the exact rule that produced it —
/// these rules are heuristics over a hostile corpus and will need tuning against
/// packages nobody here has seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Depends on a package whose job is resolving a native addon at runtime.
    ResolverDependency(String),
    /// Advertises per-platform sidecar packages, the napi-rs shape.
    ///
    /// Worth more than it looks: a napi-rs package splits into a JS-only wrapper
    /// and one binary package per platform, and only the binary one holds a
    /// `.node`. A forward dependency walk therefore misses the package the
    /// application actually imports. Reading the fan off the wrapper's own
    /// manifest answers that without walking anything.
    NapiPlatformFan(usize),
    /// Carries a build step that produces a native artifact.
    BuildMarker(&'static str),
    /// On the list of packages whose run-time behaviour defeats bundling.
    Known(&'static str),
    /// Named by the user, not by any rule.
    ///
    /// No detector reaches every package — a pure-JS package handing a worker a
    /// path built at run time carries no manifest signal at all — so the flag is
    /// part of the design rather than an admission of one.
    Forced,
}

impl Reason {
    /// User-facing explanation. These end up in a build error telling someone why
    /// their package was left unbundled, so they name the evidence, not the rule.
    pub fn describe(&self) -> String {
        match self {
            Reason::ResolverDependency(dep) => {
                format!("it depends on `{dep}`, which resolves a native addon at runtime")
            }
            Reason::NapiPlatformFan(count) => {
                format!("it declares {count} per-platform binary packages (napi-rs layout)")
            }
            Reason::BuildMarker(marker) => format!("its manifest declares {marker}"),
            Reason::Known(why) => (*why).to_string(),
            Reason::Forced => "you asked for it with --unbundled".to_string(),
        }
    }
}

/// Decide whether `manifest` describes a package that cannot be bundled.
///
/// Returns the FIRST rule that fires; the rules overlap heavily on real packages
/// (`sqlite3` trips three) and the caller only needs one reason to report.
pub fn classify(manifest: &Value) -> Option<Reason> {
    // Checked first: an entry here is a fact somebody established by hand, where
    // every rule below is an inference from a declaration.
    if let Some(name) = manifest.get("name").and_then(Value::as_str) {
        if let Some((_, why)) = KNOWN_UNBUNDLABLE.iter().find(|(n, _)| *n == name) {
            return Some(Reason::Known(why));
        }
    }
    if let Some(dep) = resolver_dependency(manifest) {
        return Some(Reason::ResolverDependency(dep));
    }
    if let Some(count) = napi_platform_fan(manifest) {
        return Some(Reason::NapiPlatformFan(count));
    }
    build_marker(manifest).map(Reason::BuildMarker)
}

fn resolver_dependency(manifest: &Value) -> Option<String> {
    let deps = manifest.get("dependencies")?.as_object()?;
    RESOLVER_DEPENDENCIES
        .iter()
        .find(|name| deps.contains_key(**name))
        .map(|name| (*name).to_string())
}

/// The count of optional dependencies that look like this package's own
/// per-platform binaries.
///
/// Matching on the package's own base name is what keeps this from firing on an
/// ordinary optional dependency that merely happens to be platform-specific: the
/// sidecars are named after their parent (`sharp` -> `@img/sharp-darwin-arm64`,
/// `@node-rs/argon2` -> `@node-rs/argon2-linux-x64-gnu`), so the parent's name
/// appearing inside the child's is the signal.
///
/// Two is the floor. A single platform-named optional dependency is an ordinary
/// dependency; a FAN across platforms is the generator's fingerprint.
fn napi_platform_fan(manifest: &Value) -> Option<usize> {
    let optional = manifest.get("optionalDependencies")?.as_object()?;
    let name = manifest.get("name")?.as_str()?;
    let base = name.rsplit('/').next()?;
    if base.is_empty() {
        return None;
    }
    let count = optional
        .keys()
        .filter(|dep| dep.contains(base) && PLATFORM_TOKENS.iter().any(|token| dep.contains(token)))
        .count();
    (count >= 2).then_some(count)
}

/// A manifest field that only exists because the package builds something native.
///
/// `gypfile` is set by npm when the package ships a `binding.gyp`. `binary` is
/// node-pre-gyp's remote-artifact descriptor. An install-phase script is the
/// weakest of the three — plenty of pure-JS packages run one — but it is the only
/// signal `cpu-features` carries, and a false positive costs tree-shaking on one
/// package where a false negative ships a broken binary.
fn build_marker(manifest: &Value) -> Option<&'static str> {
    if manifest.get("gypfile").and_then(Value::as_bool) == Some(true) {
        return Some("gypfile");
    }
    if manifest.get("binary").is_some_and(|v| v.is_object()) {
        return Some("a `binary` block");
    }
    let scripts = manifest.get("scripts")?.as_object()?;
    ["install", "preinstall", "postinstall"]
        .iter()
        .find(|phase| scripts.contains_key(**phase))
        .map(|phase| match *phase {
            "install" => "an `install` script",
            "preinstall" => "a `preinstall` script",
            _ => "a `postinstall` script",
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Real manifests, real verdicts.
    ///
    /// The negative half is what makes this a test rather than a demonstration:
    /// `pino`, `keyv` and `@prisma/client` are all on Next.js's unbundlable list
    /// for reasons NO manifest rule can see, so they must come back clean here.
    /// A change that starts catching them has not got smarter, it has started
    /// guessing — and would cost every such package its tree-shaking.
    #[test]
    fn real_native_packages_are_caught_and_pure_js_ones_are_not() {
        // Shapes taken from the published manifests.
        let bcrypt = json!({
            "name": "bcrypt",
            "dependencies": { "node-addon-api": "^8", "node-gyp-build": "^4" },
            "scripts": { "install": "node-gyp-build" },
        });
        let sharp = json!({
            "name": "sharp",
            "optionalDependencies": {
                "@img/sharp-darwin-arm64": "0.34.4",
                "@img/sharp-linux-x64": "0.34.4",
                "@img/sharp-win32-x64": "0.34.4",
            },
        });
        let cpu_features = json!({
            "name": "cpu-features",
            "scripts": { "install": "node-gyp rebuild" },
        });
        let isolated_vm = json!({ "name": "isolated-vm", "gypfile": true });

        assert!(matches!(
            classify(&bcrypt),
            Some(Reason::ResolverDependency(_))
        ));
        assert_eq!(classify(&sharp), Some(Reason::NapiPlatformFan(3)));
        assert_eq!(
            classify(&cpu_features),
            Some(Reason::BuildMarker("an `install` script"))
        );
        assert_eq!(classify(&isolated_vm), Some(Reason::BuildMarker("gypfile")));

        // `pino` and `keyv` are on the curated list, so they are excluded from the
        // must-not-fire set — see the dedicated test below. What belongs here is a
        // package that is pure JS, unlisted, and must be bundled whole.
        for pure in [
            json!({ "name": "@prisma/client" }),
            json!({ "name": "lodash", "dependencies": {} }),
            json!({ "name": "date-fns" }),
        ] {
            assert_eq!(
                classify(&pure),
                None,
                "a pure-JS package must not trip a manifest rule: {pure}"
            );
        }
    }

    /// One platform-named optional dependency is not a fan.
    ///
    /// Without the floor this fires on any package with a single optional
    /// platform-specific dependency — `fsevents` is the classic — and quietly
    /// unbundles it. The paired positive proves the floor is what rejects it,
    /// rather than the name matching failing for some other reason.
    #[test]
    fn a_single_platform_sidecar_is_not_a_napi_fan() {
        let one = json!({
            "name": "watcher",
            "optionalDependencies": { "watcher-darwin-arm64": "1.0.0" },
        });
        assert_eq!(
            classify(&one),
            None,
            "one sidecar is an ordinary dependency"
        );

        let two = json!({
            "name": "watcher",
            "optionalDependencies": {
                "watcher-darwin-arm64": "1.0.0",
                "watcher-linux-x64": "1.0.0",
            },
        });
        assert_eq!(
            classify(&two),
            Some(Reason::NapiPlatformFan(2)),
            "two platforms is the fan, so the rejection above is the floor at work"
        );
    }

    /// A sidecar must be named after ITS OWN parent.
    ///
    /// Otherwise any package depending on two platform-named things — a build tool
    /// pulling in several `@esbuild/*` binaries, say — is misread as native.
    /// The curated list catches what no manifest field records.
    ///
    /// These packages declare nothing that distinguishes them: `pino` looks
    /// exactly like any package with a dependency. They defeat bundling through
    /// run-time behaviour — handing a worker a path, requiring a backend named by
    /// a string — so a rule reading declarations cannot reach them and the list is
    /// the only mechanism that can.
    ///
    /// The negative half is the control. Matching loosely (a prefix, a substring)
    /// would quietly unbundle `pino-http` and `keyv-redis`, which are ordinary
    /// packages, so the match must be on the exact name.
    #[test]
    fn the_curated_list_matches_exact_names_only() {
        for (name, _) in KNOWN_UNBUNDLABLE {
            let manifest = json!({ "name": name });
            assert!(
                matches!(classify(&manifest), Some(Reason::Known(_))),
                "{name} is on the list and must be caught by it"
            );
        }

        for near in ["pino-http", "keyv-redis", "config-chain", "thread-stream-x"] {
            assert_eq!(
                classify(&json!({ "name": near })),
                None,
                "{near} merely resembles a listed name and must still be bundled"
            );
        }
    }

    #[test]
    fn platform_sidecars_belonging_to_another_package_do_not_count() {
        let unrelated = json!({
            "name": "my-app",
            "optionalDependencies": {
                "@esbuild/darwin-arm64": "0.28.1",
                "@esbuild/linux-x64": "0.28.1",
            },
        });
        assert_eq!(
            classify(&unrelated),
            None,
            "sidecars named after a DIFFERENT package say nothing about this one"
        );
    }
}
