//! Local-model runner provisioning (prerelease spike; `wiki` has no doc yet —
//! design record: internal research, local-ai-runner).
//!
//! Nothing inference-shaped ships inside the nub binary. The engine is a pinned
//! llama.cpp release build — the THIN variant per platform, the one that
//! delegates kernel compilation to the OS or GPU driver (Metal on macOS, Vulkan
//! on Linux/win-x64, OpenCL on win-arm64) — provisioned on demand into the nub
//! cache exactly like a Node version: HTTPS + pinned SHA-256, verify before
//! extract, fail closed. Models are GGUF files fetched from Hugging Face; the
//! default model's digest is pinned here, and a user-named repo is verified
//! against the SHA-256 the Hugging Face API publishes for its LFS blobs (same
//! trust shape as nodejs.org's SHASUMS256.txt: HTTPS authenticates the manifest,
//! the digest authenticates the artifact).
//!
//! The user-facing surface stays neutral by design: the engine serves the
//! standard OpenAI-compatible HTTP API on localhost, so application code uses
//! any client library and leaving nub is a base-URL change (the reversibility
//! filter). CUDA/ROCm builds (hundreds of MB of precompiled vendor kernels) are
//! deliberately absent from this manifest; if they ever arrive it is as an
//! explicit opt-in download, never a default.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::node::discovery;
use crate::version_management::download;
use crate::version_management::extract;

/// The pinned llama.cpp release build. Bumping it is a one-line change plus new
/// asset digests below (compute with `shasum -a 256` over the release assets).
pub const ENGINE_BUILD: &str = "b10767";

/// One platform's engine artifact: the thin, OS-delegating build only.
struct EngineAsset {
    /// `(os, arch)` per Rust's `std::env::consts` spelling.
    platform: (&'static str, &'static str),
    asset: &'static str,
    sha256: &'static str,
    /// GPU story this artifact delegates to, for the provisioning announce line.
    backend: &'static str,
}

/// llama.cpp publishes no musl or darwin-x64 thin builds; those hosts get a
/// clear error rather than a guessed artifact. linux-arm64 has no Vulkan asset
/// either — the ubuntu tarballs are x64-only, so arm64 Linux is also absent
/// from the spike.
const ENGINE_ASSETS: &[EngineAsset] = &[
    EngineAsset {
        platform: ("macos", "aarch64"),
        asset: "llama-b10767-bin-macos-arm64.tar.gz",
        sha256: "6a103c6e76023f798e4f94dc69728896207e0e0afad1e35227d88375b367890a",
        backend: "Metal",
    },
    EngineAsset {
        platform: ("linux", "x86_64"),
        asset: "llama-b10767-bin-ubuntu-vulkan-x64.tar.gz",
        sha256: "571f61771af336071b8d42b3a576089efc4f152eadeeea4c540fc51e3111bad7",
        backend: "Vulkan",
    },
    EngineAsset {
        platform: ("windows", "x86_64"),
        asset: "llama-b10767-bin-win-vulkan-x64.zip",
        sha256: "1d79bf404c8879b448320ac537c318f357a474e1729583b1a0e57b69a7bb0675",
        backend: "Vulkan",
    },
    EngineAsset {
        platform: ("windows", "aarch64"),
        asset: "llama-b10767-bin-win-opencl-adreno-arm64.zip",
        sha256: "0338447ed87d35a4679be5e5171e2e77bf7736fbb08157567e41419ee9f55a5b",
        backend: "OpenCL",
    },
];

/// The default model when `--model` is absent: small enough to download in one
/// sitting, capable enough to demo. Digest pinned from the Hugging Face LFS
/// manifest at pin time.
pub const DEFAULT_MODEL_REPO: &str = "ggml-org/Qwen3-4B-GGUF";
pub const DEFAULT_MODEL_FILE: &str = "Qwen3-4B-Q4_K_M.gguf";
const DEFAULT_MODEL_SHA256: &str =
    "ab27b9bfa375a178d6cba48f3ad892b94b7739659dcc7aae8058ce0ffed6b328";

/// `~/.cache/nub/llm` (XDG-aware via the shared cache-dir resolution).
pub fn cache_root() -> Result<PathBuf> {
    let base = discovery::cache_dir().context("no usable cache directory for nub")?;
    Ok(base.join("llm"))
}

fn host_engine_asset() -> Result<&'static EngineAsset> {
    let host = (std::env::consts::OS, std::env::consts::ARCH);
    if host.0 == "linux" && crate::version_management::host_is_musl() {
        bail!(
            "`nub llm` has no engine build for musl Linux yet (llama.cpp publishes none); glibc Linux, macOS arm64, and Windows are covered"
        );
    }
    ENGINE_ASSETS
        .iter()
        .find(|a| a.platform == host)
        .with_context(|| {
            format!(
                "`nub llm` has no engine build for {}-{} yet",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })
}

