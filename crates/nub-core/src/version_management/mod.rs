//! Node version provisioning — resolve a pin to a concrete stock Node, check
//! nub's download cache, and (when absent) download + verify + extract from
//! nodejs.org. Spec: `wiki/runtime/node-version-management.md`; structure modeled
//! MIT-clean on pacquet's `engine-runtime-node-resolver`.
//!
//! Host platform / arch normalization (`HostTarget`) and dist artifact-address
//! construction (`node_artifact`) live here; the download (`download`), xz
//! extraction (`extract`), and dist-index resolver (`node_index`) are sibling
//! submodules. Security posture: HTTPS authenticates `SHASUMS256.txt` (TLS to
//! nodejs.org), a mandatory fail-closed SHA-256 check authenticates the tarball
//! before extraction. GPG signature verification is intentionally NOT a v0.1 gate
//! (ratified by Colin 2026-05-30 — GPG-by-default is an ecosystem outlier and
//! bundled keys break on Node's key rotation; see the spec's Decisions log and
//! `wiki/research/node-provisioning-implementation.md`).

pub mod download;
pub mod extract;
pub mod manage;
pub mod node_index;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::node::version::NodeVersion;

/// Host operating system, in Node's dist-token vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeOs {
    Darwin,
    Linux,
    Windows,
}

/// Host CPU architecture, in Node's dist-token vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeArch {
    X64,
    Arm64,
    Armv7l,
    Ppc64le,
    S390x,
    X86,
}

/// The host nub is running on, normalized to what nodejs.org/dist publishes. nub
/// ships a per-platform binary, so `std::env::consts::{OS,ARCH}` already reflect
/// the host; only musl needs a runtime probe (the official dist is glibc-only, so
/// a musl host must route to unofficial-builds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTarget {
    pub os: NodeOs,
    pub arch: NodeArch,
    /// Linux/musl host — official dist has no musl build, so the address routes
    /// to unofficial-builds and the token gains a `-musl` suffix.
    pub musl: bool,
}

impl HostTarget {
    /// Detect the host. Returns `None` for an OS/arch nodejs.org doesn't publish.
    pub fn detect() -> Option<Self> {
        let os = match std::env::consts::OS {
            "macos" => NodeOs::Darwin,
            "linux" => NodeOs::Linux,
            "windows" => NodeOs::Windows,
            _ => return None,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => NodeArch::X64,
            "aarch64" => NodeArch::Arm64,
            "arm" => NodeArch::Armv7l,
            "powerpc64" => NodeArch::Ppc64le, // dist ships ppc64le (LE) only
            "s390x" => NodeArch::S390x,
            "x86" => NodeArch::X86,
            _ => return None,
        };
        let musl = os == NodeOs::Linux && detect_musl();
        Some(Self { os, arch, musl })
    }

    /// The `<platform>-<arch>` token in a dist filename, e.g. `darwin-arm64`,
    /// `linux-x64`, `linux-x64-musl`, `win-arm64`.
    pub fn platform_token(&self) -> String {
        let os = match self.os {
            NodeOs::Darwin => "darwin",
            NodeOs::Linux => "linux",
            NodeOs::Windows => "win",
        };
        let arch = match self.arch {
            NodeArch::X64 => "x64",
            NodeArch::Arm64 => "arm64",
            NodeArch::Armv7l => "armv7l",
            NodeArch::Ppc64le => "ppc64le",
            NodeArch::S390x => "s390x",
            NodeArch::X86 => "x86",
        };
        if self.musl {
            format!("{os}-{arch}-musl")
        } else {
            format!("{os}-{arch}")
        }
    }

    /// Archive extension dist uses for this OS: `zip` on Windows, `tar.xz`
    /// elsewhere. (`.tar.xz` is also published for Windows, but `.zip` needs no
    /// xz support — the extractor picks per this.)
    pub fn archive_ext(&self) -> &'static str {
        match self.os {
            NodeOs::Windows => "zip",
            _ => "tar.xz",
        }
    }
}

/// Detect a musl libc host via the dynamic-loader presence under `/lib` (the
/// spec's prescription — cheap + reliable), falling back to the compile-time
/// `target_env`. A glibc-built nub on a musl host (uncommon) is still caught by
/// the `/lib/ld-musl-*` probe.
fn detect_musl() -> bool {
    if let Ok(entries) = std::fs::read_dir("/lib") {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("ld-musl-"))
            {
                return true;
            }
        }
    }
    cfg!(target_env = "musl")
}

