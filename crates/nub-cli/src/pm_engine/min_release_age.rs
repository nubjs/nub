//! The per-invocation `minimumReleaseAge` CLI surface: [`AgeGateFlags`] and the
//! [`ReleaseAge`] value grammar behind `--minimum-release-age`.
//!
//! pnpm 12 takes neither flag on its own command line, only the settings they
//! name, spelled `--config.<key>=`. So both reach the engine in that spelling:
//! from `nubx`, and from an install command line in a nub project.
//!
//! This module also held the loose-mode auto-persist that co-wrote an immature
//! fallback pick into `minimumReleaseAgeExclude` ([#262]). Its only callers were
//! the nub-side `add`/`update`/`dedupe` runners, which the engine now claims at
//! the CLI front door (`super::verb_routing`), so it had become unreachable and
//! was removed rather than left as dead code. Re-landing it means calling it
//! from the live install path in `super::pnpm_engine`; git history has the
//! implementation, including the pnpm-workspace-yaml vs `.npmrc` write-target
//! rules.
//!
//! [#262]: https://github.com/nubjs/nub/issues/262

use std::ffi::OsString;

/// Engine minutes for a `--minimum-release-age` value, accepting BOTH surfaces
/// this setting already has:
///
/// - `<integer><unit>` (`s|m|h|d|w`) — the exact grammar `nub.jsonc`'s
///   `install.minimumReleaseAge` accepts, parsed by the same
///   [`crate::project_config::parse_duration`] so a value that works in the file
///   works on the flag and the two cannot drift.
/// - a bare integer — MINUTES, mirroring pnpm's CLI, whose type map declares
///   `minimum-release-age` as a Number.
///
/// The file deliberately REJECTS the bare form (npm counts days, pnpm counts
/// minutes, so an unqualified number in a config file is a trap). On the CLI the
/// ambiguity is already settled by the tool nub mirrors, so accepting it is
/// compatibility rather than a hazard.
///
/// Sub-minute values round UP, never down: the engine setting is whole minutes,
/// and `30s` collapsing to `0` would SILENTLY DISABLE the gate instead of
/// tightening it. A literal `0` still means zero — that is the documented "turn
/// it off" value, not a rounding artifact.
fn parse_release_age_minutes(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    if let Ok(minutes) = s.parse::<u64>() {
        return Ok(minutes);
    }
    // Take the config parser's VERDICT but not its wording: its `Display`
    // attributes the failure to `nub.jsonc`, which is a lie on a command line.
    // The grammar stays in one place; only the sentence differs.
    let dur = crate::project_config::parse_duration(s, "--minimum-release-age").map_err(|_| {
        format!(
            "expected minutes (e.g. `1440`) or a duration with a unit \
             s|m|h|d|w (e.g. `3d`), got `{raw}`"
        )
    })?;
    Ok(dur.as_secs().div_ceil(60))
}

/// A parsed `--minimum-release-age` value, in whole minutes.
///
/// The parser takes no value-parser closures — a Rust callback cannot travel in
/// a portable spec — so [`parse_release_age_minutes`] is reached through this
/// newtype's `FromStr`. Its `Err` string is what a usage error prints, which is
/// what makes that wording user-facing.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseAge(pub u64);

impl std::str::FromStr for ReleaseAge {
    type Err = String;
    fn from_str(raw: &str) -> Result<Self, String> {
        parse_release_age_minutes(raw).map(ReleaseAge)
    }
}

// The per-invocation age-gate flags. `nubx` is the one nub-parsed surface that
// flattens them: the install family's verbs are parsed by the engine at the CLI
// front door, which has no spelling for either flag, so on those command lines
// [`engine_argv`] rewrites them instead.
//
// Each flag mirrors BOTH of the surfaces this setting already has: the pnpm
// spelling (its CLI type map declares `minimum-release-age` and
// `minimum-release-age-exclude`) and the value grammar of the matching
// `nub.jsonc` field (`install.minimumReleaseAge`,
// `install.minimumReleaseAgeExclude`).
//
// NO STRICTNESS FLAG, deliberately. Under nub the gate defaults to strict, and
// the way to opt out is `--minimum-release-age=0` — turn the window OFF, rather
// than keep a window and quietly install versions that fail it. pnpm's
// `minimumReleaseAgeStrict` defaults to false, which leaves its 24h window
// advisory unless you separately opt in; nub does not reproduce that. So there
// is no `--[no-]minimum-release-age-strict`, and no
// `install.minimumReleaseAgeStrict` in `nub.jsonc` either — one axis (how
// long), not two.
//
// (Plain `//`, not rustdoc: a `///` comment on a usage `Args` struct becomes the
// augmented command's `--help` about-text and clobbers the verb's own. This one
// leaked onto `nub add --help`.)
#[derive(Debug, Default, Clone, usage_rs::Args)]
pub struct AgeGateFlags {
    /// How old a version must be before it can be installed: a duration with a
    /// unit (`30s`, `5m`, `2h`, `3d`, `1w`), or a bare number meaning minutes.
    /// `0` turns the age gate off for this run. Overrides `minimumReleaseAge`
    /// from config.
    #[usage(long, value_name = "DURATION")]
    pub minimum_release_age: Option<ReleaseAge>,

