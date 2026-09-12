use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[allow(dead_code)]
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
//
// On top of the cap, `generate-primer.mjs` prunes versions older than 3
// years unless they are the highest of their `major.minor` line or a dist-tag
// target (`--prune-age-days`), and flags such a seed `sparse` so the resolver
// refetches any pick a dropped version could outrank
// (`semver_util::sparse_pick_needs_refetch`). Measured 2026-09-03 on the
// top-2000 primer: 97,292 -> 58,806 versions, compressed 10.05 MB -> 6.35 MB,
// and a cold resolve of the 84-dependency bench fixture (1,120 packuments)
// served the same 862 from the primer with and without the prune — zero extra
// fetches. The per-version SHA-512 is the primer's dominant byte cost and does
// not compress, so version count, not compression level, is the size lever.
const DEFAULT_VERSION_CAP: usize = 100;
const FAST_COMPRESSION_LEVEL: i32 = 10;
const RELEASE_CI_COMPRESSION_LEVEL: i32 = 19;
const POPULAR_NAMES_TOP: usize = 100_000;
const POPULAR_NAMES_FORMAT: u32 = 1;
// Bump when the on-disk rkyv schema (`src/primer_schema.rs`) changes
// in a layout-breaking way. The on-disk `primer-topN-vM-sK.rkyv.zst`
// artifact is gitignored, so older `sK` files orphan harmlessly and
// the new `sK+1` is regenerated on the next build.
const PRIMER_DATA_SCHEMA: u32 = 4;

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
    let popular_names_source = std::env::var_os("AUBE_POPULAR_NAMES_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            manifest_dir.join("data").join(format!(
                "popular-top{POPULAR_NAMES_TOP}-v{POPULAR_NAMES_FORMAT}.json"
            ))
        });

    println!("cargo:rerun-if-env-changed=AUBE_PRIMER_PATH");
    println!("cargo:rerun-if-env-changed=AUBE_POPULAR_NAMES_PATH");
    println!("cargo:rerun-if-env-changed=AUBE_PRIMER_TOP");
    println!("cargo:rerun-if-env-changed=AUBE_PRIMER_VERSION_CAP");
    println!("cargo:rerun-if-env-changed=AUBE_REQUIRE_PRIMER");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed={}", popular_names_source.display());
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
            let fallback_names = write_package_blob(&out_dir, &[]);
            write_popular_names_blob(&out_dir, &popular_names_source, &fallback_names);
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
    let fallback_names = write_package_blob(&out_dir, &bytes);
    write_popular_names_blob(&out_dir, &popular_names_source, &fallback_names);
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

    generate_popular_names(workspace, manifest_dir);

    compress_json_primer(&json, source);
    let _ = std::fs::remove_file(json);
    true
}

/// Emit the download-ranked popular-names corpus alongside a primer we just
/// built from source. Best-effort by design: see the failure note below.
///
/// Release builds ship this file in the primer artifact, so they never reach
/// `generate()` at all. Every SOURCE build does — homebrew-core, the Nix flake,
/// Fedora, AUR — and without the file `write_popular_names_blob` falls back to
/// primer-derived names and leaves `AUBE_POPULAR_NAMES_RANKED` unset. That does
/// not merely degrade the similar-name gate in `aube add`, it turns it OFF:
/// the fallback list is alphabetical, so "absent from the corpus" stops meaning
/// "nobody installs this" and the gate declines to run rather than report an
/// alphabetical position as a popularity rank. A source-built binary therefore
/// accepts a typosquat that a release binary refuses.
///
/// Three deliberate properties, each of which is what keeps this from being a
/// regression risk:
///
/// 1. It runs only AFTER the primer command succeeded. An offline or
///    network-blocked build has already failed that fetch and returned, so it
///    never pays for this call.
/// 2. Its failure is swallowed. The outcome is exactly the prior behaviour — no
///    corpus, gate declines to run — so this cannot turn a working build into a
///    broken one, and cannot degrade the primer it was called after.
/// 3. It fetches from the same host the primer path already requires
///    (`raw.githubusercontent.com`), so it introduces no new network dependency.
///
/// `--popular-names-only` makes the script write the corpus and exit before any
/// packument work, so the cost is a single request.
fn generate_popular_names(workspace: &Path, manifest_dir: &Path) {
    // Honour the override rather than writing a file main() will not read.
    if std::env::var_os("AUBE_POPULAR_NAMES_PATH").is_some() {
        return;
    }
    let data = manifest_dir.join("data");
    let dest = data.join(format!(
        "popular-top{POPULAR_NAMES_TOP}-v{POPULAR_NAMES_FORMAT}.json"
    ));
    if dest.is_file() {
        return;
    }
    // Write a sibling and rename, so `dest` only ever exists complete. The
    // generator writes ~2 MB with a single non-atomic `writeFile`, and the guard
    // above trusts existence — without the rename, a build interrupted mid-write
    // (Ctrl-C, OOM, a cancelled CI job) leaves truncated JSON that is never
    // regenerated and that `write_popular_names_blob` PANICS on, so every later
    // build fails until someone hand-deletes a gitignored file. The `.partial.`
    // name still matches the `popular-top*.json` ignore rule, so an interrupted
    // run cannot leave an untracked file behind either.
    let tmp = data.join(format!(
        "popular-top{POPULAR_NAMES_TOP}-v{POPULAR_NAMES_FORMAT}.partial.json"
    ));
    let ok = Command::new("node")
        .arg(workspace.join("scripts/generate-primer.mjs"))
        .arg("--popular-names-only")
        .arg("--popular-names-out")
        .arg(&tmp)
        .status()
        .is_ok_and(|s| s.success())
        && std::fs::rename(&tmp, &dest).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        println!(
            "cargo:warning=could not generate the popular package-name corpus; the similar-name \
             check on `add` stays disabled in this build (resolution and installs are unaffected)"
        );
    }
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

