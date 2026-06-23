//! `aube cat-file <hash>` — print the contents of a file from the global store.
//!
//! Accepts either a pnpm-style integrity hash (`sha512-<base64>`) or a raw
//! hex CAS digest. Writes the file's bytes directly to stdout without any
//! encoding or framing, matching `pnpm cat-file`'s behavior — so piping into
//! `cat`, `less`, `file`, or `jq` works the same way.
//!
//! This is a read-only introspection command: no lockfile, no node_modules,
//! no project lock.

use clap::Args;
use miette::{IntoDiagnostic, miette};
use std::io::Write;

use crate::commands::open_store;

#[derive(Debug, Args)]
pub struct CatFileArgs {
    /// File hash to look up.
    ///
    /// Accepts `sha512-<base64>` (pnpm integrity format) or a raw hex
    /// CAS digest.
    pub hash: String,
}

pub async fn run(args: CatFileArgs) -> miette::Result<()> {
    let cwd = crate::dirs::project_root_or_cwd()?;
    let store = open_store(&cwd)?;

    let path = if args.hash.starts_with(aube_store::SHA512_INTEGRITY_PREFIX) {
        store
            .file_path_from_integrity(&args.hash)
            .ok_or_else(|| miette!("invalid integrity hash: {}", args.hash))?
    } else {
        // Assume raw hex. Validate before handing off to
        // `file_path_from_hex`, which uses `split_at(2)` (panics on <2
        // chars) and joins the string into a `PathBuf` (would escape the
        // store root on anything containing `/` or `..`). Hex-only +
        // length >= 2 closes both holes at once.
        //
        // Also lowercase up front: BLAKE3's hex encoding is lowercase
        // (`ab/cdef...`), so an uppercase input like `ABCDEF...` would
        // otherwise resolve to `AB/CDEF...` on case-sensitive filesystems
        // (Linux) and miss the real file.
        if args.hash.len() < 2 || !args.hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(miette!(
                "invalid hash: {}\nhelp: expected a `sha512-<base64>` integrity string or a hex CAS digest",
                args.hash
            ));
        }
        store.file_path_from_hex(&args.hash.to_ascii_lowercase())
    };

    if !path.exists() {
        return Err(miette!(
            "no file with hash {} in store\nhelp: install the owning package first, or check the hash against `{}`",
            args.hash,
            aube_util::cmd("store path")
        ));
    }

    // `tokio::fs::read` off-loads to the blocking pool so a large store
    // file (big JS bundle, sourcemap, etc.) doesn't pin the async worker
    // thread for the duration of the read.
    let bytes = tokio::fs::read(&path)
        .await
        .into_diagnostic()
        .map_err(|e| miette!("failed to read {}: {e}", path.display()))?;

    // Write raw bytes — no line conversion, no trailing newline.
    std::io::stdout()
        .write_all(&bytes)
        .into_diagnostic()
        .map_err(|e| miette!("failed to write to stdout: {e}"))?;

    Ok(())
}
