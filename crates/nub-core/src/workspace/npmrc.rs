//! Which project `.npmrc` files apply to a directory.
//!
//! Inside a workspace, project config belongs to the workspace and to the
//! invocation, not to whichever member a command targets: npm, pnpm, Yarn 1 and
//! Bun all read the workspace root's `.npmrc` (where a monorepo declares its
//! registry, scoped registries, auth and `ignore-scripts`), and none reads a
//! member's file when invoked from elsewhere. pnpm 10 and Yarn 1 also layer the
//! `.npmrc` of the package the command was run from on top; pnpm 11, npm and Bun
//! do not. The engine's own loader applies the same rule (`aube_util::WorkspaceNpmrc`),
//! and both take the per-incumbent model from the CLI's one detector, registered
//! through [`crate::incumbent`].

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

/// The incumbent's [`MemberNpmrc`] model for the project at `dir`, as the
/// embedder's incumbent detection decides it ([`crate::incumbent`]). Unregistered
/// (a unit test, a binary that never registers) it is the dominant v10 model.
pub fn member_npmrc(dir: &Path) -> MemberNpmrc {
    crate::incumbent::member_npmrc(dir)
}

/// The project-scope `.npmrc` paths for `project_root`, LOWEST precedence first.
/// Outside a workspace this is `<project_root>/.npmrc` alone. The invoking
/// package is the nearest `package.json` at or above the process cwd, or
/// `project_root` itself when the cwd is outside the workspace (the engine's rule).
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
        MemberNpmrc::Layered => match invocation_dir.and_then(|dir| within(dir, &root)) {
            Some((dir, root)) => package_dir_below_root(&dir, &root),
            None => within(project_root, &root)
                .filter(|(dir, root)| dir != root)
                .map(|_| project_root.to_path_buf()),
        },
    };
    let mut paths = vec![root.join(".npmrc")];
    paths.extend(invoked_from.map(|dir| dir.join(".npmrc")));
    paths
}

/// `dir` and `root` in one spelling, when `dir` is `root` or inside it.
fn within(dir: &Path, root: &Path) -> Option<(PathBuf, PathBuf)> {
    let (dir, root) = if dir.starts_with(root) {
        (dir.to_path_buf(), root.to_path_buf())
    } else {
        (dir.canonicalize().ok()?, root.canonicalize().ok()?)
    };
    dir.starts_with(&root).then_some((dir, root))
}

/// The nearest directory at or above `dir` holding a `package.json`, strictly
/// inside the workspace `root` (pnpm's "local prefix"). Both come from [`within`].
fn package_dir_below_root(dir: &Path, root: &Path) -> Option<PathBuf> {
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
        // Invoked from outside the workspace with the member as the target
        // (`-C packages/a`): the member is the invoking package.
        let elsewhere = Scratch::new();
        assert_eq!(
            project_npmrc_paths_from(&member, Some(elsewhere.path()), MemberNpmrc::Layered),
            vec![root_rc.clone(), member_rc.clone()]
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
}
