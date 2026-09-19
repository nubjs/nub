//! The projects a recursive `run` or `exec` walks in a pnpm project.
//!
//! pnpm finds them itself, and its answer is not the `package.json`
//! `workspaces` field nub reads for its own projects. The workspace is the
//! nearest `pnpm-workspace.yaml`, its members are that file's `packages` (the
//! root alone when the key is absent), and with no workspace file a recursive
//! command walks every package below the project. The walk here is the
//! engine's own, so the set is pnpm's by construction; only the root's place in
//! the run is decided by the caller, from [`RecursiveProjects::keeps_root`].

use anyhow::Result;
use nub_core::workspace::filter::WorkspacePackage;
use std::path::{Path, PathBuf};

use super::project_identity::{self, ProjectIdentity};

pub(crate) struct RecursiveProjects {
    /// The directory the walk starts from and filters resolve against.
    pub root: PathBuf,
    /// Every project the walk found, the root included.
    pub projects: Vec<WorkspacePackage>,
    /// Whether a recursive `run` keeps the root without being asked. pnpm's CLI
    /// drops it unless there is no workspace file, or the file's `packages`
    /// names only the root, because then the root is the whole run.
    pub keeps_root: bool,
    /// Whether a `pnpm-workspace.yaml` was found, which is what pnpm's header
    /// counts as "workspace projects" rather than "projects".
    pub is_workspace: bool,
}

/// The projects to walk from `cwd`, or `None` in a nub project.
pub(crate) fn find(cwd: &Path, project_root: &Path) -> Result<Option<RecursiveProjects>> {
    if project_identity::detect(cwd) != ProjectIdentity::Pnpm {
        return Ok(None);
    }
    let workspace_dir = pnpm_workspace::find_workspace_dir(cwd)?;
    let patterns = match &workspace_dir {
        Some(dir) => pnpm_workspace::read_workspace_manifest(dir)?
            .map(|manifest| pnpm_workspace::workspace_package_patterns(&manifest)),
        None => None,
    };
    let root = workspace_dir
        .clone()
        .unwrap_or_else(|| project_root.to_path_buf());
    let found = pnpm_workspace::find_workspace_projects(
        &root,
        &pnpm_workspace::FindWorkspaceProjectsOpts {
            patterns: patterns.clone(),
        },
    )?;
    // The manifest is the engine's own parse, never a second read by the JSON
    // name: the walk accepts `package.yaml` too, and re-reading `package.json`
    // dropped such a member from the run with no error at all.
    let projects = found
        .into_iter()
        .map(|project| {
            let manifest = project.manifest.value().clone();
            let name = manifest
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            WorkspacePackage {
                name,
                dir: project.root_dir,
                manifest,
            }
        })
        .collect();
    Ok(Some(RecursiveProjects {
        root,
        projects,
        keeps_root: patterns.as_deref().is_none_or(|patterns| patterns == ["."]),
        is_workspace: workspace_dir.is_some(),
    }))
}
