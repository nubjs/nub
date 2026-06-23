extern crate napi_build;

use std::path::Path;
use std::process::Command;

fn main() {
    napi_build::setup();

    // Stamp a compile-time build-id into the cache key (consumed in cache.rs as
    // `env!("NUB_NATIVE_BUILD_ID")`). This replaces the old runtime
    // `current_exe()` read+hash, which — because nub-native is a cdylib loaded
    // into the host Node process — hashed the ~100MB Node binary on every
    // process's first transpile, BEFORE the cache-hit check, and never actually
    // changed when nub was rebuilt (Node's binary is stable). The build-id gives
    // the intended dev-rebuild auto-invalidation with zero runtime I/O.
    //
    // Value: `git rev-parse --short HEAD`, plus a `-dirty` suffix when the
    // working tree has uncommitted changes (so local dev edits still invalidate
    // the cache). On any failure (no git, not a repo, command error) we fall back
    // to "" — matching the old `exe_hash()` fallback: the key stays well-formed
    // and stable for the process, we simply lose the auto-invalidation benefit.
    // At a fixed clean commit the value is reproducible, so a release's rebuilds
    // reuse the same cache (the desired behavior for shipped binaries).
    let build_id = git_build_id().unwrap_or_default();
    println!("cargo:rustc-env=NUB_NATIVE_BUILD_ID={build_id}");

    // Re-stamp when the commit moves (HEAD/index/refs change) or when the
    // override env var changes. The `.git/HEAD` + `.git/index` watches catch
    // commits and staged-state changes; `cargo:rerun-if-changed` on a missing
    // path is a no-op, so this is safe outside a git checkout.
    if let Some(git_dir) = git_dir() {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
        // Packed/loose ref updates (e.g. a branch move) also shift the short SHA.
        println!("cargo:rerun-if-changed={}", git_dir.join("refs").display());
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join("packed-refs").display()
        );
    }
    println!("cargo:rerun-if-env-changed=NUB_NATIVE_BUILD_ID");
}

/// `<short-sha>` or `<short-sha>-dirty`, or `None` on any git failure.
fn git_build_id() -> Option<String> {
    let sha = run_git(&["rev-parse", "--short", "HEAD"])?;
    let sha = sha.trim();
    if sha.is_empty() {
        return None;
    }
    // `git status --porcelain` is empty iff the working tree + index are clean.
    let dirty = run_git(&["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    Some(if dirty {
        format!("{sha}-dirty")
    } else {
        sha.to_string()
    })
}

/// The `.git` directory for this crate's checkout, if any.
fn git_dir() -> Option<std::path::PathBuf> {
    let out = run_git(&["rev-parse", "--git-dir"])?;
    let dir = Path::new(out.trim());
    // `--git-dir` may be relative (".git") to the crate dir; absolutize against
    // CARGO_MANIFEST_DIR so the watch paths are stable.
    if dir.is_absolute() {
        Some(dir.to_path_buf())
    } else {
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join(dir))
    }
}

/// Run a git command in this crate's directory, returning stdout on success.
fn run_git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}
