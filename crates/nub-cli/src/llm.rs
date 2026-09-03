//! `nub llm` — the local-model runner (prerelease spike).
//!
//! Both verbs provision on first use and reuse the cache after: the pinned thin
//! llama.cpp build for this platform plus a GGUF model (see `nub_core::llm` for
//! the trust model). `serve` foregrounds `llama-server` on localhost with the
//! standard OpenAI-compatible API; `run` is the one-shot demo path — an
//! ephemeral server on a free port, one streamed chat completion, teardown.

use std::io::Write as _;
use std::net::TcpListener;
use std::process::{Command as ProcCommand, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::cli::{LlmCommand, LlmProvider};

pub fn run_llm(command: LlmCommand) -> Result<i32> {
    match command {
        LlmCommand::Serve { model, port, ctx } => serve(model.as_deref(), port, ctx),
        LlmCommand::Run {
            prompt,
            model,
            ctx,
            provider,
        } => match provider {
            LlmProvider::Engine => run_once(prompt, model.as_deref(), ctx),
            LlmProvider::Os => run_os(prompt),
        },
    }
}

fn engine_args(
    server: &std::path::Path,
    model: &std::path::Path,
    port: u16,
    ctx: Option<u32>,
) -> ProcCommand {
    let mut cmd = ProcCommand::new(server);
    cmd.arg("-m")
        .arg(model)
        .args(["--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        // `--jinja` applies the model's own chat template (tool calls included);
        // without it several current models fall back to a legacy template.
        .arg("--jinja");
    if let Some(c) = ctx {
        cmd.args(["-c", &c.to_string()]);
    }
    cmd
}

fn serve(model: Option<&str>, port: u16, ctx: Option<u32>) -> Result<i32> {
    let server = nub_core::llm::ensure_engine()?;
    let model = nub_core::llm::ensure_model(model)?;
    eprintln!(
        "Serving {} at http://127.0.0.1:{port}/v1 (OpenAI-compatible) — Ctrl-C stops it.",
        model.file_name().unwrap_or_default().to_string_lossy()
    );
    let status = engine_args(&server, &model, port, ctx)
        .status()
        .with_context(|| format!("launching {}", server.display()))?;
    Ok(status.code().unwrap_or(1))
}

fn run_once(prompt: Vec<String>, model: Option<&str>, ctx: Option<u32>) -> Result<i32> {
    let prompt = prompt.join(" ");
    if prompt.trim().is_empty() {
        bail!("give `nub llm run` a prompt, e.g. `nub llm run \"say hi\"`");
    }
    let server = nub_core::llm::ensure_engine()?;
    let model = nub_core::llm::ensure_model(model)?;
    // Bind-then-drop to pick a free localhost port for the ephemeral server.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .context("finding a free port")?
        .port();
    let log_path = std::env::temp_dir().join(format!("nub-llm-server-{port}.log"));
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("create {}", log_path.display()))?;
    let mut child = engine_args(&server, &model, port, ctx)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("launching {}", server.display()))?;
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(180);
    let ready = loop {
        if nub_core::llm::server_healthy(&base) {
            break true;
        }
        if let Some(status) = child.try_wait().ok().flatten() {
            eprintln!("{}", tail_of(&log_path, 20));
            bail!("the model server exited during startup ({status})");
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("{}", tail_of(&log_path, 20));
        bail!("the model server did not become ready within 180s");
    }
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": prompt}],
        "stream": true,
    });
    let result = nub_core::llm::chat_stream(&base, &body, |delta| {
        let mut out = std::io::stdout();
        let _ = out.write_all(delta.as_bytes());
        let _ = out.flush();
    });
    println!();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&log_path);
    result?;
    Ok(0)
}