fn write_package_blob(out_dir: &Path, compressed: &[u8]) -> Vec<String> {
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
    let fallback_names = index.iter().map(|(name, _, _)| name.clone()).collect();
    for (name, offset, len) in index {
        generated.push_str(&format!("    ({name:?}, {offset}, {len}),\n"));
    }
    generated.push_str(
        "];\nstatic POPULAR_NAMES_BLOB: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/popular-names.bin\"));\n",
    );
    std::fs::write(out_dir.join("primer_index.rs"), generated).unwrap();
    fallback_names
}

/// Emit the popularity corpus, and tell the crate whether what it got is a real
/// DOWNLOAD-RANKED list or the degraded fallback.
///
/// The distinction is load-bearing, not cosmetic. The real file is ordered
/// most-downloaded first, so a name's index IS its popularity rank. The fallback
/// is `fallback_names`, which `write_package_blob` derives by iterating a
/// `BTreeMap` — i.e. ALPHABETICALLY — and holds only `primer_top()` entries (100
/// in dev, 2000 in release). Reporting an alphabetical position in a 100-name
/// list as a "top-100,000 popularity rank" is a lie, and every consumer that
/// reasons about relative popularity is wrong on it, so the similar-name gate
/// gates itself on this flag instead.
fn write_popular_names_blob(out_dir: &Path, source: &Path, fallback_names: &[String]) {
    if source.is_file() {
        println!("cargo:rustc-env=AUBE_POPULAR_NAMES_RANKED=1");
    }
    let names = if source.is_file() {
        let input = std::fs::read(source).unwrap_or_else(|e| {
            panic!(
                "failed to read popular package names {}: {e}",
                source.display()
            )
        });
        serde_json::from_slice::<Vec<String>>(&input).unwrap_or_else(|e| {
            panic!(
                "failed to parse popular package names {}: {e}",
                source.display()
            )
        })
    } else {
        if std::env::var_os("AUBE_POPULAR_NAMES_PATH").is_some() || primer_required() {
            panic!(
                "popular package names are required, but {} was missing",
                source.display()
            );
        }
        fallback_names.to_vec()
    };
    if primer_required() && names.len() != POPULAR_NAMES_TOP {
        panic!(
            "popular package names must contain exactly {POPULAR_NAMES_TOP} entries, found {}",
            names.len()
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for name in &names {
        if name.is_empty()
            || name.bytes().any(|byte| byte.is_ascii_whitespace())
            || !seen.insert(name)
        {
            panic!("popular package names contain an invalid or duplicate entry: {name:?}");
        }
    }
    let joined = names.join("\n");
    let compressed =
        zstd::stream::encode_all(Cursor::new(joined.as_bytes()), primer_compression_level())
            .unwrap();
    std::fs::write(out_dir.join("popular-names.bin"), compressed).unwrap();
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
