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
//! before it is COMMITTED into the store (extraction streams into a quarantine
//! temp dir concurrently with the download — #496; the rename into the store is
//! what the checksum gates). GPG signature verification is intentionally NOT a v0.1 gate
//! (ratified by the maintainer 2026-05-30 — GPG-by-default is an ecosystem outlier and
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
/// a v0.1 gate (HTTPS+SHA-256 is the trust root; ratified by the maintainer 2026-05-30, see
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
///      2026-06-11 (the maintainer): an existing file + existing key beats inventing a
///      `NODE_*` var nobody else reads; `.npmrc` alone can't express this (its
///      `registry=` is the npm registry, not nodejs.org). Transport config, not
///      a pin channel — outside the "no pnpm-specific channels" rule's intent.
///   3. The defaults: nodejs.org/dist (glibc), unofficial-builds (musl).
///
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
/// version dir `<store_root>/node/<version>/`. Install output on STDERR (never
/// stdout), no prompt, matching the PM provisioner in `pm::provision`:
///
/// ```text
/// Using Node.js 26.3.0 (resolved from .node-version)
/// Installing from nodejs.org... (29 MB)
/// Installed in 6.8s
/// ```
///
/// The `Using` line states the resolved version + pin provenance up front; the
/// `Installing` announce appears BEFORE the download (a slow fetch isn't
/// silence) and on a TTY the `Installed` line OVERWRITES it — a finished
/// session shows two lines. Non-TTY (CI logs, pipes) keeps all three.
/// `resolved_from` is preformatted pin provenance (e.g. `.node-version`) for
/// the `Using` line so logs say WHY this version was chosen; `None` for
/// explicit installs (`nub node install`), where the user just typed it.
///
/// Pipeline shape (#496): the `SHASUMS256.txt` fetch runs CONCURRENT with the
/// tarball download, and on the `.tar.xz` path the archive is decoded +
/// extracted while it streams in — into the quarantine `.tmp-` work dir, never
/// executed, never visible to lookups. The SHA-256 gate moves from
/// before-extraction to before-COMMIT: only after the streamed hash verifies
/// against `SHASUMS256.txt` is the tree `rename`d into the store; on mismatch
/// the guard wipes the work dir, so fail-closed holds (the unverified tarball
/// already landed on disk under the old order — the trust boundary is the
/// store commit, and that stays gated). The install is atomic — extract into a
/// sibling temp dir, then `rename` into place, so a crash or a concurrent run
/// never leaves a half-extracted dir masquerading as a cached version. The
/// Windows `.zip` needs random access, so it keeps download-then-extract
/// (still with the overlapped checksum fetch). An already-installed version
/// short-circuits with no network + no output.
pub fn provision_node(
    version: &NodeVersion,
    host: &HostTarget,
    store_root: &Path,
    resolved_from: Option<&str>,
) -> Result<PathBuf> {
    provision_node_from(
        version,
        host,
        store_root,
        resolved_from,
        &resolve_mirror_base(host),
    )
}

