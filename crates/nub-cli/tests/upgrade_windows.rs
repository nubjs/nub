//! Windows self-owned upgrade e2e — the cfg(windows) twin of cli.rs's
//! `self_owned_upgrade_runs_end_to_end_against_a_local_fake_release`, driving
//! the REAL binary as a child process so the one property no in-process test
//! can reach is exercised: the running, memory-mapped `nub.exe` swaps ITSELF
//! via the per-file rename dance (`swap_bin_files_windows`). Runs on the
//! windows-latest CI leg; entirely against `file://` fixtures through the
//! internal `NUB_RELEASE_BASE_URL` seam, so it touches no network.
//!
//! The fake release's `bin/nub.exe` is marker BYTES, not a real executable —
//! the upgrade renames it into place without ever running it, so the asserts
//! can distinguish old from new content unambiguously.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub.exe");
    path
}

fn target_token() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "win32-arm64"
    } else {
        "win32-x64"
    }
}

/// `file:///C:/…` form curl accepts on Windows.
fn file_url(p: &Path) -> String {
    format!("file:///{}", p.display().to_string().replace('\\', "/"))
}

fn tmp(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-upgrade-win-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

const FAKE_VERSION: &str = "9.9.9";
const NEW_BYTES: &[u8] = b"NEW-NUB-RELEASE-BYTES";

/// Build the fake release channel: `<root>/v9.9.9/nub-<target>.zip` (containing
/// `bin/nub.exe` = `NEW_BYTES`) + its `.sha256` sidecar. The zip is created with
/// the same System32 bsdtar the upgrade extracts with (`-a` picks zip from the
/// suffix), so the fixture proves the round-trip through the real tooling.
fn make_fake_release(root: &Path) -> String {
    let archive_name = format!("nub-{}.zip", target_token());
    let version_dir = root.join(format!("v{FAKE_VERSION}"));
    std::fs::create_dir_all(&version_dir).unwrap();

    let build = tmp("zip-build");
    std::fs::create_dir_all(build.join("bin")).unwrap();
    std::fs::write(build.join("bin").join("nub.exe"), NEW_BYTES).unwrap();
    let zip_path = version_dir.join(&archive_name);
    let status = Command::new("tar")
        .args(["-a", "-c", "-f"])
        .arg(&zip_path)
        .arg("bin")
        .current_dir(&build)
        .status()
        .expect("tar.exe must exist on Windows >= 10 1803");
    assert!(status.success(), "creating the fixture zip failed");

    let digest = sha256_hex(&std::fs::read(&zip_path).unwrap());
    std::fs::write(
        version_dir.join(format!("{archive_name}.sha256")),
        format!("{digest}  {archive_name}\n"),
    )
    .unwrap();
    archive_name
}

/// A self-owned install the running binary recognizes: `<root>/.nub/bin` with
/// the REAL nub.exe (it must execute `upgrade` itself), the nubx.exe copy
/// install.ps1 creates, and the `.nub-receipt` marker.
fn make_selfowned_install(root: &Path) -> PathBuf {
    let install = root.join(".nub");
    let bin = install.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(nub_binary(), bin.join("nub.exe")).unwrap();
    std::fs::copy(nub_binary(), bin.join("nubx.exe")).unwrap();
    std::fs::write(install.join(".nub-receipt"), "# nub self-managed install\n").unwrap();
    install
}

#[test]
fn selfowned_upgrade_swaps_the_running_exe_via_the_rename_dance() {
    let root = tmp("swap");
    let release_root = tmp("swap-rel");
    make_fake_release(&release_root);
    let install = make_selfowned_install(&root);
    let bin = install.join("bin");
    let old_bytes = std::fs::read(nub_binary()).unwrap();

    // A stale .old from a "previous" upgrade must not block the dance — the
    // pre-swap GC removes (or the rename replaces) it.
    std::fs::write(bin.join("nub.exe.old"), b"stale-old-leftover").unwrap();

    let out = Command::new(bin.join("nub.exe"))
        .args(["upgrade", "--version", FAKE_VERSION])
        .env("NUB_RELEASE_BASE_URL", file_url(&release_root))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "upgrade failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The live nub.exe is the release's bytes; the pre-upgrade binary moved
    // aside to .old (deletion is impossible while it was executing); the nubx
    // copy was refreshed from the new binary.
    assert_eq!(std::fs::read(bin.join("nub.exe")).unwrap(), NEW_BYTES);
    assert_eq!(std::fs::read(bin.join("nub.exe.old")).unwrap(), old_bytes);
    assert_eq!(std::fs::read(bin.join("nubx.exe")).unwrap(), NEW_BYTES);
}

#[test]
fn selfowned_upgrade_rejects_a_tampered_archive_and_leaves_the_install_untouched() {
    let root = tmp("tamper");
    let release_root = tmp("tamper-rel");
    let archive_name = make_fake_release(&release_root);
    let version_dir = release_root.join(format!("v{FAKE_VERSION}"));
    std::fs::write(
        version_dir.join(format!("{archive_name}.sha256")),
        format!("{}  {archive_name}\n", "0".repeat(64)),
    )
    .unwrap();

    let install = make_selfowned_install(&root);
    let bin = install.join("bin");
    let old_bytes = std::fs::read(nub_binary()).unwrap();

    let out = Command::new(bin.join("nub.exe"))
        .args(["upgrade", "--version", FAKE_VERSION])
        .env("NUB_RELEASE_BASE_URL", file_url(&release_root))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!out.status.success(), "a tampered archive must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("checksum mismatch"),
        "expected a checksum refusal, got: {stderr}"
    );
    // Verification precedes any swap: the install is byte-identical, no .old.
    assert_eq!(std::fs::read(bin.join("nub.exe")).unwrap(), old_bytes);
    assert!(!bin.join("nub.exe.old").exists());
}

#[test]
fn selfowned_dry_run_prints_the_zip_artifact() {
    let root = tmp("dry");
    let install = make_selfowned_install(&root);
    let bin = install.join("bin");

    let out = Command::new(bin.join("nub.exe"))
        .args(["upgrade", "--dry-run", "--version", FAKE_VERSION])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("via self-owned"),
        "expected the self-owned channel, got: {stdout}"
    );
    let expected = format!("nub-{}.zip", target_token());
    assert!(
        stdout.contains(&expected),
        "expected the {expected} artifact URL, got: {stdout}"
    );
}
