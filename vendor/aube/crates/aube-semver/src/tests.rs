use super::*;

// Regenerate with npm's `semver` beside a script that prints
// `semver.validRange(r)` and `semver.satisfies(v, r)` for each pair.
const VERSIONS: &[&str] = &[
    "0.0.0",
    "0.0.3",
    "0.0.4",
    "0.2.0",
    "0.2.5",
    "1.0.0-beta.1",
    "1.0.0-rc.1",
    "1.0.0-rc.2",
    "1.0.0",
    "1.2.0",
    "1.2.3-alpha",
    "1.2.3-beta.2",
    "1.2.3-beta.10",
    "1.2.3",
    "1.2.4-beta",
    "1.2.4",
    "1.3.0",
    "1.9.9",
    "2.0.0-0",
    "2.0.0",
    "2.3.4",
    "3.5.0",
    "11.2.9",
    "11.3.0-0",
    "11.15.0",
    "11.20.0",
    "11.21.0",
    "11.29.0",
    "12.0.0",
    "v1.2.3",
    "1.02.3",
];

/// `(range, semver.validRange, semver.satisfies per VERSIONS)`, from npm's
/// `semver` 7.8.6.
const NPM: &[(&str, Option<&str>, &str)] = &[
    ("<=11", Some("<12.0.0-0"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYn"),
    (
        "<=11.2",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYn",
    ),
    (
        "<=11.x",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYn",
    ),
    (
        "<=11.2.x",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYn",
    ),
    (
        "<=11.*",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYn",
    ),
    (
        "<= 11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYn",
    ),
    (
        "<=v11.2",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYn",
    ),
    (
        "<==11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYn",
    ),
    (
        ">=2 <1",
        Some(">=2.0.0 <1.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=2 <1 || 0.5.0",
        Some(">=2.0.0 <1.0.0-0||0.5.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <1.2.0",
        Some(">1.2.3 <1.2.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0 <=1.0.0",
        Some(">=1.0.0 <=1.0.0"),
        "nnnnnnnnYnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <=11.20.* || 11.15.0",
        Some(">=11.29.0 <11.21.0-0||11.15.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnYnnnnnn",
    ),
    (
        ">=11.29.0 <=11.20 || 11.15.0",
        Some(">=11.29.0 <11.21.0-0||11.15.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnYnnnnnn",
    ),
    (
        ">=11.29.0 <11.21.0-0",
        Some(">=11.29.0 <11.21.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <1.2.3",
        Some(">1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.2.3 <1.2.3",
        Some(">=1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<0.0.0-0 || 1.0.0",
        Some("1.0.0"),
        "nnnnnnnnYnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <=1.2.3-0",
        Some(">1.2.3 <=1.2.3-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (">*", Some("<0.0.0-0"), "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("<*", Some("<0.0.0-0"), "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "<x || 1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYn",
    ),
    ("<=11.029", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.02.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("<=01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("01.2.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.2.3-01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("~01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("^1.02", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3-0a",
        Some("1.2.3-0a"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "9007199254740991.0.0",
        Some("9007199254740991.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "9007199254740992.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    ("^9007199254740991", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "^9007199254740990",
        Some(">=9007199254740990.0.0 <9007199254740991.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=9007199254740990",
        Some("<9007199254740991.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn",
    ),
    (
        "~1.9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.9007199254740991.x",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "99999999999999999999999",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=99999999999999999999",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0.18446744073709551615",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 18446744073709551615.x",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 2.3.4",
        Some(">=1.2.3 <=2.3.4"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYn",
    ),
    (
        "1.2 - 2.3",
        Some(">=1.2.0 <2.4.0-0"),
        "nnnnnnnnnYnnnYnYYYnYYnnnnnnnnYn",
    ),
    (
        "1.2.3 - 2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYn",
    ),
    (
        "1 - 2",
        Some(">=1.0.0 <3.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnYYnnnnnnnnYn",
    ),
    ("* - 2", Some("<3.0.0-0"), "YYYYYnnnYYnnnYnYYYnYYnnnnnnnnYn"),
    (
        "1.2.3 - *",
        Some(">=1.2.3"),
        "nnnnnnnnnnnnnYnYYYnYYYYnYYYYYYn",
    ),
    (
        "1.2.3-alpha - 1.2.4-beta",
        Some(">=1.2.3-alpha <=1.2.4-beta"),
        "nnnnnnnnnnYYYYYnnnnnnnnnnnnnnYn",
    ),
    (
        "v1.2.3 - v2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYn",
    ),
    ("=1.2.3 - 2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.2.3 -2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3+build - 2.0.0+build",
        Some(">=1.2.3 <=2.0.0"),
        "nnnnnnnnnnnnnYnYYYnYnnnnnnnnnYn",
    ),
    ("*", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    ("", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    ("x", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    ("X", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    (
        "1.x",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYn",
    ),
    (
        "1.2.x",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYn",
    ),
    (
        "1.*.*",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYn",
    ),
    ("1.x.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("x.1", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (">1", Some(">=2.0.0"), "nnnnnnnnnnnnnnnnnnnYYYYnYYYYYnn"),
    (">1.2", Some(">=1.3.0"), "nnnnnnnnnnnnnnnnYYnYYYYnYYYYYnn"),
    (">=1.x", Some(">=1.0.0"), "nnnnnnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    ("<1.2", Some("<1.2.0-0"), "YYYYYnnnYnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "<=1.2.x",
        Some("<1.3.0-0"),
        "YYYYYnnnYYnnnYnYnnnnnnnnnnnnnYn",
    ),
    (
        "=1.x",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYn",
    ),
    (">=*", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    (">=0.0.0", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    (
        "^1.x.3",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYn",
    ),
    (
        "~1.2.3",
        Some(">=1.2.3 <1.3.0-0"),
        "nnnnnnnnnnnnnYnYnnnnnnnnnnnnnYn",
    ),
    (
        "~1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYn",
    ),
    (
        "~1",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYn",
    ),
    (
        "~>1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYn",
    ),
    (
        "~ 1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYn",
    ),
    (
        "~1.2.3-beta.2",
        Some(">=1.2.3-beta.2 <1.3.0-0"),
        "nnnnnnnnnnnYYYnYnnnnnnnnnnnnnYn",
    ),
    (
        "^1.2.3",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYn",
    ),
    (
        "^0.2.3",
        Some(">=0.2.3 <0.3.0-0"),
        "nnnnYnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0.3",
        Some(">=0.0.3 <0.0.4-0"),
        "nYnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    ("^0.0", Some("<0.1.0-0"), "YYYnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("^0.x", Some("<1.0.0-0"), "YYYYYnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "^1.2",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYn",
    ),
    (
        "^ 1.2",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYn",
    ),
    (
        "^1.2.3-beta.2",
        Some(">=1.2.3-beta.2 <2.0.0-0"),
        "nnnnnnnnnnnYYYnYYYnnnnnnnnnnnYn",
    ),
    (
        "^0.0.3-beta",
        Some(">=0.0.3-beta <0.0.4-0"),
        "nYnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.0.0-rc.1",
        Some("1.0.0-rc.1"),
        "nnnnnnYnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<1.0.0-rc.1",
        Some("<1.0.0-rc.1"),
        "YYYYYYnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0-rc.1 <1.0.0",
        Some(">=1.0.0-rc.1 <1.0.0"),
        "nnnnnnYYnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0-beta.1",
        Some(">=1.0.0-beta.1"),
        "nnnnnYYYYYnnnYnYYYnYYYYnYYYYYYn",
    ),
    (
        "* || 1.0.0-beta.1",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn",
    ),
    (
        ">=0.0.0 || 1.0.0-beta.1",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn",
    ),
    ("1.2.3 ||", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    ("|| 1.2.3", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYn"),
    (
        "1.2.3+build",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYn",
    ),
    (
        ">=1.2.3+build <2",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYn",
    ),
    (
        ">= 1.2.3 < 2",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYn",
    ),
    ("v1.2.3", Some("1.2.3"), "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYn"),
    ("=1.2.3", Some("1.2.3"), "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYn"),
    ("==1.2.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (">=1, <2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3 || 2.x",
        Some("1.2.3||>=2.0.0 <3.0.0-0"),
        "nnnnnnnnnnnnnYnnnnnYYnnnnnnnnYn",
    ),
    (
        "1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        Some(
            ">=1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa <2.0.0-0",
        ),
        "nnnnnnnnnnYYYYnYYYnnnnnnnnnnnYn",
    ),
    (
        "<=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 1.2.3-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3+cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYn",
    ),
    (
        ">=1.2.3-beta.2 <1.2.4 || 2.0.0-0",
        Some(">=1.2.3-beta.2 <1.2.4||2.0.0-0"),
        "nnnnnnnnnnnYYYnnnnYnnnnnnnnnnYn",
    ),
    (
        "  >=1.2.3   <2  ",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYn",
    ),
];

#[test]
fn ranges_read_and_match_as_npm_reads_and_matches_them() {
    let mut wrong = Vec::new();
    for (range, valid, verdicts) in NPM {
        let parsed = Range::parse(range);
        let ours = parsed.as_ref().map(ToString::to_string);
        if ours.as_deref() != *valid {
            wrong.push(format!(
                "{range:?}: validRange npm {valid:?}, ours {ours:?}"
            ));
        }
        let row: String = VERSIONS
            .iter()
            .map(|v| if satisfies(v, range) { 'Y' } else { 'n' })
            .collect();
        if row != *verdicts {
            wrong.push(format!("{range:?}: satisfies npm {verdicts}, ours {row}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "disagrees with npm:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn satisfiable_form_drops_only_the_sets_nothing_satisfies() {
    let form = |r: &str| Range::parse(r).expect(r).satisfiable_form();
    assert_eq!(form(">=11.29.0 <=11.20.* || 11.15.0"), "11.15.0");
    assert_eq!(form(">=2 <1"), "<0.0.0-0");
    assert_eq!(form(">=1.0.0 <=1.0.0"), ">=1.0.0 <=1.0.0");
    assert_eq!(form(">1.2.3 <1.2.3 || ^2"), ">=2.0.0 <3.0.0-0");
    assert_eq!(form("<=11"), "<12.0.0-0");
}
