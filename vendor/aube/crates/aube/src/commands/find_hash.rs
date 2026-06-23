//! `aube find-hash <hash>` — reverse lookup from a file hash to the
//! `<name>@<version>` packages whose index references it.
//!
//! Walks `<store>/v1/index/*.json`, parses each cached `PackageIndex`,
//! and prints every `<name>@<version>` whose index contains the given
//! hash along with the relative path the hash lives at. Accepts both
//! `sha512-<base64>` integrity strings and raw hex CAS digests;
//! the integrity form is normalized to hex before comparison.
//!
//! Complement to `cat-file` / `cat-index`: once you have a store file
//! open via `cat-file <hash>`, `find-hash <hash>` tells you which
//! package(s) shipped it. Helpful for debugging linker behavior,
//! content-addressable dedup, and "who owns this random file".
//!
//! This is a read-only introspection command — no project lock, no
//! lockfile, no node_modules.

use clap::Args;
use miette::{IntoDiagnostic, miette};
use std::collections::BTreeMap;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  # Accepts integrity strings
  $ aube find-hash sha512-abc123...
  lodash@4.17.21\tpackage/lodash.js
  express@4.19.2\tnode_modules/lodash/lodash.js

  # ...or raw hex digests
  $ aube find-hash 5d41402abc4b2a76b9719d911017c592...

  # Machine-readable
  $ aube find-hash --json sha512-abc123...
";

#[derive(Debug, Args)]
pub struct FindHashArgs {
    /// Hash to look up.
    ///
    /// Accepts `sha512-<base64>` (pnpm integrity format) or a raw hex
    /// CAS digest.
    pub hash: String,

    /// Emit machine-readable JSON instead of a plain text listing.
    ///
    /// Output is an array of `{ "name", "version", "path" }` objects.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

/// A single match: one package's index referenced the queried hash via
/// the given in-package relative path.
#[derive(Debug, serde::Serialize)]
struct Match {
    name: String,
    version: String,
    path: String,
}

pub async fn run(args: FindHashArgs) -> miette::Result<()> {
    args.network.install_overrides();
    // Normalize input: integrity → hex, or validate raw hex up front so
    // we don't compare nonsense strings against every cached index.
    let target_hex = if args.hash.starts_with(aube_store::SHA512_INTEGRITY_PREFIX) {
        aube_store::integrity_to_hex(&args.hash)
            .ok_or_else(|| miette!("invalid integrity hash: {}", args.hash))?
    } else {
        if args.hash.is_empty() || !args.hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(miette!(
                "invalid hash: {}\nhelp: expected a `sha512-<base64>` integrity string or a hex CAS digest",
                args.hash
            ));
        }
        args.hash.to_ascii_lowercase()
    };

    let cwd = crate::dirs::project_root_or_cwd()?;
    let store = crate::commands::open_store(&cwd)?;

    let index_dir = store.index_dir();
    if !index_dir.exists() {
        return Err(miette!(
            "index cache is empty at {}\nhelp: run `{}` or `{}` first to populate the store",
            index_dir.display(),
            aube_util::cmd("install"),
            aube_util::cmd("fetch")
        ));
    }

    let matches = scan_index_dir(&index_dir, &target_hex)?;

    // Always print the output before deciding the exit status, so scripts
    // that pipe `find-hash --json` into `jq` still get parseable output
    // (an empty `[]` array) on a miss — but the exit code is consistent
    // between text and JSON modes: non-zero when nothing matched.
    if args.json {
        let json = serde_json::to_string_pretty(&matches)
            .into_diagnostic()
            .map_err(|e| miette!("failed to serialize matches: {e}"))?;
        println!("{json}");
    } else {
        for m in &matches {
            println!("{}@{}\t{}", m.name, m.version, m.path);
        }
    }

    if matches.is_empty() {
        return Err(miette!(
            "no package index references hash {}\nhelp: the file may belong to a package aube hasn't fetched yet, or the hash may be wrong",
            args.hash
        ));
    }

    Ok(())
}

