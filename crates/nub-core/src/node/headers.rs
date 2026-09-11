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
//! The second guard is [`TARGET_SELECTING_OPTS`], and it is the one that keeps
//! this correct rather than merely fast: a caller building for a DIFFERENT
//! runtime has already chosen its headers, and because an `npm_config_*` value
//! overrides node-gyp's own argv, answering on its behalf would silently win.
//!
//! Windows is excluded on purpose: the official zip ships neither headers nor
//! `node.lib`, and `nodedir` there also moves node-gyp's `node.lib` lookup to
//! `<nodedir>/$(Configuration)/`, so the download path stays the working one.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// node-gyp options that CHOOSE what to compile against. Any one of them means
/// the caller has already answered the question, so nub must not answer it too.
///
/// WHY THIS GUARD IS NOT OPTIONAL. `npm_config_*` does not merely supply a
/// default — node-gyp parses argv with nopt into `this.opts` and then loops the
/// environment assigning `this.opts[name] = value` unconditionally
/// (`lib/node-gyp.js`), so an env value OVERWRITES an explicit
/// `node-gyp --nodedir=…` on the command line. And `configure` takes the
/// nodedir branch the moment nodedir is populated, never reaching the `else`
/// that downloads headers for `--target` (`lib/configure.js` `getNodeDir`).
/// Set nodedir blindly and an Electron or alternate-runtime rebuild — which
/// selects its headers through `npm_config_target` + `npm_config_disturl` —
/// silently compiles against the running Node instead, producing exactly the
/// wrong-ABI addon this module exists to avoid. Failing closed costs one header
/// download; failing open costs a binary that loads and misbehaves.
///
/// Both spellings of the dist URL are listed because node-gyp itself tests both
/// (`lib/create-config-gypi.js`: `gyp.opts.disturl || gyp.opts['dist-url']`).
/// `runtime` is not read by node-gyp core, but `@electron/rebuild` and
/// node-pre-gyp set it, so its presence marks a non-Node target.
const TARGET_SELECTING_OPTS: &[&str] = &["nodedir", "target", "disturl", "dist-url", "runtime"];

/// The two prefixes node-gyp folds into its options, in its own precedence
/// order. A package's `config.node-gyp.<key>` arrives as the second one.
const OPT_ENV_PREFIXES: &[&str] = &["npm_config_", "npm_package_config_node_gyp_"];

/// The value to export as `npm_config_nodedir` for scripts that run under
/// `node_execpath`, or `None` when node-gyp should keep its own header
/// handling. `script` is the script text when the caller has it, so a command
/// that selects its own headers in argv is left alone too.
pub fn node_gyp_nodedir(
    node_execpath: &Path,
    version: &str,
    script: Option<&str>,
) -> Option<PathBuf> {
    let env_keys = std::env::vars_os()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    nodedir_for(
        node_execpath,
        version,
        env_keys.iter().map(String::as_str),
        script,
    )
}

/// The decision, with the environment passed in so it is testable without
/// touching the ambient process state.
fn nodedir_for<'a>(
    node_execpath: &Path,
    version: &str,
    env_keys: impl Iterator<Item = &'a str>,
    script: Option<&str>,
) -> Option<PathBuf> {
    if cfg!(windows) || node_execpath.as_os_str().is_empty() || version.is_empty() {
        return None;
    }
    if env_keys.into_iter().any(selects_its_own_headers) {
        return None;
    }
    // The env check above cannot see a flag the script passes on the command
    // line, and the env would OVERRIDE that flag rather than yield to it. A
    // substring match over the script text is coarse on purpose: a false
    // positive only returns node-gyp to the behavior it had before this
    // existed, while a miss is a wrong-ABI build.
    if script.is_some_and(argv_selects_headers) {
        return None;
    }
    headers_root(node_execpath, version)
}

