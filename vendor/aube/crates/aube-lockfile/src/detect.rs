//! Declaration-aware lockfile-kind resolution.
//!
//! [`crate::detect_existing_lockfile_kind`] is the raw on-disk
//! primitive: pure filename precedence, no knowledge of what the
//! project *says* its package manager is. This module layers the
//! `package.json` declaration on top of it, because a declaration is
//! a stronger signal than file precedence: a pnpm project that picked
//! up a stray `package-lock.json` (a contributor ran `npm install`,
//! a scaffolder dropped one in) should keep getting `pnpm-lock.yaml`,
//! not silently flip formats because of where the stray file sorts in
//! the precedence list.
//!
//! The decision table implemented by [`resolve_project_lockfile_kind`]:
//!
//! | declaration | lockfiles on disk                  | result                                  |
//! | ----------- | ---------------------------------- | --------------------------------------- |
//! | X           | X's (other strays ignored)         | `Existing(X's kind)`                    |
//! | X           | none                               | `DeclaredFresh(X's kind)`               |
//! | X           | only other PMs'                    | `Err(DeclarationMismatch)`              |
//! | none        | exactly one PM's                   | `Existing(<that kind>)` (precedence)    |
//! | none        | none                               | `Fresh`                                 |
//! | none        | two or more PMs'                   | `Err(AmbiguousLockfiles)`               |
//!
//! Two carve-outs keep aube's own canonical flow intact:
//!
//! - `aube-lock.yaml` (or its branch variant) always wins, before any
//!   declaration is consulted. It is aube's own file — its presence is
//!   an unambiguous statement that aube manages the project, and the
//!   normal post-`aube import` state (`aube-lock.yaml` next to the
//!   original foreign lockfile) must keep resolving to it.
//! - A declaration naming `aube` itself (what `aube init` writes)
//!   accepts every format aube can preserve, so it never contradicts
//!   and never makes a multi-lockfile project ambiguous: aube's
//!   documented contract is "preserve whatever lockfile the project
//!   already uses".
//!
//! Both carve-outs are embedder-parameterized (the default [`AUBE`] profile
//! preserves the behavior above): [`Embedder::self_names`] is the set of
//! declared names that count as the running tool, and
//! [`Embedder::canonical_lockfile_always_wins`] lets a strict-identity
//! embedder demote the first carve-out so a canonical lockfile beside a
//! foreign one resolves through the ordinary ambiguity/contradiction
//! rules instead of silently winning.
//!
//! [`AUBE`]: aube_util::AUBE
//! [`Embedder::self_names`]: aube_util::Embedder::self_names
//! [`Embedder::canonical_lockfile_always_wins`]: aube_util::Embedder::canonical_lockfile_always_wins

use crate::io::{Error, LockfileKind, lockfile_candidates, refine_yarn_kind};
use std::path::Path;

/// Whether `name` is one of the active embedder's self-names — the names whose
/// declaration accepts every preservable lockfile format and pins the canonical
/// format for fresh projects. Standalone aube's profile is `["aube"]`; an
/// embedder shipping aube's command layer under its own name lists that name in
/// [`Embedder::self_names`](aube_util::Embedder::self_names) so
/// `"packageManager": "<embedder>@1.0.0"` resolves exactly like a declared
/// `aube` does upstream, instead of falling through as an unknown foreign tool.
fn is_self_name(name: &str) -> bool {
    aube_util::embedder().self_names.contains(&name)
}

/// Whether the canonical lockfile's presence short-circuits detection even
/// when other tools' lockfiles sit beside it (the upstream default, `true` —
/// the normal post-`aube import` state is `aube-lock.yaml` next to the
/// original foreign lockfile, and it must keep resolving). An embedder with
/// a strict identity model sets
/// [`Embedder::canonical_lockfile_always_wins`](aube_util::Embedder::canonical_lockfile_always_wins)
/// to `false`: the canonical kind then participates in the ordinary
/// declaration/ambiguity rules, so a canonical lockfile beside a foreign one
/// becomes the loud [`Error::AmbiguousLockfiles`] / [`Error::DeclarationMismatch`]
/// instead of a silent win.
fn canonical_lockfile_always_wins() -> bool {
    aube_util::embedder().canonical_lockfile_always_wins
}

/// Which `package.json` field declared the package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationSource {
    /// The corepack-style `"packageManager": "pnpm@10.0.0"` field.
    PackageManagerField,
    /// `"devEngines": { "packageManager": ... }` — either the object
    /// form (`{ "name": "pnpm", ... }`) or the array form
    /// (`[{ "name": "pnpm" }, ...]`).
    DevEngines,
}

impl DeclarationSource {
    /// The `package.json` field path, for error messages.
    pub fn field(self) -> &'static str {
        match self {
            DeclarationSource::PackageManagerField => "packageManager",
            DeclarationSource::DevEngines => "devEngines.packageManager",
        }
    }
}

