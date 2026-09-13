use super::HOST_VERBS;

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
