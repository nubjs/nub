//! `nub compile` — the compile-time pipeline (spike, host triple only).
//!
//! Runs in the full `nub` CLI on the dev machine: bundle the entry with Rolldown
//! in-process, obtain + strip + compress a Node for the target (default shape),
//! and section-inject the payload into a copy of the `nub-launcher` template,
//! ad-hoc re-signing the result. The launcher (`crates/nub-launcher`) carries the
//! runtime half. Behind the `compile` cargo feature so the heavy Rolldown/libsui/
//! zstd deps don't burden the default build.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::{Manifest, Shape, encode};
use nub_core::node::discovery;
use nub_core::node::version::{NodeVersion, VersionPin};
use nub_core::version_management;
use sha2::{Digest, Sha256};

pub struct CompileOptions {
    pub entry: String,
    pub out: Option<String>,
    pub smol: bool,
    /// Explicit `--target`; `None` → infer from the project's pin chain.
    pub target: Option<String>,
    pub platform: Option<String>,
    pub minify: bool,
}

struct ChunkOut {
    filename: String,
    code: Vec<u8>,
    is_entry: bool,
}

/// The bundled app files to embed: `(name, bytes)` per file (entry + chunks + the
/// synthesized `package.json`).
type AppFiles = Vec<(String, Vec<u8>)>;

pub fn run(opts: CompileOptions) -> Result<i32> {
    // The section-injection path is Mach-O-only in this spike (libsui `Macho`).
    // Fail fast with a clear message rather than deep inside libsui trying to
    // parse a non-Mach-O template on a Linux/Windows host.
    if !cfg!(target_os = "macos") {
        bail!("nub compile is macOS-host-only in this spike (ELF/PE injection is not wired yet)");
    }

    let host = host_triple();
    if let Some(p) = &opts.platform {
        if *p != host {
            bail!(
                "cross-compiling to '{p}' is not implemented in this spike (host is {host}); \
                 omit --platform to compile for the host"
            );
        }
    }

    let entry_path = Path::new(&opts.entry);
    if !entry_path.is_file() {
        bail!("entry file not found: {}", opts.entry);
    }
    let entry_abs =
        fs::canonicalize(entry_path).with_context(|| format!("resolving entry {}", opts.entry))?;
    let stem = entry_abs
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app".into());
    let out_path = opts
        .out
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&stem));

    // 1. Bundle (Rolldown, in-process).
    eprintln!("Bundling {} …", opts.entry);
    let chunks = bundle(&entry_abs, opts.minify)?;
    let (entry_name, app_files) = assemble_app(chunks)?;
    let app_sha = sha256_of_app(&app_files);

    // 2. Resolve the Node version through nub run's SAME pin chain (so compile
    //    can't drift from run); --target overrides it. The pin context is the
    //    entry's project dir (walk up from there).
    let shape = if opts.smol { Shape::Smol } else { Shape::Embed };
    let cache_root = discovery::cache_dir()
        .context("no writable cache dir for compile-time Node provisioning")?;
    let pin_cwd = entry_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let (pin, raw, source) = determine_target(opts.target.as_deref(), &pin_cwd)?;

    let (node_version, node_range, node_blob, node_sha) = if opts.smol {
        // Smol keeps a range: bake the acceptance FLOOR the launcher enforces
        // (`discovered >= floor`) and record the raw requirement for provenance.
        let floor = version_management::pin_floor(&pin, &cache_root)?;
        let range = non_exact_spec(&pin, &raw);
        eprintln!(
            "Using Node.js {} (resolved from {source}; satisfied at runtime)",
            range.clone().unwrap_or_else(|| floor.to_string())
        );
        (floor, range, Vec::new(), String::new())
    } else {
        // Embed bakes ONE exact version — a range/major/alias collapses to the
        // newest satisfying release at compile time. (`build_node_blob` →
        // `provision_host_node` prints the `Using Node.js … (resolved from …)`
        // line, the same surface nub run uses.)
        let exact = version_management::resolve_host_pin(&pin, &cache_root)?;
        let (v, blob, sha) = build_node_blob(&exact, &cache_root, &source)?;
        (v, None, blob, sha)
    };

    // 3. Manifest + payload.
    let manifest = Manifest {
        shape,
        entry: entry_name,
        node_version: node_version.to_string(),
        node_range,
        triple: host.clone(),
        node_sha256: node_sha,
        app_sha256: app_sha,
        minify: opts.minify,
    };
    let payload = encode(&manifest, &app_files, &node_blob);

    // 4. Inject into the launcher template + ad-hoc re-sign.
    let template = locate_launcher_template()?;
    inject_and_sign(&template, &payload, &out_path)
        .with_context(|| format!("writing {}", out_path.display()))?;
    set_executable(&out_path)?;
    smoke_probe(&out_path)?;

    let size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "Compiled {} — {} shape, Node {}, {:.1} MB",
        out_path.display(),
        if opts.smol { "smol" } else { "embed" },
        node_version,
        size as f64 / 1_000_000.0
    );
    Ok(0)
}