/// The OS-model provider: Apple's on-device Foundation Models, reached through
/// a ~100 KB dependency-free Swift shim (`llm/fm-shim.swift`) compiled once on
/// demand with the system toolchain and cached. Zero engine bytes, zero model
/// download — the OS ships the weights. macOS 26+ on Apple Silicon only.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run_os(prompt: Vec<String>) -> Result<i32> {
    let prompt = prompt.join(" ");
    if prompt.trim().is_empty() {
        bail!("give `nub llm run` a prompt, e.g. `nub llm run --provider os \"say hi\"`");
    }
    let shim = ensure_fm_shim()?;
    let request = serde_json::json!({
        "messages": [{"role": "user", "content": prompt}],
        "stream": true,
        "format": "text",
    });
    let mut child = ProcCommand::new(&shim)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("launching {}", shim.display()))?;
    // An unavailable model exits before reading stdin, so a broken pipe here is
    // expected — let the exit-code mapping below name the real cause.
    let _ = child
        .stdin
        .take()
        .context("shim stdin")?
        .write_all(request.to_string().as_bytes());
    // stdin drops closed here; stream the reply through as it arrives.
    let mut out = child.stdout.take().context("shim stdout")?;
    std::io::copy(&mut out, &mut std::io::stdout()).context("streaming the reply")?;
    println!();
    let mut stderr_text = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read as _;
        let _ = e.read_to_string(&mut stderr_text);
    }
    let status = child.wait().context("waiting for the shim")?;
    match status.code() {
        Some(0) => Ok(0),
        Some(10) => bail!("this Mac's hardware cannot run the OS model (Apple Silicon required)"),
        Some(11) => bail!(
            "Apple Intelligence is not enabled — turn it on in System Settings, or use the default engine provider"
        ),
        Some(12) => bail!("the OS model is still downloading; try again in a few minutes"),
        Some(20) => bail!("the prompt exceeds the OS model's 4,096-token context window"),
        Some(21 | 22) => bail!("the OS model declined this prompt (safety guardrails)"),
        Some(23) => bail!("the OS model rate-limited this process; try again shortly"),
        code => bail!(
            "the OS-model shim failed (exit {code:?}): {}",
            stderr_text.trim()
        ),
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn run_os(_prompt: Vec<String>) -> Result<i32> {
    bail!(
        "the `os` provider needs macOS 26+ on Apple Silicon (no OS-shipped model exists on this platform); use the default engine provider"
    );
}

/// Compile-once-and-cache the Foundation Models shim. The source is embedded in
/// the nub binary (~9 KB); the system Swift toolchain builds it in a few
/// seconds. A production build would ship the prebuilt ~100 KB binary instead —
/// compiling here keeps the spike self-contained.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn ensure_fm_shim() -> Result<std::path::PathBuf> {
    const SHIM_SOURCE: &str = include_str!("llm/fm-shim.swift");
    let dir = nub_core::llm::cache_root()?.join("fm-shim");
    let bin = dir.join("fm-shim-v1");
    if bin.is_file() {
        return Ok(bin);
    }
    let swiftc = ProcCommand::new("xcrun")
        .args(["--find", "swiftc"])
        .output()
        .ok()
        .filter(|o| o.status.success());
    if swiftc.is_none() {
        bail!(
            "the `os` provider needs the Swift toolchain (`xcode-select --install`) for a one-time shim build, or use the default engine provider"
        );
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let src = dir.join("fm-shim.swift");
    std::fs::write(&src, SHIM_SOURCE).with_context(|| format!("write {}", src.display()))?;
    eprintln!("Building the OS-model shim (one-time, a few seconds)…");
    let out = ProcCommand::new("xcrun")
        .args(["swiftc", "-O", "-parse-as-library"])
        .args(["-target", "arm64-apple-macos26.0"])
        .args(["-framework", "FoundationModels"])
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .output()
        .context("running swiftc")?;
    if !out.status.success() {
        bail!(
            "building the OS-model shim failed (macOS 26+ SDK required):\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(bin)
}

/// The last `n` lines of the server log, for startup-failure diagnostics.
fn tail_of(path: &std::path::Path, n: usize) -> String {
    let Ok(body) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
