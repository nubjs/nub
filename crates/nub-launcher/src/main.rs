//! nub-launcher — the runtime half of `nub compile` (the denort-style model).
//!
//! At compile time `nub compile` injects a payload section into a copy of this
//! binary (JS bundle, and for the default shape a zstd-compressed Node) and ad-hoc
//! re-signs it. At runtime the launcher:
//!   1. reads its own payload section,
//!   2. acquires Node — `embed` shape decompresses the bundled Node into nub's
//!      shared cache once (skipping when a compatible Node is already cached);
//!      `smol` shape discovers an existing Node, else provisions one via a
//!      curl/wget/tar shell-out,
//!   3. extracts the bundled app into the cache,
//!   4. injects version-appropriate Node flags via nub-core's `compute_inject_flags`,
//!   5. spawns Node on the app entry, forwarding signals + the exit code
//!      (nub-core's `status_forwarding_signals`, the same machinery as `nub run`).
//!
//! Node itself is bit-exact official Node — never patched. `process.execPath` is
//! therefore the real resolved Node (plain-Node semantics).

// Match the workspace convention (nub-core/nub-cli): collapsing nested `if let {
// if }` into let-chains is cosmetic churn, so allow it.
#![allow(clippy::collapsible_if)]

mod cache;
mod ui;

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::{self, Manifest, PayloadView, Shape, is_safe_relative_name};
use nub_core::node::{discovery, flags, spawn, version::NodeVersion};
use ui::FirstRun;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    // nub-core's signal-forwarding spawn plants a macOS SIGKILL-backstop watcher by
    // re-invoking `current_exe __pdeath-watch <pgid> <fd>` — which, for a compiled
    // binary, IS this launcher. Dispatch that hidden verb to the watcher entry
    // instead of decoding the payload and re-launching the app (matches how the nub
    // CLI intercepts it above argv0 detection).
    {
        let args: Vec<String> = std::env::args().collect();
        #[cfg(unix)]
        if args.get(1).map(String::as_str) == Some("__pdeath-watch") {
            return spawn::run_pdeath_watch(&args[2..]);
        }
        // Compile-time self-check: `nub compile` runs this on the produced binary
        // to catch an under-padded / corrupt section injection (which traps with
        // SIGILL) BEFORE shipping it to the user. Exercises the section-read +
        // decode + String allocation path without extracting or spawning Node.
        if args.get(1).map(String::as_str) == Some("__probe") {
            return probe();
        }
    }

    let section = match libsui::find_section(compile::SECTION_NAME) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            eprintln!(
                "nub: this executable carries no compiled payload (it is a bare launcher template)"
            );
            return 70;
        }
        Err(e) => {
            eprintln!("nub: could not read the compiled payload: {e}");
            return 70;
        }
    };

    let view = match compile::decode(section) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("nub: {e:#}");
            return 70;
        }
    };

    match launch(&view) {
        Ok(status) => exit_code(status),
        Err(e) => {
            eprintln!("nub: {e:#}");
            1
        }
    }
}

