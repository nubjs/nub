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
/// conversion source, matching npm and `aube_lockfile::lockfile_candidates`).
/// `lock.yaml` is nub's own artifact (the `nub` family). `aube-lock.yaml` is
/// deliberately absent: it is another tool's artifact, not part of nub's
/// identity model (nub never writes it and `use` neither keeps, converts,
/// nor removes it).
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

/// The primary on-disk filename for a target PM — what a conversion writes
/// and what the summary names for the fresh case.
pub(crate) fn lockfile_name(target: &str) -> &'static str {
    match target {
        "npm" => "package-lock.json",
        "pnpm" => "pnpm-lock.yaml",
        "yarn" => "yarn.lock",
        "bun" => "bun.lock",
        "nub" => NUB_LOCKFILE,
        other => unreachable!("use targets are npm/pnpm/yarn/bun/nub, got {other}"),
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
    /// The pnpm ↔ nub pair: same bytes (`nub.lock` IS pnpm-v9 format under
    /// a generic name), different filename — a rename, never a parse/rewrite,
    /// so the file stays byte-identical and the real PM's `--frozen-lockfile`
    /// acceptance is preserved exactly.
    Rename { from: PathBuf, remove: Vec<PathBuf> },
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
        // `use yarn` would silently downgrade the project's lockfile from
        // Berry to v1 — a format change the user didn't ask for, not the
        // declaration-only switch `use` promises. Refuse and point at the
        // Berry-native path. (Classic-yarn *write* fidelity is proven — see
        // the conformance yarn leg — so this is a format-preservation refusal,
        // not a write-fidelity one.)
        if target == "yarn" && aube_lockfile::yarn::is_berry_path(&kept) {
            bail!(
                "this project's yarn.lock is yarn Berry (2+) format — `nub pm use yarn` \
                 pins classic yarn, and converting a Berry yarn.lock to v1 would \
                 silently downgrade the lockfile format. Use `yarn set version` to \
                 manage Berry, or pick another manager: nub pm use pnpm | npm | bun."
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

    // `nub pm use yarn` pins *classic* (v1) yarn and writes a classic
    // yarn.lock. The classic writer is proven against real yarn: a yarn.lock
    // nub writes from any source (pnpm/npm/bun) is accepted by yarn 1.13 and
    // 1.22 under `--frozen-lockfile` with zero churn and a correct
    // node_modules (tests/conformance Direction-B yarn leg). yarn v1's frozen
    // check validates that the manifest is satisfiable by the lockfile, not
    // byte-identity, so the writer's lossy bits (no `resolved` URL, resolved
    // versions in place of declared ranges) are tolerated. So a converting
    // `use yarn` is allowed for classic. (The berry case never reaches here:
    // an existing berry yarn.lock is the `Keep`-with-classic-conflict refusal
    // above — `use yarn` does not convert *to* berry.)
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
    let from_kind = source_kind(&from);
    Ok(AlignPlan::Convert {
        from,
        from_kind,
        remove,
    })
}

/// The name of the first dependency in `graph` declared with the
/// `workspace:` protocol (`workspace:*`, `workspace:^`, …), if any. yarn v1
/// cannot express this protocol, so a `use yarn` conversion of a graph that
/// carries it must refuse rather than write a lockfile yarn rejects. The
/// `specifier` field is populated by the pnpm-v9 and bun readers (the
/// formats that actually use the protocol); npm/yarn lockfiles never carry
/// it, so this is `None` for them by construction.
fn workspace_protocol_consumer(graph: &aube_lockfile::LockfileGraph) -> Option<String> {
    graph
        .importers
        .values()
        .flatten()
        .find(|dep| {
            dep.specifier
                .as_deref()
                .is_some_and(|s| s.starts_with("workspace:"))
        })
        .map(|dep| format!("{}@{}", dep.name, dep.specifier.as_deref().unwrap_or("")))
}

/// Pure pre-write refusal: reject a `use <target>` whose planned action
/// would convert a source lockfile into a target format that cannot
/// faithfully represent it. Called BEFORE the manifest is touched (the
/// spec's "refuse before writing" contract — a half-switch that pins yarn
/// in package.json but writes no lockfile is exactly what we avoid).
///
/// Today the only such case is `use yarn` over a graph that uses the
/// `workspace:` protocol: yarn v1 has no `workspace:` support and
/// hard-rejects a lockfile that needs it ("Couldn't find any versions for
/// <pkg> that matches workspace:*"). Parsing the source here is one cheap
/// read-and-parse that only runs for a converting `use yarn`; every other
/// plan is a no-op. Berry, npm, pnpm, and bun all round-trip the protocol,
/// so the refusal is classic-yarn-specific.
pub(crate) fn refuse_unconvertible(root: &Path, target: &str, plan: &AlignPlan) -> Result<()> {
    if target != "yarn" {
        return Ok(());
    }
    let AlignPlan::Convert {
        from, from_kind, ..
    } = plan
    else {
        return Ok(());
    };
    // Only pnpm/bun lockfiles carry `workspace:` specifiers; skip the parse
    // for npm sources (npm has no workspace protocol) — but parsing any of
    // them is cheap and the importer scan is the authoritative check.
    let manifest = aube_manifest::PackageJson::from_path(&root.join("package.json"))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("reading package.json for the yarn conversion preflight")?;
    let graph = parse_source_graph(from, *from_kind, &manifest)?;
    if let Some(pkg) = workspace_protocol_consumer(&graph) {
        bail!(
            "yarn v1 does not support the `workspace:` protocol, but this project \
             uses it (e.g. `{pkg}`). Converting to a yarn.lock would produce a \
             lockfile yarn rejects (\"Couldn't find any versions … that matches \
             workspace:*\"). Replace the `workspace:` specifiers with a version \
             range or `*` in the consuming package.json files before \
             `nub pm use yarn`, or pick another manager: nub pm use pnpm | npm | bun."
        );
    }
    Ok(())
}

/// The [`LockfileKind`] of a conversion source file (content-refined for
/// yarn.lock, mirroring the engine's `refine_yarn_kind`).
fn source_kind(path: &Path) -> LockfileKind {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(NUB_LOCKFILE) | Some(NUB_LEGACY_LOCKFILE) => LockfileKind::Aube,
        Some("pnpm-lock.yaml") => LockfileKind::Pnpm,
        Some("bun.lock") => LockfileKind::Bun,
        Some("npm-shrinkwrap.json") => LockfileKind::NpmShrinkwrap,
        Some("package-lock.json") => LockfileKind::Npm,
        Some("yarn.lock") if aube_lockfile::yarn::is_berry_path(path) => LockfileKind::YarnBerry,
        Some("yarn.lock") => LockfileKind::Yarn,
        other => unreachable!("not a planned lockfile source: {other:?}"),
    }
}

/// The target's write format. `yarn` maps to *classic* (v1) yarn.lock — the
/// proven, frozen-accepted writer ([`plan_alignment`] only ever converts *to*
/// classic; an existing berry yarn.lock is the Keep-conflict refusal). `nub`
/// maps to the engine's canonical-lockfile slot, whose filename the brand
/// preflight registers as [`NUB_LOCKFILE`] (pnpm-v9 bytes either way).
fn target_kind(target: &str) -> LockfileKind {
    match target {
        "npm" => LockfileKind::Npm,
        "pnpm" => LockfileKind::Pnpm,
        "bun" => LockfileKind::Bun,
        "nub" => LockfileKind::Aube,
        // `use yarn` pins and writes *classic* (v1) yarn.lock — the proven,
        // frozen-accepted writer. Conversion never targets berry.
        "yarn" => LockfileKind::Yarn,
        other => unreachable!("conversion targets are npm/pnpm/bun/nub/yarn, got {other}"),
    }
}

/// Parse a source lockfile into its resolution graph, dispatching by kind.
/// Errors are brand-rewritten and tagged with the file being parsed — shared
/// by the conversion preflight ([`refuse_unconvertible`]) and the conversion
/// itself ([`convert_lockfile`]).
fn parse_source_graph(
    from: &Path,
    from_kind: LockfileKind,
    manifest: &aube_manifest::PackageJson,
) -> Result<aube_lockfile::LockfileGraph> {
    match from_kind {
        LockfileKind::Pnpm | LockfileKind::Aube => aube_lockfile::pnpm::parse(from),
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => {
            aube_lockfile::npm::parse(from, manifest)
        }
        LockfileKind::Yarn | LockfileKind::YarnBerry => aube_lockfile::yarn::parse(from, manifest),
        LockfileKind::Bun => aube_lockfile::bun::parse(from),
    }
    .map_err(|e| anyhow::anyhow!("{}", super::present::rewrite(&e.to_string())))
    .with_context(|| format!("parsing {}", from.display()))
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
    let graph = parse_source_graph(from, from_kind, &manifest)?;
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
    fn use_yarn_converts_to_classic_but_refuses_a_berry_target() {
        // use yarn over a foreign lockfile → Convert to *classic* yarn.lock.
        // The classic writer is proven frozen-accepted by real yarn (the gate
        // that used to refuse this is lifted — see the conformance yarn leg).
        let dir = root(
            "yarn-conv",
            &[("pnpm-lock.yaml", "lockfileVersion: '9.0'\n")],
        );
        assert!(
            matches!(
                plan_alignment(&dir, "yarn").unwrap(),
                AlignPlan::Convert { .. }
            ),
            "converting a foreign lockfile to classic yarn must be allowed"
        );
        assert_eq!(target_kind("yarn"), LockfileKind::Yarn);

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

        // nub.lock → npm is a real format change: converted, not renamed.
        let dir = root("nub-to-npm", &[(NUB_LOCKFILE, "lockfileVersion: '9.0'\n")]);
        assert!(matches!(
            plan_alignment(&dir, "npm").unwrap(),
            AlignPlan::Convert {
                from_kind: LockfileKind::Aube,
                ..
            }
        ));

        // nub.lock + a foreign family with neither being the target:
        // ambiguity, loud, naming both files.
        let dir = root(
            "nub-ambig",
            &[
                (NUB_LOCKFILE, "lockfileVersion: '9.0'\n"),
                ("package-lock.json", "{}"),
            ],
        );
        let err = plan_alignment(&dir, "bun").unwrap_err().to_string();
        assert!(
            err.contains(NUB_LOCKFILE) && err.contains("package-lock.json"),
            "ambiguity must name nub.lock and package-lock.json, got: {err}"
        );
    }

    #[test]
    fn convert_lockfile_carries_resolution_state_into_the_target_format() {
        // No network: a real (in-sync) npm v3 lockfile parses into the graph
        // and writes back as pnpm format — version + integrity preserved,
        // never delete-and-regenerate. (End-to-end, real pnpm accepts these
        // conversions with --frozen-lockfile — the conformance harness and
        // the ignored network e2e cover that; this pins the library seam.)
        let dir = root(
            "convert",
            &[
                (
                    "package.json",
                    r#"{"name":"app","version":"1.0.0","dependencies":{"is-positive":"3.1.0"}}"#,
                ),
                (
                    "package-lock.json",
                    r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": { "name": "app", "version": "1.0.0", "dependencies": { "is-positive": "3.1.0" } },
    "node_modules/is-positive": {
      "version": "3.1.0",
      "resolved": "https://registry.npmjs.org/is-positive/-/is-positive-3.1.0.tgz",
      "integrity": "sha512-8ND1j3y9/HP94TOvGzr69/FgbkX2ruOldhLEsTWwcJVfo4oRjwemJmJxt7RJkKYH8tz7vYBP9JcKQY8CLuJ90Q==",
      "engines": { "node": ">=0.10.0" }
    }
  }
}
"#,
                ),
            ],
        );
        let written = convert_lockfile(
            &dir,
            &dir.join("package-lock.json"),
            LockfileKind::Npm,
            "pnpm",
        )
        .unwrap();
        assert_eq!(written, dir.join("pnpm-lock.yaml"));
        let body = std::fs::read_to_string(&written).unwrap();
        assert!(
            body.contains("is-positive@3.1.0") || body.contains("is-positive: 3.1.0"),
            "the resolved version must survive the conversion:\n{body}"
        );
        assert!(
            body.contains("sha512-8ND1j3y9"),
            "the integrity must survive the conversion:\n{body}"
        );
    }

    #[test]
    fn use_yarn_refuses_a_workspace_protocol_graph_before_touching_the_manifest() {
        // A pnpm workspace where one member depends on another via
        // `workspace:*`. yarn v1 cannot express the protocol, so the
        // converting `use yarn` must refuse in the pure preflight — no
        // yarn.lock, manifest untouched.
        let dir = root("ws-proto", &[]);
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"app","version":"1.0.0","private":true}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n\n  packages/a:\n    dependencies:\n      b:\n        specifier: workspace:*\n        version: link:../b\n\n  packages/b: {}\n",
        )
        .unwrap();

        let plan = plan_alignment(&dir, "yarn").unwrap();
        assert!(matches!(plan, AlignPlan::Convert { .. }));
        let err = refuse_unconvertible(&dir, "yarn", &plan)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("workspace:") && err.contains("b@workspace:*"),
            "the refusal must name the protocol and the offending dep, got: {err}"
        );

        // A non-yarn target converts the same graph fine (pnpm round-trips
        // the protocol): no refusal.
        assert!(refuse_unconvertible(&dir, "pnpm", &plan).is_ok());
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
