//! The system library and header paths a native build needs to read — a BASE grant for every
//! jailed lifecycle script.
//!
//! # Why this is a base grant and not per-package
//!
//! 17 of the 230 source-read lifecycle packages cannot build without reading a SYSTEM prefix,
//! and 12 of them need it on their DEFAULT build path, not a fallback. It was briefly modelled
//! per package, with each of the 17 naming the directories it needed. That was wrong twice
//! over.
//!
//! Mechanically, the paths are DISCOVERED AT BUILD TIME by executing `pkg-config`, `pg_config`,
//! `krb5-config` or `curl-config` during `binding.gyp` variable evaluation, and their stdout is
//! what supplies the include and link flags. Nothing in a manifest names them, so a per-package
//! enumeration has nothing to enumerate. The 17 also do not cluster on one mechanism — they are
//! the ordinary native long tail (OpenSSL, ODBC, X11, Postgres, Kerberos, libsecret, JDK, Xcode
//! frameworks) — so any list is already wrong for the eighteenth package.
//!
//! And on the security side a per-package split bought nothing: **a system library path holds
//! no victim-specific information.** A hijacked package reading `/usr/lib/libssl.so` learns
//! nothing, and could have shipped its own copy of any header it wanted. The jail already
//! base-grants the interpreter, so base-granting the headers you compile against is the
//! consistent choice.
//!
//! # LIBRARY SUBPATHS ONLY — never a prefix root
//!
//! The one refinement that is load-bearing. A package prefix is not uniformly boring:
//! `/opt/homebrew/etc` holds service configuration and `/opt/homebrew/var` holds LIVE
//! DATABASES (`var/postgres`), so granting the Homebrew prefix would hand a lifecycle script
//! the developer's local Postgres data directory under the banner of "world-readable system
//! libraries". The same applies to `/usr/local/{etc,var}`. So each entry names a library or
//! header subpath — `lib`, `include`, the keg trees under `opt`/`Cellar` — and never the prefix
//! above them. `/usr/lib`, `/usr/include`, `/lib` and the Xcode SDK are library-only by
//! construction and go in whole; the Debian/Ubuntu multiarch dirs and every `pkgconfig`
//! directory sit under `/usr/lib`, `/usr/lib64` or `/usr/local/lib` and are covered by those.
//!
//! The grant is READ-ONLY. That is what makes coarse acceptable rather than merely convenient.

use std::path::PathBuf;

/// Linux. `/usr/lib` subsumes the multiarch dirs (`x86_64-linux-gnu`, `aarch64-linux-gnu`,
/// `i386-linux-gnu`, `arm-linux-gnueabi{,hf}`), `/usr/lib/pkgconfig` and `/usr/lib/jvm`, all of
/// which the corpus named individually.
#[cfg(target_os = "linux")]
const SYSTEM_READ_PATHS: &[&str] = &[
    "/lib",
    "/usr/lib",
    "/usr/lib64",
    "/usr/include",
    "/usr/local/lib",
    "/usr/local/include",
    // The second default pkg-config search directory; the other two are under the lib dirs
    // above.
    "/usr/share/pkgconfig",
    "/opt/local/lib",
    "/home/linuxbrew/.linuxbrew/lib",
    "/home/linuxbrew/.linuxbrew/include",
    // msnodesqlv8's ODBC driver trees, narrowed to the library and header subdirs: the driver
    // package also installs configuration under its prefix.
    "/opt/microsoft/msodbcsql18/lib64",
    "/opt/microsoft/msodbcsql18/include",
    "/opt/microsoft/msodbcsql17/lib64",
    "/opt/microsoft/msodbcsql17/include",
    "$JAVA_HOME",
];

/// macOS. Both Homebrew prefixes appear as four subpaths each rather than one prefix, per the
/// library-subpaths rule above. `opt` is the per-formula symlink tree and `Cellar` is what it
/// points at — both are needed for a keg's `lib`/`include` to be reachable, and a `opt/*/lib`
/// glob is not expressible (`subtree_globs` supplies the recursion, so a `*` here could only
/// widen the rule past the directory the entry names).
#[cfg(target_os = "macos")]
const SYSTEM_READ_PATHS: &[&str] = &[
    "/usr/lib",
    "/usr/include",
    "/opt/homebrew/lib",
    "/opt/homebrew/include",
    "/opt/homebrew/opt",
    "/opt/homebrew/Cellar",
    "/usr/local/lib",
    "/usr/local/include",
    "/usr/local/opt",
    "/usr/local/Cellar",
    // FRAMEWORK HEADERS ONLY LIVE HERE, and macOS is the largest bucket of the 17 (7
    // packages): @serialport/bindings links CoreFoundation and IOKit, drivelist Carbon and
    // DiskArbitration, node-mac-contacts AppKit and Contacts, spellchecker AppKit, robotjs
    // Carbon/ApplicationServices/OpenGL, gl QuartzCore/Quartz, pngquant-bin <SDK>/usr/include.
    // `$SDKROOT` is normally UNSET in a lifecycle script's env, so the two stock SDK homes are
    // named too — those are fixed Apple locations, not a guess at where a toolchain landed.
    "$SDKROOT",
    "/Library/Developer/CommandLineTools",
    "/Applications/Xcode.app/Contents/Developer",
    "/Library/Java/JavaVirtualMachines",
    "$JAVA_HOME",
];