fn launch(view: &PayloadView<'_>) -> Result<ExitStatus> {
    // Resolved ONCE and threaded through: every write this process makes lands
    // under the one directory that was proven writable + exec-capable, and the
    // probe (a mkdir plus a zero-byte file) is not repeated per payload.
    let base = cache::resolve()?;
    let notice = FirstRun::new(view.manifest.install_message.as_deref());

    let node_path = acquire_node(view, &base, &notice)?;
    let app_dir = ensure_app(view, &base)?;
    // Hand the terminal back BEFORE anything the app might print — the box lives
    // on the alternate screen, so this restores the user's scrollback intact.
    notice.finish();
    let entry = app_dir.join(&view.manifest.entry);

    let version: NodeVersion = view
        .manifest
        .node_version
        .parse()
        .unwrap_or_else(|_| NodeVersion::new(22, 15, 0));

    let user_args: Vec<String> = std::env::args().skip(1).collect();
    let node_options = std::env::var("NODE_OPTIONS").ok();
    // Pure version-banded flags (source-maps, disable-warning, experimental
    // unflags). `None` accepted-flag set = version-band behavior without the
    // extra allowed-flags probe spawn; safe for a known embedded/provisioned Node.
    let inject =
        flags::compute_inject_flags(version, &user_args, node_options.as_deref(), false, None);

    let mut cmd = Command::new(node_path.as_os_str());
    // argv0 fidelity: process.argv0 / process.title report "node" (execPath still
    // resolves to the real binary). Matches nub-core's spawn path.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.arg0("node");
    }
    for flag in &inject {
        cmd.arg(flag);
    }
    cmd.arg(&entry);
    cmd.args(&user_args);
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    // Own process group + terminating/diagnostic-signal forwarding + TTY
    // foreground handoff + macOS SIGKILL backstop — the same faithful spawn
    // `nub run`'s file path uses. NODE_OPTIONS is inherited untouched (honored).
    spawn::status_forwarding_signals(&mut cmd).map_err(|e| {
        // A mount-flag query already rejects a `noexec` candidate on Linux and
        // macOS, so reaching here means the denial came from somewhere the flag
        // does not describe — an unsupported host's mount, SELinux, an LSM. The
        // remedies are the same; the classification is what makes the message
        // actionable instead of a bare "Permission denied".
        if cache::exec_denied(&e) && node_path.starts_with(&base) {
            anyhow!(cache::noexec_remedy(&base))
        } else {
            anyhow::Error::new(e).context("spawning Node")
        }
    })
}

/// The `__probe` self-check `nub compile` runs on the produced binary at compile
/// time. Reads + decodes the injected section and touches a `String` allocation
/// (the exact code an under-padded libsui injection corrupts into a SIGILL trap),
/// then prints a stable marker — WITHOUT extracting or spawning Node.
fn probe() -> i32 {
    let Ok(Some(section)) = libsui::find_section(compile::SECTION_NAME) else {
        eprintln!("nub-probe: no payload section");
        return 70;
    };
    match compile::decode(section) {
        Ok(view) => {
            let key = short_key(&view.manifest.app_sha256);
            println!("nub-probe ok {} {}", view.manifest.entry, key);
            0
        }
        Err(e) => {
            eprintln!("nub-probe: {e:#}");
            70
        }
    }
}

// ---- Node acquisition ---------------------------------------------------------

fn acquire_node(view: &PayloadView<'_>, base: &Path, notice: &FirstRun) -> Result<PathBuf> {
    match view.manifest.shape {
        Shape::Embed => acquire_embedded_node(view, base, notice),
        Shape::Smol => acquire_smol_node(&view.manifest, base, notice),
    }
}

