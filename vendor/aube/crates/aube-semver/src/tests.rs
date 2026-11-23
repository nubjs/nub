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
    "\u{FEFF}11.29.0",
    "\u{85}11.29.0",
];

/// `(range, semver.validRange, semver.satisfies per VERSIONS)`, from npm's
/// `semver` 7.8.6.
const NPM: &[(&str, Option<&str>, &str)] = &[
    (
        "<=11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYn",
    ),
    (
        "<=11.2",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYnnnnn",
    ),
    (
        "<=11.x",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYn",
    ),
    (
        "<=11.2.x",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYnnnnn",
    ),
    (
        "<=11.*",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYn",
    ),
    (
        "<= 11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYn",
    ),
    (
        "<=v11.2",
        Some("<11.3.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnnnnnnYnnnnn",
    ),
    (
        "<==11",
        Some("<12.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYnYnnnYn",
    ),
    (
        ">=2 <1",
        Some(">=2.0.0 <1.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=2 <1 || 0.5.0",
        Some(">=2.0.0 <1.0.0-0||0.5.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <1.2.0",
        Some(">1.2.3 <1.2.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0 <=1.0.0",
        Some(">=1.0.0 <=1.0.0"),
        "nnnnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <=11.20.* || 11.15.0",
        Some(">=11.29.0 <11.21.0-0||11.15.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnYnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <=11.20 || 11.15.0",
        Some(">=11.29.0 <11.21.0-0||11.15.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnYnnnnnnnnnn",
    ),
    (
        ">=11.29.0 <11.21.0-0",
        Some(">=11.29.0 <11.21.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <1.2.3",
        Some(">1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.2.3 <1.2.3",
        Some(">=1.2.3 <1.2.3"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<0.0.0-0 || 1.0.0",
        Some("1.0.0"),
        "nnnnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.2.3 <=1.2.3-0",
        Some(">1.2.3 <=1.2.3-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">*",
        Some("<0.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<*",
        Some("<0.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<x || 1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnn",
    ),
    ("<=11.029", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.02.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("<=01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("01.2.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.2.3-01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("~01", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("^1.02", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3-0a",
        Some("1.2.3-0a"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "9007199254740991.0.0",
        Some("9007199254740991.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "9007199254740992.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^9007199254740990",
        Some(">=9007199254740990.0.0 <9007199254740991.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=9007199254740990",
        Some("<9007199254740991.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn",
    ),
    (
        "~1.9007199254740991",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">1.9007199254740991.x",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "99999999999999999999999",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=99999999999999999999",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0.18446744073709551615",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 18446744073709551615.x",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 2.3.4",
        Some(">=1.2.3 <=2.3.4"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYnnnnn",
    ),
    (
        "1.2 - 2.3",
        Some(">=1.2.0 <2.4.0-0"),
        "nnnnnnnnnYnnnYnYYYnYYnnnnnnnnYnnnnn",
    ),
    (
        "1.2.3 - 2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYnnnnn",
    ),
    (
        "1 - 2",
        Some(">=1.0.0 <3.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnYYnnnnnnnnYnnnnn",
    ),
    (
        "* - 2",
        Some("<3.0.0-0"),
        "YYYYYnnnYYnnnYnYYYnYYnnnnnnnnYnnnnn",
    ),
    (
        "1.2.3 - *",
        Some(">=1.2.3"),
        "nnnnnnnnnnnnnYnYYYnYYYYnYYYYYYnnnYn",
    ),
    (
        "1.2.3-alpha - 1.2.4-beta",
        Some(">=1.2.3-alpha <=1.2.4-beta"),
        "nnnnnnnnnnYYYYYnnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "v1.2.3 - v2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYnnnnn",
    ),
    ("=1.2.3 - 2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.2.3 -2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3+build - 2.0.0+build",
        Some(">=1.2.3 <=2.0.0"),
        "nnnnnnnnnnnnnYnYYYnYnnnnnnnnnYnnnnn",
    ),
    ("*", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn"),
    ("", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn"),
    ("x", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn"),
    ("X", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn"),
    (
        "1.x",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "1.2.x",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "1.*.*",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    ("1.x.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("x.1", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (">1", Some(">=2.0.0"), "nnnnnnnnnnnnnnnnnnnYYYYnYYYYYnnnnYn"),
    (
        ">1.2",
        Some(">=1.3.0"),
        "nnnnnnnnnnnnnnnnYYnYYYYnYYYYYnnnnYn",
    ),
    (
        ">=1.x",
        Some(">=1.0.0"),
        "nnnnnnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn",
    ),
    (
        "<1.2",
        Some("<1.2.0-0"),
        "YYYYYnnnYnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<=1.2.x",
        Some("<1.3.0-0"),
        "YYYYYnnnYYnnnYnYnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "=1.x",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (">=*", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn"),
    (">=0.0.0", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn"),
    (
        "^1.x.3",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "~1.2.3",
        Some(">=1.2.3 <1.3.0-0"),
        "nnnnnnnnnnnnnYnYnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "~1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "~1",
        Some(">=1.0.0 <2.0.0-0"),
        "nnnnnnnnYYnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "~>1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "~ 1.2",
        Some(">=1.2.0 <1.3.0-0"),
        "nnnnnnnnnYnnnYnYnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "~1.2.3-beta.2",
        Some(">=1.2.3-beta.2 <1.3.0-0"),
        "nnnnnnnnnnnYYYnYnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "^1.2.3",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "^0.2.3",
        Some(">=0.2.3 <0.3.0-0"),
        "nnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0.3",
        Some(">=0.0.3 <0.0.4-0"),
        "nYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.0",
        Some("<0.1.0-0"),
        "YYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^0.x",
        Some("<1.0.0-0"),
        "YYYYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^1.2",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "^ 1.2",
        Some(">=1.2.0 <2.0.0-0"),
        "nnnnnnnnnYnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "^1.2.3-beta.2",
        Some(">=1.2.3-beta.2 <2.0.0-0"),
        "nnnnnnnnnnnYYYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "^0.0.3-beta",
        Some(">=0.0.3-beta <0.0.4-0"),
        "nYnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.0.0-rc.1",
        Some("1.0.0-rc.1"),
        "nnnnnnYnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "<1.0.0-rc.1",
        Some("<1.0.0-rc.1"),
        "YYYYYYnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0-rc.1 <1.0.0",
        Some(">=1.0.0-rc.1 <1.0.0"),
        "nnnnnnYYnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=1.0.0-beta.1",
        Some(">=1.0.0-beta.1"),
        "nnnnnYYYYYnnnYnYYYnYYYYnYYYYYYnnnYn",
    ),
    (
        "* || 1.0.0-beta.1",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn",
    ),
    (
        ">=0.0.0 || 1.0.0-beta.1",
        Some("*"),
        "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn",
    ),
    ("1.2.3 ||", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn"),
    ("|| 1.2.3", Some("*"), "YYYYYnnnYYnnnYnYYYnYYYYnYYYYYYnnnYn"),
    (
        "1.2.3+build",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnn",
    ),
    (
        ">=1.2.3+build <2",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        ">= 1.2.3 < 2",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "v1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnn",
    ),
    (
        "=1.2.3",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnn",
    ),
    ("==1.2.3", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (">=1, <2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        "1.2.3 || 2.x",
        Some("1.2.3||>=2.0.0 <3.0.0-0"),
        "nnnnnnnnnnnnnYnnnnnYYnnnnnnnnYnnnnn",
    ),
    (
        "1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        Some(
            ">=1.2.3-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa <2.0.0-0",
        ),
        "nnnnnnnnnnYYYYnYYYnnnnnnnnnnnYnnnnn",
    ),
    (
        "<=1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3 - 1.2.3-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "1.2.3+cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        Some("1.2.3"),
        "nnnnnnnnnnnnnYnnnnnnnnnnnnnnnYnnnnn",
    ),
    (
        ">=11.20.0\u{FEFF}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnYn",
    ),
    (
        ">=11.20.0\u{85}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{1E}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{1F}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">=11.20.0\u{A0}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnYn",
    ),
    (
        ">=11.20.0\u{2028}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnYn",
    ),
    (
        ">=11.20.0\u{3000}<12.0.0",
        Some(">=11.20.0 <12.0.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnYn",
    ),
    (
        ">=11.20.0\u{180E}<12.0.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "\u{FEFF}^11.20.0\u{FEFF}",
        Some(">=11.20.0 <12.0.0-0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYnnnnnYn",
    ),
    (
        "\u{85}^11.20.0",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        "^11.20.0\u{85}",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    (
        ">= \u{FEFF}11.20.0",
        Some(">=11.20.0"),
        "nnnnnnnnnnnnnnnnnnnnnnnnnYYYYnnnnYn",
    ),
    (
        "1.2.3\u{FEFF}-\u{FEFF}2",
        Some(">=1.2.3 <3.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnYYnnnnnnnnYnnnnn",
    ),
    ("1.2.3beta", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.2.3-beta.02", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (">=1.2.3 foo", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("foo", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("^00.1.2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("~001.2", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("<=0011", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        ">=1.2.3 <2 bar || 1.0.0-rc.1",
        None,
        "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    ),
    ("1.02 - 1.03", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("latest", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    ("1.2.3 >", None, "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"),
    (
        ">=1.2.3-beta.2 <1.2.4 || 2.0.0-0",
        Some(">=1.2.3-beta.2 <1.2.4||2.0.0-0"),
        "nnnnnnnnnnnYYYnnnnYnnnnnnnnnnYnnnnn",
    ),
    (
        "  >=1.2.3   <2  ",
        Some(">=1.2.3 <2.0.0-0"),
        "nnnnnnnnnnnnnYnYYYnnnnnnnnnnnYnnnnn",
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