    /// Exempt packages from the age gate for this run (repeatable). Entry
    /// grammar matches the config field: a bare name, a `*` name glob, or a
    /// name with a version range. REPLACES any configured
    /// `minimumReleaseAgeExclude` rather than adding to it, so pass every
    /// package you still need exempt.
    #[usage(long, value_name = "PKG")]
    pub minimum_release_age_exclude: Vec<String>,
}

impl AgeGateFlags {
    /// These flags in the engine's own spelling.
    pub(crate) fn engine_args(&self) -> Vec<OsString> {
        let minutes = self
            .minimum_release_age
            .map(|ReleaseAge(minutes)| setting_arg(AGE, &minutes.to_string()));
        let excludes = self
            .minimum_release_age_exclude
            .iter()
            .map(|package| setting_arg(EXCLUDE, package));
        minutes.into_iter().chain(excludes).collect()
    }
}

const AGE: &str = "minimum-release-age";
const EXCLUDE: &str = "minimum-release-age-exclude";

/// A setting as the engine's command line takes one. A repeated exclusion
/// collects into one list, which replaces the configured list.
fn setting_arg(key: &str, value: &str) -> OsString {
    format!("--config.{key}={value}").into()
}

/// Rewrite `--minimum-release-age` and `--minimum-release-age-exclude` on an
/// engine command line into the engine's own spelling, with the duration in
/// minutes.
///
/// Either flag takes its value after `=` or as the next word. A flag with no
/// value is left for the engine to report, and nothing after `--` is read.
pub(crate) fn engine_argv(argv: Vec<OsString>) -> anyhow::Result<Vec<OsString>> {
    let mut out = Vec::with_capacity(argv.len());
    let mut rest = argv.into_iter();
    while let Some(arg) = rest.next() {
        let Some((key, joined)) = arg.to_str().and_then(age_flag) else {
            let ends_options = arg == "--";
            out.push(arg);
            if ends_options {
                out.extend(rest);
                break;
            }
            continue;
        };
        let value = match joined {
            Some(value) => value,
            None => {
                let next = rest
                    .as_slice()
                    .first()
                    .and_then(|word| word.to_str())
                    .filter(|word| !word.starts_with('-'))
                    .map(str::to_owned);
                let Some(value) = next else {
                    out.push(arg);
                    continue;
                };
                rest.next();
                value
            }
        };
        let value = if key == AGE {
            let ReleaseAge(minutes) = value
                .parse()
                .map_err(|reason| anyhow::anyhow!("nub: `--{AGE}`: {reason}"))?;
            minutes.to_string()
        } else {
            value
        };
        out.push(setting_arg(key, &value));
    }
    Ok(out)
}

