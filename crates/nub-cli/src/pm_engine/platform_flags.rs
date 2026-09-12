//! Per-invocation platform selection: `--os`, `--cpu`, `--libc`.
//!
//! These pick which platform-specific optional dependencies an install
//! materializes — the capability pnpm exposes as the `supportedArchitectures`
//! config object. Nub reads that config for compatibility, but the flags are
//! the surface it documents, because the capability is an OVERRIDE of platform
//! detection for one command rather than a durable property of a project. pnpm
//! ships the same three flag names on `install`/`add`/`dlx`, so this is a
//! parity gap being closed, not a Nub-only spelling.
//!
//! Two deliberate departures from pnpm, both additive — no configuration that
//! works under pnpm changes meaning:
//!
//! - `*` selects every value on its axis. Under pnpm `*`, `any`, `!name`, and
//!   an empty list each install ZERO platform packages and print nothing,
//!   which is the feature's sharpest edge: four of the spellings a user
//!   reaches for first fail silently.
//! - An unrecognized value warns. That is what turns the remaining silent-zero
//!   spellings into something a user can see.
//!
//! Nothing here is ever written back to a config file. A flagged install
//! leaves `.npmrc`, `package.json`, and `pnpm-workspace.yaml` untouched, which
//! is also what pnpm, npm, and Bun all do with their equivalents.

use aube_util::CliSupportedArchitectures;

/// Values that name no platform but that a user plausibly types, mapped to
/// what to suggest instead. `any` is Bun's spelling of the wildcard and pnpm
/// accepts it on the PACKAGE side, so it is the single most likely miss.
const WILDCARD_LOOKALIKES: &[&str] = &["any", "all", "every", "*.*"];

/// Platform values npm and Node actually emit, per axis. Used only to decide
/// whether to WARN — an unknown value is still passed through, so a platform
/// newer than this binary works without an upgrade.
const KNOWN_OS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "freebsd",
    "linux",
    "netbsd",
    "openbsd",
    "openharmony",
    "sunos",
    "win32",
];
const KNOWN_CPU: &[&str] = &[
    "arm", "arm64", "ia32", "loong64", "mips64el", "ppc64", "riscv64", "s390x", "x64",
];
const KNOWN_LIBC: &[&str] = &["glibc", "musl"];

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
    /// Whether any axis was named on the command line.
    pub fn is_empty(&self) -> bool {
        self.os.is_empty() && self.cpu.is_empty() && self.libc.is_empty()
    }

    /// The per-axis override, with an empty axis meaning "not named" rather
    /// than "select nothing". The parser cannot distinguish `--os` absent from
    /// `--os` given no values, and the latter is a usage error it already
    /// rejects, so an empty vec here is unambiguously absence.
    fn override_set(&self) -> CliSupportedArchitectures {
        let axis = |v: &Vec<String>| (!v.is_empty()).then(|| v.clone());
        CliSupportedArchitectures {
            os: axis(&self.os),
            cpu: axis(&self.cpu),
            libc: axis(&self.libc),
        }
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
        let mut check = |axis: &str, values: &[String], known: &[&str]| {
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
        };
        check("os", &self.os, KNOWN_OS);
        check("cpu", &self.cpu, KNOWN_CPU);
        check("libc", &self.libc, KNOWN_LIBC);
        out
    }

    /// Publish the flags into the engine context and warn about values that
    /// select nothing. One call covers the whole run: every platform-filter
    /// site reads back through
    /// `commands::install::settings::effective_supported_architectures`, so
    /// nothing needs the value threaded down to it.
    pub fn apply(&self) {
        if self.is_empty() {
            return;
        }
        for line in self.warnings() {
            if super::scope_warning_uses_dim() {
                eprintln!("\x1b[2m{line}\x1b[0m");
            } else {
                eprintln!("{line}");
            }
        }
        let set = self.override_set();
        aube_util::update_engine_context(move |c| c.cli_supported_architectures = set.clone());
    }
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
    fn an_unnamed_axis_stays_none_so_config_survives() {
        let set = flags(&["linux"], &[], &[]).override_set();
        assert_eq!(set.os.as_deref(), Some(["linux".to_string()].as_slice()));
        assert!(
            set.cpu.is_none(),
            "naming --os must not clear a configured cpu axis"
        );
        assert!(set.libc.is_none(), "naming --os must not clear libc");
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

    /// The `*` wildcard is Nub's addition — pnpm installs zero packages for it
    /// — so it needs a guard, and the resolver mechanism it rides lives in
    /// `vendor/aube`, where a test would never run under Nub's own gates. Drive
    /// the public entry point from this side instead.
    #[test]
    fn wildcard_accepts_a_package_for_a_platform_that_is_not_the_host() {
        let sel = |os: &[&str], cpu: &[&str]| aube_resolver::SupportedArchitectures {
            os: os.iter().map(|s| s.to_string()).collect(),
            cpu: cpu.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let pkg_os = vec!["win32".to_string()];
        let pkg_cpu = vec!["ia32".to_string()];

        assert!(
            aube_resolver::platform::is_supported(&pkg_os, &pkg_cpu, &[], &sel(&["*"], &["*"])),
            "`*` on both axes must accept every platform variant"
        );
        assert!(
            aube_resolver::platform::is_supported(&pkg_os, &pkg_cpu, &[], &sel(&["win32"], &["*"])),
            "`*` on one axis must not constrain the other"
        );
        // The negative control: without the wildcard the same package is
        // rejected, so the assertions above are testing the wildcard and not
        // some blanket accept-everything path.
        assert!(
            !aube_resolver::platform::is_supported(
                &pkg_os,
                &pkg_cpu,
                &[],
                &sel(&["linux"], &["x64"])
            ),
            "a non-matching selection must still reject"
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