// ---- version resolution -------------------------------------------------------

/// Resolve the target into `(pin, raw_spec, source_label)`. `--target` overrides
/// everything; otherwise the SAME pin chain `nub run` uses (`resolve_pin_chain`:
/// devEngines.runtime → .node-version → .nvmrc → .tool-versions → engines.node).
/// No silent "latest" fallback — a compiled binary's Node version must be
/// intentional/reproducible, so nothing found + no `--target` is an error (this
/// diverges from `nub run`, which falls back to latest).
fn determine_target(target: Option<&str>, cwd: &Path) -> Result<(VersionPin, String, String)> {
    if let Some(t) = target {
        return Ok((
            version_management::parse_target_spec(t)?,
            t.to_string(),
            "--target".to_string(),
        ));
    }
    match discovery::resolve_pin_chain(cwd)?.pin {
        Some((raw, pin, source)) => Ok((pin, raw, source)),
        None => bail!(
            "no Node version could be inferred for this project.\n\
             \x20\x20Pass --target <version> (e.g. --target 24, --target lts), or add a pin —\n\
             \x20\x20a .node-version file, or package.json \"engines\": {{ \"node\": \"…\" }}.\n\
             \x20\x20(nub compile does not fall back to \"latest\": a compiled binary's Node\n\
             \x20\x20version must be intentional and reproducible.)"
        ),
    }
}

/// The raw requirement to record for `--smol` — `None` for a bare exact version
/// (the floor already captures it), else the original spec string.
fn non_exact_spec(pin: &VersionPin, raw: &str) -> Option<String> {
    match pin {
        VersionPin::Exact(_) => None,
        _ => Some(raw.to_string()),
    }
}

// ---- bundling -----------------------------------------------------------------

fn bundle(entry_abs: &Path, minify: bool) -> Result<Vec<ChunkOut>> {
    use rolldown::{Bundler, BundlerOptions, InputItem, OutputFormat, Platform, RawMinifyOptions};

    let cwd = entry_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let import = format!(
        "./{}",
        entry_abs.file_name().unwrap_or_default().to_string_lossy()
    );
    let stem = entry_abs
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main".into());

    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some(stem),
            import,
        }]),
        cwd: Some(cwd),
        format: Some(OutputFormat::Esm),
        platform: Some(Platform::Node),
        minify: Some(RawMinifyOptions::Bool(minify)),
        ..Default::default()
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the bundler runtime")?;
    let output = rt.block_on(async move {
        let mut bundler = Bundler::new(options).map_err(|e| anyhow!("rolldown init: {e:?}"))?;
        bundler
            .generate()
            .await
            .map_err(|e| anyhow!("rolldown bundle failed: {e:?}"))
    })?;

    let mut chunks = Vec::new();
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(c) = asset {
            chunks.push(ChunkOut {
                filename: c.filename.to_string(),
                code: c.code.as_bytes().to_vec(),
                is_entry: c.is_entry,
            });
        }
        // Static (`Output::Asset`) emit is out of scope for the spike.
    }
    if chunks.is_empty() {
        bail!("the bundler produced no chunks");
    }
    Ok(chunks)
}

