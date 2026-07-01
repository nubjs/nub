//! Core logic shared across Nub's CLI crates.

// `collapsible_if` fires on nested `if let { if let }` once the workspace MSRV
// (1.88) unlocks let-chain suggestions; collapsing every site is cosmetic churn,
// so allow it.
#![allow(clippy::collapsible_if)]

pub mod config_cache;
pub mod node;
pub mod pm;
pub mod pnp;
pub mod version_management;
pub mod workspace;

/// The platform's PATH-list separator: `;` on Windows, `:` elsewhere (A9). The
/// standard library exposes no constant for this — only `env::join_paths` /
/// `env::split_paths` use it internally — so it's named once here for the
/// handful of sites that build a PATH by concatenation.
pub const PATH_LIST_SEPARATOR: &str = if cfg!(windows) { ";" } else { ":" };

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
    use super::{PATH_LIST_SEPARATOR, strip_utf8_bom};

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
