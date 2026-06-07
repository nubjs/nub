//! Script resolution from package.json and npm_* env injection.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Resolve a script name from package.json#scripts.
pub fn resolve_script(manifest: &serde_json::Value, name: &str) -> Option<String> {
    manifest
        .get("scripts")
        .and_then(|s| s.get(name))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Build the npm_* environment variables from package.json.
pub fn npm_env(
    manifest: &serde_json::Value,
    project_root: &Path,
    lifecycle_event: &str,
    lifecycle_script: Option<&str>,
    node_execpath: &str,
) -> HashMap<String, String> {
    let mut env_vars = HashMap::new();

    if let Some(name) = manifest.get("name").and_then(|v| v.as_str()) {
        // Also the recursion-guard key in `run_one_workspace_pkg` (workspace member
        // names are unique), so no new — and brand-forbidden NUB_* — env sentinel
        // is needed to stop a `"build": "nub run -r build"` script looping forever.
        env_vars.insert("npm_package_name".to_string(), name.to_string());
    }
    if let Some(version) = manifest.get("version").and_then(|v| v.as_str()) {
        env_vars.insert("npm_package_version".to_string(), version.to_string());
    }

    env_vars.insert(
        "npm_lifecycle_event".to_string(),
        lifecycle_event.to_string(),
    );

    if let Some(script) = lifecycle_script {
        env_vars.insert("npm_lifecycle_script".to_string(), script.to_string());
    }

    env_vars.insert(
        "npm_package_json".to_string(),
        project_root
            .join("package.json")
            .to_string_lossy()
            .to_string(),
    );

    // npm_node_execpath is the resolved Node binary, threaded in from discovery
    // (A13/A38) — no `node -e process.execPath` subprocess per `nub run`. This is
    // also more correct than shelling out to a bare `node`, which would ignore an
    // .nvmrc/.node-version pin that discovery honors.
    if !node_execpath.is_empty() {
        env_vars.insert("npm_node_execpath".to_string(), node_execpath.to_string());
    }

    env_vars.insert("npm_command".to_string(), "run-script".to_string());

    let nub_version = env!("CARGO_PKG_VERSION");
    let node_version = env::var("NODE_VERSION").unwrap_or_default();
    env_vars.insert(
        "npm_config_user_agent".to_string(),
        format!(
            "nub/{nub_version} npm/? node/{node_version} {} {}",
            env::consts::OS,
            env::consts::ARCH
        ),
    );

    if let Ok(exe) = env::current_exe() {
        env_vars.insert(
            "npm_execpath".to_string(),
            exe.to_string_lossy().to_string(),
        );
    }

    env_vars.insert(
        "INIT_CWD".to_string(),
        env::current_dir()
            .unwrap_or_else(|_| project_root.to_path_buf())
            .to_string_lossy()
            .to_string(),
    );

    env_vars
}

/// Build the PATH with node_modules/.bin directories prepended.
pub fn bin_path(project_root: &Path, workspace_root: Option<&Path>) -> String {
    let mut dirs = Vec::new();

    // Walk from project root up, adding each node_modules/.bin.
    let mut dir = project_root.to_path_buf();
    for _ in 0..16 {
        let bin_dir = dir.join("node_modules").join(".bin");
        if bin_dir.is_dir() {
            dirs.push(bin_dir.to_string_lossy().to_string());
        }
        if workspace_root.is_some() && Some(dir.as_path()) == workspace_root {
            break;
        }
        if !dir.pop() {
            break;
        }
    }

    let existing = env::var("PATH").unwrap_or_default();
    if dirs.is_empty() {
        existing
    } else {
        dirs.push(existing);
        dirs.join(crate::PATH_LIST_SEPARATOR)
    }
}

/// Find a binary in node_modules/.bin by name, walking up from `cwd`.
///
/// On Unix the entry is the extensionless name (a symlink to the package's JS,
/// `#!/usr/bin/env node`). On Windows npm/pnpm write `<name>.cmd` (the runnable
/// shim), `<name>.ps1`, and an extensionless Bash script that Windows can't run
/// — so we look for the executable extensions, in PATHEXT-ish preference, and
/// never return the unrunnable Bash stub (A40).
pub fn find_bin(name: &str, cwd: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates: &[&str] = &[".cmd", ".exe", ".bat", ".ps1"];
    #[cfg(not(windows))]
    let candidates: &[&str] = &[""];

    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        let bin_dir = dir.join("node_modules").join(".bin");
        for ext in candidates {
            let candidate = bin_dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Read a single `key = value` setting from `.npmrc` (project-level, then
/// user-level `~/.npmrc`), returning the first match's value with surrounding
/// quotes stripped. The one `.npmrc` reader in the crate — `script_shell` and
/// (P1) the PM registry lookup both go through it.
pub fn npmrc_value(project_root: &Path, key: &str) -> Option<String> {
    // Check project .npmrc first, then ~/.npmrc
    let candidates = [
        project_root.join(".npmrc"),
        dirs_next::home_dir()
            .map(|h| h.join(".npmrc"))
            .unwrap_or_default(),
    ];

    for path in &candidates {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with(key) {
                    if let Some((_, value)) = trimmed.split_once('=') {
                        let value = value.trim().trim_matches('"').trim_matches('\'');
                        if !value.is_empty() {
                            return Some(value.to_string());
                        }
                    }
                }
            }
        }
    }

    None
}

/// Read `script-shell` from `.npmrc` (project-level, then user-level).
/// Returns the shell path if set, or None to use the platform default.
pub fn script_shell(project_root: &Path) -> Option<String> {
    npmrc_value(project_root, "script-shell")
}

#[cfg(test)]
mod tests {
    use super::{find_bin, npmrc_value, script_shell};
    use std::fs;

    #[test]
    fn npmrc_value_reads_keys_and_script_shell_delegates() {
        let tmp = std::env::temp_dir().join(format!("nub-npmrc-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(
            tmp.join(".npmrc"),
            "registry=https://example.test/\nscript-shell = \"/bin/dash\"\n",
        )
        .unwrap();

        // Arbitrary keys round-trip, with surrounding quotes/whitespace stripped.
        assert_eq!(
            npmrc_value(&tmp, "registry").as_deref(),
            Some("https://example.test/")
        );
        // script_shell is now a thin delegate over npmrc_value (quote-stripped).
        assert_eq!(script_shell(&tmp).as_deref(), Some("/bin/dash"));
        // A key absent from the project .npmrc falls through to None (no ~/.npmrc
        // key named this in CI).
        assert!(npmrc_value(&tmp, "nub-no-such-key").is_none());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_bin_resolves_platform_entry() {
        let tmp = std::env::temp_dir().join(format!("nub-a40-{}", std::process::id()));
        let bin_dir = tmp.join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();

        // The resolvable entry is the runnable `.cmd` on Windows, the
        // extensionless symlink/script on Unix (A40).
        #[cfg(windows)]
        let entry = bin_dir.join("tool.cmd");
        #[cfg(not(windows))]
        let entry = bin_dir.join("tool");
        fs::write(&entry, "x").unwrap();

        assert_eq!(find_bin("tool", &tmp).as_deref(), Some(entry.as_path()));
        assert!(find_bin("missing", &tmp).is_none());

        // On Windows a lone extensionless Bash stub (no `.cmd`) is not runnable,
        // so it must NOT be returned — better to fall through to the PM delegate.
        #[cfg(windows)]
        {
            let stub_root = tmp.join("stub-only");
            let stub_dir = stub_root.join("node_modules").join(".bin");
            fs::create_dir_all(&stub_dir).unwrap();
            fs::write(stub_dir.join("stubtool"), "#!/bin/sh\n").unwrap();
            assert!(find_bin("stubtool", &stub_root).is_none());
        }

        let _ = fs::remove_dir_all(&tmp);
    }
}
