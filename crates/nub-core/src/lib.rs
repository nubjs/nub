//! Core logic shared across Nub's CLI crates.

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

#[cfg(test)]
mod tests {
    use super::PATH_LIST_SEPARATOR;

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
