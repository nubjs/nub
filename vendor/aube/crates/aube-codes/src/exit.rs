//! Bespoke Unix exit codes per error code.
//!
//! Most aube errors exit with the generic [`EXIT_GENERIC`] (`1`). A
//! curated subset — the ones a CI script or shell pipeline most often
//! wants to branch on — gets its own exit code so callers can react
//! without parsing stderr.
//!
//! Exit codes are declared inline on each [`crate::CodeMeta`] entry
//! in [`crate::errors::ALL`]; this module just provides the lookup.
//!
//! The 8-bit exit-code space is lean (POSIX reserves several values
//! 126–165 for shell signals), so codes are allocated in 10-wide
//! ranges by category, with room to grow:
//!
//! | range  | category                                       |
//! | ------ | ---------------------------------------------- |
//! | 1      | generic / unknown error                        |
//! | 2      | CLI usage error                                |
//! | 10–19  | lockfile                                       |
//! | 20–29  | resolver                                       |
//! | 30–39  | tarball / store                                |
//! | 40–49  | registry / network                             |
//! | 50–59  | scripts / build                                |
//! | 60–69  | linker                                         |
//! | 70–79  | manifest / workspace                           |
//! | 80–89  | engine / cli surface                           |
//! | 90–99  | misc / safety                                  |
//!
//! Tooling consumers should branch on the *exit code* rather than the
//! exit category, since the categories are documentation, not API.

use crate::errors;

/// Generic catch-all. Anything not explicitly assigned an exit code
/// in [`crate::errors::ALL`] resolves to this exit code.
pub const EXIT_GENERIC: i32 = 1;

/// CLI usage error — bad flags, conflicting options, missing required
/// arguments. Reserved as a convention, not currently emitted by aube
/// itself (clap exits with this code on its own).
pub const EXIT_CLI_USAGE: i32 = 2;

/// Returns the bespoke exit code for `code`, or `None` if the code
/// has no bespoke entry (the caller should use [`EXIT_GENERIC`]).
///
/// Linear-scan lookup over [`crate::errors::ALL`]. Fine for ~50
/// entries and avoids dragging in a HashMap. The failure path is not
/// hot.
pub fn exit_code_for(code: &str) -> Option<i32> {
    errors::ALL
        .iter()
        .find(|m| m.name == code)
        .and_then(|m| m.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn exit_codes_are_unique() {
        let mut seen = HashSet::new();
        for meta in errors::ALL {
            if let Some(exit) = meta.exit_code {
                assert!(
                    seen.insert(exit),
                    "duplicate exit code {exit} (on {})",
                    meta.name
                );
            }
        }
    }

    #[test]
    fn exit_codes_are_in_valid_unix_range() {
        // POSIX exit codes are 0–255. Reserve <10 for the special
        // generic/usage entries; everything in `errors::ALL` should
        // fall in [10, 125] to avoid colliding with shell signal
        // codes (126–165 are reserved by POSIX).
        for meta in errors::ALL {
            if let Some(exit) = meta.exit_code {
                assert!(
                    (10..=125).contains(&exit),
                    "exit code {exit} for {} is out of the [10, 125] range",
                    meta.name
                );
            }
        }
    }

    #[test]
    fn exit_lookup_round_trips() {
        for meta in errors::ALL {
            if let Some(expected) = meta.exit_code {
                assert_eq!(
                    exit_code_for(meta.name),
                    Some(expected),
                    "round-trip failed for {}",
                    meta.name
                );
            }
        }
    }

    #[test]
    fn unknown_code_returns_none() {
        assert_eq!(exit_code_for("ERR_AUBE_TOTALLY_MADE_UP"), None);
    }
}
