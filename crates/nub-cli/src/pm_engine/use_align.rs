//! Lockfile alignment for `nub pm use` — step 3 of the identity-setting verb
//! (spec: `identity-policy` (no such document) §`nub pm use`). The from-state
//! is the lockfile(s) on disk (artifacts carry resolution state); the
//! to-state is the target's format. Planning is pure — no writes, no
//! network — so `use` can refuse BEFORE touching the manifest or registry.
//!
//! The two targets are `nub` and `pnpm`, and their lockfiles are the same
//! bytes under different names, so every alignment is a keep, a rename, or
//! a migration of a foreign lockfile ([`super::migrate`]). nub writes no
//! npm, yarn or bun lockfile, so those formats appear here only as
//! migration SOURCES.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Nub's own lockfile name under nub identity (the two-mode model): the
/// engine's canonical-lockfile slot under nub's own basename — `nub.lock`,
/// whose format-agnostic `.lock` extension survives a future serialization
/// change. Bytes stay pnpm-lock v9 compatible. Carried on the NUB embedder
/// profile's `lockfile_basename`.
pub(crate) const NUB_LOCKFILE: &str = "nub.lock";

/// Nub's PRIOR canonical lockfile name, still recognized on read during the
/// rename transition (carried on the NUB profile's `lockfile_legacy_basenames`)
/// and migrated to [`NUB_LOCKFILE`] on the next mutating PM op. Same pnpm-lock
/// v9 bytes — the rename is byte-identical. Sunset at the next major.
pub(crate) const NUB_LEGACY_LOCKFILE: &str = "lock.yaml";

/// The known lockfile artifacts, in the engine's candidate precedence order
/// *within* each family (npm-shrinkwrap.json outranks package-lock.json as a
/// conversion source, matching npm).
/// `lock.yaml` is nub's own artifact (the `nub` family).
const LOCKFILES: &[(&str, &str)] = &[
    (NUB_LOCKFILE, "nub"),
    // Legacy nub name (pre-rename), still nub's artifact for alignment.
    (NUB_LEGACY_LOCKFILE, "nub"),
    ("pnpm-lock.yaml", "pnpm"),
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
    ("yarn.lock", "yarn"),
    ("npm-shrinkwrap.json", "npm"),
    ("package-lock.json", "npm"),
];

/// The primary on-disk filename for a target — what a migration writes and
/// what the summary names for the fresh case.
pub(crate) fn lockfile_name(target: &str) -> &'static str {
    match target {
        "pnpm" => "pnpm-lock.yaml",
        "nub" => NUB_LOCKFILE,
        other => unreachable!("use targets are nub and pnpm, got {other}"),
    }
}

/// What `nub pm use <target>` will do to the lockfiles at the project root.
/// Decided before anything is written; rendered file-by-file in the summary.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AlignPlan {
    /// No lockfile on disk — nothing to align; the next install writes the
    /// target's format (Axiom 4 / the fresh-with-pin row).
    Fresh,
    /// The target's lockfile is already on disk: kept verbatim (no rewrite),
    /// and it is authoritative — any stray other-format files are removed.
    Keep { kept: PathBuf, remove: Vec<PathBuf> },
    /// A single foreign lockfile: migrated into the target's format by
    /// [`super::migrate`], then the source file(s) removed (migrated, not
    /// abandoned — leaving them would recreate a multi-lockfile ambiguity).
    Migrate { from: PathBuf, remove: Vec<PathBuf> },
    /// The pnpm ↔ nub pair: same bytes (`nub.lock` IS pnpm-v9 format under
    /// a generic name), different filename — a rename, never a parse/rewrite,
    /// so the file stays byte-identical and the real PM's `--frozen-lockfile`
    /// acceptance is preserved exactly.
    Rename { from: PathBuf, remove: Vec<PathBuf> },
}