/// The dist addresses for one Node version + host: the tarball plus the
/// `SHASUMS256.txt` whose SHA-256 row authenticates it before extraction. No
/// `SHASUMS256.txt.sig` address — GPG signature verification is intentionally not
/// a v0.1 gate (HTTPS+SHA-256 is the trust root; ratified by Colin 2026-05-30, see
/// `wiki/runtime/node-version-management.md` Decisions). The `.sig` URL is a
/// one-line `format!` to reconstruct if the deferred best-effort GPG layer lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeArtifact {
    pub tarball_url: String,
    pub shasums_url: String,
    /// The tarball's basename — the key to find its line in `SHASUMS256.txt`.
    pub tarball_filename: String,
}

/// Build the dist addresses for `version` on `host`, rooted at `base` (the mirror
/// base URL, e.g. `https://nodejs.org/dist` — or unofficial-builds for musl; see
/// [`resolve_mirror_base`]). Pure: no network, no env.
pub fn node_artifact(version: &NodeVersion, host: &HostTarget, base: &str) -> NodeArtifact {
    let base = base.trim_end_matches('/');
    let filename = format!(
        "node-v{version}-{}.{}",
        host.platform_token(),
        host.archive_ext()
    );
    let dir = format!("{base}/v{version}");
    NodeArtifact {
        tarball_url: format!("{dir}/{filename}"),
        shasums_url: format!("{dir}/SHASUMS256.txt"),
        tarball_filename: filename,
    }
}

/// The mirror base for `host`: the ecosystem-standard `NODEJS_ORG_MIRROR` env
/// override (the nodenv / `n` convention — NODE-namespaced, not a brand
/// violation) if set, else `nodejs.org/dist` (glibc) or unofficial-builds (musl).
pub fn resolve_mirror_base(host: &HostTarget) -> String {
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    resolve_mirror_base_in(host, &root)
}

/// [`resolve_mirror_base`] with the project root made explicit (the testable
/// body). Precedence:
///   1. `NODEJS_ORG_MIRROR` — the vendor-neutral env convention (nvm/n).
///   2. `.npmrc` `node-mirror:release=` — pnpm's existing key for "fetch Node
///      dists from this mirror" (project `.npmrc`, then `~/.npmrc`). Adopted
///      2026-06-11 (Colin): an existing file + existing key beats inventing a
///      `NODE_*` var nobody else reads; `.npmrc` alone can't express this (its
///      `registry=` is the npm registry, not nodejs.org). Transport config, not
///      a pin channel — outside the "no pnpm-specific channels" rule's intent.
///   3. The defaults: nodejs.org/dist (glibc), unofficial-builds (musl).
/// An explicit mirror (env or key) overrides BOTH libc flavors — it's a user
/// override, trusted as given; musl users need their mirror to carry the
/// unofficial-builds layout (documented on the site).
pub fn resolve_mirror_base_in(host: &HostTarget, project_root: &std::path::Path) -> String {
    if let Ok(m) = std::env::var("NODEJS_ORG_MIRROR") {
        let m = m.trim_end_matches('/');
        if !m.is_empty() {
            return m.to_string();
        }
    }
    if let Some(m) = crate::workspace::scripts::npmrc_value(project_root, "node-mirror:release") {
        let m = m.trim_end_matches('/');
        if !m.is_empty() {
            return m.to_string();
        }
    }
    if host.musl {
        "https://unofficial-builds.nodejs.org/download/release".to_string()
    } else {
        "https://nodejs.org/dist".to_string()
    }
}

/// True when a Node binary is present under a version dir (`bin/node` on unix,
/// `node.exe` on Windows) — the cache-hit / install-complete signal.
fn version_dir_has_node(version_dir: &Path) -> bool {
    version_dir.join("bin").join("node").is_file() || version_dir.join("node.exe").is_file()
}