/// Provision (or reuse) the pinned engine; returns the path to `llama-server`.
pub fn ensure_engine() -> Result<PathBuf> {
    let asset = host_engine_asset()?;
    let root = cache_root()?;
    let dest_parent = root.join("engine").join(ENGINE_BUILD);
    let server_name = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    let server = dest_parent
        .join(format!("llama-{ENGINE_BUILD}"))
        .join(server_name);
    if server.is_file() {
        return Ok(server);
    }
    let url = format!(
        "https://github.com/ggml-org/llama.cpp/releases/download/{ENGINE_BUILD}/{}",
        asset.asset
    );
    eprintln!(
        "Provisioning the local-model engine (llama.cpp {ENGINE_BUILD}, {} backend)…",
        asset.backend
    );
    let staging = root.join("staging");
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("create {}", staging.display()))?;
    let archive = staging.join(asset.asset);
    let got = download_with_progress(&url, &archive, "engine")?;
    if got != asset.sha256 {
        let _ = std::fs::remove_file(&archive);
        bail!(
            "engine archive checksum mismatch for {} (expected {}, got {got}) — refusing to extract",
            asset.asset,
            asset.sha256
        );
    }
    // Extract into a temp dir beside the final location, then rename, so a
    // killed extraction never leaves a half-tree where the `is_file` probe
    // above would trust it.
    let tmp = dest_parent.with_extension("partial");
    let _ = std::fs::remove_dir_all(&tmp);
    let top = extract::extract_archive(&archive, &tmp)?;
    let top_name = top
        .file_name()
        .context("extracted engine tree has no name")?
        .to_owned();
    std::fs::create_dir_all(&dest_parent)
        .with_context(|| format!("create {}", dest_parent.display()))?;
    let final_top = dest_parent.join(&top_name);
    let _ = std::fs::remove_dir_all(&final_top);
    std::fs::rename(&top, &final_top)
        .with_context(|| format!("commit engine into {}", final_top.display()))?;
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_file(&archive);
    if !server.is_file() {
        bail!(
            "engine archive extracted but {} is missing — archive layout changed upstream?",
            server.display()
        );
    }
    Ok(server)
}

/// Resolve + provision the requested model; returns the local GGUF path.
///
/// Accepted specs: absent (the pinned default), a filesystem path to a `.gguf`,
/// `owner/repo` (picks the repo's `Q4_K_M` file, or its only GGUF), or
/// `owner/repo:file.gguf` / `owner/repo:QUANT`. Every Hugging Face download is
/// verified against the SHA-256 the HF API publishes for the LFS blob.
pub fn ensure_model(spec: Option<&str>) -> Result<PathBuf> {
    let root = cache_root()?;
    match spec {
        None => fetch_hf_model(
            &root,
            DEFAULT_MODEL_REPO,
            DEFAULT_MODEL_FILE,
            Some(DEFAULT_MODEL_SHA256),
        ),
        Some(s) if s.ends_with(".gguf") && Path::new(s).is_file() => {
            Ok(PathBuf::from(s))
        }
        Some(s) => {
            let (repo, file_hint) = match s.split_once(':') {
                Some((r, f)) => (r, Some(f)),
                None => (s, None),
            };
            if !repo.contains('/') {
                bail!(
                    "model spec `{s}` is not a local .gguf file or a Hugging Face `owner/repo[:file]` reference"
                );
            }
            let file = resolve_hf_file(repo, file_hint)?;
            fetch_hf_model(&root, repo, &file, None)
        }
    }
}

/// One row of the HF tree listing we care about.
struct HfFile {
    path: String,
    sha256: Option<String>,
    size: u64,
}

fn hf_tree(repo: &str) -> Result<Vec<HfFile>> {
    let url = format!("https://huggingface.co/api/models/{repo}/tree/main");
    let body = download::fetch_text_auth(&url, None)
        .with_context(|| format!("listing Hugging Face repo {repo}"))?;
    let rows: serde_json::Value =
        serde_json::from_str(&body).context("parsing the Hugging Face tree listing")?;
    let rows = rows
        .as_array()
        .context("unexpected Hugging Face tree shape")?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let path = r.get("path")?.as_str()?.to_string();
            let size = r.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            let sha256 = r
                .get("lfs")
                .and_then(|l| l.get("oid"))
                .and_then(|o| o.as_str())
                .map(str::to_string);
            Some(HfFile { path, sha256, size })
        })
        .collect())
}