/// True when a command line names one of [`TARGET_SELECTING_OPTS`] itself.
/// Coarse on purpose: a false positive only returns node-gyp to the behavior it
/// had before any of this existed, while a miss is a wrong-ABI build.
pub fn argv_selects_headers(text: &str) -> bool {
    TARGET_SELECTING_OPTS
        .iter()
        .any(|opt| text.contains(&format!("--{opt}")))
}

/// True for an env key naming one of [`TARGET_SELECTING_OPTS`] under either
/// prefix. node-gyp lowercases and maps `_` to `-` before looking a key up, so
/// `npm_config_DIST_URL` and `npm_config_dist-url` are the same option and both
/// have to match here.
fn selects_its_own_headers(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    OPT_ENV_PREFIXES
        .iter()
        .find_map(|prefix| lower.strip_prefix(prefix))
        .is_some_and(|name| {
            let name = name.replace('_', "-");
            TARGET_SELECTING_OPTS.contains(&name.as_str())
        })
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

    /// The baseline the guard tests vary from: nothing selected, headers match,
    /// so the local root is used.
    #[test]
    fn a_plain_environment_gets_the_local_headers() {
        let node = FakeNode::new("plain", Some("26.8.2"));
        assert_eq!(
            nodedir_for(&node.node(), "26.8.2", ["PATH", "HOME"].into_iter(), None),
            if cfg!(windows) {
                None
            } else {
                Some(node.root.clone())
            }
        );
    }

    /// An Electron rebuild selects its headers with `npm_config_target` plus
    /// `npm_config_disturl`. node-gyp takes the nodedir branch before it ever
    /// reads those, so nub answering here would compile an Electron addon
    /// against the running Node — a wrong-ABI binary that still loads.
    #[cfg(unix)]
    #[test]
    fn a_caller_that_selected_its_own_target_is_left_alone() {
        let node = FakeNode::new("selected", Some("26.8.2"));
        for key in [
            "npm_config_nodedir",
            "npm_config_target",
            "npm_config_disturl",
            "npm_config_dist_url",
            "npm_config_runtime",
            // node-gyp lowercases the key before looking it up.
            "npm_config_TARGET",
            // A package's own `config.node-gyp.target`, which outranks the above.
            "npm_package_config_node_gyp_target",
        ] {
            assert_eq!(
                nodedir_for(&node.node(), "26.8.2", ["PATH", key].into_iter(), None),
                None,
                "{key} selects the headers, so nub must not"
            );
        }
    }

    /// An unrelated `npm_config_*` must not disable the whole mechanism.
    #[cfg(unix)]
    #[test]
    fn an_unrelated_npm_config_key_does_not_disable_it() {
        let node = FakeNode::new("unrelated", Some("26.8.2"));
        for key in [
            "npm_config_registry",
            "npm_config_devdir",
            "npm_config_python",
        ] {
            assert_eq!(
                nodedir_for(&node.node(), "26.8.2", ["PATH", key].into_iter(), None),
                Some(node.root.clone()),
                "{key} chooses no headers, so the local ones still apply"
            );
        }
    }

    /// The env overrides node-gyp's argv rather than yielding to it, so a
    /// script passing its own flag has to be detected from the script text.
    #[cfg(unix)]
    #[test]
    fn a_script_that_passes_its_own_flag_is_left_alone() {
        let node = FakeNode::new("argv", Some("26.8.2"));
        let decide =
            |script: &str| nodedir_for(&node.node(), "26.8.2", ["PATH"].into_iter(), Some(script));
        assert_eq!(decide("node-gyp rebuild --nodedir=/opt/headers"), None);
        assert_eq!(decide("node-gyp rebuild --target=39.0.0"), None);
        assert_eq!(
            decide("node-gyp rebuild --dist-url=https://electronjs.org/headers"),
            None
        );
        assert_eq!(
            decide("node-gyp rebuild --verbose"),
            Some(node.root.clone()),
            "an ordinary rebuild still gets the local headers"
        );
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
