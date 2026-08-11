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
pub(crate) mod extract;
pub mod manage;
pub mod node_index;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::node::version::NodeVersion;

static LICENSE_REPAIR_NONCE: AtomicU64 = AtomicU64::new(0);

/// Local cache commit marker binding the embedded Node executable and aggregate
/// `LICENSE` to a checksum-verified distribution. It is provenance, not an oracle
/// against same-UID cache forgery, which is outside this cache's trust boundary.
const NODE_LICENSE_ATTESTATION: &str = ".nub-node-license-attestation-v1";

/// Extract one verified stock Node distribution archive into `dest_parent`.
///
/// This is the narrow extraction seam used by the compile launcher after its
/// shell downloader verifies `SHASUMS256.txt`. It accepts only Node's host
/// archive formats (`.tar.xz` and `.zip`) and keeps the same decompressed-byte,
/// per-entry, entry-count, and path-traversal limits as normal Node
/// provisioning.
pub fn extract_verified_node_archive(archive: &Path, dest_parent: &Path) -> Result<PathBuf> {
    extract::extract_archive(archive, dest_parent)
}

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
/// the host; the libc flavor is likewise the running binary's own build target
/// (`detect_musl`), since the official dist is glibc-only and a musl host must
/// route to unofficial-builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTarget {
    os: NodeOs,
    arch: NodeArch,
    /// Linux/musl host — official dist has no musl build, so the address routes
    /// to unofficial-builds and the token gains a `-musl` suffix.
    musl: bool,
}

