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
    "1.2.3beta",
    "=1.2.3",
    "6.3.2",
    "7.0.0-0",
    "7.0.0-rc.1",
    "7.0.0",
    "\u{FEFF}11.29.0",
    "\u{85}11.29.0",
];

/// `(range, semver.validRange, semver.satisfies per VERSIONS)`, from npm's
/// `semver` 7.8.6.
const NPM: &[(&str, Option<&str>, &str)] = &[
    (
        "<=11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYnnYYn",
    ),
    (
        "<=11.2",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYnnnYnnYnn",
    ),
    (
        "<=11.x",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYnnYYn",
    ),
    (
        "<=11.2.x",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYnnnYnnYnn",
    ),
    (
        "<=11.*",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYnnYYn",
    ),
    (
        "<= 11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYnnYYn",
    ),
    (
        "<=v11.2",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYnnnYnnYnn",
    ),
    (
        "<==11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYnnYYn",
    ),
    (
        ">=2 <1",
        Some(">=2.0.0 <1.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=2 <1 || 0.5.0",
        Some(">=2.0.0 <1.0.0-0||0.5.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <1.2.0",
        Some(">1.2.3 <1.2.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0 <=1.0.0",
        Some(">=1.0.0 <=1.0.0"),
        "nnnnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <=11.20.* || 11.15.0",
        Some(">=11.29.0 <11.21.0-0||11.15.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnYnnnnnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <=11.20 || 11.15.0",
        Some(">=11.29.0 <11.21.0-0||11.15.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnYnnnnnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <11.21.0-0",
        Some(">=11.29.0 <11.21.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <1.2.3",
        Some(">1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.2.3 <1.2.3",
        Some(">=1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<0.0.0-0 || 1.0.0",
        Some("1.0.0"),
        "nnnnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <=1.2.3-0",
        Some(">1.2.3 <=1.2.3-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">*",
        Some("<0.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<*",
        Some("<0.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<x || 1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    ("<=11.029", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.02.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("<=01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("01.2.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.2.3-01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("~01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("^1.02", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3-0a",
        Some("1.2.3-0a"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "9007199254740991.0.0",
        Some("9007199254740991.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "9007199254740992.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^9007199254740990",
        Some(">=9007199254740990.0.0 <9007199254740991.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=9007199254740990",
        Some("<9007199254740991.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        "~1.9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.9007199254740991.x",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "99999999999999999999999",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=99999999999999999999",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0.18446744073709551615",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 18446744073709551615.x",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 2.3.4",
        Some(">=1.2.3 <=2.3.4"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYnnnnnnnnn",
    ),
    (
        "1.2 - 2.3",
        Some(">=1.2.0 <2.4.0-0"),
        "nnnnnnnnnYnnnYnYYYnYYnnnnnnnnYnnnnnnnnn",
    ),
    (
        "1.2.3 - 2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYnnnnnnnnn",
    ),
    (
        "1 - 2",
        Some(">=1.0.0 <3.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnYYnnnnnnnnYnnnnnnnnn",
    ),
    (
        "* - 2",
        Some("<3.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYnnnnnnnnYnnnnnnnnn",
    ),
    (
        "1.2.3 - *",
        Some(">=1.2.3"),
        "nnnnnnnnnnnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        "1.2.3-alpha - 1.2.4-beta",
        Some(">=1.2.3-alpha <=1.2.4-beta"),
        "nnnnnnnnnnYYYYYnnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "v1.2.3 - v2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYnnnnnnnnn",
    ),
    (
        "=1.2.3 - 2",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    ("1.2.3 -2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3+build - 2.0.0+build",
        Some(">=1.2.3 <=2.0.0"),
        "nnnnnnnnnnnnnYnYYYnYnnnnnnnnnYnnnnnnnnn",
    ),
    ("*", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn"),
    ("", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn"),
    ("x", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn"),
    ("X", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn"),
    (
        "1.x",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "1.2.x",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "1.*.*",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    ("1.x.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("x.1", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        ">1",
        Some(">=2.0.0"),
        "nnnnnnnnnnnnnnnnnnnYYYYnYYYYYnnnnYnnYYn",
    ),
    (
        ">1.2",
        Some(">=1.3.0"),
        "nnnnnnnnnnnnnnnnYYnYYYYnYYYYYnnnnYnnYYn",
    ),
    (
        ">=1.x",
        Some(">=1.0.0"),
        "nnnnnnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        "<1.2",
        Some("<1.2.0-0"),
        "YYYYYnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=1.2.x",
        Some("<1.3.0-0"),
        "YYYYYnnnYYnnnYnYnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "=1.x",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (">=*", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn"),
    (
        ">=0.0.0",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        "^1.x.3",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "~1.2.3",
        Some(">=1.2.3 <1.3.0-0"),
        "nnnnnnnnnnnnnYnYnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "~1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "~1",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "~>1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "~ 1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "~1.2.3-beta.2",
        Some(">=1.2.3-beta.2 <1.3.0-0"),
        "nnnnnnnnnnnYYYnYnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "^1.2.3",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "^0.2.3",
        Some(">=0.2.3 <0.3.0-0"),
        "nnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0.3",
        Some(">=0.0.3 <0.0.4-0"),
        "nYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0",
        Some("<0.1.0-0"),
        "YYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.x",
        Some("<1.0.0-0"),
        "YYYYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^1.2",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "^ 1.2",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "^1.2.3-beta.2",
        Some(">=1.2.3-beta.2 <2.0.0-0"),
        "nnnnnnnnnnnYYYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "^0.0.3-beta",
        Some(">=0.0.3-beta <0.0.4-0"),
        "nYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.0.0-rc.1",
        Some("1.0.0-rc.1"),
        "nnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<1.0.0-rc.1",
        Some("<1.0.0-rc.1"),
        "YYYYYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0-rc.1 <1.0.0",
        Some(">=1.0.0-rc.1 <1.0.0"),
        "nnnnnnYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0-beta.1",
        Some(">=1.0.0-beta.1"),
        "nnnnnYYYYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        "* || 1.0.0-beta.1",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        ">=0.0.0 || 1.0.0-beta.1",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        "1.2.3 ||",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        "|| 1.2.3",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYnnYYn",
    ),
    (
        "1.2.3+build",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        ">=1.2.3+build <2",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        ">= 1.2.3 < 2",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "v1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "=1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    ("==1.2.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (">=1, <2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3 || 2.x",
        Some("1.2.3||>=2.0.0 <3.0.0-0"),
        "nnnnnnnnnnnnnYnnnnnYYnnnnnnnnYnnnnnnnnn",
    ),
    (
        "1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        Some(
            ">=1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa <2.0.0-0",
        ),
        "nnnnnnnnnnYYYYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "<=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 1.2.3-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3+cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{FEFF}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        ">=11.20.0\u{85}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{1E}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{1F}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{A0}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        ">=11.20.0\u{2028}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        ">=11.20.0\u{3000}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        ">=11.20.0\u{180E}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "\u{FEFF}^11.20.0\u{FEFF}",
        Some(">=11.20.0 <12.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        "\u{85}^11.20.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^11.20.0\u{85}",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">= \u{FEFF}11.20.0",
        Some(">=11.20.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYYnnnnnnnnYn",
    ),
    (
        "1.2.3\u{FEFF}-\u{FEFF}2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYnnnnnnnnn",
    ),
    (
        "<=6 <=7.0.0-0",
        Some("<7.0.0-0 <=7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYnnnYnnnnn",
    ),
    (
        "<=7.0.0-0 <=6",
        Some("<=7.0.0-0 <7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYnnnYnnnnn",
    ),
    (
        "<=6 <7.0.0-0",
        Some("<7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYnnnYnnnnn",
    ),
    (
        "<7.0.0-0 <=6",
        Some("<7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYnnnYnnnnn",
    ),
    (
        ">=7.0.0-0 <=6",
        Some(">=7.0.0-0 <7.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=6.x <7.0.0-0",
        Some("<7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYnnnYnnnnn",
    ),
    (
        "<=1.2 <1.3.0-0",
        Some("<1.3.0-0"),
        "YYYYYnnnYYnnnYnYnnnnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        ">=1.2.3 >1.2.3",
        Some(">=1.2.3 >1.2.3"),
        "nnnnnnnnnnnnnnnYYYnYYYYnYYYYYnnnnYnnYYn",
    ),
    (
        ">=1.2.3 <=1.2.3 <1.2.3",
        Some(">=1.2.3 <=1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    ("1.2.3beta", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3-beta.02",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.2.3 foo",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    ("foo", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("^00.1.2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("~001.2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("<=0011", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        ">=1.2.3 <2 bar || 1.0.0-rc.1",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.02 - 1.03",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    ("latest", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.2.3 >", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        ">=1.2.3-beta.2 <1.2.4 || 2.0.0-0",
        Some(">=1.2.3-beta.2 <1.2.4||2.0.0-0"),
        "nnnnnnnnnnnYYYnnnnYnnnnnnnnnnYnnnnnnnnn",
    ),
    (
        "  >=1.2.3   <2  ",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYnnnnnnnnn",
    ),
];

/// The same table with `{ loose: true }`, the mode npm and pnpm resolve with.
const NPM_LOOSE: &[(&str, Option<&str>, &str)] = &[
    (
        "<=11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYYnYYnnYYn",
    ),
    (
        "<=11.2",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYYnYYnnYnn",
    ),
    (
        "<=11.x",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYYnYYnnYYn",
    ),
    (
        "<=11.2.x",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYYnYYnnYnn",
    ),
    (
        "<=11.*",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYYnYYnnYYn",
    ),
    (
        "<= 11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYYnYYnnYYn",
    ),
    (
        "<=v11.2",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYYnYYnnYnn",
    ),
    (
        "<==11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYYnYYnnYYn",
    ),
    (
        ">=2 <1",
        Some(">=2.0.0 <1.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=2 <1 || 0.5.0",
        Some(">=2.0.0 <1.0.0-0||0.5.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <1.2.0",
        Some(">1.2.3 <1.2.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0 <=1.0.0",
        Some(">=1.0.0 <=1.0.0"),
        "nnnnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <=11.20.* || 11.15.0",
        Some(">=11.29.0 <11.21.0-0||11.15.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnYnnnnnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <=11.20 || 11.15.0",
        Some(">=11.29.0 <11.21.0-0||11.15.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnYnnnnnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <11.21.0-0",
        Some(">=11.29.0 <11.21.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <1.2.3",
        Some(">1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.2.3 <1.2.3",
        Some(">=1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<0.0.0-0 || 1.0.0",
        Some("1.0.0"),
        "nnnnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <=1.2.3-0",
        Some(">1.2.3 <=1.2.3-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">*",
        Some("<0.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<*",
        Some("<0.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<x || 1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "<=11.029",
        Some("<11.30.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYYnYYnnYYn",
    ),
    (
        "1.02.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "<=01",
        Some("<2.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "01.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3-01",
        Some("1.2.3-1"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "~01",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "^1.02",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3-0a",
        Some("1.2.3-0a"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "9007199254740991.0.0",
        Some("9007199254740991.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "9007199254740992.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^9007199254740990",
        Some(">=9007199254740990.0.0 <9007199254740991.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=9007199254740990",
        Some("<9007199254740991.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        "~1.9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.9007199254740991.x",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "99999999999999999999999",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=99999999999999999999",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0.18446744073709551615",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 18446744073709551615.x",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 2.3.4",
        Some(">=1.2.3 <=2.3.4"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2 - 2.3",
        Some(">=1.2.0 <2.4.0-0"),
        "nnnnnnnnnYnnnYnYYYnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3 - 2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1 - 2",
        Some(">=1.0.0 <3.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "* - 2",
        Some("<3.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3 - *",
        Some(">=1.2.3"),
        "nnnnnnnnnnnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        "1.2.3-alpha - 1.2.4-beta",
        Some(">=1.2.3-alpha <=1.2.4-beta"),
        "nnnnnnnnnnYYYYYnnnnnnnnnnnnnnYYYYnnnnnn",
    ),
    (
        "v1.2.3 - v2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "=1.2.3 - 2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3 -2",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3+build - 2.0.0+build",
        Some(">=1.2.3 <=2.0.0"),
        "nnnnnnnnnnnnnYnYYYnYnnnnnnnnnYYnYnnnnnn",
    ),
    ("*", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn"),
    ("", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn"),
    ("x", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn"),
    ("X", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn"),
    (
        "1.x",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.x",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.*.*",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    ("1.x.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("x.1", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        ">1",
        Some(">=2.0.0"),
        "nnnnnnnnnnnnnnnnnnnYYYYnYYYYYnnnnYnnYYn",
    ),
    (
        ">1.2",
        Some(">=1.3.0"),
        "nnnnnnnnnnnnnnnnYYnYYYYnYYYYYnnnnYnnYYn",
    ),
    (
        ">=1.x",
        Some(">=1.0.0"),
        "nnnnnnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        "<1.2",
        Some("<1.2.0-0"),
        "YYYYYnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=1.2.x",
        Some("<1.3.0-0"),
        "YYYYYnnnYYnnnYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "=1.x",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (">=*", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn"),
    (
        ">=0.0.0",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        "^1.x.3",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "~1.2.3",
        Some(">=1.2.3 <1.3.0-0"),
        "nnnnnnnnnnnnnYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "~1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "~1",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "~>1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "~ 1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "~1.2.3-beta.2",
        Some(">=1.2.3-beta.2 <1.3.0-0"),
        "nnnnnnnnnnnYYYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "^1.2.3",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "^0.2.3",
        Some(">=0.2.3 <0.3.0-0"),
        "nnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0.3",
        Some(">=0.0.3 <0.0.4-0"),
        "nYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0",
        Some("<0.1.0-0"),
        "YYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.x",
        Some("<1.0.0-0"),
        "YYYYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^1.2",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "^ 1.2",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "^1.2.3-beta.2",
        Some(">=1.2.3-beta.2 <2.0.0-0"),
        "nnnnnnnnnnnYYYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "^0.0.3-beta",
        Some(">=0.0.3-beta <0.0.4-0"),
        "nYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.0.0-rc.1",
        Some("1.0.0-rc.1"),
        "nnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<1.0.0-rc.1",
        Some("<1.0.0-rc.1"),
        "YYYYYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0-rc.1 <1.0.0",
        Some(">=1.0.0-rc.1 <1.0.0"),
        "nnnnnnYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0-beta.1",
        Some(">=1.0.0-beta.1"),
        "nnnnnYYYYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        "* || 1.0.0-beta.1",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        ">=0.0.0 || 1.0.0-beta.1",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        "1.2.3 ||",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        "|| 1.2.3",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    (
        "1.2.3+build",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        ">=1.2.3+build <2",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        ">= 1.2.3 < 2",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "v1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "=1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "==1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        ">=1, <2",
        Some("<2.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3 || 2.x",
        Some("1.2.3||>=2.0.0 <3.0.0-0"),
        "nnnnnnnnnnnnnYnnnnnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        Some(
            ">=1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa <2.0.0-0",
        ),
        "nnnnnnnnnnYYYYnYYYnnnnnnnnnnnYYYYnnnnnn",
    ),
    (
        "<=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 1.2.3-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.2.3+cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        ">=11.20.0\u{FEFF}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        ">=11.20.0\u{85}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{1E}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{1F}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{A0}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        ">=11.20.0\u{2028}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        ">=11.20.0\u{3000}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        ">=11.20.0\u{180E}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "\u{FEFF}^11.20.0\u{FEFF}",
        Some(">=11.20.0 <12.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnnnnnYn",
    ),
    (
        "\u{85}^11.20.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^11.20.0\u{85}",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">= \u{FEFF}11.20.0",
        Some(">=11.20.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYYnnnnnnnnYn",
    ),
    (
        "1.2.3\u{FEFF}-\u{FEFF}2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYYnYnnnnnn",
    ),
    (
        "<=6 <=7.0.0-0",
        Some("<7.0.0-0 <=7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYYnYYnnnnn",
    ),
    (
        "<=7.0.0-0 <=6",
        Some("<=7.0.0-0 <7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYYnYYnnnnn",
    ),
    (
        "<=6 <7.0.0-0",
        Some("<7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYYnYYnnnnn",
    ),
    (
        "<7.0.0-0 <=6",
        Some("<7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYYnYYnnnnn",
    ),
    (
        ">=7.0.0-0 <=6",
        Some(">=7.0.0-0 <7.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=6.x <7.0.0-0",
        Some("<7.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYnnnnnnnYYnYYnnnnn",
    ),
    (
        "<=1.2 <1.3.0-0",
        Some("<1.3.0-0"),
        "YYYYYnnnYYnnnYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        ">=1.2.3 >1.2.3",
        Some(">=1.2.3 >1.2.3"),
        "nnnnnnnnnnnnnnnYYYnYYYYnYYYYYnnnnYnnYYn",
    ),
    (
        ">=1.2.3 <=1.2.3 <1.2.3",
        Some(">=1.2.3 <=1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3beta",
        Some("1.2.3-beta"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnYnnnnnnn",
    ),
    (
        "1.2.3-beta.02",
        Some("1.2.3-beta.2"),
        "nnnnnnnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.2.3 foo",
        Some(">=1.2.3"),
        "nnnnnnnnnnnnnYnYYYnYYYYnYYYYYYYnYYnnYYn",
    ),
    ("foo", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "^00.1.2",
        Some(">=0.1.2 <1.0.0-0"),
        "nnnYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "~001.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "<=0011",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYYnYYnnYYn",
    ),
    (
        ">=1.2.3 <2 bar || 1.0.0-rc.1",
        Some(">=1.2.3 <2.0.0-0||1.0.0-rc.1"),
        "nnnnnnYnnnnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "1.02 - 1.03",
        Some(">=1.2.0 <1.4.0-0"),
        "nnnnnnnnnYnnnYnYYnnnnnnnnnnnnYYnYnnnnnn",
    ),
    ("latest", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3 >",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        ">=1.2.3-beta.2 <1.2.4 || 2.0.0-0",
        Some(">=1.2.3-beta.2 <1.2.4||2.0.0-0"),
        "nnnnnnnnnnnYYYnnnnYnnnnnnnnnnYYnYnnnnnn",
    ),
    (
        "  >=1.2.3   <2  ",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYYnYnnnnnn",
    ),
];

/// `(a, b, semver.intersects(a, b))`.
const NPM_INTERSECTS: &[(&str, &str, bool)] = &[
    ("^1.2.3", "^1.5.0", true),
    ("^1.2.3", "^2.0.0", false),
    (">=2 <1", "*", false),
    ("*", "*", true),
    ("1.2.3", ">=1.2.3", true),
    ("1.2.3", ">1.2.3", false),
    ("<1.2.3", ">=1.2.3", false),
    ("<=1.2.3", ">=1.2.3", true),
    ("<0.0.0-0", "*", true),
    ("<0.0.0", "*", true),
    ("<=6", "<7.0.0-0", true),
    (">=3.0.0-0 <3.0.0", "^3.0.0-beta.1", true),
    (">=3.0.0-0 <3.0.0", "^3.0.0", false),
    (">=3.0.0-0 <3.0.0", ">=2", true),
    ("<2.0.0", "^1", true),
    ("<2.0.0", ">=2", false),
    (">=11.0.0-0", "<=10", false),
    (">=11.0.0-0", "^10 || ^11", true),
    ("1.x || >=2.5.0", "2.3.x", false),
    ("1.0.0-rc.1", ">=1.0.0-0 <1.0.0", false),
    (">1.0.0 <1.0.1-0", "*", true),
    ("~1.2.3", "1.2.x", true),
];

/// `(range, semver.minVersion(range))`.
const NPM_MIN_VERSION: &[(&str, Option<&str>)] = &[
    (">=3.0.0-0 <3.0.0 >=3.0.0-beta.1", Some("3.0.0-beta.1")),
    ("^1.2.3", Some("1.2.3")),
    (">1.2.3", Some("1.2.4")),
    (">1.2.3-rc.1", Some("1.2.3-rc.1.0")),
    ("*", Some("0.0.0")),
    ("<1", Some("0.0.0")),
    (">=2 <1", None),
    ("1.x || >=0.5.0 <0.6", Some("0.5.0")),
    (">=3.0.0-0 <3.0.0 >=2", Some("3.0.0-0")),
    ("<=6 <=7.0.0-0", Some("0.0.0")),
    ("~1.2.3-beta.2 || 1.0.0", Some("1.0.0")),
    (">=1.2.3 <1.2.3", None),
];

fn disagreements(
    table: &[(&str, Option<&str>, &str)],
    parse: fn(&str) -> Option<Range>,
) -> Vec<String> {
    let mut wrong = Vec::new();
    for (range, valid, verdicts) in table {
        let parsed = parse(range);
        let ours = parsed.as_ref().map(ToString::to_string);
        if ours.as_deref() != *valid {
            wrong.push(format!(
                "{range:?}: validRange npm {valid:?}, ours {ours:?}"
            ));
        }
        let row: String = VERSIONS
            .iter()
            .map(|v| match &parsed {
                Some(r) if r.satisfies_str(v) => 'Y',
                _ => 'n',
            })
            .collect();
        if row != *verdicts {
            wrong.push(format!("{range:?}: satisfies npm {verdicts}, ours {row}"));
        }
    }
    wrong
}

#[test]
fn ranges_read_and_match_as_npm_reads_and_matches_them() {
    let wrong = disagreements(NPM, Range::parse);
    assert!(
        wrong.is_empty(),
        "disagrees with npm:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn loose_ranges_read_and_match_as_npm_loose_does() {
    let wrong = disagreements(NPM_LOOSE, Range::parse_loose);
    assert!(
        wrong.is_empty(),
        "disagrees with npm:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn intersects_and_min_version_match_npm() {
    let mut wrong = Vec::new();
    for (a, b, want) in NPM_INTERSECTS {
        let got = Range::parse(a)
            .unwrap()
            .intersects(&Range::parse(b).unwrap());
        if got != *want {
            wrong.push(format!("intersects({a:?}, {b:?}): npm {want}, ours {got}"));
        }
    }
    for (range, want) in NPM_MIN_VERSION {
        let got = Range::parse(range)
            .unwrap()
            .min_version()
            .map(|v| v.to_string());
        if got.as_deref() != *want {
            wrong.push(format!("minVersion({range:?}): npm {want:?}, ours {got:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "disagrees with npm:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn intersect_joins_every_set_of_both() {
    let window = Range::parse(">=3.0.0-0 <3.0.0").unwrap();
    let joined = Range::parse("^3.0.0-beta.1 || >=2")
        .unwrap()
        .intersect(&window);
    assert_eq!(
        joined.to_string(),
        ">=3.0.0-beta.1 <4.0.0-0 >=3.0.0-0 <3.0.0||>=2.0.0 >=3.0.0-0 <3.0.0"
    );
    assert_eq!(
        joined.min_version().map(|v| v.to_string()).as_deref(),
        Some("3.0.0-0")
    );
}

#[test]
fn parse_range_refuses_only_what_loose_npm_refuses_and_the_empty_spec() {
    assert!(parse_range("").is_err() && parse_range(" \u{FEFF}").is_err());
    assert!(parse_range("latest").is_err());
    assert_eq!(parse_range(">=1, <2").unwrap().to_string(), "<2.0.0-0");
}

#[cfg(feature = "node-semver")]
#[test]
fn a_node_semver_version_matches_as_its_npm_reading_does() {
    for (range, _, _) in NPM_LOOSE {
        let Some(parsed) = Range::parse_loose(range) else {
            continue;
        };
        for v in VERSIONS {
            let Ok(node) = node_semver::Version::parse(v) else {
                continue;
            };
            let npm = Version::parse_loose(&node.to_string()).unwrap();
            assert_eq!(
                parsed.satisfies_node(&node),
                parsed.satisfies(&npm),
                "{range:?} {v}"
            );
        }
    }
}