/// Decide the alignment for `root` → `target` (`nub` or `pnpm`). Errors are
/// the spec's refusals, raised before any write:
///
/// - multiple foreign-format lockfiles with the target's absent — nub can't
///   infer which one carries the real resolution state;
/// - a binary `bun.lockb` as the only migration source — nothing reads it
///   (bun's text `bun.lock` is the supported spelling).
pub(crate) fn plan_alignment(root: &Path, target: &str) -> Result<AlignPlan> {
    let present: Vec<(PathBuf, &str)> = LOCKFILES
        .iter()
        .map(|(file, pm)| (root.join(file), *pm))
        .filter(|(path, _)| path.is_file())
        .collect();
    if present.is_empty() {
        return Ok(AlignPlan::Fresh);
    }

    let (target_files, foreign): (Vec<_>, Vec<_>) =
        present.into_iter().partition(|(_, pm)| *pm == target);
    let remove: Vec<PathBuf> = foreign.iter().map(|(p, _)| p.clone()).collect();

    if let Some((kept, _)) = target_files.into_iter().next() {
        return Ok(AlignPlan::Keep { kept, remove });
    }

    // Target's format absent: exactly one foreign family migrates; more is
    // an ambiguity nub refuses to guess through.
    let mut foreign_pms: Vec<&str> = foreign.iter().map(|(_, pm)| *pm).collect();
    foreign_pms.dedup();
    if foreign_pms.len() > 1 {
        let files = foreign
            .iter()
            .map(|(p, _)| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "multiple lockfiles found ({files}) and none is {target}'s — nub can't \
             infer which lockfile to migrate. Remove the stale ones first, then rerun \
             `nub pm use {target}`."
        );
    }

    let (from, from_pm) = foreign
        .first()
        .cloned()
        .expect("foreign is non-empty past the present.is_empty() guard");
    if from.file_name().is_some_and(|n| n == "bun.lockb") {
        bail!(
            "bun.lockb (binary format) is not supported — run `bun install \
             --save-text-lockfile` to generate a bun.lock text file first, then \
             rerun `nub pm use {target}`."
        );
    }
    // pnpm ↔ nub is a filename change over identical bytes: rename, never
    // parse/rewrite (byte fidelity is the real-pnpm acceptance story). The
    // rename consumes `from`, so only the OTHER foreign files stay removable.
    if matches!((from_pm, target), ("pnpm", "nub") | ("nub", "pnpm")) {
        let remove = remove.into_iter().filter(|p| *p != from).collect();
        return Ok(AlignPlan::Rename { from, remove });
    }
    Ok(AlignPlan::Migrate { from, remove })
}

