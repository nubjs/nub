//! Core logic shared across Nub's CLI crates.

// `collapsible_if` fires on nested `if let { if let }` now that the workspace
// MSRV supports let chains; collapsing every site is cosmetic churn,
// so allow it.
#![allow(clippy::collapsible_if)]

pub mod compile;
pub mod config_cache;
pub mod node;
pub mod pm;
pub mod pnp;
pub mod quarantine;
pub mod version_management;
#[cfg(windows)]
#[doc(hidden)]
pub mod windows_security;
pub mod workspace;

/// The platform's PATH-list separator: `;` on Windows, `:` elsewhere (A9). The
/// standard library exposes no constant for this — only `env::join_paths` /
/// `env::split_paths` use it internally — so it's named once here for the
/// handful of sites that build a PATH by concatenation.
pub const PATH_LIST_SEPARATOR: &str = if cfg!(windows) { ";" } else { ":" };

/// The filenames a bare command name can take inside one PATH directory. On
/// Windows a tool is spelled with an extension on disk — `llvm-strip.exe` — so a
/// probe for `dir.join(name)` there matches nothing and its caller silently
/// concludes the tool is absent. Every such probe goes through here so the
/// PATHEXT rule is written once: the launcher's Node discovery and the
/// compiler's strip lookup previously carried separate copies of it.
///
/// Only the extensions `CreateProcessW` can launch as an image are offered, which
/// is narrower than PATHEXT. A `.BAT`/`.CMD` needs `cmd.exe` to interpret it, so
/// admitting one would hand a caller a path it cannot spawn — the launcher's
/// discovery states the same rule for its own reason: a candidate that would fail
/// at spawn must lose to the next one rather than be selected. A name that already
/// carries an extension is taken as spelled, and it is the caller's business
/// whether that spelling runs.
#[cfg(windows)]
pub fn command_candidates(dir: &std::path::Path, name: &str) -> Vec<std::path::PathBuf> {
    if std::path::Path::new(name).extension().is_some() {
        return vec![dir.join(name)];
    }
    let configured = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE".into());
    configured
        .split(';')
        .filter(|extension| matches!(extension.to_ascii_uppercase().as_str(), ".EXE" | ".COM"))
        .map(|extension| dir.join(format!("{name}{extension}")))
        .collect()
}

/// See the Windows counterpart. Elsewhere a command is spelled on disk exactly as
/// it is invoked, so there is one candidate.
#[cfg(not(windows))]
pub fn command_candidates(dir: &std::path::Path, name: &str) -> Vec<std::path::PathBuf> {
    vec![dir.join(name)]
}

