//! Lockfile-format-version → PM-version-range inference.
//!
//! Lockfiles record a **format version, never the PM's own version**, so the
//! tightest honest inference is a major-version *family* (the full marker table
//! lives in `pm-version-pinning-and-inference` (no such document) §"What lockfiles
//! encode"). That family is exactly what the unpinned-PATH-miss default
//! provisioning path needs: pick the latest PM *within the major the committed
//! lockfile implies*, so the provisioned default never converts or rejects the
//! lockfile (`package-manager-provisioning` (no such document) §"What it is").
//!
//! Pure library: no network, no CLI wiring, no walk-up — the caller hands us the
//! workspace root (the dir that owns the lockfile) and gets a hint back.

use std::path::Path;

use super::Pm;

/// A provisionable PM family inferred from a lockfile's format version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmVersionHint {
    pub pm: Pm,
    /// A node-semver-style range `registry::resolve_dist` already accepts —
    /// a bare major (`"8"`) or an open range (`">=9"`); never an exact version
    /// (lockfiles can't support that precision).
    pub range: String,
}

/// What the lockfile at the workspace root says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockfileHint {
    /// A PM nub can provision, with the version family the lockfile implies.
    Pm(PmVersionHint),
    /// A bun lockfile (`bun.lock` / `bun.lockb`). Nub never provisions bun, but
    /// the caller needs to *name* bun in messages — "this is a bun project, nub
    /// doesn't provision bun" beats a generic "no lockfile found".
    Bun,
}

/// Infer the PM-version family from the lockfile at `root` (no walk-up; pass
/// the workspace root). `None` means no recognized lockfile or an unknown pnpm
/// or npm format version (see [`yarn_hint`] for yarn's). The first lockfile in
/// precedence order (pnpm, yarn, bun, npm — same order as the CLI's
/// `detect_package_manager`) claims the project: a parse miss on it returns
/// `None` rather than falling through to a stray sibling lockfile, which would
/// infer the wrong PM family entirely. PM *name* detection lives elsewhere, so
/// a `None` here still lets the caller fall back to the `latest` dist-tag of
/// the named PM.
pub fn infer(root: &Path) -> Option<LockfileHint> {
    if let Ok(yaml) = std::fs::read_to_string(root.join("pnpm-lock.yaml")) {
        return pnpm_range(&yaml).map(|range| {
            LockfileHint::Pm(PmVersionHint {
                pm: Pm::Pnpm,
                range,
            })
        });
    }
    if let Ok(content) = std::fs::read_to_string(root.join("yarn.lock")) {
        return yarn_hint(&content).map(LockfileHint::Pm);
    }
    if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        return Some(LockfileHint::Bun);
    }
    for name in ["npm-shrinkwrap.json", "package-lock.json"] {
        // shrinkwrap first: npm itself prefers it over package-lock.json.
        if let Ok(json) = std::fs::read_to_string(root.join(name)) {
            return npm_range(&json)
                .map(|range| LockfileHint::Pm(PmVersionHint { pm: Pm::Npm, range }));
        }
    }
    None
}

/// `pnpm-lock.yaml`'s top-level `lockfileVersion` scalar → pnpm major family.
/// pnpm wrote the scalar as a float (`5.4`) through v7 and a quoted string
/// (`'6.0'`) since v8; both spellings are handled. Hand line-scan for one flat
/// top-level key, mirroring `resolve::committed_yarn_path` — no YAML dependency.
fn pnpm_range(yaml: &str) -> Option<String> {
    for line in yaml.lines() {
        // Top-level keys are unindented; a leading space means nested config.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(rest) = line.trim().strip_prefix("lockfileVersion:") else {
            continue;
        };
        let range = match super::resolve::strip_yaml_value(rest) {
            "5.3" => "6",
            "5.4" => "7",
            // '6.1' was a pnpm-8-prerelease window; same family.
            "6.0" | "6.1" => "8",
            // '9.0' is ambiguous: pnpm 9, 10, and 11 all write it. Open range —
            // we're picking a provisioning DEFAULT, so newest-satisfying is the
            // right resolution (and any of the three reads the file).
            "9.0" => ">=9",
            _ => return None,
        };
        return Some(range.to_string());
    }
    None
}

/// `package-lock.json` / `npm-shrinkwrap.json` `lockfileVersion` → npm family.
/// Each maps to the npm major most likely to round-trip the file unchanged
/// (newer npm auto-upgrades old formats on touch — a churned diff, not a
/// failure, but a default provision shouldn't rewrite the committed lockfile):
/// 1 → npm 6 (written by 5–6), 2 → npm 8 (default-written by 7–8 only),
/// 3 → npm ≥9 (the current default format), absent (the pre-npm-5 shrinkwrap
/// shape) → npm 6, the oldest major worth provisioning that still reads it.
fn npm_range(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = value.as_object()?;
    let range = match obj.get("lockfileVersion") {
        None => "6",
        Some(v) => match v.as_u64() {
            Some(1) => "6",
            Some(2) => "8",
            Some(3) => ">=9",
            _ => return None,
        },
    };
    Some(range.to_string())
}

