//! Per-invocation platform selection: `--os`, `--cpu`, `--libc`.
//!
//! These pick which platform-specific optional dependencies an install
//! materializes — the capability pnpm exposes as the `supportedArchitectures`
//! config object. pnpm ships the same three flags on `install`/`add`/`update`
//! and `dlx`, and the engine parses them there. Nub reads them again on its own
//! projects' command lines and on `nubx`, which is its own verb.
//!
//! Two deliberate departures from pnpm, both additive — no value that selects
//! something under pnpm changes meaning:
//!
//! - `*` selects every value on its axis. Under pnpm `*`, `any`, `!name`, and
//!   an empty list each install ZERO platform packages and print nothing,
//!   which is the feature's sharpest edge: four of the spellings a user
//!   reaches for first fail silently. The engine's matcher has no wildcard, so
//!   `*` reaches it spelled out as every platform Node reports on that axis. A
//!   package that excludes one of them (`"os": ["!win32"]`) is therefore left
//!   out, by the engine's rule for any list naming that platform.
//! - An unrecognized value warns. That is what turns the remaining silent-zero
//!   spellings into something a user can see.
//!
//! A pnpm-incumbent project gets neither: its command line reaches the engine
//! as pnpm reads it.
//!
//! Nothing here is ever written back to a config file. A flagged install
//! leaves `.npmrc`, `package.json`, and `pnpm-workspace.yaml` untouched, which
//! is also what pnpm, npm, and Bun all do with their equivalents.

use std::ffi::OsString;

/// Values that name no platform but that a user plausibly types, mapped to
/// what to suggest instead. `any` is Bun's spelling of the wildcard and pnpm
/// accepts it on the PACKAGE side, so it is the single most likely miss.
const WILDCARD_LOOKALIKES: &[&str] = &["any", "all", "every", "*.*"];

/// The platform values Node reports, per axis, and the two C libraries a Linux
/// package declares. They decide whether a value warns and what `*` selects. A
/// value outside them still reaches the engine, so a platform newer than this
/// binary works without an upgrade.
const KNOWN_OS: &[&str] = &[
    "aix",
    "android",
    "cygwin",
    "darwin",
    "freebsd",
    "haiku",
    "linux",
    "netbsd",
    "openbsd",
    "openharmony",
    "sunos",
    "win32",
];
const KNOWN_CPU: &[&str] = &[
    "arm", "arm64", "ia32", "loong64", "mips", "mipsel", "mips64el", "ppc", "ppc64", "riscv64",
    "s390", "s390x", "x64",
];
const KNOWN_LIBC: &[&str] = &["glibc", "musl"];

/// Each axis by flag name, in [`PlatformFlags::axes`] order.
const AXES: [(&str, &[&str]); 3] = [("os", KNOWN_OS), ("cpu", KNOWN_CPU), ("libc", KNOWN_LIBC)];

// (Plain `//`, not rustdoc: a `///` comment on a usage `Args` struct becomes the
// augmented command's `--help` about-text and clobbers the verb's own — the
// same hazard `AgeGateFlags` documents, which leaked onto `nub add --help`.)
#[derive(Debug, Default, Clone, usage_rs::Args)]
pub struct PlatformFlags {
    /// Operating system(s) to install optional dependencies for. Repeatable or
    /// comma-separated. Use `current` for this machine's own, `*` for every
    /// one. REPLACES any configured `supportedArchitectures.os`.
    #[usage(long, value_name = "OS", delimiter = ',')]
    pub os: Vec<String>,

    /// CPU architecture(s) to install optional dependencies for. Repeatable or
    /// comma-separated. Use `current` for this machine's own, `*` for every
    /// one. REPLACES any configured `supportedArchitectures.cpu`.
    #[usage(long, value_name = "CPU", delimiter = ',')]
    pub cpu: Vec<String>,

