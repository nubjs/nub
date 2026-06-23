use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

mod primer_schema {
    include!("src/primer_schema.rs");
}

use primer_schema::Seed;

const DEV_TOP: usize = 100;
const RELEASE_TOP: usize = 2000;
// 100 covers ≥97% of resolved versions across our 7 benchmark fixtures
// (aube-bench plus the vlt-benchmarks set: astro, babylon, large, next,
// svelte, vue). Bumping back to v=1000 only buys ~1.1pp of aggregate
// hit-rate for +10.6 MB on the embedded primer (9.04 MB vs ~19.6 MB,
// both measured locally with PRIMER_DATA_SCHEMA=2). Misses fall back
// cleanly via `PickResult::NoMatch` in resolve.rs — one extra packument
// fetch per long-tail version pick, typically old `react-is`, hoisted
// `@typescript-eslint/*`, or stale `core-js@2.x` style versions.
const DEFAULT_VERSION_CAP: usize = 100;
const FAST_COMPRESSION_LEVEL: i32 = 10;
const RELEASE_CI_COMPRESSION_LEVEL: i32 = 19;
// Bump when the on-disk rkyv schema (`src/primer_schema.rs`) changes
// in a layout-breaking way. The on-disk `primer-topN-vM-sK.rkyv.zst`
// artifact is gitignored, so older `sK` files orphan harmlessly and
// the new `sK+1` is regenerated on the next build.
const PRIMER_DATA_SCHEMA: u32 = 2;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let source = std::env::var_os("AUBE_PRIMER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let top = primer_top();
            let version_cap = version_cap();
            manifest_dir.join("data").join(format!(
                "primer-top{top}-v{version_cap}-s{PRIMER_DATA_SCHEMA}.rkyv.zst"
            ))
        });

    println!("cargo:rerun-if-env-changed=AUBE_PRIMER_PATH");
    println!("cargo:rerun-if-env-changed=AUBE_PRIMER_TOP");
    println!("cargo:rerun-if-env-changed=AUBE_PRIMER_VERSION_CAP");
    println!("cargo:rerun-if-env-changed=AUBE_REQUIRE_PRIMER");
    println!("cargo:rerun-if-changed={}", source.display());
    let json = source.with_extension("json");
    println!("cargo:rerun-if-changed={}", json.display());

    if !source.is_file() {
        if std::env::var_os("AUBE_PRIMER_PATH").is_some() {
            panic!(
                "AUBE_PRIMER_PATH does not point to a file: {}",
                source.display()
            );
        }
        let generated = if json.is_file() {
            compress_json_primer(&json, &source);
            let _ = std::fs::remove_file(&json);
            true
        } else {
            let script = manifest_dir
                .parent()
                .and_then(Path::parent)
                .map(|w| w.join("scripts/generate-primer.mjs"));
            matches!(&script, Some(s) if s.is_file())
                && generate(&manifest_dir, &source, primer_top())
        };
        if !generated {
            if primer_required() {
                panic!(
                    "metadata primer is required, but {} was missing and could not be generated",
                    source.display()
                );
            }
            // No primer data file and no working generator. Three cases:
            //   1. published crate / downstream consumer (no script),
            //   2. cross-rs Docker container building Linux release
            //      binaries (script visible via mount, but no `node`),
            //   3. Fedora COPR mock chroot building the SRPM (script in
            //      tarball, but no `node`).
            // Ship an empty primer; runtime falls back to network packument
            // fetches.
            write_package_blob(&out_dir, &[]);
            return;
        }
    }

    let generated_at = std::fs::metadata(&source)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
        })
        .as_secs();
    println!("cargo:rustc-env=AUBE_PRIMER_GENERATED_AT={generated_at}");

    let bytes = std::fs::read(&source)
        .unwrap_or_else(|e| panic!("failed to read primer {}: {e}", source.display()));
    write_package_blob(&out_dir, &bytes);
}

fn primer_top() -> usize {
    if let Some(top) = std::env::var_os("AUBE_PRIMER_TOP") {
        return top
            .to_string_lossy()
            .parse()
            .expect("AUBE_PRIMER_TOP must be a positive integer");
    }
    match std::env::var("PROFILE").as_deref() {
        Ok("release" | "release-native" | "release-pgo") => RELEASE_TOP,
        _ => DEV_TOP,
    }
}

