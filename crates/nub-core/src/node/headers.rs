//! Filling node-gyp's header cache from a Node nub provisioned.
//!
//! node-gyp compiles an addon against the headers of the Node it targets and,
//! on first use of each version, downloads `node-v<ver>-headers.tar.gz` from
//! nodejs.org into `<devdir>/<ver>`. Every Node in nub's download store already
//! carries those headers under `<root>/include/node/` — the same 2,810 files as
//! the tarball, which differs only in a `config.gypi` describing the Linux build
//! that produced it (node-gyp reads the running Node's `process.config`
//! instead). So when node-gyp is about to run, nub writes the cache entry
//! node-gyp would otherwise download, and node-gyp finds it and skips the
//! download (`lib/install.js`: "version is good"). That is what lets a
//! from-source build run offline.
//!
//! WHY THE CACHE AND NOT `npm_config_nodedir`. Exporting `npm_config_nodedir`
//! also skips the download, but node-gyp folds `npm_config_*` OVER its parsed
//! argv (`lib/node-gyp.js`) and the variable is inherited by every descendant.
//! A build that targets a different runtime then compiles against the running
//! Node's headers instead, and it usually picks that target in a child process
//! nub cannot see: `@electron/rebuild` forks node-gyp with the ambient env and
//! its own `--target`, and electron-builder spreads `process.env` under its
//! `npm_config_target`. A cache entry cannot misfire that way. node-gyp keys it
//! by the version it has ALREADY resolved from its own argv and env, so the
//! entry for this Node is read only by a build for this Node.
//!
//! Windows is excluded: the official zip ships no headers, and node-gyp also
//! requires `<arch>/node.lib` inside a Windows cache entry.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// node-gyp's `installVersion` for the layout written here: `include/` plus
/// this marker file, which is what extracting the headers tarball leaves
/// (`lib/install.js`). A node-gyp that later bumps the number treats the entry
/// as stale and downloads, exactly as it does without nub.
const NODE_GYP_INSTALL_VERSION: u32 = 11;

/// Make sure node-gyp's header cache holds the headers of the Node at
/// `node_execpath`, so a node-gyp run under it never downloads them. Returns
/// the cache entry when it holds a complete set after the call, whether nub
/// wrote it now or it was already there. Best effort throughout: on any failure
/// node-gyp falls back to its own download.
pub fn seed_node_gyp_cache(node_execpath: &Path, version: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let store = super::discovery::node_store_dir()?;
    let home = dirs_next::home_dir()?;
    let env = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value)))
        .collect::<Vec<_>>();
    seed_for(
        node_execpath,
        version,
        &store,
        &node_gyp_devdir(&env, &home),
    )
}

/// The decision, with nub's store and node-gyp's devdir passed in so it is
/// testable without touching either real directory.
fn seed_for(node_execpath: &Path, version: &str, store: &Path, devdir: &Path) -> Option<PathBuf> {
    // Only a Node from nub's own store. node-gyp keys the entry by version
    // alone and keeps it after this build, so every later build for that
    // version reads it too; a distro or Homebrew build of the same version can
    // bundle different library headers, and must not stand in for the release.
    if !node_execpath.starts_with(store) {
        return None;
    }
    let root = headers_root(node_execpath, version)?;
    seed_entry(&root, devdir, version.trim_start_matches('v'))
}

