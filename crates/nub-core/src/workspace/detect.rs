//! Workspace and project root detection.

use std::fs;
use std::path::{Path, PathBuf};

/// A detected workspace or standalone project.
#[derive(Debug)]
pub struct Project {
    /// The project root (nearest package.json).
    pub root: PathBuf,
    /// The workspace root, if different from root.
    pub workspace_root: Option<PathBuf>,
    /// Parsed package.json at root.
    pub manifest: serde_json::Value,
}

/// Walk up from `cwd` to find the project root and workspace root.
pub fn detect_project(cwd: &Path) -> Option<Project> {
    let mut dir = cwd.to_path_buf();
    let mut project_root = None;
    let mut workspace_root = None;

    for _ in 0..32 {
        let pkg_path = dir.join("package.json");
        if pkg_path.is_file() {
            if let Ok(content) = fs::read_to_string(&pkg_path) {
                if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                    if project_root.is_none() {
                        project_root = Some((dir.clone(), manifest.clone()));
                    }
                    if manifest.get("workspaces").is_some() {
                        workspace_root = Some(dir.clone());
                        break;
                    }
                }
            }
        }

        // Also check for pnpm-workspace.yaml.
        let pnpm_ws = dir.join("pnpm-workspace.yaml");
        if pnpm_ws.is_file() {
            workspace_root = Some(dir.clone());
            if project_root.is_none() {
                let pkg_path = dir.join("package.json");
                if let Ok(content) = fs::read_to_string(&pkg_path) {
                    if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                        project_root = Some((dir.clone(), manifest));
                    }
                }
            }
            break;
        }

        if !dir.pop() {
            break;
        }
    }

    project_root.map(|(root, manifest)| Project {
        root,
        workspace_root,
        manifest,
    })
}

/// List workspace member package.json paths matching a filter.
pub fn find_workspace_members(workspace_root: &Path, _filter: Option<&str>) -> Vec<PathBuf> {
    // Simplified: read the workspace root's package.json for the
    // workspaces field and glob-match. Full glob support deferred.
    let pkg_path = workspace_root.join("package.json");
    let content = match fs::read_to_string(&pkg_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let manifest: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let patterns = match manifest.get("workspaces") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<_>>(),
        _ => return vec![],
    };

    let mut members = Vec::new();
    for pattern in &patterns {
        let base = pattern.trim_end_matches("/*").trim_end_matches("/**");
        let search_dir = workspace_root.join(base);
        if let Ok(entries) = fs::read_dir(&search_dir) {
            for entry in entries.flatten() {
                let member_pkg = entry.path().join("package.json");
                if member_pkg.is_file() {
                    members.push(entry.path());
                }
            }
        }
    }

    members
}
