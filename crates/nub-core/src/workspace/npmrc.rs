//! Which project `.npmrc` files apply to a directory.
//!
//! Inside a workspace, project config belongs to the workspace and to the
//! invocation, not to whichever member a command targets: npm, pnpm, Yarn 1 and
//! Bun all read the workspace root's `.npmrc` (where a monorepo declares its
//! registry, scoped registries, auth and `ignore-scripts`), and none reads a
//! member's file when invoked from elsewhere. pnpm 10 and Yarn 1 also layer the
//! `.npmrc` of the package the command was run from on top; pnpm 11, npm and Bun
//! do not. The engine's own loader applies the same rule (`aube_util::WorkspaceNpmrc`),
//! with its model chosen through [`member_npmrc`], so both sides agree.

use std::path::{Path, PathBuf};

use crate::workspace::detect::detect_project;

/// Whether the invoking package's `.npmrc` layers over the workspace root's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberNpmrc {
    /// The root `.npmrc`, then the invoking package's own on top: pnpm 10 and
    /// earlier (and an undeclared pnpm major), Yarn, nub identity.
    Layered,
    /// The root `.npmrc` alone: pnpm 11+, npm, Bun.
    Ignored,
}

/// The incumbent's [`MemberNpmrc`] model for the project at `dir`, read from the
/// workspace root: the declared `packageManager` / `devEngines.packageManager`
/// first, else the lockfile beside it (a `pnpm-lock.yaml` outranks a foreign one,
/// as in the CLI's config-surface walk). pnpm reversed this at v11 (its loader
/// reads `workspaceDir ?? localPrefix`), so only a declared major ≥ 11 drops the
/// member layer; an unknown major takes the dominant v10 model.
pub fn member_npmrc(dir: &Path) -> MemberNpmrc {
    if let Some((name, version)) = crate::pm::resolve::declared_pm_raw(dir) {
        let major = version
            .as_deref()
            .and_then(|v| v.split('.').next())
            .and_then(|m| m.trim_start_matches('v').parse::<u64>().ok());
        return match (name.as_str(), major) {
            ("npm" | "bun", _) => MemberNpmrc::Ignored,
            ("pnpm", Some(m)) if m >= 11 => MemberNpmrc::Ignored,
            _ => MemberNpmrc::Layered,
        };
    }
    let root = detect_project(dir)
        .map(|p| p.workspace_root.unwrap_or(p.root))
        .unwrap_or_else(|| dir.to_path_buf());
    if root.join("pnpm-lock.yaml").is_file() {
        return MemberNpmrc::Layered;
    }
    let foreign_root_only = [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "bun.lock",
        "bun.lockb",
    ];
    if foreign_root_only.iter().any(|f| root.join(f).is_file()) {
        MemberNpmrc::Ignored
    } else {
        MemberNpmrc::Layered
    }
}

/// The project-scope `.npmrc` paths for `project_root`, LOWEST precedence first.
/// Outside a workspace this is `<project_root>/.npmrc` alone. The invoking
/// package is the nearest `package.json` at or above the process cwd.
pub fn project_npmrc_paths(project_root: &Path) -> Vec<PathBuf> {
    let invocation = std::env::current_dir().ok();
    project_npmrc_paths_from(
        project_root,
        invocation.as_deref(),
        member_npmrc(project_root),
    )
}

fn project_npmrc_paths_from(
    project_root: &Path,
    invocation_dir: Option<&Path>,
    model: MemberNpmrc,
) -> Vec<PathBuf> {
    let Some(root) = detect_project(project_root).and_then(|p| p.workspace_root) else {
        return vec![project_root.join(".npmrc")];
    };
    let invoked_from = match model {
        MemberNpmrc::Ignored => None,
        MemberNpmrc::Layered => invocation_dir.and_then(|dir| package_dir_below_root(dir, &root)),
    };
    let mut paths = vec![root.join(".npmrc")];
    paths.extend(invoked_from.map(|dir| dir.join(".npmrc")));
    paths
}