/// Whether a candidate is a file this process could actually execute. On Unix a
/// present-but-non-executable file is not a usable tool, and letting it win would
/// turn a clean "not found" into a spawn failure further along.
fn is_executable_file(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// The first of `names` that resolves to an executable file on `path`, as the
/// matched PATH ITSELF. Returning the path rather than the name is what keeps
/// discovery and execution aligned: resolving the name a second time at spawn can
/// land on a different PATH entry or a different extension than the one just
/// proved to exist.
///
/// Search is name-major — every directory is tried for the first name before the
/// second is considered — so an earlier name is a genuine preference rather than
/// a tie broken by PATH order.
pub fn find_on_path_in(
    path: Option<&std::ffi::OsStr>,
    names: &[&str],
) -> Option<std::path::PathBuf> {
    let path = path?;
    for name in names {
        for dir in std::env::split_paths(path) {
            if let Some(found) = command_candidates(&dir, name)
                .into_iter()
                .find(|candidate| is_executable_file(candidate))
            {
                return Some(found);
            }
        }
    }
    None
}

/// [`find_on_path_in`] against this process's own `PATH`.
pub fn find_on_path(names: &[&str]) -> Option<std::path::PathBuf> {
    find_on_path_in(std::env::var_os("PATH").as_deref(), names)
}

/// Strip a leading UTF-8 BOM (U+FEFF, bytes `EF BB BF`) so `serde_json` accepts
/// the document. Windows PowerShell 5.1 / .NET `Encoding.UTF8` and many Windows
/// editors write `package.json` with a BOM; npm/pnpm tolerate it, `serde_json`
/// does not (it rejects the BOM as an unexpected value "at line 1 column 1").
/// `str::trim`/`trim_start` do NOT remove it (U+FEFF is not ASCII whitespace).
/// Every nub-side manifest read funnels through this before parsing. (The
/// vendored aube engine strips the BOM at its own reader independently.)
pub fn strip_utf8_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::{PATH_LIST_SEPARATOR, find_on_path_in, strip_utf8_bom};
    use std::path::PathBuf;

    /// Writes `name` into `dir` and makes it executable, so it is a candidate the
    /// probe is allowed to return rather than a file that only looks like one.
    fn put_tool(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Asserts the probe resolved to `expected`, ignoring extension case.
    ///
    /// The candidate is built from the PATHEXT entry, and Windows spells that
    /// `.EXE` while the file on disk is `llvm-strip.exe`. Both name the same file
    /// on a case-insensitive filesystem and both spawn, so requiring byte
    /// equality would be asserting an incidental property of PATHEXT rather than
    /// the contract, which is that the probe finds the right FILE and hands back
    /// something runnable. A probe that finds nothing still fails this.
    fn assert_resolved(found: Option<PathBuf>, expected: &std::path::Path) {
        let found = found.unwrap_or_else(|| {
            panic!(
                "expected the probe to resolve {}, got None",
                expected.display()
            )
        });
        assert!(
            found.is_file(),
            "the probe returned {}, which is not a file",
            found.display()
        );
        assert_eq!(
            found.to_string_lossy().to_lowercase(),
            expected.to_string_lossy().to_lowercase(),
            "the probe resolved to the wrong file"
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nub-core-path-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The regression this guards: probing for a bare `llvm-strip` on Windows,
    /// where the tool is on disk as `llvm-strip.exe`. A probe that only tries the
    /// bare spelling finds nothing, and `nub compile`'s caller then embeds an
    /// unstripped Node — quietly, and about 4 MB heavier — on every Windows host.
    /// Driving the real entry point is the point: asserting on candidate
    /// generation alone stays green against the bare-`join` implementation.
    #[test]
    fn a_bare_name_resolves_to_the_hosts_own_spelling() {
        let dir = scratch("spelling");
        let spelled = if cfg!(windows) {
            "llvm-strip.exe"
        } else {
            "llvm-strip"
        };
        let expected = put_tool(&dir, spelled);

        let path = std::env::join_paths([&dir]).unwrap();
        assert_resolved(find_on_path_in(Some(&path), &["llvm-strip"]), &expected);
        assert_eq!(
            find_on_path_in(Some(&path), &["definitely-not-a-stripper"]),
            None,
            "a tool that is absent must not resolve"
        );
    }

    /// An earlier name wins even when a later one sits in an earlier PATH entry,
    /// because `llvm-strip` is preferred over `strip` for reasons PATH order knows
    /// nothing about (it is the only one that handles a foreign object format).
    #[test]
    fn an_earlier_name_outranks_path_order() {
        let first = scratch("pref-first");
        let second = scratch("pref-second");
        put_tool(&first, if cfg!(windows) { "strip.exe" } else { "strip" });
        let preferred = put_tool(
            &second,
            if cfg!(windows) {
                "llvm-strip.exe"
            } else {
                "llvm-strip"
            },
        );

        let path = std::env::join_paths([&first, &second]).unwrap();
        // Name-major: the preferred tool wins from a later PATH entry.
        assert_resolved(
            find_on_path_in(Some(&path), &["llvm-strip", "strip"]),
            &preferred,
        );
    }

    /// A file that is present but carries no execute bit is not a usable tool.
    /// Accepting it would turn a clean "not found" — which callers handle, by
    /// falling back or naming the missing tool — into a spawn failure further
    /// along, where the cause is much harder to read.
    #[cfg(unix)]
    #[test]
    fn a_file_without_the_execute_bit_does_not_resolve() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("nonexec");
        let path = dir.join("llvm-strip");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let joined = std::env::join_paths([&dir]).unwrap();
        assert_eq!(
            find_on_path_in(Some(&joined), &["llvm-strip"]),
            None,
            "a non-executable file must not satisfy a probe whose result gets spawned"
        );
    }

    /// A batch file is not an image `CreateProcessW` can launch, so offering one
    /// would hand back a path that cannot be spawned. Unix has no such split, and
    /// there the file is simply not named `.bat`.
    #[cfg(windows)]
    #[test]
    fn a_batch_file_is_not_offered_as_a_spawnable_tool() {
        let dir = scratch("batch");
        put_tool(&dir, "llvm-strip.bat");
        let path = std::env::join_paths([&dir]).unwrap();
        assert_eq!(
            find_on_path_in(Some(&path), &["llvm-strip"]),
            None,
            "a .bat needs cmd.exe, so it must not satisfy a probe whose result gets spawned"
        );
    }

    #[test]
    fn strip_utf8_bom_removes_only_a_leading_bom() {
        // Present → removed; the rest is untouched.
        assert_eq!(strip_utf8_bom("\u{feff}{\"a\":1}"), "{\"a\":1}");
        // Absent → borrowed through unchanged.
        assert_eq!(strip_utf8_bom("{\"a\":1}"), "{\"a\":1}");
        // A BOM that isn't leading is left alone (not our concern; valid JSON
        // never has one mid-document, and stripping it would corrupt content).
        assert_eq!(strip_utf8_bom("{}\u{feff}"), "{}\u{feff}");
        assert_eq!(strip_utf8_bom(""), "");
    }

    #[test]
    fn path_list_separator_matches_platform() {
        // Derive the real separator from std (join_paths uses the platform's)
        // and assert our const agrees — catches a `;`/`:` swap and, on the
        // windows-latest CI leg, confirms the Windows value is `;` (A9).
        let joined = std::env::join_paths(["a", "b"]).unwrap();
        assert_eq!(
            joined.to_string_lossy(),
            format!("a{PATH_LIST_SEPARATOR}b"),
            "PATH_LIST_SEPARATOR must match std's path-list separator"
        );
    }
}