/// [`provision_node`] with the mirror base explicit — the seam the local-server
/// provisioning tests drive (env mutation would race the parallel harness).
pub fn provision_node_from(
    version: &NodeVersion,
    host: &HostTarget,
    store_root: &Path,
    resolved_from: Option<&str>,
    mirror_base: &str,
) -> Result<PathBuf> {
    let node_store = store_root.join("node");
    let final_dir = node_store.join(version.to_string());
    if version_dir_has_node(&final_dir) {
        return Ok(final_dir); // cache hit — silent
    }

    let art = node_artifact(version, host, mirror_base);
    // Overlapped with the tarball download below; joined at the verify gate. On
    // an early download error the thread is left to finish on its own (bounded
    // by the client timeout) — the process/caller isn't blocked on it.
    let shasums_thread = {
        let url = art.shasums_url.clone();
        std::thread::spawn(move || download::fetch_text(&url))
    };

    // Sibling temp dir on the same filesystem → the final placement is an atomic
    // rename. The guard cleans it up on every exit path.
    let work = node_store.join(format!(".tmp-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let _guard = WorkGuard(work.clone());

    let started = Instant::now();
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    match resolved_from {
        Some(p) => eprintln!("Using Node.js {version} (resolved from {p})"),
        None => eprintln!("Using Node.js {version}"),
    }
    let mut announced = false;
    let mut on_progress = |_done: u64, total: Option<u64>| {
        if !announced {
            announced = true;
            let size = match total {
                Some(t) => format!(" ({} MB)", t / 1_000_000),
                None => String::new(),
            };
            if tty {
                eprint!("Installing from nodejs.org...{size}");
            } else {
                eprintln!("Installing from nodejs.org...{size}");
            }
        }
    };

    // `.tar.xz` streams straight into the extractor; `.zip` (Windows) downloads
    // to disk first (central directory needs random access).
    let (sha, streamed_top) = if art.tarball_filename.ends_with(".tar.xz") {
        let (sha, top) =
            download::download_and_extract_tar_xz(&art.tarball_url, &work, &mut on_progress)
                .with_context(|| format!("downloading Node {version}"))?;
        (sha, Some(top))
    } else {
        let tarball = work.join(&art.tarball_filename);
        let sha = download::download_to_file(&art.tarball_url, &tarball, &mut on_progress)
            .with_context(|| format!("downloading Node {version}"))?;
        (sha, None)
    };

    let shasums = shasums_thread
        .join()
        .map_err(|_| anyhow::anyhow!("checksum fetch thread panicked"))?
        .with_context(|| format!("fetching checksums for Node {version}"))?;
    // The commit gate: nothing below runs — and the streamed tree never leaves
    // the guarded work dir — unless the hash matches.
    download::verify_checksum(&sha, &shasums, &art.tarball_filename)?;

    let extracted = match streamed_top {
        Some(top) => top,
        None => extract::extract_archive(&work.join(&art.tarball_filename), &work)?,
    };

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

    // \r + clear-to-EOL rewrites the Installing line on a TTY (it was printed
    // without a newline there); non-TTY just gets a third line.
    let rewrite = if tty { "\r\x1b[K" } else { "" };
    eprintln!(
        "{rewrite}Installed in {:.1}s",
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

    /// A minimal HTTP server for the streamed-provisioning tests: serves
    /// `SHASUMS256.txt` and one tarball from memory on a loopback port. Each
    /// connection is handled on its own thread — the checksum fetch and the
    /// tarball download arrive CONCURRENTLY by design. The daemon accept loop
    /// dies with the test process.
    fn serve_dist(
        shasums: String,
        tarball_name: String,
        tarball: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Counts tarball GETs — the retry-behavior tests assert on it.
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_out = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let shasums = shasums.clone();
                let tarball_name = tarball_name.clone();
                let tarball = tarball.clone();
                let hits = hits.clone();
                std::thread::spawn(move || {
                    let mut req = [0u8; 2048];
                    let n = stream.read(&mut req).unwrap_or(0);
                    let head = String::from_utf8_lossy(&req[..n]);
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let (status, body): (&str, Vec<u8>) = if path.ends_with("/SHASUMS256.txt") {
                        ("200 OK", shasums.into_bytes())
                    } else if path.ends_with(&tarball_name) {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        ("200 OK", tarball)
                    } else {
                        ("404 Not Found", Vec::new())
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(&body);
                });
            }
        });
        (base, hits_out)
    }

    /// A tiny but valid Node-shaped `.tar.xz` (top dir with `bin/node`) built in
    /// memory, plus its SHA-256 — the fixture the streamed pipeline consumes.
    fn node_fixture_tar_xz(top: &str) -> (Vec<u8>, String) {
        let mut bytes = Vec::new();
        {
            let enc = liblzma::write::XzEncoder::new(&mut bytes, 6);
            let mut builder = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_size(3);
            h.set_mode(0o755);
            h.set_cksum();
            builder
                .append_data(&mut h, format!("{top}/bin/node"), &b"#!\n"[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        use sha2::{Digest, Sha256};
        let sha = format!("{:x}", Sha256::digest(&bytes));
        (bytes, sha)
    }

    /// End-to-end streamed provisioning against a local server: concurrent
    /// checksum fetch + streamed download/extract + verify + atomic commit, no
    /// real network. Asserts the installed layout, the cleaned work dir, and the
    /// second-call cache hit.
    #[test]
    fn provision_streams_and_commits_after_verify() {
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let version = ver("99.99.99");
        let name = "node-v99.99.99-linux-x64";
        let (tarball, sha) = node_fixture_tar_xz(name);
        let (base, _hits) = serve_dist(
            format!("{sha}  {name}.tar.xz\n"),
            format!("{name}.tar.xz"),
            tarball,
        );
        let store = std::env::temp_dir().join(format!("nub-prov-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);

        let dir = provision_node_from(&version, &h, &store, None, &base).expect("provision");
        assert!(dir.join("bin").join("node").is_file());
        // The quarantine work dir must be gone after commit.
        let leftovers: Vec<_> = std::fs::read_dir(store.join("node"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "work dir leaked: {leftovers:?}");
        // Second call: silent cache hit, no server contact needed.
        let again = provision_node_from(&version, &h, &store, None, &base).expect("cache hit");
        assert_eq!(again, dir);
        let _ = std::fs::remove_dir_all(&store);
    }

    /// The commit gate: a forged/mismatched checksum must abort AFTER the
    /// streamed extraction but BEFORE anything reaches the store, leaving no
    /// version dir and no work-dir residue.
    #[test]
    fn provision_refuses_to_commit_on_checksum_mismatch() {
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let version = ver("99.99.98");
        let name = "node-v99.99.98-linux-x64";
        let (tarball, _sha) = node_fixture_tar_xz(name);
        let wrong = "0".repeat(64);
        let (base, _hits) = serve_dist(
            format!("{wrong}  {name}.tar.xz\n"),
            format!("{name}.tar.xz"),
            tarball,
        );
        let store = std::env::temp_dir().join(format!("nub-prov-mismatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);

        let err = provision_node_from(&version, &h, &store, None, &base).unwrap_err();
        assert!(
            format!("{err:#}").contains("checksum mismatch"),
            "unexpected error: {err:#}"
        );
        assert!(
            !store.join("node").join(version.to_string()).exists(),
            "a mismatched tarball must never be committed"
        );
        let leftovers: Vec<_> = std::fs::read_dir(store.join("node"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "work dir leaked: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&store);
    }

    /// A mid-stream extraction failure (corrupt archive under a truthful
    /// Content-Length) must surface the EXTRACTION error, fail fast (one
    /// download attempt, no transient retry), and leave nothing behind. Guards
    /// the exit-reason precedence in `download_and_extract_tar_xz`: without it,
    /// the extractor's early exit reads as a short body and retries 3× with a
    /// misleading error. The body is sized well past the channel's backpressure
    /// window so the extractor provably dies while bytes are still unread.
    #[test]
    fn provision_fails_fast_on_mid_stream_corruption() {
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let version = ver("99.99.97");
        let name = "node-v99.99.97-linux-x64";
        // Valid xz magic, then 4 MiB of garbage — the decoder errors on the
        // first chunk while the download still has megabytes unread.
        let mut corrupt = b"\xfd7zXZ\x00".to_vec();
        corrupt.extend(std::iter::repeat_n(0xAAu8, 4 << 20));
        use sha2::{Digest, Sha256};
        let sha = format!("{:x}", Sha256::digest(&corrupt));
        let (base, hits) = serve_dist(
            format!("{sha}  {name}.tar.xz\n"),
            format!("{name}.tar.xz"),
            corrupt,
        );
        let store = std::env::temp_dir().join(format!("nub-prov-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);

        let err = provision_node_from(&version, &h, &store, None, &base).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("short response body"),
            "the extraction error must not be masked as a short body: {msg}"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a corrupt archive is fatal — it must not be re-downloaded"
        );
        assert!(!store.join("node").join(version.to_string()).exists());
        let _ = std::fs::remove_dir_all(&store);
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

        let dir = provision_node(&version, &host, &store, None).expect("provision");
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
        let again = provision_node(&version, &host, &store, None).expect("cache hit");
        assert_eq!(again, dir);
        let _ = std::fs::remove_dir_all(&store);
    }
}
