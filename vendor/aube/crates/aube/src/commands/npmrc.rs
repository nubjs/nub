//! In-place editor for `.npmrc` files.
//!
//! `aube login` / `aube logout` need to set or remove single keys in the
//! user's `~/.npmrc` without disturbing the rest of the file. The registry
//! crate already has a *reader* (`NpmConfig::load`) which collapses the
//! file into a typed struct — that's lossy for rewriting (comments,
//! ordering, env-var placeholders all get thrown away). This module keeps
//! the raw lines verbatim and only mutates matching `key=value` entries.
//!
//! Keys are matched by literal equality of the part before `=`. That's
//! enough for the auth use case, where the keys are fully qualified
//! (`//host/:_authToken`, `@scope:registry`, `_authToken`).

use aube_registry::config::{NpmConfig, normalize_registry_url_pub, registry_uri_key_pub};
use miette::{Context, IntoDiagnostic, miette};
use std::path::{Path, PathBuf};

/// Re-export of [`registry_uri_key_pub`] under the name used elsewhere
/// in aube. `login` and `logout` call this to build the
/// `//host[:port]/path/` prefix that keys `.npmrc` auth entries; the
/// canonical implementation lives in `aube-registry` so both the
/// registry client and the npmrc editor agree on the shape of the key.
pub fn registry_host_key(url: &str) -> String {
    registry_uri_key_pub(url)
}

/// Parsed-ish view of a `.npmrc`: each line is kept as-is except for
/// `key=value` lines, which are split so `set` / `remove` can address them
/// by key. Comments, blanks, and malformed lines pass through untouched.
pub struct NpmrcEdit {
    lines: Vec<Line>,
}

enum Line {
    Raw(String),
    Entry { key: String, value: String },
}

impl NpmrcEdit {
    pub fn load(path: &Path) -> miette::Result<Self> {
        let content = if path.exists() {
            std::fs::read_to_string(path)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read {}", path.display()))?
        } else {
            String::new()
        };
        let mut lines = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                lines.push(Line::Raw(line.to_string()));
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                lines.push(Line::Entry {
                    key: k.trim().to_string(),
                    value: v.trim().to_string(),
                });
            } else {
                lines.push(Line::Raw(line.to_string()));
            }
        }
        Ok(Self { lines })
    }

    /// Set `key=value`, removing every existing entry for `key` first and
    /// appending the new one. Removing all duplicates (rather than
    /// updating just the first) matters because `NpmConfig::apply` is
    /// last-write-wins at parse time — a stale trailing duplicate would
    /// otherwise silently override the value we just set.
    pub fn set(&mut self, key: &str, value: &str) {
        self.lines.retain(|line| match line {
            Line::Entry { key: k, .. } => k != key,
            Line::Raw(_) => true,
        });
        self.lines.push(Line::Entry {
            key: key.to_string(),
            value: value.to_string(),
        });
    }

    /// Return every `key=value` entry in file order, dropping raw
    /// comment/blank lines. Used by `config list` to enumerate the file
    /// without reparsing it and duplicating the comment-handling logic.
    pub fn entries(&self) -> Vec<(String, String)> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Entry { key, value } => Some((key.clone(), value.clone())),
                Line::Raw(_) => None,
            })
            .collect()
    }

    /// Remove all entries matching `key`. Returns `true` if any were
    /// removed.
    pub fn remove(&mut self, key: &str) -> bool {
        self.remove_matching(|k| k == key)
    }

    /// Remove all entries whose key satisfies `predicate`. Returns
    /// `true` if any were removed.
    pub fn remove_matching(&mut self, predicate: impl Fn(&str) -> bool) -> bool {
        let before = self.lines.len();
        self.lines.retain(|line| match line {
            Line::Entry { key: k, .. } => !predicate(k),
            Line::Raw(_) => true,
        });
        before != self.lines.len()
    }

    /// Atomically replace `path` with the current contents, following
    /// the final component when `path` is a symlink. Writes into a
    /// sibling temp file and `rename`s over the real target — on POSIX
    /// the rename is atomic, so a crash/OOM/disk-full mid-write leaves
    /// the original `~/.npmrc` intact rather than truncated (which
    /// would silently wipe every stored auth token).
    pub fn save(&self, path: &Path) -> miette::Result<()> {
        let write_path = symlink_target_or_self(path).into_diagnostic()?;
        let mut out = String::new();
        for line in &self.lines {
            match line {
                Line::Raw(s) => out.push_str(s),
                Line::Entry { key, value } => {
                    out.push_str(key);
                    out.push('=');
                    out.push_str(value);
                }
            }
            out.push('\n');
        }

        aube_util::fs_atomic::atomic_write(&write_path, out.as_bytes())
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write {}", write_path.display()))?;
        // .npmrc commonly holds _authToken values. Default umask
        // leaves the file at 0644 after the rename, readable by every
        // other user on a shared host. Force 0600 so only the owner
        // can read the token back.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&write_path, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_TOKEN_CHMOD_FAILED,
                    "failed to chmod 0600 {}: {e}. File may be world-readable, check filesystem permissions",
                    write_path.display()
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn symlink_target_or_self(path: &Path) -> std::io::Result<PathBuf> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(path.to_path_buf());
    };
    if !meta.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }
    std::fs::canonicalize(path)
}

