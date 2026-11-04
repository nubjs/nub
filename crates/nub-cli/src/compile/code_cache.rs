//! Build-time ESM bytecode for native extracted artifacts. Node still loads the
//! original source and owns cache validation; this only seeds its existing cache.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use nub_core::compile::{AppFile, COMPILE_BOOTSTRAP_NAME};
use serde::Deserialize;

pub(super) const PACK_NAME: &str = "__nub_code_cache.bin";
const GENERATOR: &str = include_str!("code_cache_generate.cjs");
const INSTALLER: &str = include_str!("code_cache_install.cjs");
const MIN_SOURCE_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
struct Index {
    version: String,
    arch: String,
    tag: u32,
}

pub(super) fn attach(files: &mut Vec<AppFile<Vec<u8>>>, node: &Path) -> Result<bool> {
    // An included file can already own the optional pack's name. It must never
    // be overwritten or turn a previously valid build into a collision error.
    if files
        .iter()
        .any(|file| file.name.eq_ignore_ascii_case(PACK_NAME))
    {
        return Ok(false);
    }
    let Some(bootstrap) = files
        .iter()
        .position(|file| file.name == COMPILE_BOOTSTRAP_NAME)
    else {
        return Ok(false);
    };
    let packages: HashMap<_, _> = files
        .iter()
        .filter_map(|file| {
            let directory = if file.name == "package.json" {
                ""
            } else {
                file.name.strip_suffix("/package.json")?
            };
            let module = serde_json::from_slice::<serde_json::Value>(&file.bytes)
                .ok()
                .is_some_and(|json| json.get("type").and_then(|v| v.as_str()) == Some("module"));
            Some((directory, module))
        })
        .collect();
    let sources: Vec<_> = files
        .iter()
        .filter(|file| is_esm(&file.name, &packages))
        .filter_map(|file| {
            std::str::from_utf8(&file.bytes)
                .ok()
                .map(|source| (&file.name, source))
        })
        .collect();
    if sources
        .iter()
        .map(|(_, source)| source.len())
        .sum::<usize>()
        < MIN_SOURCE_BYTES
    {
        return Ok(false);
    }

    // File-backed stdin avoids blocking a producer against the child's output
    // pipe for a large bundle. No application module is linked or evaluated.
    let mut input = tempfile::tempfile().context("staging compile-cache sources")?;
    serde_json::to_writer(&mut input, &sources)?;
    input.seek(SeekFrom::Start(0))?;
    let output = Command::new(node)
        .args([
            "--predictable",
            "--experimental-vm-modules",
            "--no-warnings",
            "-e",
            GENERATOR,
        ])
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_COMPILE_CACHE")
        .env_remove("NODE_DISABLE_COMPILE_CACHE")
        .env_remove("NODE_REPL_EXTERNAL_MODULE")
        .stdin(Stdio::from(input))
        .output()
        .context("generating ESM compile caches with the target Node")?;
    if !output.status.success() {
        bail!(
            "target Node could not generate ESM caches: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let pack = output.stdout;
    let length: [u8; 4] = pack
        .get(..4)
        .context("missing compile-cache index")?
        .try_into()?;
    let index_end = 4usize
        .checked_add(u32::from_le_bytes(length) as usize)
        .context("compile-cache index is too large")?;
    let index: Index = serde_json::from_slice(
        pack.get(4..index_end)
            .context("truncated compile-cache index")?,
    )?;
    // Keep the extracted pack compressed too: raw eager bytecode can be several
    // times larger than the source. Only the first cache seed decompresses it.
    let pack = zstd::encode_all(&pack[..], 9).context("compressing the compile-cache pack")?;
    let id = crate::cli::sha256_hex(&pack);
    let args = serde_json::to_string(&(PACK_NAME, id, index.version, index.arch, index.tag))?;
    let suffix = format!("\n;{INSTALLER}(...{args});\n");
    files[bootstrap].bytes.extend_from_slice(suffix.as_bytes());
    files[bootstrap].plain_size = Some(files[bootstrap].bytes.len() as u64);
    files.push(AppFile::plain(PACK_NAME, pack));
    Ok(true)
}

fn is_esm(name: &str, packages: &HashMap<&str, bool>) -> bool {
    if name.ends_with(".mjs") {
        return true;
    }
    if !name.ends_with(".js") {
        return false;
    }
    let mut directory = name.rsplit_once('/').map_or("", |(parent, _)| parent);
    loop {
        // A package never inherits the surrounding project's module type.
        if directory == "node_modules" || directory.ends_with("/node_modules") {
            return false;
        }
        if let Some(module) = packages.get(directory) {
            return *module;
        }
        if directory.is_empty() {
            return false;
        }
        directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_node_accepts_generated_caches_and_preserves_fallbacks() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/compile/code_cache.test.cjs");
        let output = Command::new("node")
            .arg("--test")
            .arg(script)
            .env_remove("NODE_OPTIONS")
            .output()
            .expect("Node is required by the compile test suite");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn small_programs_do_not_start_a_cache_generator() {
        let mut files = vec![
            AppFile::plain(COMPILE_BOOTSTRAP_NAME, Vec::new()),
            AppFile::plain("main.mjs", b"console.log(1)".to_vec()),
        ];
        assert!(!attach(&mut files, Path::new("nonexistent-node")).unwrap());
        assert_eq!(files.len(), 2);
        assert!(files[0].bytes.is_empty());
    }

    #[test]
    fn an_included_pack_name_is_not_overwritten() {
        let mut files = vec![AppFile::plain(
            "__NUB_CODE_CACHE.BIN",
            b"user asset".to_vec(),
        )];
        assert!(!attach(&mut files, Path::new("nonexistent-node")).unwrap());
        assert_eq!(files[0].bytes, b"user asset");
    }

    #[test]
    fn js_cache_entries_follow_the_nearest_package_scope() {
        let packages = HashMap::from([
            ("", true),
            ("src/node_modules/esm", true),
            ("src/node_modules/esm/common", false),
            ("src/node_modules/cjs", false),
        ]);
        for name in [
            "main.js",
            "src/local.js",
            "src/node_modules/esm/lib/index.js",
            "src/node_modules/cjs/module.mjs",
        ] {
            assert!(is_esm(name, &packages), "{name}");
        }
        for name in [
            "main.cjs",
            "data.json",
            "src/node_modules/unknown/index.js",
            "src/node_modules/esm/common/index.js",
            "src/node_modules/cjs/index.js",
        ] {
            assert!(!is_esm(name, &packages), "{name}");
        }
    }

    #[test]
    fn attaching_a_pack_updates_the_bootstrap_size_and_caches_esm_js() {
        let version = Command::new("node")
            .args(["-p", "Number(process.versions.node.split('.')[0]) >= 24"])
            .env_remove("NODE_OPTIONS")
            .output()
            .expect("Node is required by the compile test suite");
        assert!(version.status.success());
        if version.stdout != b"true\n" && version.stdout != b"true\r\n" {
            return;
        }
        let source: String = (0..8000)
            .map(|i| format!("export function f{i}(x) {{ return x + {i}; }}\n"))
            .collect();
        let mut files = vec![
            AppFile::plain(COMPILE_BOOTSTRAP_NAME, b"// bootstrap\n".to_vec()),
            AppFile::plain("package.json", br#"{"type":"module"}"#.to_vec()),
            AppFile::plain("large.js", source.into_bytes()),
        ];
        assert!(attach(&mut files, Path::new("node")).unwrap());
        assert_eq!(files.len(), 4);
        for file in &files {
            assert_eq!(
                file.plain_size,
                Some(file.bytes.len() as u64),
                "{}",
                file.name
            );
        }
        let pack = zstd::decode_all(&files[3].bytes[..]).unwrap();
        let end = 4 + u32::from_le_bytes(pack[..4].try_into().unwrap()) as usize;
        let index: serde_json::Value = serde_json::from_slice(&pack[4..end]).unwrap();
        assert_eq!(index["entries"][0][0], "large.js");
    }
}