/// A package manager named by the project's `package.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredPackageManager {
    /// The bare tool name (`pnpm`, `npm`, `yarn`, `bun`, `aube`, ...),
    /// version stripped.
    pub name: String,
    pub source: DeclarationSource,
}

/// Read the package manager declared by `<project_dir>/package.json`,
/// if any. `packageManager` outranks `devEngines.packageManager`;
/// within `devEngines`, both the object and the array form are
/// understood. An array that names more than one distinct tool is no
/// pin at all ("any of these is acceptable"), so it resolves to
/// `None`. Unreadable or malformed `package.json` also resolves to
/// `None` — manifest parse errors are surfaced by the manifest loader,
/// not duplicated here.
pub fn declared_package_manager(project_dir: &Path) -> Option<DeclaredPackageManager> {
    let raw = std::fs::read_to_string(project_dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if let Some(spec) = json.get("packageManager").and_then(|v| v.as_str()) {
        // `name@version` (optionally `+sha512.…`); a bare name without
        // a version is tolerated since only the name matters here.
        let name = spec.split('@').next().unwrap_or_default();
        if !name.is_empty() {
            return Some(DeclaredPackageManager {
                name: name.to_string(),
                source: DeclarationSource::PackageManagerField,
            });
        }
    }
    let dev_engines_pm = json.get("devEngines")?.get("packageManager")?;
    let names: Vec<&str> = match dev_engines_pm {
        serde_json::Value::Object(obj) => obj
            .get("name")
            .and_then(|v| v.as_str())
            .into_iter()
            .collect(),
        serde_json::Value::Array(entries) => entries
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()))
            .collect(),
        _ => Vec::new(),
    };
    let first = *names.first()?;
    if names.iter().any(|n| *n != first) {
        // Several acceptable tools declared — no single pin to honor.
        return None;
    }
    Some(DeclaredPackageManager {
        name: first.to_string(),
        source: DeclarationSource::DevEngines,
    })
}

/// The lockfile format a declared package manager's fresh write
/// targets (and whose filename the contradiction error names). `None`
/// for names that don't map onto a format aube knows — foreign tools
/// are the `packageManagerStrict` guard's concern, not detection's.
/// `aube` is deliberately absent: the self-name accepts every format
/// and is special-cased in [`resolve_project_lockfile_kind`]. Whether
/// an *existing* file matches a declaration goes through [`family`]
/// instead, so npm's shrinkwrap and a berry `yarn.lock` count for
/// their tools.
fn declared_lockfile_kind(name: &str) -> Option<LockfileKind> {
    match name {
        "npm" => Some(LockfileKind::Npm),
        "pnpm" => Some(LockfileKind::Pnpm),
        "yarn" => Some(LockfileKind::Yarn),
        "bun" => Some(LockfileKind::Bun),
        _ => None,
    }
}

/// The PM family a lockfile kind belongs to, for the ambiguity check.
/// Kinds owned by the same tool (npm's shrinkwrap + package-lock,
/// yarn classic + berry) never make a project ambiguous together.
fn family(kind: LockfileKind) -> &'static str {
    match kind {
        LockfileKind::Aube => "aube",
        LockfileKind::Pnpm => "pnpm",
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => "npm",
        LockfileKind::Yarn | LockfileKind::YarnBerry => "yarn",
        LockfileKind::Bun => "bun",
    }
}

/// Result of [`resolve_project_lockfile_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedLockfileKind {
    /// A lockfile exists on disk and the declaration policy picked it.
    Existing(LockfileKind),
    /// No lockfile on disk, but `package.json` declares a package
    /// manager — a fresh write should use that tool's format rather
    /// than the configured default.
    DeclaredFresh(LockfileKind),
    /// No lockfile and no declaration; the caller falls back to its
    /// configured default format.
    Fresh,
}

impl ResolvedLockfileKind {
    /// Collapse to the kind a write should target, if the policy
    /// produced one. `None` only for [`ResolvedLockfileKind::Fresh`].
    pub fn kind(self) -> Option<LockfileKind> {
        match self {
            ResolvedLockfileKind::Existing(k) | ResolvedLockfileKind::DeclaredFresh(k) => Some(k),
            ResolvedLockfileKind::Fresh => None,
        }
    }
}