/// `yarn.lock` content → yarn family. Classic always writes the
/// `# yarn lockfile v1` header comment; Berry writes a top-level `__metadata:`
/// block with its `LOCKFILE_VERSION` and cache key. A Yarn that writes the file
/// differently rewrites it, which fails `yarn install --immutable`, so each
/// format maps to exactly the releases that write it. Formats only change at a
/// minor, and one changed without a `version` bump: 4.1 started prefixing each
/// checksum with the cache key (`10c0/…`).
///
/// | `version` | `cacheKey` | checksums | written by |
/// |---|---|---|---|
/// | 4 | 5–7 | | 2.x (the 2.3–2.4 cache key is 7) |
/// | 4 | 8 | | 3.0 |
/// | 5 | 8 | | 3.1 |
/// | 6 | 8 | | 3.2–3.8 |
/// | 8 | `10c…` | bare | 4.0 |
/// | 8 | `10c…` | prefixed | 4.1–4.13 |
/// | 9 | `10c…` | prefixed | 4.14 |
/// | 10 | `10c…` | prefixed | 4.15–4.18 |
///
/// The newest range stops at the next unreleased minor, which may change the
/// format again: extend it once that release's `LOCKFILE_VERSION` is checked.
/// An unknown or missing version takes the newest Berry.
fn yarn_hint(content: &str) -> Option<PmVersionHint> {
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "# yarn lockfile v1" {
            return Some(PmVersionHint {
                pm: Pm::Yarn,
                range: "1".to_string(),
            });
        }
        if line.starts_with("__metadata:") {
            let metadata: Vec<&str> = lines
                .take_while(|l| l.starts_with(char::is_whitespace))
                .map(str::trim)
                .collect();
            // `cacheKey` is a number through Yarn 3 and `10c0`-shaped in Yarn 4;
            // only its leading cache version tells releases apart.
            let field = |key: &str| {
                let value = metadata.iter().find_map(|l| l.strip_prefix(key))?;
                super::resolve::strip_yaml_value(value)
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<u32>()
                    .ok()
            };
            let range = match (field("version:"), field("cacheKey:")) {
                (Some(4), Some(7)) => ">=2.3 <3",
                (Some(4), Some(8)) => "~3.0",
                (Some(..=4), _) => "2",
                (Some(5), _) => "~3.1",
                (Some(6), _) => ">=3.2 <4",
                (Some(8), _) => {
                    // No dependency, no checksum: 4.0 and 4.1+ write the same file.
                    match content
                        .lines()
                        .find_map(|l| l.trim().strip_prefix("checksum:"))
                    {
                        Some(sum) if !sum.contains('/') => "~4.0",
                        Some(_) => ">=4.1 <4.14",
                        None => ">=4.0 <4.14",
                    }
                }
                (Some(9), _) => "~4.14",
                (Some(10), _) => ">=4.15 <4.19",
                _ => ">=2",
            };
            return Some(PmVersionHint {
                pm: Pm::YarnBerry,
                range: range.to_string(),
            });
        }
    }
    None // Neither marker (empty/truncated file): no honest inference.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique temp dir under the system temp root (mirrors `resolve.rs`'s
    /// `tmpdir`) — only the `infer` dispatch tests need a real directory; the
    /// per-format tables run on literal snippets.
    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-lockver-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pnpm_lockfile_version_maps_to_major_family_across_quoting_eras() {
        // pnpm ≤7 wrote a float scalar, ≥8 a single-quoted string — both forms
        // per row, real spelling for that era first.
        for (line, want) in [
            ("lockfileVersion: 5.3", "6"),
            ("lockfileVersion: '5.4'", "7"),
            ("lockfileVersion: 5.4", "7"),
            ("lockfileVersion: '6.0'", "8"),
            ("lockfileVersion: '6.1'", "8"),
        ] {
            assert_eq!(
                pnpm_range(line).as_deref(),
                Some(want),
                "{line:?} must infer pnpm {want}"
            );
        }
        // Unknown format versions (ancient or future) yield no inference.
        assert_eq!(pnpm_range("lockfileVersion: '12.0'"), None);
        assert_eq!(pnpm_range("settings:\n  autoInstallPeers: true\n"), None);
    }

    #[test]
    fn pnpm_9_0_is_ambiguous_across_majors_9_10_11_so_infers_open_range() {
        // pnpm 9, 10, and 11 ALL write lockfileVersion '9.0' — the open range
        // lets default provisioning resolve to the newest, which is correct for
        // a default (every major in the family reads the file).
        let yaml = "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n";
        assert_eq!(pnpm_range(yaml).as_deref(), Some(">=9"));
    }

    #[test]
    fn npm_lockfile_version_maps_to_the_round_tripping_npm_major() {
        for (json, want) in [
            (r#"{"name":"a","lockfileVersion":1,"dependencies":{}}"#, "6"),
            (r#"{"name":"a","lockfileVersion":2,"packages":{}}"#, "8"),
            (r#"{"name":"a","lockfileVersion":3,"packages":{}}"#, ">=9"),
            // Field absent = the pre-npm-5 shrinkwrap shape → npm 6 still reads it.
            (r#"{"name":"a","dependencies":{}}"#, "6"),
        ] {
            assert_eq!(
                npm_range(json).as_deref(),
                Some(want),
                "{json} must infer npm {want}"
            );
        }
        assert_eq!(
            npm_range("not json"),
            None,
            "unparseable file: no inference"
        );
        assert_eq!(
            npm_range(r#"{"lockfileVersion":99}"#),
            None,
            "unknown future format version: no inference"
        );
    }

    #[test]
    fn yarn_classic_header_infers_v1_and_berry_metadata_infers_its_major() {
        let classic = "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n\
                       # yarn lockfile v1\n\n\nleft-pad@^1.0.0:\n  version \"1.3.0\"\n";
        assert_eq!(
            yarn_hint(classic),
            Some(PmVersionHint {
                pm: Pm::Yarn,
                range: "1".to_string()
            })
        );

        let berry = |metadata: &str| {
            yarn_hint(&format!(
                "# This file is generated by running \"yarn install\".\n\n\
                 __metadata:\n{metadata}\n\"left-pad@npm:^1.0.0\":\n  version: 1.3.0\n  \
                 checksum: 10c0/3fb59c76\n"
            ))
        };
        // Each format maps to only the releases that write it: a newer minor
        // in the same major rewrites the metadata and fails an immutable install.
        for (metadata, range) in [
            ("  version: 4\n  cacheKey: 7", ">=2.3 <3"),
            ("  version: 4\n  cacheKey: 8", "~3.0"),
            ("  version: 5\n  cacheKey: 8", "~3.1"),
            ("  version: 6\n  cacheKey: 8", ">=3.2 <4"),
            // The key order is Berry's own, but the scan doesn't depend on it.
            ("  cacheKey: 10c0\n  version: 8", ">=4.1 <4.14"),
            ("  version: 9\n  cacheKey: 10c0", "~4.14"),
            ("  version: 10\n  cacheKey: 10c0", ">=4.15 <4.19"),
            // A format newer than any known release: the newest Berry, never
            // classic (which would rewrite the file as v1).
            ("  version: 99", ">=2"),
        ] {
            assert_eq!(
                berry(metadata),
                Some(PmVersionHint {
                    pm: Pm::YarnBerry,
                    range: range.to_string()
                }),
                "{metadata:?}"
            );
        }
        // Format 8 alone spans 4.0–4.13; 4.0 is the release whose checksums
        // carry no cache-key prefix.
        let bare = "__metadata:\n  version: 8\n  cacheKey: 10c0\n\n\"left-pad@npm:1.3.0\":\n  checksum: 3fb59c76\n";
        assert_eq!(yarn_hint(bare).unwrap().range, "~4.0");
        let no_deps = "__metadata:\n  version: 8\n  cacheKey: 10c0\n";
        assert_eq!(yarn_hint(no_deps).unwrap().range, ">=4.0 <4.14");
        assert_eq!(yarn_hint(""), None, "empty yarn.lock: no honest inference");
    }

    #[test]
    fn infer_dispatches_on_lockfile_presence_with_pnpm_winning_ties() {
        let dir = tmpdir("dispatch");
        assert_eq!(infer(&dir), None, "no lockfile: no hint");

        // bun lockfiles are detected (so the caller can NAME bun) but carry no
        // provisionable hint — nub never provisions bun.
        std::fs::write(dir.join("bun.lockb"), b"\x00bun\x00").unwrap();
        assert_eq!(infer(&dir), Some(LockfileHint::Bun));

        std::fs::write(
            dir.join("package-lock.json"),
            r#"{"lockfileVersion":3,"packages":{}}"#,
        )
        .unwrap();
        assert_eq!(
            infer(&dir),
            Some(LockfileHint::Bun),
            "bun.lockb outranks package-lock.json (CLI precedence order)"
        );

        // A pnpm lockfile outranks everything below it...
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        assert_eq!(
            infer(&dir),
            Some(LockfileHint::Pm(PmVersionHint {
                pm: Pm::Pnpm,
                range: ">=9".to_string()
            }))
        );

        // ...and an unreadable winner returns None rather than falling through
        // to a sibling lockfile of a DIFFERENT pm (wrong-family hazard).
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '99.0'\n").unwrap();
        assert_eq!(
            infer(&dir),
            None,
            "an unknown pnpm format version must not fall through to bun/npm"
        );
    }
}