    /// C library/libraries to install optional dependencies for (`glibc`,
    /// `musl`). Only consulted on Linux — no other platform's packages declare
    /// one. REPLACES any configured `supportedArchitectures.libc`.
    #[usage(long, value_name = "LIBC", delimiter = ',')]
    pub libc: Vec<String>,
}

impl PlatformFlags {
    fn axes(&self) -> [&Vec<String>; 3] {
        [&self.os, &self.cpu, &self.libc]
    }

    fn axis_mut(&mut self, axis: usize) -> &mut Vec<String> {
        match axis {
            0 => &mut self.os,
            1 => &mut self.cpu,
            _ => &mut self.libc,
        }
    }

    /// The engine's own flags for these values, with `*` spelled out. An axis
    /// nobody named stays off the command line, so a configured value survives
    /// on it.
    pub(crate) fn engine_args(&self) -> Vec<OsString> {
        AXES.iter()
            .zip(self.axes())
            .filter(|(_, values)| !values.is_empty())
            .map(|((name, known), values)| format!("--{name}={}", spell_out(values, known)).into())
            .collect()
    }

    /// Warnings for values that will select nothing. Returns the lines to
    /// print; the caller owns rendering so this stays testable.
    ///
    /// The check is a membership test against the known values rather than a
    /// prediction about the actual dependency graph: a value that names no
    /// platform can never match a package, whatever the project depends on. A
    /// value that IS a platform but happens to match no package in this
    /// particular tree is not a mistake and stays silent.
    fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ((axis, known), values) in AXES.iter().zip(self.axes()) {
            for v in values {
                if v == "current" || v == "*" || known.contains(&v.as_str()) {
                    continue;
                }
                let hint = if WILDCARD_LOOKALIKES.contains(&v.as_str()) {
                    " — use `*` to select every one".to_string()
                } else if let Some(negated) = v.strip_prefix('!') {
                    format!(
                        " — negation isn't supported here; list the {axis} values you want instead of excluding `{negated}`"
                    )
                } else {
                    format!(" — known values are {}", known.join(", "))
                };
                out.push(format!(
                    "nub: `--{axis} {v}` matches no platform, so no optional dependency will be selected for it{hint}."
                ));
            }
        }
        out
    }

    /// Print a warning for each value that selects nothing.
    pub(crate) fn warn(&self) {
        for line in self.warnings() {
            if super::scope_warning_uses_dim() {
                eprintln!("\x1b[2m{line}\x1b[0m");
            } else {
                eprintln!("{line}");
            }
        }
    }
}

/// Spell each `*` among the platform flags of an engine command line out in
/// place, and return every value the flags named.
///
/// Read the way the engine's grammar reads them: one or more values after the
/// flag, comma-separated or not, up to the next option, or the values joined to
/// the flag by `=`. Nothing after `--` is read.
pub(crate) fn expand_engine_argv(argv: &mut [OsString]) -> PlatformFlags {
    let mut named = PlatformFlags::default();
    // The axis whose flag is still taking separate values.
    let mut open = None;
    for arg in argv.iter_mut().skip(1) {
        let Some(word) = arg.to_str().map(str::to_owned) else {
            open = None;
            continue;
        };
        if word == "--" {
            break;
        }
        let (axis, prefix, values) = match word.strip_prefix("--") {
            Some(option) => {
                let (name, joined) = match option.split_once('=') {
                    Some((name, values)) => (name, Some(values)),
                    None => (option, None),
                };
                let axis = AXES.iter().position(|(axis, _)| *axis == name);
                open = axis.filter(|_| joined.is_none());
                match (axis, joined) {
                    (Some(axis), Some(values)) => (axis, format!("--{name}="), values),
                    _ => continue,
                }
            }
            None if word.starts_with('-') => {
                open = None;
                continue;
            }
            None => match open {
                Some(axis) => (axis, String::new(), word.as_str()),
                None => continue,
            },
        };
        let parts: Vec<&str> = values.split(',').collect();
        named
            .axis_mut(axis)
            .extend(parts.iter().map(|part| (*part).to_owned()));
        if parts.contains(&"*") {
            *arg = format!("{prefix}{}", spell_out(&parts, AXES[axis].1)).into();
        }
    }
    named
}

