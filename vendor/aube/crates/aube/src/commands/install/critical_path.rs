/// Heuristic: is this registry name likely to run a native build
/// (`node-gyp`, `prebuild-install`, `cmake-js`, `@napi-rs/cli`) at
/// install time? Used by the critical-path fetch reorder to float
/// these packages to the front of the download queue so their build
/// step can pipeline with the remaining tarball downloads.
///
/// Curated allowlist: covers the long-tail native packages that
/// dominate cold-install wall time on a 1000-pkg graph. False
/// negatives are fine (the package fetches in normal order); false
/// positives are also fine (the package fetches earlier, no harm).
/// Update this list when bench data shows a new heavy native dep.
pub(super) fn is_likely_native_build(registry_name: &str) -> bool {
    // Exact-match heavy hitters. `ws` and `sass` deliberately
    // omitted: pure-JS by default; only `node-sass` is the
    // deprecated native build.
    const EXACT: &[&str] = &[
        "sharp",
        "esbuild",
        "fsevents",
        "canvas",
        "bcrypt",
        "node-sass",
        "sqlite3",
        "better-sqlite3",
        "lmdb",
        "msgpackr-extract",
        "sodium-native",
        "node-gyp",
        "prebuild-install",
        "node-gyp-build",
    ];
    if EXACT.contains(&registry_name) {
        return true;
    }
    // Scoped prefixes: `@swc/*`, `@parcel/*`, `@napi-rs/*`,
    // `@next/swc-*`, `@rollup/rollup-*`, `@esbuild/*` all ship
    // platform-specific native binaries.
    const PREFIXES: &[&str] = &[
        "@swc/",
        "@parcel/",
        "@napi-rs/",
        "@next/swc-",
        "@rollup/rollup-",
        "@esbuild/",
        "@playwright/",
        "@biomejs/",
    ];
    PREFIXES.iter().any(|p| registry_name.starts_with(p))
}

#[cfg(test)]
mod critical_path_tests {
    use super::is_likely_native_build;

    #[test]
    fn flags_exact_native_packages() {
        for name in [
            "sharp",
            "esbuild",
            "fsevents",
            "node-gyp",
            "better-sqlite3",
            "sodium-native",
        ] {
            assert!(is_likely_native_build(name), "{name} should match");
        }
    }

    #[test]
    fn flags_scoped_native_prefixes() {
        for name in [
            "@swc/core",
            "@swc/cli",
            "@parcel/source-map",
            "@napi-rs/cli",
            "@next/swc-linux-x64-gnu",
            "@rollup/rollup-linux-x64-gnu",
            "@esbuild/linux-x64",
            "@playwright/test",
            "@biomejs/biome",
        ] {
            assert!(is_likely_native_build(name), "{name} should match");
        }
    }

    #[test]
    fn does_not_flag_pure_js_packages() {
        for name in [
            "react",
            "lodash",
            "@types/node",
            "swc",  // unscoped, not native
            "ws",   // pure JS
            "sass", // dart-sass, pure JS
            "@scope/random",
        ] {
            assert!(!is_likely_native_build(name), "{name} should NOT match");
        }
    }

    #[test]
    fn sort_is_stable_within_groups() {
        // Mirror the sort applied at install/mod.rs. Stable sort
        // must keep relative order within natives and non-natives.
        let mut items = [
            ("react", 1),
            ("sharp", 2),
            ("lodash", 3),
            ("esbuild", 4),
            ("@types/node", 5),
            ("@swc/core", 6),
        ];
        items.sort_by_key(|(name, _)| !is_likely_native_build(name));
        let order: Vec<&str> = items.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            order,
            [
                "sharp",
                "esbuild",
                "@swc/core",
                "react",
                "lodash",
                "@types/node"
            ]
        );
    }
}
