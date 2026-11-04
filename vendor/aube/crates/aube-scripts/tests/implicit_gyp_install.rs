//! The implicit `node-gyp rebuild` install default, and npm's `gypfile`
//! opt-out from it.
//!
//! npm synthesizes the default in `@npmcli/run-script`
//! (`lib/run-script-pkg.js`) under `!scripts.install && !scripts.preinstall
//! && gypfile !== false && isNodeGypPackage(path)`. A package that ships a
//! `binding.gyp` for source builds but installs from prebuilt binaries opts
//! out with `"gypfile": false`; `better-sqlite3` >= 13 is the canonical case.
//! Ignoring the opt-out makes `install` run `node-gyp rebuild` on a package
//! npm installs with no toolchain at all, so the install fails on any machine
//! without Python and a C++ compiler.

use aube_manifest::PackageJson;
use aube_scripts::implicit_install_script;

fn manifest(json: &str) -> PackageJson {
    serde_json::from_str(json).expect("fixture manifest must parse")
}

#[test]
fn binding_gyp_with_no_install_script_defaults_to_node_gyp_rebuild() {
    // Positive control: without this the `gypfile` cases below would pass
    // against a function that never returns the default at all.
    let m = manifest(r#"{ "name": "native-thing", "version": "1.0.0" }"#);
    assert_eq!(
        implicit_install_script(&m, true),
        Some("node-gyp rebuild"),
        "a package shipping binding.gyp and no install/preinstall still gets npm's default"
    );
    assert_eq!(
        implicit_install_script(&m, false),
        None,
        "no binding.gyp means no default install script"
    );
}

#[test]
fn gypfile_false_opts_out_of_the_implicit_node_gyp_rebuild() {
    let m = manifest(r#"{ "name": "better-sqlite3", "version": "13.0.3", "gypfile": false }"#);
    assert_eq!(
        implicit_install_script(&m, true),
        None,
        "\"gypfile\": false is the author's opt-out: npm skips node-gyp rebuild, so must we"
    );
}

#[test]
fn only_gypfile_false_opts_out_matching_npms_strict_inequality() {
    // npm's gate is `gypfile !== false`, so `true` and a non-boolean both
    // leave the default in place. Guards against a truthiness-style check.
    for json in [
        r#"{ "name": "n", "version": "1.0.0", "gypfile": true }"#,
        r#"{ "name": "n", "version": "1.0.0", "gypfile": "false" }"#,
    ] {
        assert_eq!(
            implicit_install_script(&manifest(json), true),
            Some("node-gyp rebuild"),
            "only a literal `false` opts out, in {json}"
        );
    }
}

#[test]
fn an_explicit_install_script_still_wins_over_the_default() {
    let m = manifest(
        r#"{ "name": "n", "version": "1.0.0", "scripts": { "install": "prebuild-install" } }"#,
    );
    assert_eq!(implicit_install_script(&m, true), None);
}