/// Write `<devdir>/<version>` from `<root>/include`, staged beside it and
/// renamed into place so a concurrent node-gyp or nub never sees half an
/// entry. An entry that already exists belongs to node-gyp — its own download,
/// a partial one it will redo, or an earlier seed — and is never touched.
fn seed_entry(root: &Path, devdir: &Path, version: &str) -> Option<PathBuf> {
    let entry = devdir.join(version);
    let complete = |entry: &Path| entry.join("installVersion").is_file();
    if entry.exists() {
        return complete(&entry).then_some(entry);
    }
    std::fs::create_dir_all(devdir).ok()?;
    let stage = devdir.join(format!(".{version}.nub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    let staged = copy_tree(&root.join("include"), &stage.join("include"))
        .and_then(|()| {
            std::fs::write(
                stage.join("installVersion"),
                format!("{NODE_GYP_INSTALL_VERSION}\n"),
            )
        })
        .and_then(|()| std::fs::rename(&stage, &entry));
    if staged.is_err() {
        // Losing the rename to a concurrent writer lands here too; the entry
        // it left is as good as ours.
        let _ = std::fs::remove_dir_all(&stage);
    }
    complete(&entry).then_some(entry)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// node-gyp's cache root, resolved as node-gyp resolves it: a `devdir` option
/// from the environment, where a package's own `config.node-gyp.devdir`
/// outranks `npm_config_devdir` and a leading `~` means the home directory
/// (`bin/node-gyp.js`), else env-paths' cache directory for `node-gyp`.
fn node_gyp_devdir(env: &[(String, OsString)], home: &Path) -> PathBuf {
    let option = |name: &str| {
        env.iter()
            .find(|(key, value)| key.eq_ignore_ascii_case(name) && !value.is_empty())
            .map(|(_, value)| value.as_os_str())
    };
    if let Some(dir) =
        option("npm_package_config_node_gyp_devdir").or_else(|| option("npm_config_devdir"))
    {
        return match dir.to_str().and_then(|dir| dir.strip_prefix('~')) {
            Some(rest) => {
                let mut expanded = home.as_os_str().to_owned();
                expanded.push(rest);
                PathBuf::from(expanded)
            }
            None => PathBuf::from(dir),
        };
    }
    if cfg!(target_os = "macos") {
        return home.join("Library").join("Caches").join("node-gyp");
    }
    env.iter()
        .find(|(key, value)| key == "XDG_CACHE_HOME" && !value.is_empty())
        .map(|(_, value)| PathBuf::from(value))
        .unwrap_or_else(|| home.join(".cache"))
        .join("node-gyp")
}

/// The install root of `node_execpath` (`<root>/bin/node`) when
/// `<root>/include/node/node_version.h` names exactly `version`. The path is
/// tried as given and then resolved through symlinks, so both a Homebrew
/// `/opt/homebrew/bin/node` (headers linked beside it) and a shim pointing
/// into a version manager's store find their headers.
pub fn headers_root(node_execpath: &Path, version: &str) -> Option<PathBuf> {
    let want = version.trim_start_matches('v');
    let as_given = Some(node_execpath.to_path_buf());
    let resolved = std::fs::canonicalize(node_execpath).ok();
    as_given.into_iter().chain(resolved).find_map(|exe| {
        let bin_dir = exe.parent()?;
        if bin_dir.file_name() != Some(OsStr::new("bin")) {
            return None;
        }
        let root = bin_dir.parent()?;
        let header = root.join("include").join("node").join("node_version.h");
        let text = std::fs::read_to_string(header).ok()?;
        (header_version(&text)? == want).then(|| root.to_path_buf())
    })
}

/// `major.minor.patch` from the `NODE_*_VERSION` macros of a `node_version.h`.
fn header_version(text: &str) -> Option<String> {
    let mut parts = [None, None, None];
    for line in text.lines() {
        for (slot, macro_name) in [
            "#define NODE_MAJOR_VERSION ",
            "#define NODE_MINOR_VERSION ",
            "#define NODE_PATCH_VERSION ",
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(rest) = line.strip_prefix(macro_name) {
                parts[slot] = rest.trim().parse::<u32>().ok();
            }
        }
    }
    match parts {
        [Some(major), Some(minor), Some(patch)] => Some(format!("{major}.{minor}.{patch}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nub-node-headers-{}-{name}", std::process::id()))
    }

    /// A directory removed on drop, holding a fake store and devdir side by side.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = scratch_path(name);
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct FakeNode {
        root: PathBuf,
    }

    impl FakeNode {
        fn new(name: &str, header_version: Option<&str>) -> Self {
            Self::at(scratch_path(name), header_version)
        }

        fn at(root: PathBuf, header_version: Option<&str>) -> Self {
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::write(root.join("bin").join("node"), b"").unwrap();
            if let Some(version) = header_version {
                let mut parts = version.split('.');
                let (major, minor, patch) = (
                    parts.next().unwrap(),
                    parts.next().unwrap(),
                    parts.next().unwrap(),
                );
                let include = root.join("include").join("node");
                std::fs::create_dir_all(&include).unwrap();
                std::fs::write(
                    include.join("node_version.h"),
                    format!(
                        "#ifndef SRC_NODE_VERSION_H_\n#define NODE_MAJOR_VERSION {major}\n#define NODE_MINOR_VERSION {minor}\n#define NODE_PATCH_VERSION {patch}\n#define NODE_VERSION_IS_LTS 0\n"
                    ),
                )
                .unwrap();
            }
            Self { root }
        }

        fn node(&self) -> PathBuf {
            self.root.join("bin").join("node")
        }
    }

    impl Drop for FakeNode {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn matching_headers_beside_the_binary_name_the_install_root() {
        let node = FakeNode::new("match", Some("26.8.2"));
        assert_eq!(
            headers_root(&node.node(), "26.8.2"),
            Some(node.root.clone())
        );
        assert_eq!(
            headers_root(&node.node(), "v26.8.2"),
            Some(node.root.clone()),
            "a `v`-prefixed version must compare equal"
        );
    }

    #[test]
    fn a_version_mismatch_keeps_node_gyp_on_its_own_download() {
        let node = FakeNode::new("mismatch", Some("26.8.2"));
        assert_eq!(headers_root(&node.node(), "26.8.1"), None);
    }

    #[test]
    fn a_root_without_headers_is_skipped() {
        let node = FakeNode::new("bare", None);
        assert_eq!(headers_root(&node.node(), "26.8.2"), None);
    }

    #[test]
    fn a_binary_outside_a_bin_dir_is_skipped() {
        let node = FakeNode::new("flat", Some("26.8.2"));
        assert_eq!(headers_root(&node.root.join("node"), "26.8.2"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_binary_resolves_to_its_real_root() {
        let real = FakeNode::new("real", Some("26.8.2"));
        let shim_root =
            std::env::temp_dir().join(format!("nub-node-headers-{}-shim", std::process::id()));
        let _ = std::fs::remove_dir_all(&shim_root);
        std::fs::create_dir_all(shim_root.join("bin")).unwrap();
        let shim = shim_root.join("bin").join("node");
        std::os::unix::fs::symlink(real.node(), &shim).unwrap();
        let found = headers_root(&shim, "26.8.2");
        let _ = std::fs::remove_dir_all(&shim_root);
        assert_eq!(found, Some(std::fs::canonicalize(&real.root).unwrap()));
    }

    /// The entry node-gyp would have downloaded: the Node's `include/` tree plus
    /// the `installVersion` marker node-gyp reads before skipping the download.
    #[test]
    fn seeding_writes_the_entry_node_gyp_checks_for() {
        let scratch = Scratch::new("seed");
        let store = scratch.0.join("store");
        let node = FakeNode::at(store.join("26.8.2"), Some("26.8.2"));
        let devdir = scratch.0.join("devdir");
        let entry = devdir.join("26.8.2");
        assert_eq!(
            seed_for(&node.node(), "26.8.2", &store, &devdir),
            Some(entry.clone())
        );
        assert_eq!(
            std::fs::read_to_string(entry.join("installVersion")).unwrap(),
            "11\n"
        );
        assert_eq!(
            std::fs::read(entry.join("include").join("node").join("node_version.h")).unwrap(),
            std::fs::read(
                node.root
                    .join("include")
                    .join("node")
                    .join("node_version.h")
            )
            .unwrap(),
        );
        let left: Vec<_> = std::fs::read_dir(&devdir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["26.8.2"], "the staging directory must not survive");
    }

    /// The entry outlives this build and is keyed by version alone, so a Node
    /// nub did not provision never fills it.
    #[test]
    fn a_node_outside_the_store_never_fills_the_cache() {
        let scratch = Scratch::new("outside");
        let node = FakeNode::at(scratch.0.join("nvm").join("26.8.2"), Some("26.8.2"));
        let devdir = scratch.0.join("devdir");
        let store = scratch.0.join("store");
        assert_eq!(seed_for(&node.node(), "26.8.2", &store, &devdir), None);
        assert!(!devdir.exists());
    }

    /// Whatever node-gyp already has is left exactly as it is: a partial
    /// download it will redo, or an entry from an older layout.
    #[test]
    fn an_existing_entry_is_left_to_node_gyp() {
        let scratch = Scratch::new("existing");
        let store = scratch.0.join("store");
        let node = FakeNode::at(store.join("26.8.2"), Some("26.8.2"));
        let devdir = scratch.0.join("devdir");
        let entry = devdir.join("26.8.2");
        std::fs::create_dir_all(&entry).unwrap();
        assert_eq!(seed_for(&node.node(), "26.8.2", &store, &devdir), None);
        assert!(
            !entry.join("include").exists(),
            "a partial entry is node-gyp's to redo"
        );
        std::fs::write(entry.join("installVersion"), "9\n").unwrap();
        seed_for(&node.node(), "26.8.2", &store, &devdir);
        assert_eq!(
            std::fs::read_to_string(entry.join("installVersion")).unwrap(),
            "9\n"
        );
        assert!(!entry.join("include").exists());
    }

    /// node-gyp's own precedence: a package's `config.node-gyp.devdir` over
    /// `npm_config_devdir` in either case, `~` for the home directory, else the
    /// platform cache directory.
    #[test]
    fn the_devdir_is_the_one_node_gyp_resolves() {
        let home = Path::new("/home/u");
        let env = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), OsString::from(value)))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            node_gyp_devdir(&env(&[("npm_config_devdir", "~/gyp")]), home),
            PathBuf::from("/home/u/gyp")
        );
        assert_eq!(
            node_gyp_devdir(&env(&[("NPM_CONFIG_DEVDIR", "/abs")]), home),
            PathBuf::from("/abs")
        );
        assert_eq!(
            node_gyp_devdir(
                &env(&[
                    ("npm_config_devdir", "/user"),
                    ("npm_package_config_node_gyp_devdir", "/pkg"),
                ]),
                home
            ),
            PathBuf::from("/pkg")
        );
        let default = node_gyp_devdir(&env(&[("XDG_CACHE_HOME", "/xdg")]), home);
        if cfg!(target_os = "macos") {
            assert_eq!(default, PathBuf::from("/home/u/Library/Caches/node-gyp"));
        } else {
            assert_eq!(default, PathBuf::from("/xdg/node-gyp"));
            assert_eq!(
                node_gyp_devdir(&[], home),
                PathBuf::from("/home/u/.cache/node-gyp")
            );
        }
    }

    #[test]
    fn header_version_needs_all_three_macros() {
        assert_eq!(
            header_version(
                "#define NODE_MAJOR_VERSION 22\n#define NODE_MINOR_VERSION 15\n#define NODE_PATCH_VERSION 0\n"
            ),
            Some("22.15.0".to_string())
        );
        assert_eq!(
            header_version("#define NODE_MAJOR_VERSION 22\n#define NODE_MINOR_VERSION 15\n"),
            None
        );
        assert_eq!(header_version("#define NODE_MAJOR_VERSION_X 22\n"), None);
    }
}