/// Pick the GGUF file a bare `owner/repo` (or `owner/repo:QUANT`) means.
fn resolve_hf_file(repo: &str, hint: Option<&str>) -> Result<String> {
    let files = hf_tree(repo)?;
    let ggufs: Vec<&HfFile> = files
        .iter()
        .filter(|f| f.path.ends_with(".gguf"))
        .collect();
    if ggufs.is_empty() {
        bail!("{repo} holds no .gguf files");
    }
    if let Some(h) = hint {
        if let Some(f) = ggufs.iter().find(|f| f.path == h) {
            return Ok(f.path.clone());
        }
        let needle = h.to_ascii_lowercase();
        let matches: Vec<&&HfFile> = ggufs
            .iter()
            .filter(|f| f.path.to_ascii_lowercase().contains(&needle))
            .collect();
        match matches.len() {
            1 => return Ok(matches[0].path.clone()),
            0 => bail!(
                "{repo} has no GGUF matching `{h}`; available: {}",
                list_names(&ggufs)
            ),
            _ => bail!(
                "`{h}` is ambiguous in {repo}: {}",
                list_names(&matches.iter().map(|f| **f).collect::<Vec<_>>())
            ),
        }
    }
    if ggufs.len() == 1 {
        return Ok(ggufs[0].path.clone());
    }
    if let Some(f) = ggufs.iter().find(|f| f.path.contains("Q4_K_M")) {
        return Ok(f.path.clone());
    }
    bail!(
        "{repo} holds several GGUF files; pick one with `{repo}:<file>`: {}",
        list_names(&ggufs)
    )
}

fn list_names(files: &[&HfFile]) -> String {
    files
        .iter()
        .map(|f| f.path.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Download `repo/file` into the model cache (or reuse it), verifying SHA-256.
/// `pinned` overrides the API-published digest for the built-in default.
fn fetch_hf_model(
    root: &Path,
    repo: &str,
    file: &str,
    pinned: Option<&str>,
) -> Result<PathBuf> {
    let dir = root.join("models").join(repo.replace('/', "--"));
    let dest = dir.join(file);
    if dest.is_file() {
        return Ok(dest);
    }
    if file.contains("-of-") {
        bail!("multi-part GGUF files ({file}) are not supported yet — pick a single-file quantization");
    }
    let expected = match pinned {
        Some(sha) => sha.to_string(),
        None => {
            let files = hf_tree(repo)?;
            let row = files
                .iter()
                .find(|f| f.path == file)
                .with_context(|| format!("{file} not found in {repo}"))?;
            row.sha256.clone().with_context(|| {
                format!("{repo}/{file} has no LFS digest in the Hugging Face listing — cannot verify it")
            })?
        }
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");
    eprintln!("Downloading model {repo} ({file})…");
    let partial = dest.with_extension("gguf.partial");
    let got = download_with_progress(&url, &partial, "model")?;
    if got != expected {
        let _ = std::fs::remove_file(&partial);
        bail!(
            "model checksum mismatch for {repo}/{file} (expected {expected}, got {got}) — refusing to keep it"
        );
    }
    std::fs::rename(&partial, &dest)
        .with_context(|| format!("commit model into {}", dest.display()))?;
    Ok(dest)
}

/// Stream a download with a coarse stderr progress line (tty only), returning
/// the SHA-256 of the bytes written.
fn download_with_progress(url: &str, dest: &Path, what: &str) -> Result<String> {
    use std::io::IsTerminal;
    let tty = std::io::stderr().is_terminal();
    let mut last_pct: i64 = -1;
    let sha = download::download_to_file(url, dest, |done, total| {
        if !tty {
            return;
        }
        if let Some(total) = total.filter(|t| *t > 0) {
            let pct = (done * 100 / total) as i64;
            if pct != last_pct {
                last_pct = pct;
                let mut err = std::io::stderr();
                let _ = write!(
                    err,
                    "\r  {what}: {pct}% of {:.1} MiB",
                    total as f64 / (1024.0 * 1024.0)
                );
                let _ = err.flush();
            }
        }
    })?;
    if tty && last_pct >= 0 {
        eprintln!();
    }
    Ok(sha)
}

/// POST an OpenAI-style chat request to `base_url` and stream the response's
/// text deltas into `on_delta`. Blocking; used by the one-shot `nub llm run`.
pub fn chat_stream(
    base_url: &str,
    body: &serde_json::Value,
    mut on_delta: impl FnMut(&str),
) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(None)
        .build()
        .context("building the chat HTTP client")?;
    // Serialize by hand: nub-core's reqwest omits the `json` feature.
    let resp = client
        .post(format!("{base_url}/v1/chat/completions"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .context("sending the chat request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("chat request failed: {status}: {text}");
    }
    // The response is SSE: `data: {json}` lines, terminated by `data: [DONE]`.
    let reader = std::io::BufReader::new(resp);
    for line in std::io::BufRead::lines(reader) {
        let line = line.context("reading the chat stream")?;
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload.trim() == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        if let Some(delta) = v
            .pointer("/choices/0/delta/content")
            .and_then(|c| c.as_str())
        {
            on_delta(delta);
        }
    }
    Ok(())
}

/// GET `{base_url}/health`, true on HTTP 200 — llama-server's readiness probe.
pub fn server_healthy(base_url: &str) -> bool {
    download::fetch_text_auth(&format!("{base_url}/health"), None).is_ok()
}
