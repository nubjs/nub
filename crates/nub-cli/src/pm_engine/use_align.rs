//! Lockfile alignment for `nub pm use` — step 3 of the identity-setting verb
//! (spec: wiki/commands/pm/identity-policy.md §`nub pm use`). The from-state
//! is the lockfile(s) on disk (artifacts carry resolution state); the
//! to-state is the target PM's format. Planning is pure — no writes, no
//! network — so `use` can refuse BEFORE touching the manifest or registry;
//! execution converts through the engine's conformance-gated writers
//! (aube-lockfile parse → `write_lockfile_as`), preserving resolution state
//! rather than delete-and-regenerating.
//!
//! The yarn asterisk: the engine's yarn.lock *write* fidelity is unproven
//! (the same write-tier gate as `install_family`), so a `use yarn` that
//! would require a conversion is refused outright — a pin-only half-switch
//! would leave the declaration and the artifacts disagreeing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use aube_lockfile::LockfileKind;

/// The known lockfile artifacts, in the engine's candidate precedence order
/// *within* each family (npm-shrinkwrap.json outranks package-lock.json as a
/// conversion source, matching npm and `aube_lockfile::lockfile_candidates`).
/// `aube-lock.yaml` is deliberately absent: it is another tool's artifact,
/// not part of nub's identity model (nub never writes it and `use` neither
/// keeps, converts, nor removes it).
const LOCKFILES: &[(&str, &str)] = &[
    ("pnpm-lock.yaml", "pnpm"),
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
    ("yarn.lock", "yarn"),
    ("npm-shrinkwrap.json", "npm"),
    ("package-lock.json", "npm"),
];

/// The primary on-disk filename for a target PM — what a conversion writes
/// and what the summary names for the fresh case.
pub(crate) fn lockfile_name(target: &str) -> &'static str {
    match target {
        "npm" => "package-lock.json",
        "pnpm" => "pnpm-lock.yaml",
        "yarn" => "yarn.lock",
        "bun" => "bun.lock",
        other => unreachable!("use targets are npm/pnpm/yarn/bun, got {other}"),
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
    /// A single other-format lockfile: converted to the target's format via
    /// the gated writers, then the source file(s) removed (migrated, not
    /// abandoned — leaving them would recreate a multi-lockfile ambiguity).
    Convert {
        from: PathBuf,
        from_kind: LockfileKind,
        remove: Vec<PathBuf>,
    },
}

/// Decide the alignment for `root` → `target` (one of npm/pnpm/yarn/bun).
/// Errors are the spec's refusals, raised before any write:
///
/// - multiple foreign-format lockfiles with the target's absent — nub can't
///   infer which one carries the real resolution state;
/// - `use yarn` needing a conversion — the yarn write gate;
/// - a classic-yarn target over a Berry yarn.lock — same filename, but the
///   formats are incompatible, so it is a conversion, and therefore gated;
/// - a binary `bun.lockb` as the only conversion source — the engine can't
///   read it (text `bun.lock` is the supported format).
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
        // Same filename, different format: a Berry yarn.lock under a classic
        // `use yarn` is a conversion in disguise — refuse via the gate.
        if target == "yarn" && aube_lockfile::yarn::is_berry_path(&kept) {
            bail!(
                "this project's yarn.lock is yarn Berry (2+) format — `nub pm use yarn` \
                 pins classic yarn, and converting a Berry yarn.lock would rewrite \
                 yarn.lock, which nub refuses (yarn.lock write fidelity is unproven \
                 in the embedded engine). Use `yarn set version` to manage Berry, or \
                 pick another manager: nub pm use pnpm | npm | bun."
            );
        }
        return Ok(AlignPlan::Keep { kept, remove });
    }

    // Target's format absent: exactly one foreign family converts; more is
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

    if target == "yarn" {
        bail!(
            "`nub pm use yarn` would need to convert {} into yarn.lock, and nub \
             refuses to write yarn.lock (write fidelity is unproven in the embedded \
             engine). Generate it with yarn itself (`yarn install`), then rerun \
             `nub pm use yarn` — with yarn.lock in place, only the declaration is written.",
            foreign[0]
                .0
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
    }

    let (from, _) = foreign
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
    let from_kind = source_kind(&from);
    Ok(AlignPlan::Convert {
        from,
        from_kind,
        remove,
    })
}

/// The [`LockfileKind`] of a conversion source file (content-refined for
/// yarn.lock, mirroring the engine's `refine_yarn_kind`).
fn source_kind(path: &Path) -> LockfileKind {
    match path.file_name().and_then(|n| n.to_str()) {
        Some("pnpm-lock.yaml") => LockfileKind::Pnpm,
        Some("bun.lock") => LockfileKind::Bun,
        Some("npm-shrinkwrap.json") => LockfileKind::NpmShrinkwrap,
        Some("package-lock.json") => LockfileKind::Npm,
        Some("yarn.lock") if aube_lockfile::yarn::is_berry_path(path) => LockfileKind::YarnBerry,
        Some("yarn.lock") => LockfileKind::Yarn,
        other => unreachable!("not a planned lockfile source: {other:?}"),
    }
}

/// The target's write format. yarn never reaches here ([`plan_alignment`]
/// refuses every converting `use yarn`).
fn target_kind(target: &str) -> LockfileKind {
    match target {
        "npm" => LockfileKind::Npm,
        "pnpm" => LockfileKind::Pnpm,
        "bun" => LockfileKind::Bun,
        other => unreachable!("conversion targets are npm/pnpm/bun, got {other}"),
    }
}

