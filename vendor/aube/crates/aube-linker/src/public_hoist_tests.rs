use super::*;

fn linker_with(patterns: &[&str]) -> Linker {
    // Construct a Linker without touching disk: we only call
    // `public_hoist_matches`, which never looks at `store` or
    // `virtual_store`. A dummy store is acceptable because
    // Store::clone is cheap and this test never invokes a method
    // that would actually touch the CAS.
    let store = Store::at(std::env::temp_dir().join("aube-public-hoist-test"));
    let strs: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
    Linker::new(&store, LinkStrategy::Copy).with_public_hoist_pattern(&strs)
}

#[test]
fn empty_pattern_matches_nothing() {
    let l = linker_with(&[]);
    assert!(!l.public_hoist_matches("react"));
    assert!(!l.public_hoist_matches("eslint"));
}

#[test]
fn wildcard_matches_substring() {
    let l = linker_with(&["*eslint*", "*prettier*"]);
    assert!(l.public_hoist_matches("eslint"));
    assert!(l.public_hoist_matches("eslint-plugin-react"));
    assert!(l.public_hoist_matches("@typescript-eslint/parser"));
    assert!(l.public_hoist_matches("prettier"));
    assert!(!l.public_hoist_matches("react"));
}

#[test]
fn exact_name_match() {
    let l = linker_with(&["react"]);
    assert!(l.public_hoist_matches("react"));
    assert!(!l.public_hoist_matches("react-dom"));
}

#[test]
fn negation_excludes_positive_match() {
    let l = linker_with(&["*eslint*", "!eslint-config-*"]);
    assert!(l.public_hoist_matches("eslint"));
    assert!(l.public_hoist_matches("eslint-plugin-react"));
    assert!(!l.public_hoist_matches("eslint-config-next"));
}

#[test]
fn case_insensitive() {
    let l = linker_with(&["*ESLINT*"]);
    assert!(l.public_hoist_matches("eslint"));
    assert!(l.public_hoist_matches("ESLint"));
}

#[test]
fn invalid_patterns_are_silently_dropped() {
    // `[` opens an unclosed character class — glob::Pattern::new
    // rejects it; the builder skips the pattern instead of
    // failing install. The accompanying valid pattern still
    // matches.
    let l = linker_with(&["[unterminated", "react"]);
    assert!(l.public_hoist_matches("react"));
    assert!(!l.public_hoist_matches("eslint"));
}