/// Walk every `*.json` file in the cache dir — both at the root
/// (integrity-less entries) and one level down under `<integrity-hex>/`
/// subdirs (integrity-keyed entries) — parse each as a `PackageIndex`,
/// and collect every entry whose `hex_hash` matches `target_hex`.
/// Cache entries that fail to parse or whose filename can't be decoded
/// into `{name}@{version}` are skipped silently — the cache is a
/// best-effort artifact, not a source of truth.
fn scan_index_dir(index_dir: &std::path::Path, target_hex: &str) -> miette::Result<Vec<Match>> {
    let mut matches: Vec<Match> = Vec::new();
    scan_one_level(index_dir, target_hex, &mut matches)?;

    // Recurse exactly one level deep for the `<integrity-hex>/` subdirs.
    // The layout is flat by design; anything deeper would be foreign.
    let entries = std::fs::read_dir(index_dir)
        .into_diagnostic()
        .map_err(|e| miette!("failed to read {}: {e}", index_dir.display()))?;
    for entry in entries {
        let entry = entry
            .into_diagnostic()
            .map_err(|e| miette!("failed to read directory entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            scan_one_level(&path, target_hex, &mut matches)?;
        }
    }

    matches.sort_by(|a, b| (&a.name, &a.version, &a.path).cmp(&(&b.name, &b.version, &b.path)));
    Ok(matches)
}

fn scan_one_level(
    dir: &std::path::Path,
    target_hex: &str,
    matches: &mut Vec<Match>,
) -> miette::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(miette!("failed to read {}: {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry
            .into_diagnostic()
            .map_err(|e| miette!("failed to read directory entry: {e}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((name, version)) = split_stem(stem) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(index): Result<BTreeMap<String, aube_store::StoredFile>, _> =
            serde_json::from_str(&content)
        else {
            continue;
        };
        for (rel_path, file) in index {
            if file.hex_hash == target_hex {
                matches.push(Match {
                    name: name.clone(),
                    version: version.clone(),
                    path: rel_path,
                });
            }
        }
    }
    Ok(())
}

/// Reverse the `{safe_name}@{version}.json` naming scheme used by
/// `Store::save_index`. The safe-name rule is `/` → `__`, which isn't
/// globally bijective — a non-scoped package whose name legitimately
/// contains `__` (e.g. `foo__bar`) would round-trip back as `foo/bar`
/// under naive `replace("__", "/")`.
///
/// Exploit the structure of npm names: only scoped packages contain
/// `/`, and always exactly one — between the scope and the package
/// name. So:
///   - Non-scoped (no leading `@`): keep the safe name verbatim; the
///     `__` must be literal.
///   - Scoped (leading `@`): replace only the *first* `__` with `/`.
///
/// (A scoped package whose scope itself contains `__` — e.g.
/// `@foo__bar/baz` — is still ambiguous vs. `@foo/bar__baz` and
/// decodes to the latter. That's a limitation of the underlying
/// `save_index` encoding, not fixable in the reverse direction alone.)
///
/// Integrity (when present) lives in the parent directory name, not
/// the filename, so no suffix stripping is needed here.
fn split_stem(stem: &str) -> Option<(String, String)> {
    let at = stem.rfind('@')?;
    if at == 0 {
        return None;
    }
    let safe_name = &stem[..at];
    let version = &stem[at + 1..];
    if version.is_empty() {
        // A stem like `lodash@` (corrupt cache file `lodash@.json`)
        // would otherwise emit a `lodash@\tindex.js` line — drop it,
        // matching what `scan_index_dir`'s doc comment promises.
        return None;
    }
    let name = if let Some(rest) = safe_name.strip_prefix('@') {
        if let Some(sep) = rest.find("__") {
            format!("@{}/{}", &rest[..sep], &rest[sep + 2..])
        } else {
            safe_name.to_string()
        }
    } else {
        safe_name.to_string()
    };
    Some((name, version.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_stem_plain() {
        assert_eq!(
            split_stem("lodash@4.17.21"),
            Some(("lodash".into(), "4.17.21".into()))
        );
    }

    #[test]
    fn split_stem_scoped() {
        // `@babel/core@7.0.0` is saved as `@babel__core@7.0.0.json`;
        // split_stem receives the stem without the .json.
        assert_eq!(
            split_stem("@babel__core@7.0.0"),
            Some(("@babel/core".into(), "7.0.0".into()))
        );
    }

    #[test]
    fn split_stem_rejects_bare_scope_name() {
        // `@scope` on its own (no / or version) is never a real cache
        // entry and would confuse `rfind('@')`.
        assert_eq!(split_stem("@scope"), None);
    }

    #[test]
    fn split_stem_preserves_double_underscore_in_unscoped_name() {
        // Regression: naive `replace("__", "/")` would corrupt this to
        // `foo/bar`. Non-scoped names can contain `__` legitimately.
        assert_eq!(
            split_stem("foo__bar@1.0.0"),
            Some(("foo__bar".into(), "1.0.0".into()))
        );
    }

    #[test]
    fn split_stem_rejects_empty_version() {
        // Corrupt cache file like `lodash@.json` would otherwise emit
        // a match with an empty version field.
        assert_eq!(split_stem("lodash@"), None);
        assert_eq!(split_stem("@scope__pkg@"), None);
    }

    #[test]
    fn split_stem_scoped_with_trailing_double_underscore_in_pkg() {
        // `@scope/pkg__name@1.0.0` is saved as `@scope__pkg__name@...`
        // The first `__` is the scope/pkg separator; subsequent ones
        // are literal.
        assert_eq!(
            split_stem("@scope__pkg__name@1.0.0"),
            Some(("@scope/pkg__name".into(), "1.0.0".into()))
        );
    }

    #[test]
    fn split_stem_preserves_build_metadata_in_version() {
        // Integrity moved out of the filename into a parent subdir,
        // so no part of the stem ever needs stripping. Build
        // metadata in the version passes through verbatim.
        assert_eq!(
            split_stem("foo@1.0.0+build123"),
            Some(("foo".into(), "1.0.0+build123".into()))
        );
        assert_eq!(
            split_stem("foo@1.0.0+deadbeefdeadbeef"),
            Some(("foo".into(), "1.0.0+deadbeefdeadbeef".into()))
        );
    }

    #[test]
    fn scan_index_dir_finds_a_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let index = serde_json::json!({
            "index.js": {
                "hex_hash": "deadbeef",
                "store_path": "/tmp/foo",
                "executable": false,
            },
            "package.json": {
                "hex_hash": "cafefeed",
                "store_path": "/tmp/bar",
                "executable": false,
            }
        });
        std::fs::write(
            dir.join("lodash@4.17.21.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        let matches = scan_index_dir(dir, "deadbeef").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "lodash");
        assert_eq!(matches[0].version, "4.17.21");
        assert_eq!(matches[0].path, "index.js");
    }

    #[test]
    fn scan_index_dir_matches_across_files_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let shared_hash = "deadbeef";
        let a_index = serde_json::json!({
            "a.js": { "hex_hash": shared_hash, "store_path": "/tmp/a", "executable": false }
        });
        let b_index = serde_json::json!({
            "b.js": { "hex_hash": shared_hash, "store_path": "/tmp/b", "executable": false }
        });
        std::fs::write(
            dir.join("b-pkg@1.0.0.json"),
            serde_json::to_string(&a_index).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("a-pkg@1.0.0.json"),
            serde_json::to_string(&b_index).unwrap(),
        )
        .unwrap();

        let matches = scan_index_dir(dir, shared_hash).unwrap();
        assert_eq!(matches.len(), 2);
        // Sorted alphabetically by name.
        assert_eq!(matches[0].name, "a-pkg");
        assert_eq!(matches[1].name, "b-pkg");
    }

    #[test]
    fn scan_index_dir_ignores_non_json_and_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        std::fs::write(dir.join("README"), "not json").unwrap();
        std::fs::write(dir.join("broken@1.0.0.json"), "{ not valid json").unwrap();

        let matches = scan_index_dir(dir, "deadbeef").unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn scan_index_dir_descends_into_integrity_subdirs() {
        // Integrity-keyed entries live under `<16 hex>/<name>@<ver>.json`.
        // scan_index_dir must walk those too, not just the root.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let target = "deadbeef";
        let index = serde_json::json!({
            "lib.js": { "hex_hash": target, "store_path": "/tmp/x", "executable": false }
        });

        // Integrity-keyed: under a hex subdir.
        let subdir = dir.join("aabbccddeeff0011");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(
            subdir.join("lodash@4.17.21.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        // Integrity-less: at the root.
        std::fs::write(
            dir.join("other-pkg@1.0.0.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        let matches = scan_index_dir(dir, target).unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].name, "lodash");
        assert_eq!(matches[1].name, "other-pkg");
    }
}