/// Execute a [`AlignPlan::Convert`]: parse the source lockfile's graph and
/// write it in the target's format through the engine's gated writers.
/// Returns the path written. The caller removes the source file(s) only
/// after this succeeds — a failed conversion must leave the project intact.
///
/// The brand preflight must already be registered ([`super::engine_session`]
/// or [`super::engine_brand_preflight`]): the write path reads workspace
/// config transitively (branch-lockfile naming), and the toggled getters
/// freeze on first read.
pub(crate) fn convert_lockfile(
    root: &Path,
    from: &Path,
    from_kind: LockfileKind,
    target: &str,
) -> Result<PathBuf> {
    let manifest = aube_manifest::PackageJson::from_path(&root.join("package.json"))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("reading package.json for the lockfile conversion")?;
    let graph = match from_kind {
        LockfileKind::Pnpm | LockfileKind::Aube => aube_lockfile::pnpm::parse(from),
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => aube_lockfile::npm::parse(from),
        LockfileKind::Yarn | LockfileKind::YarnBerry => aube_lockfile::yarn::parse(from, &manifest),
        LockfileKind::Bun => aube_lockfile::bun::parse(from),
    }
    .map_err(|e| anyhow::anyhow!("{}", super::present::rewrite(&e.to_string())))
    .with_context(|| format!("parsing {}", from.display()))?;
    let written = aube_lockfile::write_lockfile_as(root, &graph, &manifest, target_kind(target))
        .map_err(|e| anyhow::anyhow!("{}", super::present::rewrite(&e.to_string())))
        .with_context(|| format!("writing {}", lockfile_name(target)))?;
    Ok(written)
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
    fn plan_covers_the_spec_rows_keep_convert_fresh_and_authoritative_removal() {
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

        // single other format → Convert + the source removed.
        let dir = root("conv", &[("package-lock.json", "{}")]);
        assert_eq!(
            plan_alignment(&dir, "pnpm").unwrap(),
            AlignPlan::Convert {
                from: dir.join("package-lock.json"),
                from_kind: LockfileKind::Npm,
                remove: vec![dir.join("package-lock.json")]
            }
        );

        // multiple with the target's present → target authoritative, others
        // removed (no conversion).
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

    #[test]
    fn yarn_gate_refuses_conversions_in_both_directions_of_the_same_filename() {
        // use yarn over a foreign lockfile → refused (would write yarn.lock).
        let dir = root(
            "yarn-conv",
            &[("pnpm-lock.yaml", "lockfileVersion: '9.0'\n")],
        );
        let err = plan_alignment(&dir, "yarn").unwrap_err().to_string();
        assert!(
            err.contains("refuses to write yarn.lock"),
            "converting to yarn must hit the write gate, got: {err}"
        );

        // use yarn with a CLASSIC yarn.lock in place → fine (declaration-only).
        let dir = root("yarn-keep", &[("yarn.lock", "# yarn lockfile v1\n")]);
        assert!(matches!(
            plan_alignment(&dir, "yarn").unwrap(),
            AlignPlan::Keep { .. }
        ));

        // use yarn over a BERRY yarn.lock → same filename, incompatible format:
        // a conversion in disguise, refused via the gate.
        let dir = root(
            "yarn-berry",
            &[("yarn.lock", "__metadata:\n  version: 8\n")],
        );
        let err = plan_alignment(&dir, "yarn").unwrap_err().to_string();
        assert!(
            err.contains("Berry"),
            "a Berry yarn.lock under classic `use yarn` must refuse, got: {err}"
        );

        // …while converting AWAY from yarn (a read) is allowed.
        let dir = root("yarn-away", &[("yarn.lock", "# yarn lockfile v1\n")]);
        assert!(matches!(
            plan_alignment(&dir, "pnpm").unwrap(),
            AlignPlan::Convert {
                from_kind: LockfileKind::Yarn,
                ..
            }
        ));
    }

    #[test]
    fn binary_bun_lockb_is_named_as_unconvertible_but_kept_under_use_bun() {
        // The engine reads only the text bun.lock; a binary-only project can't
        // be a conversion source.
        let dir = root("lockb", &[("bun.lockb", "\0\0binary")]);
        let err = plan_alignment(&dir, "pnpm").unwrap_err().to_string();
        assert!(
            err.contains("bun.lockb") && err.contains("--save-text-lockfile"),
            "the binary-lockfile refusal must carry the bun remedy, got: {err}"
        );
        // …but `use bun` keeps it: it IS bun's artifact.
        assert!(matches!(
            plan_alignment(&dir, "bun").unwrap(),
            AlignPlan::Keep { .. }
        ));
    }

    #[test]
    fn shrinkwrap_outranks_package_lock_as_the_npm_conversion_source() {
        // Both npm artifacts present: shrinkwrap is the source npm itself
        // honors first; both are removed after the migration.
        let dir = root(
            "shrinkwrap",
            &[("npm-shrinkwrap.json", "{}"), ("package-lock.json", "{}")],
        );
        match plan_alignment(&dir, "pnpm").unwrap() {
            AlignPlan::Convert {
                from,
                from_kind,
                mut remove,
            } => {
                assert_eq!(from, dir.join("npm-shrinkwrap.json"));
                assert_eq!(from_kind, LockfileKind::NpmShrinkwrap);
                remove.sort();
                assert_eq!(
                    remove,
                    vec![
                        dir.join("npm-shrinkwrap.json"),
                        dir.join("package-lock.json")
                    ]
                );
            }
            other => panic!("expected Convert, got {other:?}"),
        }
    }
}