/// Turn the bundler chunks into the app file set: every chunk verbatim, plus a
/// `package.json {"type":"module"}` so Node runs the ESM `.js` entry as ESM.
/// Returns the entry filename (the one entry chunk) and the file list.
fn assemble_app(chunks: Vec<ChunkOut>) -> Result<(String, AppFiles)> {
    let entry_name = chunks
        .iter()
        .find(|c| c.is_entry)
        .map(|c| c.filename.clone())
        .context("the bundler emitted no entry chunk")?;

    let mut files: Vec<(String, Vec<u8>)> =
        chunks.into_iter().map(|c| (c.filename, c.code)).collect();
    files.push(("package.json".to_string(), br#"{"type":"module"}"#.to_vec()));
    Ok((entry_name, files))
}

// ---- Node blob (default/embed shape) ------------------------------------------

/// Provision the official Node for the target, strip + ad-hoc re-sign it (when
/// the host has the tools), and zstd-19 compress. Returns the concrete version,
/// the compressed blob, and the hash of the DECOMPRESSED (runnable) bytes.
fn build_node_blob(
    version: &NodeVersion,
    cache_root: &Path,
    resolved_from: &str,
) -> Result<(NodeVersion, Vec<u8>, String)> {
    // The exact version's own string round-trips the index; provision prints the
    // `Using Node.js <v> (resolved from <source>)` line + downloads.
    let (version, dir) = version_management::provision_host_node(
        &version.to_string(),
        cache_root,
        Some(resolved_from),
    )?;
    let node_bin = dir.join("bin").join("node");
    if !node_bin.is_file() {
        bail!(
            "provisioned Node {version} but its binary is missing at {}",
            node_bin.display()
        );
    }

    let bytes = strip_and_resign(&node_bin)?;
    let sha = sha256_hex(&bytes);
    eprintln!(
        "Compressing Node ({:.0} MB) with zstd-19 …",
        bytes.len() as f64 / 1_000_000.0
    );
    let blob = zstd::encode_all(&bytes[..], 19).context("zstd-19 compressing Node")?;
    Ok((version, blob, sha))
}

/// Produce the runnable Node bytes to embed: strip symbols (llvm-strip/strip if
/// present) then ad-hoc re-sign (stripping invalidates the Mach-O signature, and
/// an arm64 binary must be at least ad-hoc signed to run) — verifying the result
/// still executes, and falling back to the unstripped-but-validly-signed original
/// on any failure. Records which path was taken.
fn strip_and_resign(node_bin: &Path) -> Result<Vec<u8>> {
    let original = fs::read(node_bin).with_context(|| format!("reading {}", node_bin.display()))?;

    let strip_tool = which_first(&["llvm-strip", "strip"]);
    let Some(strip) = strip_tool else {
        eprintln!("note: no llvm-strip/strip on PATH — embedding the Node binary unstripped");
        return Ok(original);
    };

    let tmp = std::env::temp_dir().join(format!("nub-compile-node-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, &original).with_context(|| format!("staging Node at {}", tmp.display()))?;
    // fs::write lands 0644; the post-resign `--version` verification must be able to
    // EXEC the staged binary, so restore the executable bit before strip/verify.
    set_executable(&tmp)?;
    let _guard = FileGuard(tmp.clone());

    let stripped_ok = run_ok(&strip, &[tmp.as_os_str()]);
    if stripped_ok && which_first(&["codesign"]).is_some() {
        let resigned = run_ok(
            "codesign",
            &[
                "--force".as_ref(),
                "-s".as_ref(),
                "-".as_ref(),
                tmp.as_os_str(),
            ],
        );
        if resigned && run_ok(tmp.to_str().unwrap_or_default(), &["--version".as_ref()]) {
            eprintln!("Stripped + ad-hoc re-signed the embedded Node");
            return fs::read(&tmp).with_context(|| format!("reading stripped {}", tmp.display()));
        }
    }

    eprintln!("note: strip/re-sign unavailable or failed verification — embedding Node unstripped");
    Ok(original)
}

// ---- launcher template + injection --------------------------------------------

fn locate_launcher_template() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("NUB_LAUNCHER_TEMPLATE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        bail!(
            "NUB_LAUNCHER_TEMPLATE points at a missing file: {}",
            p.display()
        );
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("nub-launcher");
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    bail!(
        "could not find the nub-launcher template. Set NUB_LAUNCHER_TEMPLATE to its path \
         (spike: build crates/nub-launcher and point it at target/release/nub-launcher)."
    )
}

fn inject_and_sign(template: &Path, payload: &[u8], out: &Path) -> Result<()> {
    let template_bytes = fs::read(template)
        .with_context(|| format!("reading launcher template {}", template.display()))?;
    let macho = libsui::Macho::from(template_bytes)
        .map_err(|e| anyhow!("parsing the launcher template as Mach-O: {e:?}"))?;
    let macho = macho
        .write_section(nub_core::compile::SECTION_NAME, payload.to_vec())
        .map_err(|e| anyhow!("injecting the payload section: {e:?}"))?;
    let mut file = fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    macho
        .build_and_sign(&mut file)
        .map_err(|e| anyhow!("building + ad-hoc signing the executable: {e:?}"))?;
    Ok(())
}

/// Run the produced binary's `__probe` self-check: it reads + decodes the injected
/// section and touches a `String` allocation — the exact path an under-padded
/// section injection corrupts into a SIGILL trap. Catching that HERE turns a
/// would-be runtime crash for the user into a compile-time error.
fn smoke_probe(bin: &Path) -> Result<()> {
    // `Command::new` PATH-searches a bare name, so the default `--out` (the entry
    // stem, no directory component) would probe a stray PATH binary or fail to
    // spawn. Anchor a relative path to the cwd the file was just written to.
    let bin = if bin.is_absolute() || bin.components().count() > 1 {
        bin.to_path_buf()
    } else {
        Path::new(".").join(bin)
    };
    let out = std::process::Command::new(&bin)
        .arg("__probe")
        .output()
        .with_context(|| format!("running the self-probe on {}", bin.display()))?;
    let ok =
        out.status.success() && String::from_utf8_lossy(&out.stdout).starts_with("nub-probe ok");
    if !ok {
        bail!(
            "the produced executable failed its self-probe (exit {:?}) — the launcher template \
             likely has insufficient Mach-O header padding for section injection (see \
             crates/nub-launcher/build.rs)",
            out.status.code()
        );
    }
    Ok(())
}

// ---- helpers ------------------------------------------------------------------

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn host_triple() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

fn which_first(names: &[&str]) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for name in names {
        for dir in std::env::split_paths(&path) {
            if dir.join(name).is_file() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn run_ok(program: &str, args: &[&std::ffi::OsStr]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Content hash of the app payload — name + length + bytes of each file, in order.
fn sha256_of_app(files: &[(String, Vec<u8>)]) -> String {
    let mut h = Sha256::new();
    for (name, data) in files {
        h.update(name.as_bytes());
        h.update((data.len() as u64).to_le_bytes());
        h.update(data);
    }
    format!("{:x}", h.finalize())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nub-compile-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn no_pin_and_no_target_errors_without_falling_back_to_latest() {
        let dir = fresh_dir("nopin");
        let err = determine_target(None, &dir).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--target"), "should point at --target: {msg}");
        assert!(
            msg.contains("reproducible"),
            "should state the reproducibility rationale: {msg}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn infers_from_node_version_file_and_reports_the_source() {
        let dir = fresh_dir("nodever");
        fs::write(dir.join(".node-version"), "22\n").unwrap();
        let (_pin, raw, source) = determine_target(None, &dir).unwrap();
        assert_eq!(raw, "22");
        assert_eq!(source, ".node-version");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_target_overrides_the_chain() {
        let dir = fresh_dir("override");
        fs::write(dir.join(".node-version"), "18\n").unwrap();
        let (pin, raw, source) = determine_target(Some("24.5.0"), &dir).unwrap();
        assert_eq!(raw, "24.5.0");
        assert_eq!(source, "--target");
        assert!(matches!(pin, VersionPin::Exact(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_exact_spec_records_ranges_but_not_bare_exacts() {
        let exact = version_management::parse_target_spec("24.5.0").unwrap();
        assert_eq!(non_exact_spec(&exact, "24.5.0"), None);
        let range = version_management::parse_target_spec(">=20").unwrap();
        assert_eq!(non_exact_spec(&range, ">=20"), Some(">=20".to_string()));
        let major = version_management::parse_target_spec("24").unwrap();
        assert_eq!(non_exact_spec(&major, "24"), Some("24".to_string()));
    }
}
