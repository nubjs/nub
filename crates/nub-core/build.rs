//! Single-binary blob generation (the `embed-runtime` feature only).
//!
//! When `embed-runtime` is on (release/CI), tar + zstd-19 the staged `runtime/`
//! tree into `$OUT_DIR/runtime.tar.zst`, which `node::runtime_cache` pulls in via
//! `include_bytes!`. We also bake the cache key (`runtime-<version>-<blobhash8>`)
//! as a `rustc-env` so the runtime const is a compile-time literal — zero startup
//! cost to compute, and content-safe (a different blob ⇒ a different key ⇒ a clean
//! cache miss ⇒ re-extract).
//!
//! When the feature is OFF (the default `--profile fast` dev loop) this is a
//! near-no-op: no tar, no zstd, no re-run-on-runtime-change — `find_preload` walks
//! to the in-repo `runtime/` exactly as before, so the measured ~5 s incremental
//! loop is untouched. The blob-producing deps (`tar`/`zstd`/`sha2`) are optional
//! build-deps gated by the feature, so a feature-off build never even compiles
//! libzstd's C.
//!
//! The blob carries the CONTENTS of the staging dir at the tar root (`preload.mjs`,
//! `addons/nub-native.node`, `node_modules/…`) — NOT a `runtime/` prefix — so
//! extraction lands them directly in `<cache>/runtime-<key>/`, reproducing the
//! sidecar's internal layout. That layout is load-bearing: the addon resolves
//! `./addons/nub-native.node` relative to the preload, and the fast-tier
//! `--require <stem>.cjs` is the byte-identical sibling of the extracted `.mjs`.

#[cfg(feature = "embed-runtime")]
fn main() {
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // Staging dir: default `<repo>/runtime` (= manifest_dir/../../runtime). CI
    // stages the per-platform addon + the vendored node_modules into the repo's
    // `runtime/` BEFORE this build, so the default already points at the assembled
    // tree. `NUB_RUNTIME_STAGING_DIR` overrides it (absolute, or relative to the
    // repo root) for local release-style packaging.
    println!("cargo:rerun-if-env-changed=NUB_RUNTIME_STAGING_DIR");
    let repo_root = manifest_dir.join("../..");
    let staging = match std::env::var_os("NUB_RUNTIME_STAGING_DIR") {
        Some(v) => {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                p
            } else {
                repo_root.join(p)
            }
        }
        None => repo_root.join("runtime"),
    };
    let staging = staging.canonicalize().unwrap_or_else(|e| {
        panic!(
            "embed-runtime: staging runtime dir {} not found: {e} \
             (set NUB_RUNTIME_STAGING_DIR or stage runtime/ before the build)",
            staging.display()
        )
    });

    // Fail loud on an incomplete stage — a feature-on build that embedded a
    // runtime missing its preload would ship a binary that can't transpile.
    let preload = staging.join("preload.mjs");
    if !preload.is_file() {
        panic!(
            "embed-runtime: {} has no preload.mjs — the runtime stage is incomplete \
             (expected the JS + addons/ + node_modules/ assembled tree)",
            staging.display()
        );
    }

    // Re-tar only when the staged tree changes (CI re-stages each build; a local
    // feature-on rebuild with an unchanged runtime/ skips the work).
    println!("cargo:rerun-if-changed={}", staging.display());

    // tar(CONTENTS at root) → in-memory bytes.
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_dir_all("", &staging)
        .expect("embed-runtime: tar the staged runtime tree");
    let tar_bytes = builder
        .into_inner()
        .expect("embed-runtime: finalize the runtime tar");

    // zstd level 19 (measured sweet spot: ~2.7 MB; 22 saves nothing, xz adds a dep
    // + slower decode for ~0.3 MB).
    let blob = zstd::encode_all(&tar_bytes[..], 19).expect("embed-runtime: zstd-compress the tar");

    let dest = out_dir.join("runtime.tar.zst");
    std::fs::write(&dest, &blob).expect("embed-runtime: write runtime.tar.zst");

    // Cache key = runtime-<pkg version>-<first 8 hex of sha256(blob)>. Version for
    // readability + the `~/.cache/nub/node/<version>/` sibling convention; the hash
    // suffix makes it content-safe.
    let mut hasher = Sha256::new();
    hasher.update(&blob);
    let digest = hasher.finalize();
    let hash8: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    println!("cargo:rustc-env=NUB_RUNTIME_CACHE_KEY=runtime-{version}-{hash8}");

    // R2 integrity backstop: bake the BLAKE3 digest of the directly-LOADED
    // entrypoints — the preload scripts node `--require`s and the addon it
    // `dlopen`s — as compile-time consts. `runtime_cache::verify_entrypoints`
    // re-hashes the EXTRACTED files against these on the load path; a mismatch means
    // the on-disk cache diverged from what this binary embeds (stale / AV-corrupted
    // / tampered), and the binary self-heals by re-extracting the trusted blob. The
    // digests live INSIDE the (signed) binary, so a tampered on-disk file can't swap
    // its own expected hash alongside it (the Electron-asar insight). The full-blob
    // `<hash8>` above is the cache KEY (32-bit, of the COMPRESSED archive) — wrong
    // preimage + too short to verify an extracted file; these are the real per-file
    // digests.
    //
    // BLAKE3 (not SHA-256, which the cache key uses): the runtime re-hashes the ~9 MB
    // addon on every warm load and software SHA-256 is ~28 ms on aarch64 vs ~6 ms for
    // BLAKE3 — see the runtime dep note. Hashing the STAGED file is equivalent to
    // hashing the EXTRACTED file (tar is byte-exact), which the
    // `embedded_blob_verifies_clean` test confirms end-to-end against the real blob.
    // All four are required (fail loud): release.yml always stages the real addon,
    // and the addon-less ubuntu `embed-runtime` PR job stages a placeholder — so a
    // release can never silently ship an unhashed entrypoint.
    for (rel, var) in [
        ("preload.mjs", "NUB_RUNTIME_HASH_PRELOAD_MJS"),
        ("preload.cjs", "NUB_RUNTIME_HASH_PRELOAD_CJS"),
        ("watch-env-guard.cjs", "NUB_RUNTIME_HASH_WATCH_ENV_GUARD"),
        ("addons/nub-native.node", "NUB_RUNTIME_HASH_ADDON"),
    ] {
        let p = staging.join(rel);
        let bytes = std::fs::read(&p).unwrap_or_else(|e| {
            panic!(
                "embed-runtime: cannot read entrypoint {} for integrity hashing: {e} \
                 (stage the full runtime incl. addons/nub-native.node before the build)",
                p.display()
            )
        });
        let hex = blake3::hash(&bytes).to_hex();
        println!("cargo:rustc-env={var}={hex}");
    }
}

#[cfg(not(feature = "embed-runtime"))]
fn main() {}