/// Declaration-aware replacement for
/// [`crate::detect_existing_lockfile_kind`] on every path that decides
/// which lockfile format to resolve against or write. See the module
/// docs for the full decision table. Passive consumers that only
/// report what's on disk (doctor, freshness hashing) keep using the
/// raw primitive.
pub fn resolve_project_lockfile_kind(project_dir: &Path) -> Result<ResolvedLockfileKind, Error> {
    let existing: Vec<(std::path::PathBuf, LockfileKind)> =
        lockfile_candidates(project_dir, /*include_aube=*/ true)
            .into_iter()
            .filter(|(path, _)| path.exists())
            .collect();
    // aube's own lockfile always wins — see module docs. An embedder with a
    // strict identity model opts out (Embedder::canonical_lockfile_always_wins
    // = false), letting the canonical kind fall through to the ordinary
    // declaration/ambiguity rules below.
    if canonical_lockfile_always_wins() && existing.iter().any(|(_, k)| *k == LockfileKind::Aube) {
        return Ok(ResolvedLockfileKind::Existing(LockfileKind::Aube));
    }
    let declared = declared_package_manager(project_dir);
    match declared.as_ref().filter(|d| !is_self_name(&d.name)) {
        Some(decl) => match declared_lockfile_kind(&decl.name) {
            Some(declared_kind) => {
                // Match on family, not exact kind: declared `npm` is
                // satisfied by a shrinkwrap too, and the candidate
                // order already encodes npm's own shrinkwrap-first
                // precedence.
                if let Some((path, k)) = existing.iter().find(|(_, k)| family(*k) == decl.name) {
                    return Ok(ResolvedLockfileKind::Existing(refine_yarn_kind(path, *k)));
                }
                if existing.is_empty() {
                    return Ok(ResolvedLockfileKind::DeclaredFresh(declared_kind));
                }
                Err(Error::DeclarationMismatch {
                    declared: decl.name.clone(),
                    field: decl.source.field(),
                    expected: declared_kind.filename(),
                    found: join_filenames(&existing),
                })
            }
            // Foreign tool name (vlt, deno, …): no lockfile format to
            // pin, fall through to the undeclared rules.
            None => resolve_undeclared(&existing),
        },
        // Undeclared, or declared `aube` itself (accepts any format).
        None => match resolve_undeclared(&existing) {
            Ok(ResolvedLockfileKind::Fresh) if declared.is_some() => {
                Ok(ResolvedLockfileKind::DeclaredFresh(LockfileKind::Aube))
            }
            // Declared `aube` + several foreign lockfiles is still
            // ambiguous: aube preserves the existing format, and with
            // two candidates there's no fact of the matter which one.
            other => other,
        },
    }
}

/// Today's behavior, minus silent precedence among different tools'
/// lockfiles: one family resolves by precedence, several is an error.
fn resolve_undeclared(
    existing: &[(std::path::PathBuf, LockfileKind)],
) -> Result<ResolvedLockfileKind, Error> {
    let Some((first_path, first_kind)) = existing.first() else {
        return Ok(ResolvedLockfileKind::Fresh);
    };
    if existing
        .iter()
        .any(|(_, k)| family(*k) != family(*first_kind))
    {
        return Err(Error::AmbiguousLockfiles {
            found: join_filenames(existing),
        });
    }
    Ok(ResolvedLockfileKind::Existing(refine_yarn_kind(
        first_path,
        *first_kind,
    )))
}