/// Whether a migration source is a hoisting package manager's lockfile, whose
/// flat `node_modules` let a project import packages it never declared.
pub(crate) fn is_hoisting_source(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("package-lock.json" | "npm-shrinkwrap.json" | "yarn.lock")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nub-use-align-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            std::fs::write(dir.join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn plan_covers_the_spec_rows_keep_migrate_fresh_and_authoritative_removal() {
        // none → Fresh.
        assert_eq!(
            plan_alignment(&root("fresh", &[]), "pnpm").unwrap(),
            AlignPlan::Fresh
        );

        // already the target's format → Keep, nothing removed.
        let dir = root("keep", &[("pnpm-lock.yaml", "lockfileVersion: '9.0'\n")]);
        assert_eq!(
            plan_alignment(&dir, "pnpm").unwrap(),
            AlignPlan::Keep {
                kept: dir.join("pnpm-lock.yaml"),
                remove: vec![]
            }
        );

        // single foreign format → Migrate + the source removed.
        let dir = root("conv", &[("package-lock.json", "{}")]);
        assert_eq!(
            plan_alignment(&dir, "pnpm").unwrap(),
            AlignPlan::Migrate {
                from: dir.join("package-lock.json"),
                remove: vec![dir.join("package-lock.json")]
            }
        );

        // multiple with the target's present → target authoritative, others
        // removed (no migration).
        let dir = root(
            "multi-keep",
            &[
                ("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"),
                ("package-lock.json", "{}"),
                ("yarn.lock", "# yarn lockfile v1\n"),
            ],
        );
        match plan_alignment(&dir, "pnpm").unwrap() {
            AlignPlan::Keep { kept, mut remove } => {
                assert_eq!(kept, dir.join("pnpm-lock.yaml"));
                remove.sort();
                assert_eq!(
                    remove,
                    vec![dir.join("package-lock.json"), dir.join("yarn.lock")]
                );
            }
            other => panic!("expected Keep, got {other:?}"),
        }

        // multiple without the target's → refuse naming the files + remedy.
        let dir = root(
            "multi-ambig",
            &[("package-lock.json", "{}"), ("yarn.lock", "# v1\n")],
        );
        let err = plan_alignment(&dir, "pnpm").unwrap_err().to_string();
        assert!(
            err.contains("yarn.lock")
                && err.contains("package-lock.json")
                && err.contains("nub pm use pnpm"),
            "the ambiguity refusal must name the files and the remedy, got: {err}"
        );
    }

    /// bun's binary lockfile is the one foreign artifact nothing here reads,
    /// so it is refused with the command that turns it into one that is read.
    #[test]
    fn binary_bun_lockb_is_refused_as_a_migration_source() {
        let dir = root("lockb", &[("bun.lockb", "\0\0binary")]);
        let err = plan_alignment(&dir, "pnpm").unwrap_err().to_string();
        assert!(
            err.contains("bun.lockb") && err.contains("--save-text-lockfile"),
            "the binary-lockfile refusal must carry the bun remedy, got: {err}"
        );
    }

    #[test]
    fn pnpm_nub_pair_renames_byte_identically_in_both_directions() {
        // pnpm → nub: a rename — the source is consumed, nothing to remove.
        // (pnpm + a second foreign family stays the spec's ambiguity error,
        // covered below: the rename shortcut never widens the multi-lockfile
        // rules.)
        let dir = root("to-nub", &[("pnpm-lock.yaml", "lockfileVersion: '9.0'\n")]);
        assert_eq!(
            plan_alignment(&dir, "nub").unwrap(),
            AlignPlan::Rename {
                from: dir.join("pnpm-lock.yaml"),
                remove: vec![]
            }
        );

        // nub → pnpm: the reverse rename.
        let dir = root("to-pnpm", &[(NUB_LOCKFILE, "lockfileVersion: '9.0'\n")]);
        assert_eq!(
            plan_alignment(&dir, "pnpm").unwrap(),
            AlignPlan::Rename {
                from: dir.join(NUB_LOCKFILE),
                remove: vec![]
            }
        );

        // nub.lock under `use nub` is already nub's artifact: kept.
        let dir = root("keep-nub", &[(NUB_LOCKFILE, "lockfileVersion: '9.0'\n")]);
        assert!(matches!(
            plan_alignment(&dir, "nub").unwrap(),
            AlignPlan::Keep { .. }
        ));
    }

    #[test]
    fn shrinkwrap_outranks_package_lock_as_the_npm_migration_source() {
        // Both npm artifacts present: shrinkwrap is the source npm itself
        // honors first; both are removed after the migration.
        let dir = root(
            "shrinkwrap",
            &[("npm-shrinkwrap.json", "{}"), ("package-lock.json", "{}")],
        );
        match plan_alignment(&dir, "pnpm").unwrap() {
            AlignPlan::Migrate { from, mut remove } => {
                assert_eq!(from, dir.join("npm-shrinkwrap.json"));
                assert!(is_hoisting_source(&from));
                remove.sort();
                assert_eq!(
                    remove,
                    vec![
                        dir.join("npm-shrinkwrap.json"),
                        dir.join("package-lock.json")
                    ]
                );
            }
            other => panic!("expected Migrate, got {other:?}"),
        }
    }
}