/// The nearest directory at or above `dir` holding a `package.json`, when it sits
/// strictly inside the workspace `root` (pnpm's "local prefix").
fn package_dir_below_root(dir: &Path, root: &Path) -> Option<PathBuf> {
    let (dir, root) = if dir.starts_with(root) {
        (dir.to_path_buf(), root.to_path_buf())
    } else {
        (dir.canonicalize().ok()?, root.canonicalize().ok()?)
    };
    if !dir.starts_with(&root) {
        return None;
    }
    dir.ancestors()
        .take_while(|d| *d != root)
        .find(|d| d.join("package.json").is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "nub-npmrc-layers-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn workspace(manifest: &str) -> Scratch {
        let root = Scratch::new();
        std::fs::write(root.path().join("package.json"), manifest).unwrap();
        let member = root.path().join("packages").join("a");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(member.join("package.json"), r#"{"name":"a"}"#).unwrap();
        root
    }

    #[test]
    fn member_layer_follows_the_invoking_package_and_the_incumbent() {
        let ws = workspace(r#"{"name":"root","workspaces":["packages/*"]}"#);
        let root = ws.path();
        let member = root.join("packages").join("a");
        let root_rc = root.join(".npmrc");
        let member_rc = member.join(".npmrc");

        // Run from inside the member (a subdir of it, even): root, then member.
        assert_eq!(
            project_npmrc_paths_from(&member, Some(&member.join("src")), MemberNpmrc::Layered),
            vec![root_rc.clone(), member_rc.clone()]
        );
        // Targeting the member from the root (`--filter`): the root alone.
        assert_eq!(
            project_npmrc_paths_from(&member, Some(root), MemberNpmrc::Layered),
            vec![root_rc.clone()]
        );
        // pnpm 11 / npm / Bun never read the member's file.
        assert_eq!(
            project_npmrc_paths_from(&member, Some(&member), MemberNpmrc::Ignored),
            vec![root_rc.clone()]
        );
        // Outside any workspace: the project's own file.
        let single = Scratch::new();
        std::fs::write(single.path().join("package.json"), r#"{"name":"s"}"#).unwrap();
        assert_eq!(
            project_npmrc_paths_from(single.path(), Some(single.path()), MemberNpmrc::Layered),
            vec![single.path().join(".npmrc")]
        );
    }

    #[test]
    fn member_npmrc_mirrors_each_incumbent() {
        let cases = [
            (
                r#""packageManager":"pnpm@10.34.9""#,
                None,
                MemberNpmrc::Layered,
            ),
            (
                r#""packageManager":"pnpm@11.28.10""#,
                None,
                MemberNpmrc::Ignored,
            ),
            (
                r#""packageManager":"npm@11.20.0""#,
                None,
                MemberNpmrc::Ignored,
            ),
            (
                r#""packageManager":"bun@1.4.3""#,
                None,
                MemberNpmrc::Ignored,
            ),
            (
                r#""packageManager":"yarn@1.22.22""#,
                None,
                MemberNpmrc::Layered,
            ),
            (
                r#""packageManager":"nub@0.9.6""#,
                None,
                MemberNpmrc::Layered,
            ),
            (
                r#""private":true"#,
                Some("package-lock.json"),
                MemberNpmrc::Ignored,
            ),
            (r#""private":true"#, Some("bun.lock"), MemberNpmrc::Ignored),
            (
                r#""private":true"#,
                Some("pnpm-lock.yaml"),
                MemberNpmrc::Layered,
            ),
            (r#""private":true"#, Some("yarn.lock"), MemberNpmrc::Layered),
            (r#""private":true"#, None, MemberNpmrc::Layered),
        ];
        for (field, lockfile, expected) in cases {
            let ws = workspace(&format!(
                r#"{{"name":"root","workspaces":["packages/*"],{field}}}"#
            ));
            if let Some(lockfile) = lockfile {
                std::fs::write(ws.path().join(lockfile), "").unwrap();
            }
            let member = ws.path().join("packages").join("a");
            assert_eq!(member_npmrc(&member), expected, "{field} + {lockfile:?}");
        }
    }
}