fn join_filenames(existing: &[(std::path::PathBuf, LockfileKind)]) -> String {
    existing
        .iter()
        .filter_map(|(path, _)| path.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) {
        std::fs::write(dir.path().join(name), body).unwrap();
    }

    fn manifest_with(dir: &tempfile::TempDir, fields: &str) {
        write(dir, "package.json", &format!(r#"{{"name":"t"{fields}}}"#));
    }

    fn resolve(dir: &tempfile::TempDir) -> Result<ResolvedLockfileKind, Error> {
        resolve_project_lockfile_kind(dir.path())
    }

    // ── declaration parsing ──────────────────────────────────────────

    #[test]
    fn package_manager_field_outranks_dev_engines() {
        let d = dir();
        manifest_with(
            &d,
            r#","packageManager":"pnpm@10.0.0","devEngines":{"packageManager":{"name":"npm"}}"#,
        );
        let decl = declared_package_manager(d.path()).unwrap();
        assert_eq!(decl.name, "pnpm");
        assert_eq!(decl.source, DeclarationSource::PackageManagerField);
    }

    #[test]
    fn dev_engines_object_form_declares() {
        let d = dir();
        manifest_with(
            &d,
            r#","devEngines":{"packageManager":{"name":"bun","onFail":"error"}}"#,
        );
        let decl = declared_package_manager(d.path()).unwrap();
        assert_eq!(decl.name, "bun");
        assert_eq!(decl.source, DeclarationSource::DevEngines);
    }

    #[test]
    fn dev_engines_array_form_declares_when_names_agree() {
        let d = dir();
        manifest_with(
            &d,
            r#","devEngines":{"packageManager":[{"name":"npm","version":"10"},{"name":"npm","version":"11"}]}"#,
        );
        assert_eq!(declared_package_manager(d.path()).unwrap().name, "npm");
    }

    #[test]
    fn dev_engines_array_with_distinct_tools_is_no_pin() {
        let d = dir();
        manifest_with(
            &d,
            r#","devEngines":{"packageManager":[{"name":"npm"},{"name":"pnpm"}]}"#,
        );
        assert_eq!(declared_package_manager(d.path()), None);
    }

    // ── decision table ───────────────────────────────────────────────

    #[test]
    fn declared_pm_with_matching_lockfile_resolves_to_it() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"pnpm@10.0.0""#);
        write(&d, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Existing(LockfileKind::Pnpm)
        );
    }

    #[test]
    fn declared_pm_outranks_a_stray_foreign_lockfile() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"pnpm@10.0.0""#);
        write(&d, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
        write(&d, "package-lock.json", "{}");
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Existing(LockfileKind::Pnpm),
            "stray package-lock.json must not flip a declared-pnpm project"
        );
    }

    #[test]
    fn declared_pm_without_lockfiles_pins_the_fresh_format() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"npm@11.0.0""#);
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::DeclaredFresh(LockfileKind::Npm)
        );
    }

    #[test]
    fn declared_pm_with_only_foreign_lockfiles_is_a_contradiction() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"pnpm@10.0.0""#);
        write(&d, "package-lock.json", "{}");
        let err = resolve(&d).unwrap_err();
        let Error::DeclarationMismatch {
            declared,
            expected,
            found,
            ..
        } = &err
        else {
            panic!("expected DeclarationMismatch, got {err:?}");
        };
        assert_eq!(declared, "pnpm");
        assert_eq!(*expected, "pnpm-lock.yaml");
        assert_eq!(found, "package-lock.json");
    }

    #[test]
    fn undeclared_single_lockfile_keeps_todays_behavior() {
        let d = dir();
        manifest_with(&d, "");
        write(&d, "bun.lock", "{}");
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Existing(LockfileKind::Bun)
        );
    }

    #[test]
    fn undeclared_no_lockfile_keeps_todays_behavior() {
        let d = dir();
        manifest_with(&d, "");
        assert_eq!(resolve(&d).unwrap(), ResolvedLockfileKind::Fresh);
    }

    #[test]
    fn undeclared_multiple_lockfiles_is_ambiguous() {
        let d = dir();
        manifest_with(&d, "");
        write(&d, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
        write(&d, "yarn.lock", "# yarn lockfile v1\n");
        let err = resolve(&d).unwrap_err();
        let Error::AmbiguousLockfiles { found } = &err else {
            panic!("expected AmbiguousLockfiles, got {err:?}");
        };
        assert_eq!(found, "pnpm-lock.yaml, yarn.lock");
    }

    // ── carve-outs and refinements ───────────────────────────────────

    #[test]
    fn aube_lock_wins_over_declaration_and_strays() {
        // The normal post-`aube import` state: aube-lock.yaml next to
        // the original foreign lockfile, manifest still declaring pnpm.
        let d = dir();
        manifest_with(&d, r#","packageManager":"pnpm@10.0.0""#);
        write(&d, "aube-lock.yaml", "lockfileVersion: '9.0'\n");
        write(&d, "package-lock.json", "{}");
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Existing(LockfileKind::Aube)
        );
    }

    #[test]
    fn declared_aube_preserves_any_existing_format() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"aube@1.0.0""#);
        write(&d, "package-lock.json", "{}");
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Existing(LockfileKind::Npm)
        );
    }

    #[test]
    fn declared_aube_fresh_project_pins_aube_format() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"aube@1.0.0""#);
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::DeclaredFresh(LockfileKind::Aube)
        );
    }

    #[test]
    fn unknown_declared_tool_falls_back_to_undeclared_rules() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"vlt@1.0.0""#);
        write(&d, "yarn.lock", "# yarn lockfile v1\n");
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Existing(LockfileKind::Yarn)
        );
        std::fs::remove_file(d.path().join("yarn.lock")).unwrap();
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Fresh,
            "an unknown tool name must not pin a fresh format"
        );
    }

    #[test]
    fn declared_npm_accepts_shrinkwrap_with_npm_precedence() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"npm@11.0.0""#);
        write(&d, "npm-shrinkwrap.json", "{}");
        write(&d, "package-lock.json", "{}");
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Existing(LockfileKind::NpmShrinkwrap)
        );
    }

    #[test]
    fn declared_yarn_refines_to_berry_by_content() {
        let d = dir();
        manifest_with(&d, r#","packageManager":"yarn@4.0.0""#);
        write(&d, "yarn.lock", "__metadata:\n  version: 8\n");
        assert_eq!(
            resolve(&d).unwrap(),
            ResolvedLockfileKind::Existing(LockfileKind::YarnBerry)
        );
    }
}
