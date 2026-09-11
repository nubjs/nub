//! Pointing node-gyp at the headers the running Node already carries.
//!
//! node-gyp compiles an addon against the headers of the Node it targets and,
//! unless told otherwise, downloads `node-v<ver>-headers.tar.gz` from
//! nodejs.org into `~/.cache/node-gyp/<ver>` on first use. Every Node nub
//! provisions (and every nvm/fnm/volta/Homebrew one) already ships those
//! headers under `<root>/include/node/`, but node-gyp reads them only when the
//! binary was configured with `--use-prefix-to-find-headers` — a distro
//! packager option the official tarballs do not set (`node-gyp/lib/configure.js`,
//! `getNodeDir`). Exporting `npm_config_nodedir=<root>` makes node-gyp compile
//! against the local copy and skip the download outright (`lib/install.js`:
//! "--nodedir flag was passed; skipping install"), which is what lets a
//! from-source build run offline and inside a network-denied build jail.
//!
//! The version check mirrors node-gyp's own prefix branch. node-gyp does not
//! re-validate a `nodedir` it was handed, so a root whose `node_version.h`
//! names a different release than the binary running the script — a distro
//! `/usr/bin/node` beside a stale `libnode-dev` — must never be exported.
//!
//! Windows is excluded on purpose: the official zip ships neither headers nor
//! `node.lib`, and `nodedir` there also moves node-gyp's `node.lib` lookup to
//! `<nodedir>/$(Configuration)/`, so the download path stays the working one.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The value to export as `npm_config_nodedir` for scripts that run under
/// `node_execpath`, or `None` when node-gyp should keep its own header
/// download. A user-set `npm_config_nodedir` (any letter case — node-gyp
/// matches the prefix case-insensitively) always wins.
pub fn node_gyp_nodedir(node_execpath: &Path, version: &str) -> Option<PathBuf> {
    if cfg!(windows) || node_execpath.as_os_str().is_empty() || version.is_empty() {
        return None;
    }
    if std::env::vars_os().any(|(key, _)| key.eq_ignore_ascii_case("npm_config_nodedir")) {
        return None;
    }
    headers_root(node_execpath, version)
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

    struct FakeNode {
        root: PathBuf,
    }

    impl FakeNode {
        fn new(name: &str, header_version: Option<&str>) -> Self {
            let root = std::env::temp_dir()
                .join(format!("nub-node-headers-{}-{name}", std::process::id()));
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