/// Windows. `%LOCALAPPDATA%` and `%ProgramFiles%` are resolved from the environment for the
/// same reason `$SDKROOT` is: a path matcher cannot know either.
#[cfg(windows)]
const SYSTEM_READ_PATHS: &[&str] = &[
    "C:/OpenSSL-Win64",
    "C:/OpenSSL-Win32",
    "%LOCALAPPDATA%/node-libcurl-vcpkg",
    "%ProgramFiles%/Microsoft SQL Server",
    "%JAVA_HOME%",
];

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
const SYSTEM_READ_PATHS: &[&str] = &[];

/// The host platform's system read paths, with every env anchor resolved.
///
/// An entry may lead with `$NAME` (POSIX) or `%NAME%` (Windows) because `$JAVA_HOME` and
/// `$SDKROOT` have no meaning to a path matcher and hardcoding a guess for either would be
/// wrong on most hosts. Resolution happens HERE, against the build environment, rather than in
/// the policy grammar: `fold` rejects an unknown `$` token on the `fs` axis by design (the
/// fail-open closer), so an unresolved anchor must never reach it. An unset variable DROPS the
/// entry, which is the conservative direction — the package then fails exactly as it would with
/// no grant.
pub fn system_read_paths() -> Vec<PathBuf> {
    SYSTEM_READ_PATHS.iter().filter_map(resolve).collect()
}

fn resolve(entry: &&str) -> Option<PathBuf> {
    let entry = *entry;
    let (var, rest) = if let Some(tail) = entry.strip_prefix('$') {
        let (var, rest) = tail.split_once('/').unwrap_or((tail, ""));
        (var, rest)
    } else if let Some(tail) = entry.strip_prefix('%') {
        let (var, rest) = tail.split_once('%')?;
        (var, rest.trim_start_matches('/'))
    } else {
        return Some(PathBuf::from(entry));
    };
    let base = std::env::var_os(var)?;
    if base.is_empty() {
        return None;
    }
    let mut path = PathBuf::from(base);
    if !rest.is_empty() {
        path.push(rest);
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recommendation is worthless if the host platform resolves to nothing, and a `cfg`
    /// typo above is exactly how that would happen silently on one platform while the other two
    /// stayed green.
    #[test]
    fn the_host_platform_resolves_to_a_non_empty_set() {
        assert!(
            !system_read_paths().is_empty(),
            "no system read paths for this target — the cfg arms above have diverged"
        );
    }

    /// Every resolved path is absolute, and none carries a glob. A `*` could only widen a rule
    /// past the directory the entry names, since `subtree_globs` supplies the recursion.
    #[test]
    fn every_resolved_path_is_absolute_and_glob_free() {
        for path in system_read_paths() {
            let s = path.to_string_lossy();
            assert!(
                path.is_absolute(),
                "`{s}` is not absolute — an env anchor resolved to a relative value"
            );
            assert!(
                !s.contains('*') && !s.contains('?'),
                "`{s}` must name a directory, not a pattern"
            );
        }
    }

    /// The library-subpaths rule, asserted rather than left to review. A prefix root would drag
    /// in `etc` (service config) and `var` (live databases, e.g. `var/postgres`) under the
    /// banner of world-readable system libraries — the exact mistake the refinement exists to
    /// prevent.
    #[test]
    fn no_entry_names_a_package_manager_prefix_root() {
        for entry in SYSTEM_READ_PATHS {
            for root in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
                assert_ne!(
                    *entry, root,
                    "`{root}` is a prefix ROOT: it contains etc/ and var/, so it must be named \
                     by library subpath instead"
                );
            }
        }
    }

    /// An unset anchor drops its entry rather than producing a literal `$NAME` path, which
    /// `fold` would reject on the fs axis and which no candidate could ever match.
    #[test]
    fn an_unset_env_anchor_drops_the_entry() {
        assert_eq!(resolve(&"$NUB_DEFINITELY_UNSET_ANCHOR"), None);
        assert_eq!(resolve(&"%NUB_DEFINITELY_UNSET_ANCHOR%/lib"), None);
        assert!(!system_read_paths().iter().any(|p| {
            let s = p.to_string_lossy();
            s.contains('$') || s.contains('%')
        }));
    }
}