/// Best-effort cleanup of the temp work dir on any return path.
struct WorkGuard(PathBuf);
impl Drop for WorkGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Download + verify + extract a stock Node into nub's store, returning the
/// version dir `<store_root>/node/<version>/`. uv-style silent install: a single
/// `Installing…` line + a `✓ Installed…` line on STDERR (never stdout), no
/// prompt, identical in TTY/non-TTY. The SHA-256 is verified BEFORE extraction
/// (executables landing on disk), and the install is atomic — extract into a
/// sibling temp dir, then `rename` into place, so a crash or a concurrent run
/// never leaves a half-extracted dir masquerading as a cached version. An
/// already-installed version short-circuits with no network + no output.
pub fn provision_node(
    version: &NodeVersion,
    host: &HostTarget,
    store_root: &Path,
) -> Result<PathBuf> {
    let node_store = store_root.join("node");
    let final_dir = node_store.join(version.to_string());
    if version_dir_has_node(&final_dir) {
        return Ok(final_dir); // cache hit — silent
    }

    let art = node_artifact(version, host, &resolve_mirror_base(host));
    let shasums = download::fetch_text(&art.shasums_url)
        .with_context(|| format!("fetching checksums for Node {version}"))?;

    // Sibling temp dir on the same filesystem → the final placement is an atomic
    // rename. The guard cleans it up on every exit path.
    let work = node_store.join(format!(".tmp-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let _guard = WorkGuard(work.clone());

    let started = Instant::now();
    let tarball = work.join(&art.tarball_filename);
    let mut announced = false;
    let sha = download::download_to_file(&art.tarball_url, &tarball, |_done, total| {
        if !announced {
            announced = true;
            match total {
                Some(t) => eprintln!(
                    "Installing Node {version} from nodejs.org ({} MB)...",
                    t / 1_000_000
                ),
                None => eprintln!("Installing Node {version} from nodejs.org..."),
            }
        }
    })
    .with_context(|| format!("downloading Node {version}"))?;

    // Verify BEFORE extracting.
    download::verify_checksum(&sha, &shasums, &art.tarball_filename)?;

    let extracted = extract::extract_archive(&tarball, &work)?;

    // Atomic place. If a concurrent run already installed it, keep theirs.
    if !version_dir_has_node(&final_dir) {
        std::fs::create_dir_all(&node_store).ok();
        if let Err(e) = std::fs::rename(&extracted, &final_dir) {
            if !version_dir_has_node(&final_dir) {
                return Err(e).with_context(|| {
                    format!("installing Node {version} into {}", final_dir.display())
                });
            }
        }
    }

    eprintln!(
        "✓ Installed Node {version} in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(final_dir)
}

#[cfg(test)]
mod tests {
    // node-mirror:release — the pnpm .npmrc key adopted for Node-dist mirrors.
    // Env precedence (NODEJS_ORG_MIRROR first) is documented, not asserted:
    // mutating process env races the parallel test harness.
    #[test]
    fn npmrc_node_mirror_key_overrides_the_dist_base() {
        if std::env::var_os("NODEJS_ORG_MIRROR").is_some() {
            return; // ambient env outranks the key; skip rather than mutate env
        }
        let dir = std::env::temp_dir().join(format!("nub-mirror-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".npmrc"),
            "node-mirror:release=https://mirror.corp.example/node/\n",
        )
        .unwrap();
        let glibc = super::HostTarget {
            os: super::NodeOs::Darwin,
            arch: super::NodeArch::Arm64,
            musl: false,
        };
        let musl = super::HostTarget {
            os: super::NodeOs::Linux,
            arch: super::NodeArch::X64,
            musl: true,
        };
        assert_eq!(
            super::resolve_mirror_base_in(&glibc, &dir),
            "https://mirror.corp.example/node",
            "the key overrides the base, trailing slash trimmed"
        );
        assert_eq!(
            super::resolve_mirror_base_in(&musl, &dir),
            "https://mirror.corp.example/node",
            "an explicit mirror overrides the musl default too"
        );
        let empty = dir.join("none");
        std::fs::create_dir_all(&empty).unwrap();
        if crate::workspace::scripts::npmrc_value(&empty, "node-mirror:release").is_none() {
            assert!(
                super::resolve_mirror_base_in(&glibc, &empty).starts_with("https://nodejs.org"),
                "no key, no env: the public default"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn host(os: NodeOs, arch: NodeArch, musl: bool) -> HostTarget {
        HostTarget { os, arch, musl }
    }

    fn ver(s: &str) -> NodeVersion {
        s.parse().unwrap()
    }

    #[test]
    fn platform_tokens_match_dist_filenames() {
        assert_eq!(
            host(NodeOs::Darwin, NodeArch::Arm64, false).platform_token(),
            "darwin-arm64"
        );
        assert_eq!(
            host(NodeOs::Darwin, NodeArch::X64, false).platform_token(),
            "darwin-x64"
        );
        assert_eq!(
            host(NodeOs::Linux, NodeArch::X64, false).platform_token(),
            "linux-x64"
        );
        assert_eq!(
            host(NodeOs::Linux, NodeArch::Arm64, false).platform_token(),
            "linux-arm64"
        );
        assert_eq!(
            host(NodeOs::Linux, NodeArch::Armv7l, false).platform_token(),
            "linux-armv7l"
        );
        assert_eq!(
            host(NodeOs::Linux, NodeArch::Ppc64le, false).platform_token(),
            "linux-ppc64le"
        );
        assert_eq!(
            host(NodeOs::Linux, NodeArch::S390x, false).platform_token(),
            "linux-s390x"
        );
        assert_eq!(
            host(NodeOs::Windows, NodeArch::X64, false).platform_token(),
            "win-x64"
        );
        assert_eq!(
            host(NodeOs::Windows, NodeArch::Arm64, false).platform_token(),
            "win-arm64"
        );
        // musl appends the suffix (unofficial-builds naming).
        assert_eq!(
            host(NodeOs::Linux, NodeArch::X64, true).platform_token(),
            "linux-x64-musl"
        );
    }

    #[test]
    fn archive_ext_is_zip_on_windows_else_tar_xz() {
        assert_eq!(
            host(NodeOs::Windows, NodeArch::X64, false).archive_ext(),
            "zip"
        );
        assert_eq!(
            host(NodeOs::Darwin, NodeArch::Arm64, false).archive_ext(),
            "tar.xz"
        );
        assert_eq!(
            host(NodeOs::Linux, NodeArch::X64, false).archive_ext(),
            "tar.xz"
        );
    }

    #[test]
    fn artifact_urls_match_the_real_dist_layout() {
        let a = node_artifact(
            &ver("22.13.0"),
            &host(NodeOs::Darwin, NodeArch::Arm64, false),
            "https://nodejs.org/dist",
        );
        assert_eq!(
            a.tarball_url,
            "https://nodejs.org/dist/v22.13.0/node-v22.13.0-darwin-arm64.tar.xz"
        );
        assert_eq!(
            a.shasums_url,
            "https://nodejs.org/dist/v22.13.0/SHASUMS256.txt"
        );
        assert_eq!(a.tarball_filename, "node-v22.13.0-darwin-arm64.tar.xz");
    }

    #[test]
    fn artifact_trims_trailing_slash_and_handles_windows_zip() {
        let a = node_artifact(
            &ver("20.11.0"),
            &host(NodeOs::Windows, NodeArch::X64, false),
            "https://nodejs.org/dist/",
        );
        assert_eq!(
            a.tarball_url,
            "https://nodejs.org/dist/v20.11.0/node-v20.11.0-win-x64.zip"
        );
        assert_eq!(a.tarball_filename, "node-v20.11.0-win-x64.zip");
    }

    #[test]
    fn musl_artifact_uses_the_musl_token() {
        // The musl BASE is chosen by resolve_mirror_base (unofficial-builds); the
        // token itself carries the -musl suffix regardless.
        let a = node_artifact(
            &ver("22.13.0"),
            &host(NodeOs::Linux, NodeArch::X64, true),
            "https://unofficial-builds.nodejs.org/download/release",
        );
        assert_eq!(
            a.tarball_url,
            "https://unofficial-builds.nodejs.org/download/release/v22.13.0/node-v22.13.0-linux-x64-musl.tar.xz"
        );
    }

    #[test]
    fn detect_resolves_this_host() {
        // The dev box + every CI runner is a published platform.
        let h = HostTarget::detect().expect("host should be a published Node platform");
        assert!(!h.platform_token().is_empty());
    }

    /// Full real provisioning: download + verify + extract Node 22.13.0 into a
    /// temp store, confirm the installed binary runs + reports the right version,
    /// and that a second call is a cache hit. `#[ignore]` — network + ~25MB.
    ///   cargo test -p nub-core --lib version_management::tests::provision -- --ignored
    #[test]
    #[ignore = "network: provisions a real Node (~25MB) into a temp store"]
    fn provision_real_node_into_store() {
        let host = HostTarget::detect().unwrap();
        let version: NodeVersion = "22.13.0".parse().unwrap();
        let store = std::env::temp_dir().join(format!("nub-prov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);

        let dir = provision_node(&version, &host, &store).expect("provision");
        assert!(
            version_dir_has_node(&dir),
            "installed node binary must be present"
        );
        let out = std::process::Command::new(dir.join("bin").join("node"))
            .arg("--version")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "v22.13.0");

        // Second call short-circuits (cache hit) to the same dir, no re-download.
        let again = provision_node(&version, &host, &store).expect("cache hit");
        assert_eq!(again, dir);
        let _ = std::fs::remove_dir_all(&store);
    }
}