/// The setting a `--minimum-release-age…` word names, with any value joined to
/// it by `=`.
fn age_flag(word: &str) -> Option<(&'static str, Option<String>)> {
    let option = word.strip_prefix("--")?;
    let (name, joined) = match option.split_once('=') {
        Some((name, value)) => (name, Some(value.to_owned())),
        None => (option, None),
    };
    [AGE, EXCLUDE]
        .into_iter()
        .find(|key| *key == name)
        .map(|key| (key, joined))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zero must reach the engine as a literal `0`, because `0` is the ONLY way
    /// to turn the gate off — nub ships no strictness flag, so a value that
    /// arrived as anything else (or was dropped as "unset") would leave a user
    /// who asked for no window still gated. The engine reads exactly `0` as no
    /// window.
    #[test]
    fn zero_reaches_the_engine_as_the_off_switch() {
        let flags = AgeGateFlags {
            minimum_release_age: Some(ReleaseAge(0)),
            ..Default::default()
        };
        assert_eq!(
            flags.engine_args(),
            ["--config.minimum-release-age=0"],
            "0 must be published verbatim, not elided as a falsy/default value"
        );
        // Every spelling of "no window" collapses to the same 0.
        assert_eq!(parse_release_age_minutes("0"), Ok(0));
        assert_eq!(parse_release_age_minutes("0s"), Ok(0));
        assert_eq!(parse_release_age_minutes("0d"), Ok(0));
    }

    /// The flag takes both surfaces the setting already has: `nub.jsonc`'s
    /// unit grammar, and pnpm's bare-number-means-minutes.
    #[test]
    fn release_age_accepts_the_config_units_and_pnpms_bare_minutes() {
        // Bare number = minutes, matching pnpm's CLI type map.
        assert_eq!(parse_release_age_minutes("1440"), Ok(1440));
        assert_eq!(parse_release_age_minutes("0"), Ok(0));
        // Every unit `nub.jsonc`'s install.minimumReleaseAge accepts.
        assert_eq!(parse_release_age_minutes("5m"), Ok(5));
        assert_eq!(parse_release_age_minutes("2h"), Ok(120));
        assert_eq!(parse_release_age_minutes("3d"), Ok(4320));
        assert_eq!(parse_release_age_minutes("1w"), Ok(10_080));
        // `1d` is the default window, so this is the identity a user checks.
        assert_eq!(
            parse_release_age_minutes("1d"),
            parse_release_age_minutes("1440")
        );
    }

    /// Sub-minute values must round UP. Truncating `30s` to `0` would turn a
    /// request to TIGHTEN the gate into silently disabling it.
    #[test]
    fn sub_minute_release_age_rounds_up_so_it_never_disables_the_gate() {
        assert_eq!(parse_release_age_minutes("30s"), Ok(1));
        assert_eq!(parse_release_age_minutes("1s"), Ok(1));
        assert_eq!(parse_release_age_minutes("90s"), Ok(2));
        // An explicit zero is the documented "off" value, not a rounding artifact.
        assert_eq!(parse_release_age_minutes("0s"), Ok(0));
    }

    /// The flag inherits the config grammar's rejections, so a value the file
    /// refuses cannot sneak in through the CLI. Asserted through `FromStr`,
    /// which is the path the parser takes and the source of the printed reason.
    #[test]
    fn release_age_rejects_what_the_config_grammar_rejects() {
        for bad in ["3y", "-1d", "3 d", "d", "3_0d", ""] {
            let err = bad.parse::<ReleaseAge>().expect_err(&format!(
                "{bad:?} must be rejected on the flag, as in nub.jsonc"
            ));
            assert!(
                err.contains("expected minutes") && err.contains(bad),
                "{bad:?} rejected with an unusable reason: {err}"
            );
        }
        assert_eq!("2h".parse::<ReleaseAge>().map(|a| a.0), Ok(120));
    }

    /// Every exclusion reaches the engine as its own setting, which the engine
    /// collects into one list. An install command line reaches it in the same
    /// spelling, with the duration in minutes and nothing past `--` touched.
    #[test]
    fn the_flags_reach_the_engine_as_settings() {
        let flags = AgeGateFlags {
            minimum_release_age_exclude: vec!["react".into(), "@myorg/*".into()],
            ..Default::default()
        };
        assert_eq!(
            flags.engine_args(),
            [
                "--config.minimum-release-age-exclude=react",
                "--config.minimum-release-age-exclude=@myorg/*"
            ]
        );

        let words = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        let rewritten = engine_argv(words(&[
            "nub",
            "add",
            "tool",
            "--minimum-release-age=2h",
            "--minimum-release-age-exclude",
            "@internal/*",
            "--",
            "--minimum-release-age=1",
        ]))
        .expect("a valid duration");
        assert_eq!(
            rewritten,
            words(&[
                "nub",
                "add",
                "tool",
                "--config.minimum-release-age=120",
                "--config.minimum-release-age-exclude=@internal/*",
                "--",
                "--minimum-release-age=1",
            ])
        );
        assert!(engine_argv(words(&["nub", "add", "--minimum-release-age=3y"])).is_err());
    }
}