/// `embed` shape: use a cached compatible Node if present, else decompress the
/// bundled one into nub's shared cache once. Dedup rule (design gap #6): the
/// embedded Node is stripped and the official provisioned Node is not, but both
/// run identically, so an already-present official Node of the same version is
/// accepted and decompression is skipped.
fn acquire_embedded_node(
    view: &PayloadView<'_>,
    base: &Path,
    notice: &FirstRun,
) -> Result<PathBuf> {
    let m = &view.manifest;
    let key = short_key(&m.node_sha256);
    let node_cache = base
        .join("compile-node")
        .join(format!("{}-{}", m.node_version, key));
    let node_bin = node_cache.join(node_exe_name());

    // Warm: this exact embedded Node already extracted.
    if node_bin.is_file() {
        return Ok(node_bin);
    }

    // Dedup: an official Node of the same version already in nub's store — but
    // only when it can actually run HERE. The store is keyed by version alone, so
    // a foreign-libc Node of the same version (a musl Node poisoned into a glibc
    // host's store by an older nub, or a cross-provision) sits under the same
    // path; reusing it would spawn a Node the host's loader can't resolve
    // (`libstdc++.so.6` relocation errors). Gate the reuse on libc compatibility.
    for store in node_stores(base) {
        let official = node_in_version_dir(&store.join(&m.node_version));
        if official.is_file() && store_node_matches_target(&official, &m.triple) {
            return Ok(official);
        }
    }

    // Cold: decompress the embedded blob into the cache (atomic tmp + rename).
    if view.node_blob.is_empty() {
        bail!("embed-shape payload is missing its Node blob");
    }
    notice.announce();
    let tmp = base.join(format!(
        ".compile-node.{}.{}.tmp",
        std::process::id(),
        rand_suffix()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let tmp_bin = tmp.join(node_exe_name());
    decompress_to_file(view.node_blob, &tmp_bin).context("decompressing the embedded Node")?;
    set_executable(&tmp_bin)?;

    fs::create_dir_all(node_cache.parent().unwrap()).ok();
    match fs::rename(&tmp, &node_cache) {
        Ok(()) => {}
        Err(_) => {
            // A concurrent launcher won the race, or a stale dir exists — adopt it
            // if the binary is there, else surface the error.
            let _ = fs::remove_dir_all(&tmp);
            if !node_bin.is_file() {
                bail!(
                    "failed to publish the extracted Node to {}",
                    node_cache.display()
                );
            }
        }
    }
    Ok(node_bin)
}

/// `smol` shape discovery + provisioning. Order: nub's own Node store → PATH →
/// shell-out provision. Acceptance rule (orchestrator default): any discovered
/// Node `>= --target` (regardless of major) qualifies; provision the exact target
/// only when nothing does.
fn acquire_smol_node(m: &Manifest, base: &Path, notice: &FirstRun) -> Result<PathBuf> {
    let target: NodeVersion = m.node_version.parse().map_err(|_| {
        anyhow!(
            "compiled target version '{}' is unparseable",
            m.node_version
        )
    })?;

    // 1. nub's Node store — a version dir named by its concrete version.
    for store in node_stores(base) {
        if let Some(node) = best_store_node(&store, &target) {
            return Ok(node);
        }
    }

    // 2. PATH node, if it satisfies the target.
    if let Some((path, ver)) = probe_path_node() {
        if ver >= target {
            return Ok(path);
        }
    }

    // 3. Provision the exact target via shell-out.
    provision_smol_node(&target, base, notice)
}

/// Node stores to READ, nearest first: the probed cache base, then the location
/// nub-core would name. They differ whenever the probe fell past `~/.cache/nub`
/// — an explicit `NUB_COMPILE_CACHE_DIR`, or a read-only home on Lambda — and a
/// Node the CLI installed earlier still counts on a box where only one of the two
/// is writable now.
fn node_stores(base: &Path) -> Vec<PathBuf> {
    let mut out = vec![base.join("node")];
    if let Some(store) = discovery::node_store_dir() {
        if store != out[0] {
            out.push(store);
        }
    }
    out
}

/// Scan nub's store (`<cache>/node/<version>/bin/node`) for the newest installed
/// Node satisfying `target`.
fn best_store_node(store: &Path, target: &NodeVersion) -> Option<PathBuf> {
    let mut best: Option<(NodeVersion, PathBuf)> = None;
    for entry in fs::read_dir(store).ok()?.flatten() {
        let Ok(ver) = entry.file_name().to_string_lossy().parse::<NodeVersion>() else {
            continue;
        };
        if ver < *target {
            continue;
        }
        let bin = node_in_version_dir(&entry.path());
        if !bin.is_file() {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| ver > *b) {
            best = Some((ver, bin));
        }
    }
    best.map(|(_, p)| p)
}

/// Resolve `node` on PATH to its path + version, or `None` if absent/unparseable.
fn probe_path_node() -> Option<(PathBuf, NodeVersion)> {
    let out = Command::new("node").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let ver: NodeVersion = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    // Resolve the actual path so we don't re-PATH-search at spawn.
    let path = which_on_path(node_exe_name()).unwrap_or_else(|| PathBuf::from("node"));
    Some((path, ver))
}

/// Minimal `which`: first PATH entry containing an executable `name`.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The spike provisions only from `.tar.xz` dists (the `tar -xJf` shell-out), so a
/// `.zip` host (Windows) has no provisioning path yet. Reject it as an `Err` — the
/// caller turns that into an actionable non-zero exit, never a silent success.
fn ensure_smol_provision_supported(archive_ext: &str, version: &NodeVersion) -> Result<()> {
    if archive_ext != "tar.xz" {
        bail!(
            "--smol provisioning on this platform is not implemented in the spike \
             (only tar.xz hosts); install Node {version} manually, or use a binary \
             built with the default (embed-Node) shape"
        );
    }
    Ok(())
}

/// Provision `version` for the host from nodejs.org via a lean shell-out
/// (`curl`→`wget`, then `tar`). No in-binary TLS stack — the design's `--smol`
/// rule. Installs into nub's store so `nub run` and future launches reuse it.
fn provision_smol_node(version: &NodeVersion, base: &Path, notice: &FirstRun) -> Result<PathBuf> {
    let token = host_platform_token();
    // Unsupported-host guard: this MUST surface as an `Err` (→ non-zero exit via
    // `run`), never a printed-then-exit-0 dead end — a build tool that reports
    // failure while exiting 0 breaks every CI/`set -e` caller downstream.
    ensure_smol_provision_supported(host_archive_ext(), version)?;
    let mirror = smol_mirror_base();
    let filename = format!("node-v{version}-{token}.tar.xz");
    let url = format!("{mirror}/v{version}/{filename}");

    // The probed base, not `discovery::node_store_dir()`: provisioning WRITES, so
    // it must land where the write probe succeeded.
    let store = base.join("node");
    fs::create_dir_all(&store).ok();
    let work = store.join(format!(".smol.{version}.{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;

    let tarball = work.join(&filename);
    notice.announce();
    download_via_shell(&url, &tarball, notice).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;

    // Verify the tarball's SHA-256 against SHASUMS256.txt BEFORE anything reaches
    // the shared store — `nub run` and the embed-shape dedup path later TRUST that
    // store without re-verifying, so an unverified write would poison them. Matches
    // version_management::provision_node_from's fail-closed commit gate.
    let shasums_path = work.join("SHASUMS256.txt");
    let shasums_url = format!("{mirror}/v{version}/SHASUMS256.txt");
    download_via_shell(&shasums_url, &shasums_path, notice).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;
    let shasums = fs::read_to_string(&shasums_path)
        .with_context(|| format!("reading {}", shasums_path.display()))?;
    verify_tarball_checksum(&tarball, &shasums, &filename).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;

    // Extract (xz-aware tar). `tar -xJf` works on macOS bsdtar + GNU tar.
    let status = Command::new("tar")
        .arg("-xJf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .status()
        .context("running `tar` to extract the Node archive (is `tar` installed?)")?;
    if !status.success() {
        let _ = fs::remove_dir_all(&work);
        bail!("`tar` failed to extract the Node archive");
    }

    // The archive unpacks to `node-v<ver>-<token>/`; move it into the store as
    // `<store>/<version>/`.
    let extracted = work.join(format!("node-v{version}-{token}"));
    let final_dir = store.join(version.to_string());
    if !node_in_version_dir(&final_dir).is_file() {
        let _ = fs::rename(&extracted, &final_dir);
    }
    let _ = fs::remove_dir_all(&work);

    let node = node_in_version_dir(&final_dir);
    if !node.is_file() {
        bail!(
            "provisioned Node {version} but its binary is missing at {}",
            node.display()
        );
    }
    Ok(node)
}

/// Download `url` to `dest` via `curl`, then `wget`. When neither exists, the
/// no-downloader error names the fix (a binary built with the default shape).
///
/// While the first-run box owns the terminal the downloader's own progress meter
/// is silenced — two writers on one screen is torn output; the box's spinner is
/// the only feedback. It is silenced off a TTY too: nobody is watching a pipe or
/// a log file, and a progress meter there is the one thing that breaks the
/// otherwise byte-exact silence a non-interactive first run promises. An
/// interactive run without the box still gets the meter.
fn download_via_shell(url: &str, dest: &Path, notice: &FirstRun) -> Result<()> {
    let muted = notice.owns_terminal() || !std::io::stderr().is_terminal();

    if command_exists("curl") {
        let mut cmd = Command::new("curl");
        cmd.args(["-fSL", "--retry", "2"]);
        if muted {
            cmd.arg("-s");
        }
        let status = cmd
            .arg("-o")
            .arg(dest)
            .arg(url)
            .status()
            .context("running curl")?;
        if status.success() {
            return Ok(());
        }
        bail!("curl failed to download {url}");
    }
    if command_exists("wget") {
        let mut cmd = Command::new("wget");
        if muted {
            cmd.arg("-q");
        }
        let status = cmd
            .arg("-O")
            .arg(dest)
            .arg(url)
            .status()
            .context("running wget")?;
        if status.success() {
            return Ok(());
        }
        bail!("wget failed to download {url}");
    }
    bail!(
        "no HTTPS downloader found (need `curl` or `wget`) to provision Node.\n\
         \x20\x20Install curl or wget, or use a binary built with the default \
         (embed-Node) shape, which needs no download."
    )
}

fn command_exists(name: &str) -> bool {
    which_on_path(name).is_some()
}

/// Fail-closed SHA-256 gate: the tarball's digest must match its line in
/// SHASUMS256.txt, or the install aborts before anything is committed. Mirrors
/// `version_management::download::verify_checksum` exactly.
fn verify_tarball_checksum(tarball: &Path, shasums: &str, filename: &str) -> Result<()> {
    let expected = checksum_for(shasums, filename)
        .with_context(|| format!("{filename} is not listed in SHASUMS256.txt — refusing"))?;
    let actual = sha256_hex_of_file(tarball)?;
    if actual.eq_ignore_ascii_case(&expected) {
        Ok(())
    } else {
        bail!("checksum mismatch for {filename}: expected {expected}, got {actual}");
    }
}

/// The SHASUMS256.txt hash for `filename` (`<64-hex>  <name>`), lowercased, or
/// `None` if absent/malformed. Byte-identical to nub-core's `checksum_for`.
fn checksum_for(shasums: &str, filename: &str) -> Option<String> {
    shasums.lines().find_map(|line| {
        let (hash, rest) = line.split_once(char::is_whitespace)?;
        let name = rest.trim_start();
        let valid = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        (valid && name == filename).then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_hex_of_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// The `NODEJS_ORG_MIRROR` override (nvm/n convention), else the default dist
/// base (unofficial-builds for musl).
fn smol_mirror_base() -> String {
    if let Ok(m) = std::env::var("NODEJS_ORG_MIRROR") {
        let m = m.trim_end_matches('/');
        if !m.is_empty() {
            return m.to_string();
        }
    }
    if is_musl() {
        "https://unofficial-builds.nodejs.org/download/release".to_string()
    } else {
        "https://nodejs.org/dist".to_string()
    }
}

// ---- app extraction -----------------------------------------------------------

/// Extract the bundled app files into `<cache>/compile-app/<app-key>/` (atomic
/// tmp + rename). A warm run finds the dir present and skips the write.
fn ensure_app(view: &PayloadView<'_>, base: &Path) -> Result<PathBuf> {
    let key = short_key(&view.manifest.app_sha256);
    let app_dir = base.join("compile-app").join(key);
    if app_dir.join(&view.manifest.entry).is_file() {
        return Ok(app_dir);
    }

    let tmp = base.join(format!(
        ".compile-app.{}.{}.tmp",
        std::process::id(),
        rand_suffix()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    for (name, data) in &view.app_files {
        // Refuse a payload file name that could escape the extraction dir — a
        // corrupted/hostile section must not write outside `tmp` via `..`, an
        // absolute path, or a leading separator. Names are nested and partly
        // user-derived since `--include` landed, so `nub compile` checks the
        // same predicate at build time; this stays as the last line of defense.
        if !is_safe_relative_name(name) {
            bail!("compiled payload has an unsafe file name: {name:?}");
        }
        let dest = tmp.join(name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&dest, data).with_context(|| format!("writing {}", dest.display()))?;
    }
    fs::create_dir_all(app_dir.parent().unwrap()).ok();
    match fs::rename(&tmp, &app_dir) {
        Ok(()) => {}
        Err(_) => {
            let _ = fs::remove_dir_all(&tmp);
            if !app_dir.join(&view.manifest.entry).is_file() {
                bail!(
                    "failed to publish the extracted app to {}",
                    app_dir.display()
                );
            }
        }
    }
    Ok(app_dir)
}

// ---- helpers ------------------------------------------------------------------

/// A short, filesystem-safe key from a hex content hash.
fn short_key(hex: &str) -> String {
    let s: String = hex.chars().take(16).collect();
    if s.is_empty() { "0".into() } else { s }
}

/// Stream the decoder into the file rather than materializing the whole ~94 MB
/// Node in a `Vec` first. Same wall time (the decode dominates; the write is
/// ~60 ms of it) at roughly half the peak RSS — which is what decides the run on
/// a memory-capped host whose writable filesystem is tmpfs charged against that
/// same limit.
fn decompress_to_file(compressed: &[u8], dest: &Path) -> Result<()> {
    let mut decoder =
        zstd::stream::Decoder::new(compressed).map_err(|e| anyhow!("zstd init: {e}"))?;
    let file = fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    std::io::copy(&mut decoder, &mut out)
        .with_context(|| format!("decompressing Node into {}", dest.display()))?;
    out.flush()
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod +x {}", path.display()))
}
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// The Node executable's filename on this platform.
fn node_exe_name() -> &'static str {
    if cfg!(windows) { "node.exe" } else { "node" }
}

/// The Node executable inside a provisioned version dir. The two dist layouts
/// differ: the Windows zip puts `node.exe` at the root, the tarballs put
/// `bin/node`.
fn node_in_version_dir(version_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        version_dir.join("node.exe")
    } else {
        version_dir.join("bin").join("node")
    }
}

fn host_platform_token() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "arm" => "armv7l",
        "powerpc64" => "ppc64le",
        "s390x" => "s390x",
        "x86" => "x86",
        other => other,
    };
    if os == "linux" && is_musl() {
        format!("{os}-{arch}-musl")
    } else {
        format!("{os}-{arch}")
    }
}

fn host_archive_ext() -> &'static str {
    if std::env::consts::OS == "windows" {
        "zip"
    } else {
        "tar.xz"
    }
}

/// Whether this launcher runs on musl — its OWN build-target libc, since the
/// per-triple launcher is built for and shipped to exactly one platform. NOT a
/// `/lib` scan: a `ld-musl-*` loader file merely existing (a glibc host with musl
/// cross-libs) is not this host running musl, and treating it as such made
/// `--smol` fetch a musl Node onto a glibc box. Mirrors nub-core's `detect_musl`.
fn is_musl() -> bool {
    cfg!(target_env = "musl")
}

// ---- store-node libc compatibility --------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ElfLibc {
    Glibc,
    Musl,
}

/// Whether a Node already in nub's version store may be reused for a payload
/// targeting `triple`, instead of decompressing the embedded one. The store is
/// keyed by version alone, so this is the guard that a musl and a glibc Node of
/// the SAME version — not interchangeable — are told apart.
///
/// Only Linux splits libc; a macOS/Windows store node of the right version is
/// format-compatible by construction (the store lives on the host the payload
/// targets), so those short-circuit to reuse. On Linux the candidate's libc is
/// read from its OWN ELF interpreter (`PT_INTERP`) — `ld-musl-*` ⇒ musl,
/// `ld-linux*` ⇒ glibc — which is definitional and needs no exec. A static Node
/// (no interpreter) runs against any libc, so it is accepted; anything unreadable
/// as an ELF is refused, degrading to the always-correct embedded-Node path.
fn store_node_matches_target(node: &Path, triple: &str) -> bool {
    if !triple.starts_with("linux") {
        return true;
    }
    let want_musl = triple.ends_with("-musl");
    match read_elf_interp_libc(node) {
        Some(Some(ElfLibc::Musl)) => want_musl,
        Some(Some(ElfLibc::Glibc)) => !want_musl,
        Some(None) => true, // valid ELF, no interpreter (static) → libc-agnostic
        None => false,      // unreadable / not a 64-bit LE ELF → don't risk reuse
    }
}

/// Read a candidate's `PT_INTERP` and classify its libc, over a BOUNDED prefix —
/// the interpreter string lives in the first LOAD segment, so a Node binary's
/// tens of MB are never slurped to answer this.
fn read_elf_interp_libc(path: &Path) -> Option<Option<ElfLibc>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    classify_elf_interp(&buf)
}

/// The pure parser behind [`read_elf_interp_libc`], over an in-memory prefix — the
/// seam the tests drive with hand-built ELF headers. `None` = not a 64-bit LE ELF
/// (or an unrecognized interpreter); `Some(None)` = valid ELF, no interpreter
/// (statically linked); `Some(Some(libc))` = the dynamic linker's libc.
fn classify_elf_interp(buf: &[u8]) -> Option<Option<ElfLibc>> {
    // ELF ident: magic, EI_CLASS==2 (64-bit), EI_DATA==1 (LE). `nub compile` only
    // targets linux-{x64,arm64}, both 64-bit LE, so any other ELF in the store is
    // foreign — refuse rather than classify it.
    if buf.len() < 64 || &buf[0..4] != b"\x7fELF" || buf[4] != 2 || buf[5] != 1 {
        return None;
    }
    let rd_u16 =
        |o: usize| -> Option<u16> { Some(u16::from_le_bytes(buf.get(o..o + 2)?.try_into().ok()?)) };
    let rd_u32 =
        |o: usize| -> Option<u32> { Some(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?)) };
    let rd_u64 =
        |o: usize| -> Option<u64> { Some(u64::from_le_bytes(buf.get(o..o + 8)?.try_into().ok()?)) };

    let e_phoff = rd_u64(0x20)? as usize;
    let e_phentsize = rd_u16(0x36)? as usize;
    let e_phnum = rd_u16(0x38)? as usize;
    const PT_INTERP: u32 = 3;
    for i in 0..e_phnum {
        let ph = e_phoff.checked_add(i.checked_mul(e_phentsize)?)?;
        if rd_u32(ph)? != PT_INTERP {
            continue;
        }
        let p_offset = rd_u64(ph + 8)? as usize;
        let p_filesz = rd_u64(ph + 32)? as usize;
        let end = p_offset.checked_add(p_filesz)?;
        let interp = buf.get(p_offset..end)?;
        let interp = interp.split(|&b| b == 0).next().unwrap_or(interp);
        let s = std::str::from_utf8(interp).ok()?;
        if s.contains("ld-musl") {
            return Some(Some(ElfLibc::Musl));
        }
        if s.contains("ld-linux") {
            return Some(Some(ElfLibc::Glibc));
        }
        return None; // unrecognized interpreter — refuse rather than guess
    }
    Some(None) // no PT_INTERP → statically linked
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[cfg(unix)]
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        128 + sig
    } else {
        1
    }
}
#[cfg(not(unix))]
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `--smol` checksum gate: a tarball whose SHA-256 does not match
    /// SHASUMS256.txt must be REFUSED (so provisioning bails before the
    /// rename-into-store), and a matching one accepted. This is what keeps a
    /// shell-out-provisioned Node from poisoning the shared store.
    #[test]
    fn checksum_gate_refuses_mismatch_and_accepts_match() {
        let dir = std::env::temp_dir().join(format!("nub-smol-sum-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let filename = "node-v99.99.99-test.tar.xz";
        let tarball = dir.join(filename);
        fs::write(&tarball, b"pretend tarball bytes").unwrap();
        let real = sha256_hex_of_file(&tarball).unwrap();

        // Wrong hash listed → refused (mismatch), nothing extracted.
        let bad = format!("{}  {filename}\n", "0".repeat(64));
        let err = verify_tarball_checksum(&tarball, &bad, filename).unwrap_err();
        assert!(
            format!("{err:#}").contains("checksum mismatch"),
            "got: {err:#}"
        );

        // Not listed at all → refused (fail-closed, never accept an absent entry).
        let absent = format!("{real}  some-other-file.tar.xz\n");
        assert!(verify_tarball_checksum(&tarball, &absent, filename).is_err());

        // Correct hash → accepted.
        let good = format!("{real}  {filename}\n");
        assert!(verify_tarball_checksum(&tarball, &good, filename).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    /// A `.zip` host (Windows) has no spike provisioning path, and that MUST be an
    /// `Err` so `run` exits non-zero — a build tool that prints failure but exits 0
    /// silently passes in every CI/`set -e` caller. A `.tar.xz` host is accepted.
    #[test]
    fn unsupported_smol_host_is_an_error_not_a_silent_success() {
        let version = NodeVersion::new(22, 15, 0);

        let err = ensure_smol_provision_supported("zip", &version).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not implemented"), "got: {msg}");
        assert!(msg.contains("embed-Node"), "got: {msg}");

        assert!(ensure_smol_provision_supported("tar.xz", &version).is_ok());
    }

    #[test]
    fn rejects_escaping_payload_file_names() {
        assert!(is_safe_relative_name("main.js"));
        assert!(is_safe_relative_name("chunk-abc.js"));
        assert!(is_safe_relative_name("nested/chunk.js"));
        assert!(!is_safe_relative_name(""));
        assert!(!is_safe_relative_name("../evil"));
        assert!(!is_safe_relative_name("a/../../evil"));
        assert!(!is_safe_relative_name("/etc/passwd"));
        assert!(!is_safe_relative_name("./main.js"));
    }

    /// A minimal 64-bit LE ELF carrying one `PT_INTERP` program header pointing at
    /// `interp` — enough to exercise the libc classifier without a real binary.
    fn elf_with_interp(interp: &str) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // 64-bit
        b[5] = 1; // little-endian
        b[0x20..0x28].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        b[0x36..0x38].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize (64-bit)
        b[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        let interp_off = 64 + 56;
        let mut interp_bytes = interp.as_bytes().to_vec();
        interp_bytes.push(0);
        let mut ph = vec![0u8; 56];
        ph[0..4].copy_from_slice(&3u32.to_le_bytes()); // p_type = PT_INTERP
        ph[8..16].copy_from_slice(&(interp_off as u64).to_le_bytes()); // p_offset
        ph[32..40].copy_from_slice(&(interp_bytes.len() as u64).to_le_bytes()); // p_filesz
        b.extend_from_slice(&ph);
        b.extend_from_slice(&interp_bytes);
        b
    }

    /// A 64-bit LE ELF with no program headers → statically linked.
    fn elf_static() -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        b[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        b[0x38..0x3a].copy_from_slice(&0u16.to_le_bytes());
        b
    }

    #[test]
    fn elf_interp_classifier_reads_the_libc() {
        assert_eq!(
            classify_elf_interp(&elf_with_interp("/lib64/ld-linux-x86-64.so.2")),
            Some(Some(ElfLibc::Glibc))
        );
        assert_eq!(
            classify_elf_interp(&elf_with_interp("/lib/ld-linux-aarch64.so.1")),
            Some(Some(ElfLibc::Glibc))
        );
        assert_eq!(
            classify_elf_interp(&elf_with_interp("/lib/ld-musl-x86_64.so.1")),
            Some(Some(ElfLibc::Musl))
        );
        // Valid ELF, no interpreter → static, libc-agnostic.
        assert_eq!(classify_elf_interp(&elf_static()), Some(None));
        // Not an ELF at all → unclassifiable.
        assert_eq!(classify_elf_interp(b"#!/usr/bin/env node\n"), None);
    }

    /// The dedup gate: a musl store Node is refused for a glibc payload (the VM
    /// bug), a glibc one for a musl payload, and either is reused for its matching
    /// libc; a non-Linux target has no libc split and always reuses.
    #[test]
    fn store_node_gate_matches_only_compatible_libc() {
        let dir = std::env::temp_dir().join(format!("nub-libc-gate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let glibc = dir.join("glibc-node");
        let musl = dir.join("musl-node");
        fs::write(&glibc, elf_with_interp("/lib64/ld-linux-x86-64.so.2")).unwrap();
        fs::write(&musl, elf_with_interp("/lib/ld-musl-x86_64.so.1")).unwrap();

        assert!(store_node_matches_target(&glibc, "linux-x64"));
        assert!(!store_node_matches_target(&musl, "linux-x64"));
        assert!(store_node_matches_target(&musl, "linux-x64-musl"));
        assert!(!store_node_matches_target(&glibc, "linux-x64-musl"));
        // No libc concept off Linux → always compatible.
        assert!(store_node_matches_target(&musl, "darwin-arm64"));

        let _ = fs::remove_dir_all(&dir);
    }
}