impl HostTarget {
    /// Detect the host. Returns `None` for an OS/arch nodejs.org doesn't publish.
    pub(crate) fn detect() -> Option<Self> {
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
    fn platform_token(&self) -> String {
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
    fn archive_ext(&self) -> &'static str {
        match self.os {
            NodeOs::Windows => "zip",
            _ => "tar.xz",
        }
    }

    /// The exact `files` value in the Node distribution index for the archive
    /// [`node_artifact`] will fetch. The index uses `osx` (not the filename's
    /// `darwin`), marks Darwin tarballs and Windows zip files explicitly, and
    /// carries musl in the Linux key.
    fn index_artifact_key(&self) -> String {
        let arch = match self.arch {
            NodeArch::X64 => "x64",
            NodeArch::Arm64 => "arm64",
            NodeArch::Armv7l => "armv7l",
            NodeArch::Ppc64le => "ppc64le",
            NodeArch::S390x => "s390x",
            NodeArch::X86 => "x86",
        };
        match self.os {
            NodeOs::Darwin => format!("osx-{arch}-tar"),
            NodeOs::Linux if self.musl => format!("linux-{arch}-musl"),
            NodeOs::Linux => format!("linux-{arch}"),
            NodeOs::Windows => format!("win-{arch}-zip"),
        }
    }
}

/// Whether the host's libc is musl — the one host fact `nub compile` needs to
/// name the host's own triple (`linux-x64-musl` vs `linux-x64`). Re-exported
/// rather than re-implemented so the compile target vocabulary and the dist
/// mirror routing can never disagree about the same host.
pub fn host_is_musl() -> bool {
    detect_musl()
}

/// Whether THIS host runs musl — the running nub binary's own build-target libc,
/// NOT a filesystem probe.
///
/// This is `cfg!(target_env = "musl")` and nothing else. It replaced a `/lib`
/// scan that returned `true` on the mere presence of a `ld-musl-*` loader file,
/// which FALSE-POSITIVED on any glibc host carrying musl cross-libs (`musl-tools`,
/// a CI runner with a musl Rust target, the nub Linux VM): the default
/// `nub compile` then embedded a musl Node into a glibc artifact and it died at
/// runtime with `libstdc++.so.6` relocation errors. A loader file only proves
/// "some musl loader exists somewhere", never "THIS host runs musl".
///
/// Why the binary's own libc is authoritative for the host's: nub ships as
/// per-platform packages gated by the npm `libc` field (`glibc` vs `musl`) whose
/// selector keys on Node's `glibcVersionRuntime` (the robust detect-libc signal),
/// so the glibc binary only ever lands on glibc hosts and the musl binary only on
/// musl hosts. The musl binary is statically linked (musl's default crt-static)
/// yet still reports `target_env = "musl"`, so static linking does not change this
/// answer. The one case it cannot see — a static-musl binary hand-run on a glibc
/// host — is a misuse the per-platform install flow prevents, and no in-process
/// signal resolves it anyway (a static ELF carries no interpreter to inspect).
fn detect_musl() -> bool {
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
    tarball_url: String,
    shasums_url: String,
    /// The tarball's basename — the key to find its line in `SHASUMS256.txt`.
    tarball_filename: String,
}

/// Build the dist addresses for `version` on `host`, rooted at `base` (the mirror
/// base URL, e.g. `https://nodejs.org/dist` — or unofficial-builds for musl; see
/// [`resolve_mirror_base`]). Pure: no network, no env.
pub(crate) fn node_artifact(version: &NodeVersion, host: &HostTarget, base: &str) -> NodeArtifact {
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
pub(crate) fn resolve_mirror_base(host: &HostTarget) -> String {
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
fn resolve_mirror_base_in(host: &HostTarget, project_root: &std::path::Path) -> String {
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

/// Best-effort cleanup for a same-directory temporary file.
struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
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
pub(crate) fn provision_node(
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

/// Download + verify + extract the official Node build for an EXPLICIT target
/// platform, returning its version dir. This is the cross-compile entry
/// `nub compile --platform` reaches: it reuses the whole host pipeline (mirror
/// routing incl. musl → unofficial-builds, streamed download, SHA-256 commit
/// gate) with the target substituted for the detected host.
///
/// INVARIANT the caller must hold: `store_root` is scoped per target platform
/// for a NON-host target. The store layout is keyed by version alone
/// (`<store_root>/node/<version>/`), so a foreign Node written into the host's
/// store root would be indistinguishable from a host one and `nub run` would
/// happily try to execute it.
pub fn provision_node_for_platform(
    version: &NodeVersion,
    os: NodeOs,
    arch: NodeArch,
    musl: bool,
    store_root: &Path,
    resolved_from: Option<&str>,
) -> Result<PathBuf> {
    let target = HostTarget { os, arch, musl };
    provision_node_from(
        version,
        &target,
        store_root,
        resolved_from,
        &resolve_mirror_base(&target),
    )
}

/// Provision the official Node build for an explicit compile target and return
/// its exact aggregate root `LICENSE`. A normal cache hit is enough for runtime
/// execution, but an embed artifact redistributes Node and therefore must not
/// proceed without the notice. A legacy/incomplete cache is repaired from the
/// same resolved mirror and version before returning.
pub fn provision_node_with_license_for_platform(
    version: &NodeVersion,
    os: NodeOs,
    arch: NodeArch,
    musl: bool,
    store_root: &Path,
    resolved_from: Option<&str>,
) -> Result<(PathBuf, Vec<u8>)> {
    let target = HostTarget { os, arch, musl };
    let mirror = resolve_mirror_base(&target);
    let dir = provision_node_from(version, &target, store_root, resolved_from, &mirror)?;
    let license =
        ensure_node_license_from(version, &target, &dir, &store_root.join("node"), &mirror)?;
    Ok((dir, license))
}

fn node_license(version_dir: &Path) -> Result<Vec<u8>> {
    let path = version_dir.join("LICENSE");
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    Ok(bytes)
}

fn node_binary_path(version_dir: &Path, target: &HostTarget) -> PathBuf {
    match target.os {
        NodeOs::Windows => version_dir.join("node.exe"),
        _ => version_dir.join("bin").join("node"),
    }
}

fn node_binary(version_dir: &Path, target: &HostTarget) -> Result<Vec<u8>> {
    let path = node_binary_path(version_dir, target);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeLicenseAttestation {
    artifact: String,
    archive_sha256: String,
    license_sha256: String,
    node_sha256: String,
}

impl NodeLicenseAttestation {
    fn from_verified(
        artifact: &NodeArtifact,
        archive_sha256: &str,
        license: &[u8],
        node: &[u8],
    ) -> Self {
        Self {
            artifact: artifact.tarball_filename.clone(),
            archive_sha256: archive_sha256.to_owned(),
            license_sha256: sha256_hex(license),
            node_sha256: sha256_hex(node),
        }
    }

    fn encode(&self) -> String {
        format!(
            "nub-node-license-attestation-v1\nartifact={}\narchive-sha256={}\nlicense-sha256={}\nnode-sha256={}\n",
            self.artifact, self.archive_sha256, self.license_sha256, self.node_sha256
        )
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes).context("Node LICENSE attestation is not UTF-8")?;
        let mut lines = text.lines();
        if lines.next() != Some("nub-node-license-attestation-v1") {
            anyhow::bail!("unrecognized Node LICENSE attestation format");
        }
        let artifact = lines
            .next()
            .and_then(|line| line.strip_prefix("artifact="))
            .filter(|value| !value.is_empty())
            .context("Node LICENSE attestation has no artifact")?
            .to_owned();
        let archive_sha256 = lines
            .next()
            .and_then(|line| line.strip_prefix("archive-sha256="))
            .filter(|value| is_sha256_hex(value))
            .context("Node LICENSE attestation has an invalid archive digest")?
            .to_owned();
        let license_sha256 = lines
            .next()
            .and_then(|line| line.strip_prefix("license-sha256="))
            .filter(|value| is_sha256_hex(value))
            .context("Node LICENSE attestation has an invalid license digest")?
            .to_owned();
        let node_sha256 = lines
            .next()
            .and_then(|line| line.strip_prefix("node-sha256="))
            .filter(|value| is_sha256_hex(value))
            .context("Node LICENSE attestation has an invalid node digest")?
            .to_owned();
        if lines.next().is_some() {
            anyhow::bail!("Node LICENSE attestation has trailing data");
        }
        Ok(Self {
            artifact,
            archive_sha256,
            license_sha256,
            node_sha256,
        })
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn node_license_attestation_path(version_dir: &Path) -> PathBuf {
    version_dir.join(NODE_LICENSE_ATTESTATION)
}

/// Accept only a receipt for this artifact whose LICENSE and Node executable
/// digests match current bytes. `expected` prevents accepting a different racer
/// after this process has verified a repair archive.
fn attested_node_license(
    version: &NodeVersion,
    target: &HostTarget,
    version_dir: &Path,
    expected: Option<&NodeLicenseAttestation>,
) -> Result<Vec<u8>> {
    let license = node_license(version_dir)?;
    let node = node_binary(version_dir, target)?;
    let receipt_path = node_license_attestation_path(version_dir);
    let receipt = NodeLicenseAttestation::parse(
        &std::fs::read(&receipt_path)
            .with_context(|| format!("reading {}", receipt_path.display()))?,
    )?;
    let artifact = node_artifact(version, target, "").tarball_filename;
    if receipt.artifact != artifact {
        anyhow::bail!(
            "Node LICENSE attestation names {}, expected {artifact}",
            receipt.artifact
        );
    }
    if receipt.license_sha256 != sha256_hex(&license) {
        anyhow::bail!("Node LICENSE does not match its attestation");
    }
    if receipt.node_sha256 != sha256_hex(&node) {
        anyhow::bail!("Node executable does not match its attestation");
    }
    if let Some(expected) = expected {
        if receipt != *expected {
            anyhow::bail!("Node LICENSE cache changed during repair");
        }
    }
    Ok(license)
}

/// Persist an attestation only in a verified extraction quarantine. The enclosing
/// directory is atomically renamed into the cache after this returns, so readers
/// never observe a fresh distribution with a missing receipt.
fn write_node_license_attestation(
    version_dir: &Path,
    attestation: &NodeLicenseAttestation,
) -> Result<()> {
    let path = node_license_attestation_path(version_dir);
    std::fs::write(&path, attestation.encode())
        .with_context(|| format!("writing {}", path.display()))
}

/// Ensure the redistributable notice and executable are both bound to this Node
/// artifact. Missing, stale, corrupt, or unattested state is re-downloaded from the
/// same artifact.
fn ensure_node_license_from(
    version: &NodeVersion,
    target: &HostTarget,
    version_dir: &Path,
    node_store: &Path,
    mirror_base: &str,
) -> Result<Vec<u8>> {
    if let Ok(license) = attested_node_license(version, target, version_dir, None) {
        return Ok(license);
    }

    let art = node_artifact(version, target, mirror_base);
    let nonce = LICENSE_REPAIR_NONCE.fetch_add(1, Ordering::Relaxed);
    let work = node_store.join(format!(
        ".tmp-license-{version}-{}-{nonce}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let _guard = WorkGuard(work.clone());

    let shasums_thread = {
        let url = art.shasums_url.clone();
        std::thread::spawn(move || download::fetch_text(&url))
    };
    let (sha, extracted) = if art.tarball_filename.ends_with(".tar.xz") {
        download::download_and_extract_tar_xz(&art.tarball_url, &work, |_, _| {})
            .with_context(|| format!("downloading Node {version} to repair its LICENSE"))?
    } else {
        let archive = work.join(&art.tarball_filename);
        let sha = download::download_to_file(&art.tarball_url, &archive, |_, _| {})
            .with_context(|| format!("downloading Node {version} to repair its LICENSE"))?;
        let extracted = extract::extract_archive(&archive, &work)?;
        (sha, extracted)
    };
    let shasums = shasums_thread
        .join()
        .map_err(|_| anyhow::anyhow!("checksum fetch thread panicked"))?
        .with_context(|| format!("fetching checksums for Node {version}"))?;
    download::verify_checksum(&sha, &shasums, &art.tarball_filename)?;

    let license = node_license(&extracted)?;
    let node = node_binary(&extracted, target)?;
    let node_permissions = std::fs::metadata(node_binary_path(&extracted, target))?.permissions();
    let attestation = NodeLicenseAttestation::from_verified(&art, &sha, &license, &node);
    atomic_write_node_license(
        version,
        target,
        version_dir,
        &license,
        &node,
        node_permissions,
        &attestation,
    )?;
    // The postcondition is exact, not merely "some nonempty LICENSE": if a
    // concurrent writer won with different bytes/receipt, reject it rather than
    // redistributing an arbitrary complete race winner.
    attested_node_license(version, target, version_dir, Some(&attestation))
}

/// Stage one complete file in the target directory and install it without an
/// overwrite. If an old/corrupt value occupies the name, move it aside before
/// retrying; every visible value is therefore whole-file atomic. Hard links avoid
/// Windows' replace-existing rename limitation.
fn atomic_replace_file(
    version_dir: &Path,
    name: &str,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> Result<()> {
    let dest = version_dir.join(name);
    if std::fs::read(&dest).ok().as_deref() == Some(bytes) {
        return Ok(());
    }

    let nonce = LICENSE_REPAIR_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = version_dir.join(format!(".tmp-{name}-{}-{nonce}", std::process::id()));
    let _tmp_guard = FileGuard(tmp.clone());
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
    }
    if let Some(permissions) = permissions {
        std::fs::set_permissions(&tmp, permissions)
            .with_context(|| format!("set permissions on {}", tmp.display()))?;
    }

    loop {
        match std::fs::hard_link(&tmp, &dest) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::read(&dest).ok().as_deref() == Some(bytes) {
                    return Ok(());
                }
                let displaced = version_dir.join(format!(
                    ".tmp-stale-{name}-{}-{}",
                    std::process::id(),
                    LICENSE_REPAIR_NONCE.fetch_add(1, Ordering::Relaxed)
                ));
                let _displaced_guard = FileGuard(displaced.clone());
                match std::fs::rename(&dest, &displaced) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => {
                        return Err(err)
                            .with_context(|| format!("moving stale {} aside", dest.display()));
                    }
                }
            }
            Err(err) => return Err(err).with_context(|| format!("installing {}", dest.display())),
        }
    }
}

/// Repair the notice then publish its receipt. The receipt is the commit marker:
/// readers reject every intermediate state (new LICENSE without receipt, or a
/// receipt whose digest does not match LICENSE), so a crash or concurrent repairer
/// can never make an arbitrary complete file redistributable.
fn atomic_write_node_license(
    version: &NodeVersion,
    target: &HostTarget,
    version_dir: &Path,
    license: &[u8],
    node: &[u8],
    node_permissions: std::fs::Permissions,
    attestation: &NodeLicenseAttestation,
) -> Result<()> {
    for _ in 0..16 {
        if attested_node_license(version, target, version_dir, Some(attestation)).is_ok() {
            return Ok(());
        }
        let node_path = node_binary_path(version_dir, target);
        let node_name = node_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Node executable has no UTF-8 filename")?;
        let node_dir = node_path
            .parent()
            .context("Node executable has no parent directory")?;
        // The platform layouts are fixed (`bin/node` or `node.exe`), so stage in
        // the executable's own directory while retaining its archive mode.
        atomic_replace_file(node_dir, node_name, node, Some(node_permissions.clone()))?;
        atomic_replace_file(version_dir, "LICENSE", license, None)?;
        atomic_replace_file(
            version_dir,
            NODE_LICENSE_ATTESTATION,
            attestation.encode().as_bytes(),
            None,
        )?;
        if attested_node_license(version, target, version_dir, Some(attestation)).is_ok() {
            return Ok(());
        }
    }
    anyhow::bail!("Node distribution cache kept changing during repair")
}

/// Resolve a pin to the newest published version satisfying it, against the dist
/// index for an EXPLICIT target platform. The cross-compile twin of
/// [`resolve_host_pin`] — same resolution, but the index is fetched from the
/// mirror that actually serves the target (musl targets resolve against
/// unofficial-builds, whose release set differs from nodejs.org's).
pub fn resolve_pin_for_platform(
    pin: &VersionPin,
    os: NodeOs,
    arch: NodeArch,
    musl: bool,
    cache_root: &Path,
) -> Result<NodeVersion> {
    let target = HostTarget { os, arch, musl };
    let mirror = resolve_mirror_base(&target);
    let index = node_index::load_index(cache_root, &mirror)
        .context("loading the Node release index to resolve the compile target")?;
    resolve_pin_in_index_for_artifact(pin, &index, &target.index_artifact_key())
}

// ---- pin-based resolution for `nub compile` (reuses nub run's grammar) ---------
//
// `nub compile` infers its Node version from the SAME pin chain `nub run` uses
// (`resolve_pin_chain`), so the two can't drift. A `VersionPin` is the parsed
// pin from that chain (or from `--target`). Embed resolves it to one EXACT
// version to bake; `--smol` keeps a floor the launcher satisfies at runtime.

use crate::node::version::VersionPin;

/// Parse a `--target` value into a pin using the SAME grammar the pin chain
/// accepts (concrete / major / major.minor / range / alias). The single entry
/// nub-cli uses so `--target` and a `.node-version`/`engines.node` pin resolve
/// identically.
pub fn parse_target_spec(spec: &str) -> Result<VersionPin> {
    VersionPin::parse_allowing_ranges(spec)
        .map_err(|_| anyhow::anyhow!("invalid --target {spec:?}: not a version, range, or alias"))
}

/// Resolve a pin to the NEWEST host Node version satisfying it, against the dist
/// index (no download). The embed shape bakes exactly one version, so a range /
/// major / alias must collapse to a concrete release at compile time.
pub fn resolve_host_pin(pin: &VersionPin, cache_root: &Path) -> Result<NodeVersion> {
    let host = HostTarget::detect()
        .context("this host is not a platform nodejs.org publishes a Node build for")?;
    let mirror = resolve_mirror_base(&host);
    let index = node_index::load_index(cache_root, &mirror)
        .context("loading the Node release index to resolve the compile target")?;
    resolve_pin_in_index(pin, &index)
}

/// Resolve a pin against an already-loaded index. Shared by the host and
/// explicit-platform entries so the two can never resolve a pin differently.
/// Goes through `node_index`'s own resolvers (which read the private index rows)
/// rather than touching `IndexEntry` directly.
fn resolve_pin_in_index(pin: &VersionPin, index: &[node_index::IndexEntry]) -> Result<NodeVersion> {
    let resolved = match pin {
        VersionPin::Range(alts) => node_index::resolve_range(alts, index),
        VersionPin::Exact(v) => node_index::resolve_spec(&v.to_string(), index),
        VersionPin::MajorMinor(major, minor) => {
            node_index::resolve_spec(&format!("{major}.{minor}"), index)
        }
        VersionPin::Major(major) => node_index::resolve_spec(&format!("{major}"), index),
        VersionPin::Alias(alias) => node_index::resolve_spec(alias, index),
    };
    resolved.context("no published Node release satisfies the requested Node version")
}

/// [`resolve_pin_in_index`] restricted to releases that advertise the archive
/// key for an explicit compilation target. A release's presence in an index is
/// not enough: historical and unofficial mirrors may omit a platform artifact.
fn resolve_pin_in_index_for_artifact(
    pin: &VersionPin,
    index: &[node_index::IndexEntry],
    artifact_key: &str,
) -> Result<NodeVersion> {
    let resolved = match pin {
        VersionPin::Range(alts) => {
            node_index::resolve_range_for_artifact(alts, index, artifact_key)
        }
        VersionPin::Exact(v) => {
            node_index::resolve_spec_for_artifact(&v.to_string(), index, artifact_key)
        }
        VersionPin::MajorMinor(major, minor) => {
            node_index::resolve_spec_for_artifact(&format!("{major}.{minor}"), index, artifact_key)
        }
        VersionPin::Major(major) => {
            node_index::resolve_spec_for_artifact(&major.to_string(), index, artifact_key)
        }
        VersionPin::Alias(alias) => {
            node_index::resolve_spec_for_artifact(alias, index, artifact_key)
        }
    };
    resolved.context(
        "no published Node release satisfies the requested Node version for the target platform",
    )
}

/// Resolve the minimum acceptable *published* Node release for a `--smol`
/// launcher targeting `os`/`arch`/`musl`. The launcher intentionally enforces
/// only `discovered >= floor`, but its baked floor must exist at the target
/// mirror: synthetic `20.1.3` floors can be semver-correct yet impossible to
/// provision. Lower-bounded pins therefore select the oldest indexed release
/// satisfying the full pin. Aliases and upper-only ranges retain normal
/// newest-satisfying resolution because they have no natural lower contract.
pub fn resolve_pin_floor_for_platform(
    pin: &VersionPin,
    os: NodeOs,
    arch: NodeArch,
    musl: bool,
    cache_root: &Path,
) -> Result<NodeVersion> {
    let target = HostTarget { os, arch, musl };
    let mirror = resolve_mirror_base(&target);
    let index = node_index::load_index(cache_root, &mirror)
        .context("loading the Node release index to resolve the compile target")?;
    resolve_pin_floor_in_index(pin, &index, &target.index_artifact_key())
}

fn resolve_pin_floor_in_index(
    pin: &VersionPin,
    index: &[node_index::IndexEntry],
    artifact_key: &str,
) -> Result<NodeVersion> {
    let resolved = match pin {
        VersionPin::Exact(v) => {
            node_index::resolve_lowest_spec_for_artifact(&v.to_string(), index, artifact_key)
        }
        VersionPin::MajorMinor(major, minor) => node_index::resolve_lowest_spec_for_artifact(
            &format!("{major}.{minor}"),
            index,
            artifact_key,
        ),
        VersionPin::Major(major) => {
            node_index::resolve_lowest_spec_for_artifact(&major.to_string(), index, artifact_key)
        }
        VersionPin::Range(alts) if range_floor(alts).is_some() => {
            node_index::resolve_lowest_range_for_artifact(alts, index, artifact_key)
        }
        // Upper-only ranges and aliases have no lower contract, so keep the
        // documented normal-resolution behavior (newest matching release).
        VersionPin::Range(_) | VersionPin::Alias(_) => {
            resolve_pin_in_index_for_artifact(pin, index, artifact_key).ok()
        }
    };
    resolved.context("no published Node release satisfies the requested Node version")
}

/// The lowest satisfiable branch floor across `||` alternatives. Comparators in
/// one [`semver::VersionReq`] are ANDed, so that branch's floor is its *highest*
/// lower comparator; alternatives are ORed, so their floors use the minimum.
/// `None` when any alternative has no unambiguous lower floor or when a derived
/// candidate does not actually satisfy its branch.
fn range_floor(alternatives: &[semver::VersionReq]) -> Option<NodeVersion> {
    let mut floor: Option<NodeVersion> = None;
    for req in alternatives {
        let branch_floor = req.comparators.iter().filter_map(comparator_floor).max()?;

        // A lower-bound-looking comparator is not sufficient by itself: a
        // conflicting upper bound or prerelease constraint can exclude it.
        // Returning no floor makes callers resolve a real satisfying release
        // from the index instead of provisioning a version the range rejects.
        if !req.matches(&branch_floor.0) {
            return None;
        }

        floor = Some(match floor {
            Some(f) if f <= branch_floor => f,
            _ => branch_floor,
        });
    }
    floor
}

/// The first stable release a single lower comparator can accept. A strict
/// comparator advances at the precision it names (`>20` → `21.0.0`,
/// `>20.1` → `20.2.0`, `>20.1.2` → `20.1.3`). Values outside NodeVersion's
/// `u32` components and increments at their maximum are deliberately
/// unrepresentable: the caller falls back to index resolution rather than
/// wrapping into an unrelated, disallowed version.
fn comparator_floor(c: &semver::Comparator) -> Option<NodeVersion> {
    let major = u32::try_from(c.major).ok()?;
    let minor = u32::try_from(c.minor.unwrap_or(0)).ok()?;
    let patch = u32::try_from(c.patch.unwrap_or(0)).ok()?;

    match c.op {
        semver::Op::GreaterEq | semver::Op::Exact | semver::Op::Tilde | semver::Op::Caret => {
            Some(NodeVersion::new(major, minor, patch))
        }
        semver::Op::Greater => match (c.minor, c.patch) {
            (None, _) => Some(NodeVersion::new(major.checked_add(1)?, 0, 0)),
            (Some(_), None) => Some(NodeVersion::new(major, minor.checked_add(1)?, 0)),
            (Some(_), Some(_)) => Some(NodeVersion::new(major, minor, patch.checked_add(1)?)),
        },
        semver::Op::Less | semver::Op::LessEq | semver::Op::Wildcard => None,
        _ => None,
    }
}

#[cfg(test)]
mod pin_resolution_tests {
    use super::*;

    fn pin(s: &str) -> VersionPin {
        parse_target_spec(s).unwrap()
    }

    #[test]
    fn range_floor_uses_the_highest_and_lower_bound() {
        // Comparators in one branch are ANDed, so the stricter lower bound wins.
        assert_eq!(
            range_floor(match &pin(">=20 >=20.11 <23") {
                VersionPin::Range(a) => a,
                _ => panic!("expected a range"),
            }),
            Some(NodeVersion::new(20, 11, 0))
        );
    }

    #[test]
    fn range_floor_uses_the_lowest_or_branch_floor() {
        // `||` alternatives: the most permissive (lowest) branch floor wins.
        assert_eq!(
            range_floor(match &pin("^18 || >=20") {
                VersionPin::Range(a) => a,
                _ => panic!("expected a range"),
            }),
            Some(NodeVersion::new(18, 0, 0))
        );
    }

    #[test]
    fn range_floor_advances_strict_bounds_at_their_precision() {
        for (range, expected) in [
            (">20", NodeVersion::new(21, 0, 0)),
            (">20.1", NodeVersion::new(20, 2, 0)),
            (">20.1.2", NodeVersion::new(20, 1, 3)),
        ] {
            assert_eq!(
                range_floor(match &pin(range) {
                    VersionPin::Range(a) => a,
                    _ => panic!("expected a range"),
                }),
                Some(expected),
                "{range}"
            );
        }
    }

    #[test]
    fn range_floor_keeps_exact_tilde_and_caret_lower_bounds() {
        for range in ["=20.1.2", "~20.1.2", "^20.1.2"] {
            assert_eq!(
                range_floor(match &pin(range) {
                    VersionPin::Range(a) => a,
                    _ => panic!("expected a range"),
                }),
                Some(NodeVersion::new(20, 1, 2)),
                "{range}"
            );
        }
    }

    #[test]
    fn range_floor_falls_back_for_ambiguous_or_unrepresentable_branches() {
        for range in ["<23", ">20 <20", ">20.1.4294967295"] {
            assert_eq!(
                range_floor(match &pin(range) {
                    VersionPin::Range(a) => a,
                    _ => panic!("expected a range"),
                }),
                None,
                "{range}"
            );
        }
    }

    #[test]
    fn target_spec_rejects_garbage() {
        assert!(parse_target_spec("not-a-version").is_err());
    }
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
    // The archive is checksum-verified above and `extracted` is still quarantine,
    // so recording this receipt makes the initial compile provisioning reusable
    // offline without a second download.
    let license = node_license(&extracted)?;
    let node = node_binary(&extracted, host)?;
    write_node_license_attestation(
        &extracted,
        &NodeLicenseAttestation::from_verified(&art, &sha, &license, &node),
    )?;

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
    fn index_artifact_keys_match_target_archives() {
        for (target, expected) in [
            (host(NodeOs::Darwin, NodeArch::X64, false), "osx-x64-tar"),
            (
                host(NodeOs::Darwin, NodeArch::Arm64, false),
                "osx-arm64-tar",
            ),
            (host(NodeOs::Linux, NodeArch::X64, false), "linux-x64"),
            (host(NodeOs::Linux, NodeArch::Arm64, false), "linux-arm64"),
            (host(NodeOs::Linux, NodeArch::X64, true), "linux-x64-musl"),
            (
                host(NodeOs::Linux, NodeArch::Arm64, true),
                "linux-arm64-musl",
            ),
            (host(NodeOs::Windows, NodeArch::X64, false), "win-x64-zip"),
            (
                host(NodeOs::Windows, NodeArch::Arm64, false),
                "win-arm64-zip",
            ),
        ] {
            assert_eq!(target.index_artifact_key(), expected);
        }
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

    /// Regression guard for the musl false-positive: detection is exactly the
    /// running binary's build-target libc, immune to whatever loader files exist
    /// under `/lib`. A re-introduced filesystem scan would break this on any
    /// glibc box that has musl cross-libs installed (the scan says musl, the cfg
    /// says glibc) — the dev VM and musl-cross CI runners are exactly such boxes.
    #[test]
    fn musl_detection_is_the_build_target_not_a_loader_file() {
        assert_eq!(detect_musl(), cfg!(target_env = "musl"));
        assert_eq!(host_is_musl(), cfg!(target_env = "musl"));
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

    /// A tiny but valid Node-shaped `.tar.xz` (top dir with `bin/node` and the
    /// aggregate root `LICENSE`) built in memory, plus its SHA-256.
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
            let mut h = tar::Header::new_gnu();
            h.set_size(b"Node license\n".len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder
                .append_data(&mut h, format!("{top}/LICENSE"), &b"Node license\n"[..])
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
        let (base, hits) = serve_dist(
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
        // The checksum-verified initial extraction persisted its receipt, so compile
        // can reuse the exact notice without a second archive download.
        assert_eq!(
            ensure_node_license_from(&version, &h, &dir, &store.join("node"), &base).unwrap(),
            b"Node license\n"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Second call: silent cache hit, no server contact needed.
        let again = provision_node_from(&version, &h, &store, None, &base).expect("cache hit");
        assert_eq!(again, dir);
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn missing_cached_license_is_repaired_from_the_same_verified_dist() {
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let version = ver("99.99.96");
        let name = "node-v99.99.96-linux-x64";
        let (tarball, sha) = node_fixture_tar_xz(name);
        let (base, hits) = serve_dist(
            format!("{sha}  {name}.tar.xz\n"),
            format!("{name}.tar.xz"),
            tarball,
        );
        let store = std::env::temp_dir().join(format!("nub-license-repair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);

        let dir = provision_node_from(&version, &h, &store, None, &base).expect("provision");
        std::fs::remove_file(dir.join("LICENSE")).expect("remove cached license");
        let license = ensure_node_license_from(&version, &h, &dir, &store.join("node"), &base)
            .expect("repair license");
        assert_eq!(license, b"Node license\n");
        assert_eq!(std::fs::read(dir.join("LICENSE")).unwrap(), license);
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "repair must re-download the same exact version from the same mirror"
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn cached_node_tamper_repairs_from_the_verified_distribution() {
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let version = ver("99.99.95");
        let name = "node-v99.99.95-linux-x64";
        let (tarball, sha) = node_fixture_tar_xz(name);
        let (base, hits) = serve_dist(
            format!("{sha}  {name}.tar.xz\n"),
            format!("{name}.tar.xz"),
            tarball,
        );
        let store = std::env::temp_dir().join(format!(
            "nub-node-attestation-repair-{}-{}",
            std::process::id(),
            LICENSE_REPAIR_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&store);
        let dir = provision_node_from(&version, &h, &store, None, &base).unwrap();
        std::fs::write(dir.join("bin/node"), b"tampered").unwrap();

        assert_eq!(
            ensure_node_license_from(&version, &h, &dir, &store.join("node"), &base).unwrap(),
            b"Node license\n"
        );
        assert_eq!(std::fs::read(dir.join("bin/node")).unwrap(), b"#!\n");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn stale_or_unattested_cached_license_is_not_accepted() {
        let version = ver("99.99.95");
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let dir = std::env::temp_dir().join(format!(
            "nub-license-attestation-{}-{}",
            std::process::id(),
            LICENSE_REPAIR_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/node"), b"#!\n").unwrap();
        std::fs::write(dir.join("LICENSE"), b"stale but nonempty\n").unwrap();
        assert!(
            attested_node_license(&version, &h, &dir, None).is_err(),
            "a nonempty legacy LICENSE without a receipt is not proof"
        );

        let artifact = node_artifact(&version, &h, "https://example.test");
        let receipt = NodeLicenseAttestation::from_verified(
            &artifact,
            &"a".repeat(64),
            b"other\n",
            b"node\n",
        );
        write_node_license_attestation(&dir, &receipt).unwrap();
        assert!(
            attested_node_license(&version, &h, &dir, None).is_err(),
            "a receipt whose digest does not match LICENSE is not accepted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_cached_license_is_replaced_with_an_attested_notice() {
        let version = ver("99.99.94");
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let artifact = node_artifact(&version, &h, "https://example.test");
        let license = b"repaired license\n";
        let receipt =
            NodeLicenseAttestation::from_verified(&artifact, &"a".repeat(64), license, b"#!\n");
        let dir = std::env::temp_dir().join(format!("nub-empty-license-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/node"), b"#!\n").unwrap();
        std::fs::write(dir.join("LICENSE"), []).unwrap();

        atomic_write_node_license(
            &version,
            &h,
            &dir,
            license,
            b"#!\n",
            std::fs::metadata(dir.join("LICENSE"))
                .unwrap()
                .permissions(),
            &receipt,
        )
        .unwrap();
        assert_eq!(
            attested_node_license(&version, &h, &dir, Some(&receipt)).unwrap(),
            license
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_exact_license_repairs_leave_one_attested_notice() {
        let nonce = LICENSE_REPAIR_NONCE.fetch_add(1, Ordering::Relaxed);
        let version = ver("99.99.93");
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let artifact = node_artifact(&version, &h, "https://example.test");
        let license = b"verified license\n".to_vec();
        let receipt =
            NodeLicenseAttestation::from_verified(&artifact, &"a".repeat(64), &license, b"#!\n");
        let dir = std::env::temp_dir().join(format!(
            "nub-concurrent-license-repair-{}-{nonce}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/node"), b"#!\n").unwrap();
        std::fs::write(dir.join("LICENSE"), []).unwrap();

        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let repairs: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                let start = start.clone();
                let version = version.clone();
                let receipt = receipt.clone();
                let license = license.clone();
                std::thread::spawn(move || {
                    start.wait();
                    atomic_write_node_license(
                        &version,
                        &h,
                        &dir,
                        &license,
                        b"#!\n",
                        std::fs::metadata(dir.join("LICENSE"))
                            .unwrap()
                            .permissions(),
                        &receipt,
                    )
                })
            })
            .collect();
        for repair in repairs {
            repair
                .join()
                .expect("repair thread panicked")
                .expect("repair");
        }

        assert_eq!(
            attested_node_license(&version, &h, &dir, Some(&receipt)).unwrap(),
            license
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "repair temp files leaked: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn license_repair_replaces_an_unattested_complete_winner() {
        let nonce = LICENSE_REPAIR_NONCE.fetch_add(1, Ordering::Relaxed);
        let version = ver("99.99.92");
        let h = host(NodeOs::Linux, NodeArch::X64, false);
        let artifact = node_artifact(&version, &h, "https://example.test");
        let license = b"verified license\n";
        let receipt =
            NodeLicenseAttestation::from_verified(&artifact, &"a".repeat(64), license, b"#!\n");
        let dir =
            std::env::temp_dir().join(format!("nub-license-winner-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/node"), b"#!\n").unwrap();
        std::fs::write(dir.join("LICENSE"), b"arbitrary winner\n").unwrap();

        atomic_write_node_license(
            &version,
            &h,
            &dir,
            license,
            b"#!\n",
            std::fs::metadata(dir.join("LICENSE"))
                .unwrap()
                .permissions(),
            &receipt,
        )
        .unwrap();
        assert_eq!(
            attested_node_license(&version, &h, &dir, Some(&receipt)).unwrap(),
            license
        );
        let _ = std::fs::remove_dir_all(&dir);
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