/// Resolve the registry URL that `login` / `logout` should act on.
/// Precedence:
/// 1. `--registry` flag.
/// 2. `--scope` → the scope's registry from merged `.npmrc` (if set).
/// 3. The default `registry` from merged `.npmrc` (falls back to npmjs).
///
/// Lives here rather than in either command module so both sides pick up
/// any future change (env-var support, `--prefix`, etc) without drifting.
pub fn resolve_registry(flag: Option<&str>, scope: Option<&str>) -> miette::Result<String> {
    if let Some(r) = flag {
        return Ok(normalize_registry_url_pub(r));
    }
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let config = NpmConfig::load(&cwd);
    if let Some(scope) = scope
        && let Some(url) = config.scoped_registries.get(scope)
    {
        return Ok(url.clone());
    }
    Ok(config.registry)
}

/// `~/.npmrc`, or an error if we can't locate the user's home directory.
///
/// Reads HOME first (every Unix, and POSIX-compat Windows toolchains
/// that set it). Falls back to USERPROFILE on Windows since vanilla
/// Windows does not set HOME. Old code was HOME-only, so `aube login`
/// on a native Windows shell errored out with "$HOME is not set"
/// instead of using C:\Users\<user>\.npmrc. Same issue would make
/// `aube logout` fail and `aube config` never find the user file.
pub fn user_npmrc_path() -> miette::Result<PathBuf> {
    let home = read_home_env().ok_or_else(|| {
        miette!("could not locate home directory. set HOME or USERPROFILE to point at ~/.npmrc")
    })?;
    Ok(home.join(".npmrc"))
}

fn read_home_env() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(PathBuf::from(h));
    }
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(p));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_existing_and_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".npmrc");
        std::fs::write(
            &path,
            "# top comment\n\
             registry=https://registry.npmjs.org/\n\
             //registry.npmjs.org/:_authToken=old\n\
             ; trailing\n",
        )
        .unwrap();

        let mut edit = NpmrcEdit::load(&path).unwrap();
        edit.set("//registry.npmjs.org/:_authToken", "new");
        edit.save(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# top comment"));
        assert!(after.contains("registry=https://registry.npmjs.org/"));
        assert!(after.contains("//registry.npmjs.org/:_authToken=new"));
        assert!(!after.contains("=old"));
        assert!(after.contains("; trailing"));
    }

    #[test]
    fn set_appends_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".npmrc");
        std::fs::write(&path, "registry=https://r.example.com/\n").unwrap();

        let mut edit = NpmrcEdit::load(&path).unwrap();
        edit.set("//r.example.com/:_authToken", "tok");
        edit.save(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("//r.example.com/:_authToken=tok"));
    }

    #[test]
    fn remove_drops_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".npmrc");
        std::fs::write(
            &path,
            "registry=https://r.example.com/\n\
             //r.example.com/:_authToken=tok\n",
        )
        .unwrap();

        let mut edit = NpmrcEdit::load(&path).unwrap();
        assert!(edit.remove("//r.example.com/:_authToken"));
        edit.save(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("registry=https://r.example.com/"));
        assert!(!after.contains("_authToken"));
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-npmrc");
        let link = dir.path().join(".npmrc");
        std::fs::write(&target, "registry=https://r.example.com/\n").unwrap();
        std::os::unix::fs::symlink("real-npmrc", &link).unwrap();

        let mut edit = NpmrcEdit::load(&link).unwrap();
        edit.set("minimumReleaseAge", "2880");
        edit.save(&link).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let after = std::fs::read_to_string(&target).unwrap();
        assert!(after.contains("minimumReleaseAge=2880"));
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_symlink_chain() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-npmrc");
        let mid = dir.path().join("dotfiles-npmrc");
        let link = dir.path().join(".npmrc");
        std::fs::write(&target, "registry=https://r.example.com/\n").unwrap();
        std::os::unix::fs::symlink("real-npmrc", &mid).unwrap();
        std::os::unix::fs::symlink("dotfiles-npmrc", &link).unwrap();

        let mut edit = NpmrcEdit::load(&link).unwrap();
        edit.set("minimumReleaseAge", "2880");
        edit.save(&link).unwrap();

        for path in [&link, &mid] {
            assert!(
                std::fs::symlink_metadata(path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        let after = std::fs::read_to_string(&target).unwrap();
        assert!(after.contains("minimumReleaseAge=2880"));
    }

    #[test]
    fn remove_missing_key_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".npmrc");
        std::fs::write(&path, "registry=https://r.example.com/\n").unwrap();

        let mut edit = NpmrcEdit::load(&path).unwrap();
        assert!(!edit.remove("//r.example.com/:_authToken"));
    }

    #[test]
    fn load_nonexistent_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.npmrc");
        let mut edit = NpmrcEdit::load(&path).unwrap();
        edit.set("foo", "bar");
        edit.save(&path).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "foo=bar\n");
    }

    #[test]
    fn registry_host_key_strips_scheme() {
        assert_eq!(
            registry_host_key("https://registry.npmjs.org/"),
            "//registry.npmjs.org/"
        );
        assert_eq!(
            registry_host_key("http://localhost:4873/"),
            "//localhost:4873/"
        );
    }
}