/// `values` comma-joined, with each `*` replaced by every value on its axis.
fn spell_out<S: AsRef<str>>(values: &[S], known: &[&str]) -> String {
    values
        .iter()
        .flat_map(|value| match value.as_ref() {
            "*" => known.to_vec(),
            other => vec![other],
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(os: &[&str], cpu: &[&str], libc: &[&str]) -> PlatformFlags {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        PlatformFlags {
            os: own(os),
            cpu: own(cpu),
            libc: own(libc),
        }
    }

    #[test]
    fn an_unnamed_axis_stays_off_the_command_line_so_config_survives() {
        assert_eq!(flags(&["linux"], &[], &[]).engine_args(), ["--os=linux"]);
    }

    /// `*` is Nub's addition and the engine's matcher has no wildcard, so it
    /// has to arrive spelled out as every platform on its own axis, leaving the
    /// other axes as named.
    #[test]
    fn the_wildcard_reaches_the_engine_as_every_platform_on_its_axis() {
        let every_os = format!("--os={}", KNOWN_OS.join(","));
        assert_eq!(
            flags(&["*"], &["arm64"], &[]).engine_args(),
            [every_os.as_str(), "--cpu=arm64"]
        );
    }

    /// An install command line is read the way the engine reads it: values up
    /// to the next option or joined by `=`, and nothing past `--`.
    #[test]
    fn an_engine_command_line_spells_out_the_wildcard_where_the_engine_reads_it() {
        let words = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        let mut argv = words(&[
            "nub",
            "install",
            "--os",
            "*",
            "linux",
            "--frozen-lockfile",
            "*",
            "--cpu=arm64,*",
            "--",
            "--os",
            "*",
        ]);
        let named = expand_engine_argv(&mut argv);

        let every_os = KNOWN_OS.join(",");
        let every_cpu = format!("--cpu=arm64,{}", KNOWN_CPU.join(","));
        assert_eq!(
            argv,
            words(&[
                "nub",
                "install",
                "--os",
                &every_os,
                "linux",
                "--frozen-lockfile",
                "*",
                &every_cpu,
                "--",
                "--os",
                "*",
            ])
        );
        assert_eq!(named.os, ["*", "linux"]);
        assert_eq!(named.cpu, ["arm64", "*"]);
        assert!(named.libc.is_empty());
    }

    #[test]
    fn current_and_wildcard_pass_through_without_warning() {
        let f = flags(&["current", "*"], &["current", "x64"], &["musl"]);
        assert!(
            f.warnings().is_empty(),
            "current/* are the documented spellings: {:?}",
            f.warnings()
        );
    }

    #[test]
    fn wildcard_lookalikes_are_pointed_at_the_real_wildcard() {
        let w = flags(&["any"], &[], &[]).warnings();
        assert_eq!(w.len(), 1, "one bad value, one warning: {w:?}");
        assert!(
            w[0].contains("use `*`"),
            "the fix for `any` is the real wildcard: {}",
            w[0]
        );
    }

    #[test]
    fn negation_is_named_as_unsupported_rather_than_ignored() {
        let w = flags(&["!win32"], &[], &[]).warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("negation isn't supported"),
            "a `!` value silently selects nothing under pnpm; say so: {}",
            w[0]
        );
    }

    #[test]
    fn a_real_platform_never_warns_even_when_the_tree_has_no_such_package() {
        // The check is "does this name a platform", not "does this tree
        // contain one" — a cross-build for a platform you have no native dep
        // for is legitimate and must stay quiet.
        assert!(
            flags(&["win32"], &["arm64"], &["glibc"])
                .warnings()
                .is_empty()
        );
    }
}