fn version_cap() -> usize {
    if let Some(cap) = std::env::var_os("AUBE_PRIMER_VERSION_CAP") {
        return cap
            .to_string_lossy()
            .parse()
            .expect("AUBE_PRIMER_VERSION_CAP must be a positive integer");
    }
    DEFAULT_VERSION_CAP
}

fn generate(manifest_dir: &Path, source: &Path, top: usize) -> bool {
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("resolver crate lives under crates/aube-resolver");
    let json = source.with_extension("json");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();

    let status = match Command::new("node")
        .arg(workspace.join("scripts/generate-primer.mjs"))
        .arg("--top")
        .arg(top.to_string())
        .arg("--versions")
        .arg(version_cap().to_string())
        .arg("--out")
        .arg(&json)
        .status()
    {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "cargo:warning=node not found in PATH; shipping empty primer \
                 (runtime falls back to network packument fetches)"
            );
            return false;
        }
        Err(e) => {
            if primer_required() {
                panic!("failed to run scripts/generate-primer.mjs: {e}");
            }
            println!(
                "cargo:warning=failed to run scripts/generate-primer.mjs: {e}; \
                 shipping empty primer (runtime falls back to network packument fetches)"
            );
            return false;
        }
    };
    if !status.success() {
        if primer_required() {
            panic!("scripts/generate-primer.mjs failed");
        }
        println!(
            "cargo:warning=scripts/generate-primer.mjs failed; shipping empty primer \
             (runtime falls back to network packument fetches)"
        );
        let _ = std::fs::remove_file(&json);
        return false;
    }

    compress_json_primer(&json, source);
    let _ = std::fs::remove_file(json);
    true
}

fn compress_json_primer(json: &Path, source: &Path) {
    let input = std::fs::read(json)
        .unwrap_or_else(|e| panic!("failed to read primer JSON {}: {e}", json.display()));
    let primer: BTreeMap<String, Seed> = serde_json::from_slice(&input).unwrap();
    let archived = rkyv::to_bytes::<rkyv::rancor::Error>(&primer).unwrap();
    let compressed =
        zstd::stream::encode_all(Cursor::new(archived), primer_compression_level()).unwrap();
    std::fs::write(source, compressed).unwrap();
}

fn write_package_blob(out_dir: &Path, compressed: &[u8]) {
    let mut blob = Vec::new();
    let mut index = Vec::new();
    if !compressed.is_empty() {
        let archived = zstd::stream::decode_all(Cursor::new(compressed)).unwrap();
        let primer =
            rkyv::from_bytes::<BTreeMap<String, Seed>, rkyv::rancor::Error>(&archived).unwrap();
        for (name, seed) in primer {
            let archived = rkyv::to_bytes::<rkyv::rancor::Error>(&seed).unwrap();
            let compressed =
                zstd::stream::encode_all(Cursor::new(archived), primer_compression_level())
                    .unwrap();
            let offset = blob.len();
            let len = compressed.len();
            blob.extend_from_slice(&compressed);
            index.push((name, offset, len));
        }
    }
    if primer_required() && index.is_empty() {
        panic!("metadata primer is required, but the embedded primer is empty");
    }
    std::fs::write(out_dir.join("primer-packages.bin"), blob).unwrap();

    let mut generated =
        "static PRIMER_BLOB: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/primer-packages.bin\"));\nstatic PRIMER_INDEX: &[(&str, usize, usize)] = &[\n"
            .to_string();
    for (name, offset, len) in index {
        generated.push_str(&format!("    ({name:?}, {offset}, {len}),\n"));
    }
    generated.push_str("];\n");
    std::fs::write(out_dir.join("primer_index.rs"), generated).unwrap();
}

fn primer_required() -> bool {
    matches!(
        std::env::var("AUBE_REQUIRE_PRIMER").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn primer_compression_level() -> i32 {
    match std::env::var("PROFILE").as_deref() {
        Ok("release" | "release-native" | "release-pgo")
            if std::env::var_os("GITHUB_ACTIONS").is_some() =>
        {
            RELEASE_CI_COMPRESSION_LEVEL
        }
        _ => FAST_COMPRESSION_LEVEL,
    }
}
