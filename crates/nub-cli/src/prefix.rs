//! The `prefix` field of `nub.jsonc`: a command nub puts in front of what it
//! launches for `nub <file>`, `nub run`, and `nub watch`, so that an env loader,
//! a secrets broker, or any other wrapper (`dotenvx run --`, `doppler run --`,
//! `nice -n 10`) sees every process the project starts.
//!
//! The scope is the three surfaces that run the PROJECT'S code. `nubx` and
//! `nub dlx` run somebody else's CLI and carry their own `exec` / `dlx` config,
//! so they never take it; nor does the `node` PATH shim, whose job is version
//! resolution rather than augmentation.
//!
//! Where the wrapper goes differs by surface, and the difference is the point.
//! A file run wraps Node itself: `<prefix…> <node> <args…>`. A script wraps the
//! SHELL, `<prefix…> sh -c <body>`, so a body that starts no Node process at all
//! still runs behind it — which is what `dotenvx run -- npm run build` gives
//! today, and what an inner-`node`-only wrap would silently miss. That is also
//! the reason a re-entrancy marker is needed: a script's `nub run other`, or a
//! wrapper whose own bin is a `#!/usr/bin/env node` script, re-enters nub in the
//! same project and would wrap again without bound.
//!
//! nub's own `.env*` loading is unchanged by the field, surface by surface: a
//! file run hands the loaded values to the wrapper, a script keeps them
//! Node-scoped (the inner `node` loads them, the shell never sees them), and a
//! watch passes them to Node as `--env-file` arguments behind the wrapper. A
//! wrapper that must own the environment outright pairs the field with
//! `"envFile": false`. Compat mode (`--node`, `NODE_COMPAT`, `nodeCompat`) is a
//! bare spawn and takes no prefix, as it takes no env-owner loader.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

/// Set on the wrapped process so a nested nub in the SAME project does not wrap
/// again. Carries the project root rather than a bare flag: a nested nub in a
/// different project must still wrap its own. Internal `__NUB_*` plumbing.
pub(crate) const WRAPPED_ENV: &str = "__NUB_PREFIX_WRAPPED";

/// A resolved prefix: the program as an absolute path, then its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prefix {
    program: PathBuf,
    args: Vec<String>,
    /// The directory the re-entrancy marker names — the project root, or the
    /// working directory when no project is detected.
    root: PathBuf,
}

impl Prefix {
    /// Resolve the configured prefix for a launch rooted at `root`, or `None`
    /// when the field is unset or this process is already behind it.
    ///
    /// `bin_dirs` is the `node_modules/.bin` chain a bare program name is looked
    /// up in before `PATH` — the same order a script body resolves its commands
    /// in, so `dotenvx` as a devDependency works without an install on `PATH`.
    /// A path form (`./tools/wrap`, `~/bin/wrap`, an absolute path) has already
    /// been anchored to the file that declared it by the config layer.
    pub(crate) fn resolve(root: &Path, bin_dirs: &[PathBuf]) -> Result<Option<Prefix>> {
        let Some(words) = crate::project_config::effective_prefix() else {
            return Ok(None);
        };
        if wrapped_for(root) {
            return Ok(None);
        }
        let (program, args) = words
            .split_first()
            .expect("the parser refuses an empty prefix");
        let program = match locate(program, bin_dirs) {
            Some(path) => path,
            None => bail!(
                "ERR_NUB_PREFIX_NOT_FOUND: `{program}` (prefix in {source}) is not installed \
                 in this project or on PATH",
                source = crate::project_config::prefix_source_label(),
            ),
        };
        Ok(Some(Prefix {
            program,
            args: args.to_vec(),
            root: root.to_path_buf(),
        }))
    }

    /// A command that runs the prefix, ready for the wrapped command line to be
    /// appended as arguments. Carries the re-entrancy marker.
    pub(crate) fn command(&self) -> Command {
        let mut cmd = nub_core::node::spawn::loader_command(&self.program);
        cmd.args(&self.args);
        cmd.env(WRAPPED_ENV, &self.root);
        cmd
    }

    /// The prefix as argv, for a spawner that assembles its own command line.
    pub(crate) fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.to_string_lossy().into_owned())
            .chain(self.args.iter().cloned())
            .collect()
    }

    /// The marker to stamp on a process spawned from an argv built by
    /// [`Self::argv`] rather than [`Self::command`].
    pub(crate) fn marker(&self) -> (&'static str, String) {
        (WRAPPED_ENV, self.root.to_string_lossy().into_owned())
    }
}

/// Whether a parent nub already wrapped this process for `root`.
fn wrapped_for(root: &Path) -> bool {
    std::env::var_os(WRAPPED_ENV).is_some_and(|marked| same_dir(Path::new(&marked), root))
}

/// Canonicalize both sides: the marker travels through a spawn, and a symlinked
/// or `..`-relative root would otherwise compare unequal to the same directory.
fn same_dir(marked: &Path, root: &Path) -> bool {
    match (marked.canonicalize(), root.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => marked == root,
    }
}

/// Turn the program word into a path: a path form must exist as written; a bare
/// name is looked up in the `.bin` chain, then `PATH`.
fn locate(program: &str, bin_dirs: &[PathBuf]) -> Option<PathBuf> {
    let as_path = Path::new(program);
    if as_path.components().count() > 1 || as_path.is_absolute() {
        return as_path.is_file().then(|| as_path.to_path_buf());
    }
    let names = candidate_names(program, cfg!(windows));
    let in_bin = bin_dirs
        .iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file());
    if in_bin.is_some() {
        return in_bin;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// The spellings a bare name may resolve to, in preference order.
///
/// On Windows an npm install writes an extensionless POSIX shim BESIDE the
/// runnable `.cmd`, and `CreateProcess` cannot run the former — so the
/// launchers come first and the bare name last, the order the local-bin
/// resolver already uses.
fn candidate_names(program: &str, windows: bool) -> Vec<String> {
    if windows {
        vec![
            format!("{program}.cmd"),
            format!("{program}.exe"),
            format!("{program}.bat"),
            program.to_string(),
        ]
    } else {
        vec![program.to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_resolves_through_the_bin_chain_before_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        std::fs::write(bin.join("wrap"), "").expect("write");
        assert_eq!(
            locate("wrap", std::slice::from_ref(&bin)),
            Some(bin.join("wrap"))
        );
        // A path form is taken as written and must exist.
        assert_eq!(locate("./missing/wrap", std::slice::from_ref(&bin)), None);
        let file = dir.path().join("wrap.sh");
        std::fs::write(&file, "").expect("write");
        assert_eq!(
            locate(file.to_str().expect("utf8"), &[]),
            Some(file.clone())
        );
    }

    /// npm leaves `wrap` (a POSIX script) next to `wrap.cmd` on Windows; only
    /// the latter can be spawned, so it must win.
    #[test]
    fn a_windows_lookup_prefers_the_runnable_launcher() {
        assert_eq!(
            candidate_names("wrap", true),
            ["wrap.cmd", "wrap.exe", "wrap.bat", "wrap"].map(String::from)
        );
        assert_eq!(candidate_names("wrap", false), ["wrap".to_string()]);
    }

    #[test]
    fn the_marker_matches_the_same_directory_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let other = tempfile::tempdir().expect("tempdir");
        assert!(same_dir(dir.path(), dir.path()));
        // Only a path that exists canonicalizes; `..` through a real child
        // lands on the same directory.
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        assert!(same_dir(&dir.path().join("sub").join(".."), dir.path()));
        assert!(!same_dir(dir.path(), other.path()));
    }
}
