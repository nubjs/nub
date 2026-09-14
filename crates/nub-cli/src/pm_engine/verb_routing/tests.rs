use super::{HOST_VERBS, names_nubs_own_config};
use std::ffi::OsString;

/// Every verb nub keeps has to be a verb the engine actually serves,
/// or the entry guards nothing and the reason it was written is gone —
/// a pin move that renames one would otherwise hand nub's verb to the
/// engine with nothing to say so.
#[test]
fn every_verb_nub_keeps_is_one_the_engine_would_have_taken() {
    let unknown: Vec<&&str> = HOST_VERBS
        .iter()
        .filter(|verb| {
            let argv = ["nub", verb].map(std::ffi::OsString::from);
            pnpm_cli::command_name(&argv).as_deref() != Some(**verb)
        })
        .collect();
    assert!(
        unknown.is_empty(),
        "the engine names no such verb: {unknown:?}"
    );
}

/// A config command line stays nub's when it names something pnpm has no
/// counterpart for, wherever its flags sit.
#[test]
fn a_config_line_naming_nubs_own_surface_stays_with_nub() {
    let argv = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
    for own in [
        &["nub", "config", "init"][..],
        &["nub", "config", "path"],
        &["nub", "config", "--global", "set", "dlx.consent", "never"],
        &["nub", "set", "nodeCompat=true"],
        &["nub", "config", "get", "exec.implicit-dlx"],
    ] {
        assert!(names_nubs_own_config(&argv(own)), "{own:?}");
    }
    for pnpms in [
        &["nub", "config", "list"][..],
        &[
            "nub",
            "config",
            "set",
            "--location",
            "project",
            "nodeLinker",
            "hoisted",
        ],
        &["nub", "get", "registry"],
    ] {
        assert!(!names_nubs_own_config(&argv(pnpms)), "{pnpms:?}");
    }
}
