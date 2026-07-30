//! `nub compile` — the compile-time pipeline.
//!
//! Runs in the full `nub` CLI on the dev machine: bundle the entry with Rolldown
//! in-process, obtain + strip + compress a Node for the target (default shape),
//! and inject the payload into a copy of the `nub-launcher` template built for
//! the target platform. The launcher (`crates/nub-launcher`) carries the runtime
//! half. Behind the `compile` cargo feature so the heavy Rolldown/libsui/zstd
//! deps don't burden the default build.
//!
//! EVERYTHING PLATFORM-DEPENDENT DISPATCHES ON THE TARGET, NEVER THE HOST — the
//! container format, the Node dist build, the launcher template, the strip tool,
//! and whether anything is signed at all. The host decides only what it is
//! physically able to do: execute the produced artifact (the `__probe` smoke
//! check) and shell out to `codesign`. A cross-compile therefore differs from a
//! native one in exactly two places, and both degrade to a weaker check rather
//! than an error — see [`verify_artifact`] and [`prepare_node_bytes`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::{
    ContainerFormat, Manifest, SUPPORTED_TRIPLES, Shape, TargetArch, TargetOs, TargetPlatform,
    encode,
};
use nub_core::node::discovery;
use nub_core::node::version::{NodeVersion, VersionPin};
use nub_core::version_management::{self, NodeArch, NodeOs};
use sha2::{Digest, Sha256};

mod assets;
pub mod bundle;
mod external;
mod inject;

pub use bundle::{BundleOptions, SourcemapMode};

/// Shown while a first run unpacks the embedded Node (or provisions one under
/// `--smol`). Deliberately generic: the launcher has no app name of its own, and
/// naming the runtime would leak an implementation detail into the app's UI.
pub const DEFAULT_INSTALL_MESSAGE: &str = "Initializing...";

pub struct CompileOptions {
    pub entry: String,
    pub out: Option<String>,
    pub smol: bool,
    /// Explicit `--target`; `None` → infer from the project's pin chain.
    pub target: Option<String>,
    pub platform: Option<String>,
    /// `--include`: paths embedded verbatim, never bundled or transformed.
    pub include: Vec<String>,
    /// `--exclude`: paths pruned from what `--include` selected.
    pub exclude: Vec<String>,
    /// Custom first-run line; `None` takes [`DEFAULT_INSTALL_MESSAGE`]. The flag
    /// only customizes the text — there is no spelling that suppresses it, since
    /// the alternative is a multi-second silent hang while Node is unpacked.
    pub install_message: Option<String>,
    /// The bundler-flag surface, shared verbatim with `nub build`.
    pub bundle: BundleOptions,
}

/// The app files to embed: `(name, bytes)` per file — entry + chunks, any
/// shipped source map, every `--include`d asset, and the synthesized
/// `package.json`. Names are `/`-separated and relative to the extracted app dir.
type AppFiles = Vec<(String, Vec<u8>)>;

pub fn run(mut opts: CompileOptions) -> Result<i32> {
    let target = resolve_platform(opts.platform.as_deref())?;
    // Resolved BEFORE any work: a cross-compile whose launcher template is missing
    // must fail in the first second, not after downloading and recompressing a
    // ~100 MB Node for the target.
    let template_path = locate_launcher_template(&target)?;

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
        .unwrap_or_else(|| PathBuf::from(format!("{stem}{}", target.exe_suffix())));

    // 1. Resolve `--include`/`--exclude` BEFORE bundling: a typo'd include is a
    //    sub-second failure, and paying for a full bundle first would hide that
    //    behind the slowest step in the pipeline.
    let entry_dir = entry_abs.parent().unwrap_or(Path::new("."));
    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let layout = assets::plan(entry_dir, &cwd, &opts.include, &opts.exclude)?;

    // 2. Bundle (Rolldown, in-process). The target's platform/arch are baked in
    //    as defines UNDER the user's, so a cross-compiled `process.platform`
    //    branch dead-code-eliminates for the machine the artifact will run on,
    //    not the one it was built on.
    opts.bundle.auto_define = target_defines(&target);
    opts.bundle
        .auto_define
        .extend(external::entry_defines(&opts.bundle.external));
    eprintln!("Bundling {} …", opts.entry);
    let bundled = bundle::bundle(&entry_abs, &opts.bundle)?;
    let mut entry_name = layout.bundle_path(&bundled.entry);
    let mut app_files = assemble_app(&bundled, &layout, &target)?;
    if !opts.bundle.external.is_empty() {
        let shim = external::shim(&app_files, &entry_name, &opts.bundle.external)?;
        entry_name = shim.entry;
        app_files.extend(shim.files);
        eprintln!(
            "External (must be installed where the binary runs): {}",
            opts.bundle.external.join(", ")
        );
    }
    let app_sha = sha256_of_app(&app_files);
    if !layout.assets.is_empty() {
        eprintln!("Embedding {} file(s) …", layout.assets.len());
    }

    // 3. Resolve the Node version through nub run's SAME pin chain (so compile
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

    let (node_version, node_blob, node_sha) = if opts.smol {
        // Smol bakes the acceptance FLOOR the launcher enforces (`discovered >=
        // floor`) — and ONLY that. A range's upper bound is deliberately not
        // carried into the artifact, so the raw spec is echoed here for the
        // compiling user and goes no further.
        let floor = version_management::pin_floor(&pin, &cache_root)?;
        external::check_node_support(&floor, &source, &opts.bundle.external)?;
        eprintln!(
            "Using Node.js {} (resolved from {source}; satisfied at runtime)",
            non_exact_spec(&pin, &raw).unwrap_or_else(|| floor.to_string())
        );
        (floor, Vec::new(), String::new())
    } else {
        // Embed bakes ONE exact version — a range/major/alias collapses to the
        // newest satisfying release at compile time. (`build_node_blob` →
        // provisioning prints the `Using Node.js … (resolved from …)` line, the
        // same surface nub run uses.)
        let (os, arch, musl) = dist_platform(&target);
        let exact =
            version_management::resolve_pin_for_platform(&pin, os, arch, musl, &cache_root)?;
        external::check_node_support(&exact, &source, &opts.bundle.external)?;
        let (blob, sha) = build_node_blob(&exact, &target, &cache_root, &source)?;
        (exact, blob, sha)
    };

    // 4. Manifest + payload.
    let manifest = Manifest {
        shape,
        entry: entry_name,
        node_version: node_version.to_string(),
        triple: target.triple(),
        node_sha256: node_sha,
        app_sha256: app_sha,
        minify: opts.bundle.minify,
        install_message: Some(install_message(&opts)),
    };
    let payload = encode(&manifest, &app_files, &node_blob);

    // 5. Inject the payload into the target's launcher template.
    let template = fs::read(&template_path)
        .with_context(|| format!("reading launcher template {}", template_path.display()))?;
    inject::inject(&target, &template, &payload, &out_path)
        .with_context(|| format!("writing {}", out_path.display()))?;
    set_executable(&out_path)?;
    verify_artifact(&out_path, &target)?;
    write_detached_maps(&bundled, &out_path)?;

    let size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "Compiled {} — {} shape, Node {}, {}, {:.1} MB",
        out_path.display(),
        if opts.smol { "smol" } else { "embed" },
        node_version,
        target.triple(),
        size as f64 / 1_000_000.0
    );
    Ok(0)
}

// ---- target platform ----------------------------------------------------------

/// Resolve `--platform` into the target, defaulting to the host.
fn resolve_platform(platform: Option<&str>) -> Result<TargetPlatform> {
    match platform {
        Some(token) => TargetPlatform::parse(token).ok_or_else(|| {
            anyhow!(
                "unknown --platform {token:?}. Supported: {}",
                SUPPORTED_TRIPLES.join(", ")
            )
        }),
        None => TargetPlatform::host().context(
            "this host is not one of nub's compile targets — pass --platform <triple> explicitly",
        ),
    }
}

/// The target in the Node dist vocabulary, for provisioning the embedded Node.
/// `TargetPlatform` deliberately admits only the platforms nub publishes a
/// launcher for, which is a subset of what nodejs.org publishes — so this
/// conversion is total, and stays total as long as that containment holds.
fn dist_platform(target: &TargetPlatform) -> (NodeOs, NodeArch, bool) {
    let os = match target.os {
        TargetOs::Darwin => NodeOs::Darwin,
        TargetOs::Linux => NodeOs::Linux,
        TargetOs::Win32 => NodeOs::Windows,
    };
    let arch = match target.arch {
        TargetArch::X64 => NodeArch::X64,
        TargetArch::Arm64 => NodeArch::Arm64,
    };
    (os, arch, target.musl)
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

/// The first-run text to bake into the artifact. Always Some at the manifest:
/// `None` there means "print nothing", and a first run unpacks ~100 MB of Node,
/// so an omitted flag takes the default rather than leaving the user staring at
/// a silent terminal.
fn install_message(opts: &CompileOptions) -> String {
    opts.install_message
        .clone()
        .unwrap_or_else(|| DEFAULT_INSTALL_MESSAGE.to_string())
}

/// The raw requirement to ECHO for `--smol` — `None` for a bare exact version
/// (the floor already captures it), else the original spec string.
fn non_exact_spec(pin: &VersionPin, raw: &str) -> Option<String> {
    match pin {
        VersionPin::Exact(_) => None,
        _ => Some(raw.to_string()),
    }
}

// ---- bundling -----------------------------------------------------------------

/// The constants baked in for the TARGET, not the build host. Written as JSON so
/// they land in the bundle as string literals — Rolldown/esbuild `define` values
/// are JS expressions, so a bare `darwin` would define an identifier.
///
/// `NODE_ENV` is a literal `"production"`, never the compiling machine's value:
/// nothing about the build environment is allowed to leak into the artifact.
fn target_defines(target: &TargetPlatform) -> Vec<(String, String)> {
    let os = match target.os {
        TargetOs::Darwin => "darwin",
        TargetOs::Linux => "linux",
        TargetOs::Win32 => "win32",
    };
    let arch = match target.arch {
        TargetArch::X64 => "x64",
        TargetArch::Arm64 => "arm64",
    };
    vec![
        ("process.platform".into(), format!("\"{os}\"")),
        ("process.arch".into(), format!("\"{arch}\"")),
        ("process.env.NODE_ENV".into(), "\"production\"".into()),
    ]
}

/// Bundle output + embedded assets, in the payload's write order.
///
/// The synthesized `package.json` sits BESIDE the entry rather than at the app
/// root: Node resolves a module's type from the nearest package.json above it,
/// so the entry's own directory is the one position no `--include`d file can
/// shadow. With no assets the entry is already at the root, so this is the same
/// file in the same place it has always been.
fn assemble_app(
    bundled: &bundle::BundleResult,
    layout: &assets::Layout,
    target: &TargetPlatform,
) -> Result<AppFiles> {
    let mut files: AppFiles = bundled
        .files
        .iter()
        .map(|f| (layout.bundle_path(&f.name), f.bytes.clone()))
        .collect();

    for asset in &layout.assets {
        // Overwriting a chunk would replace compiled code with whatever the user
        // pointed at — always a mistake, and silent until the binary runs.
        if files.iter().any(|(name, _)| *name == asset.rel) {
            bail!(
                "--include would overwrite compiled output: {} is also a bundle chunk. \
                 Rename the file or drop it from --include.",
                asset.rel
            );
        }
        let bytes = fs::read(&asset.source)
            .with_context(|| format!("reading {}", asset.source.display()))?;
        files.push((asset.rel.clone(), bytes));
    }

    let pkg = layout.bundle_path("package.json");
    match files.iter().find(|(name, _)| *name == pkg) {
        // An embedded package.json ships verbatim — that is what --include
        // promises — so it, not nub, governs the entry's module type. The bundle
        // is ESM, so a manifest that says otherwise is a compile-time error
        // rather than a "Cannot use import statement outside a module" on a
        // user's machine.
        Some((_, bytes)) => {
            let parsed: serde_json::Value = serde_json::from_slice(bytes)
                .with_context(|| format!("parsing the embedded {pkg}"))?;
            if parsed.get("type").and_then(|t| t.as_str()) != Some("module") {
                bail!(
                    "the embedded {pkg} must declare \"type\": \"module\" — it sits beside the \
                     compiled entry, which is an ES module. Add the field, or drop the file \
                     from --include."
                );
            }
        }
        None => files.push((pkg, br#"{"type":"module"}"#.to_vec())),
    }

    // The launcher refuses any payload name that could escape its extraction dir.
    // Names are partly user-derived since `--include`, so check the SAME predicate
    // on the WHOLE set here — rather than shipping an executable that aborts on
    // someone else's machine. Checked against the TARGET's rules, never the host's:
    // `a\..\..\x` is one ordinary filename on Linux and a traversal on Windows, so
    // a host-parsed gate lets a cross-compile bake a name its own launcher refuses.
    let rules = target.name_rules();
    for (name, _) in &files {
        if !nub_core::compile::is_safe_relative_name_for(rules, name) {
            bail!(
                "this path cannot be embedded for {}: {name:?}. An --include'd path must \
                 sit inside the tree that holds the entry, and its name must be a plain \
                 relative path that is also legal on the target — on Windows that rules \
                 out `\\`, `<>:\"|?*`, a trailing dot or space, and the reserved device \
                 names (CON, PRN, AUX, NUL, COM1-9, LPT1-9).",
                target.triple()
            );
        }
    }
    Ok(files)
}

/// `--sourcemap=external` maps land BESIDE the executable, deliberately outside
/// it: the point of the mode is to keep source text out of what you ship while
/// still having a map to hand an error tracker.
fn write_detached_maps(bundled: &bundle::BundleResult, out_path: &Path) -> Result<()> {
    let dir = out_path.parent().unwrap_or_else(|| Path::new("."));
    for map in &bundled.detached_maps {
        let path = dir.join(&map.name);
        fs::write(&path, &map.bytes)
            .with_context(|| format!("writing source map {}", path.display()))?;
        eprintln!("Wrote {}", path.display());
    }
    Ok(())
}

// ---- Node blob (default/embed shape) ------------------------------------------

/// Provision the official Node for `target`, strip it per the target's policy,
/// and zstd-19 compress. Returns the compressed blob and the hash of the
/// DECOMPRESSED (runnable) bytes.
fn build_node_blob(
    version: &NodeVersion,
    target: &TargetPlatform,
    cache_root: &Path,
    resolved_from: &str,
) -> Result<(Vec<u8>, String)> {
    let (os, arch, musl) = dist_platform(target);
    // Provisioning prints the `Using Node.js <v> (resolved from <source>)` line +
    // downloads (verified against SHASUMS256.txt before it commits).
    let dir = version_management::provision_node_for_platform(
        version,
        os,
        arch,
        musl,
        &node_store_root(cache_root, target),
        Some(resolved_from),
    )?;
    let node_bin = node_binary_in(&dir, target);
    if !node_bin.is_file() {
        bail!(
            "provisioned Node {version} for {} but its binary is missing at {}",
            target.triple(),
            node_bin.display()
        );
    }

    let bytes = prepare_node_bytes(&node_bin, target)?;
    let sha = sha256_hex(&bytes);
    eprintln!(
        "Compressing Node ({:.0} MB) with zstd-19 …",
        bytes.len() as f64 / 1_000_000.0
    );
    let blob = zstd::encode_all(&bytes[..], 19).context("zstd-19 compressing Node")?;
    Ok((blob, sha))
}

/// The store root a target's Node is provisioned into. A NON-host Node must not
/// land in the host's store: that store is keyed by version alone, so `nub run`
/// (and this pipeline's own host path) would treat a foreign binary as runnable
/// here. Scope it by triple instead.
fn node_store_root(cache_root: &Path, target: &TargetPlatform) -> PathBuf {
    if target.is_host() {
        cache_root.to_path_buf()
    } else {
        cache_root.join("compile-dist").join(target.triple())
    }
}

/// Where the `node` executable sits inside a provisioned version dir: the Windows
/// zip puts `node.exe` at the root, the tarballs put `bin/node`.
fn node_binary_in(version_dir: &Path, target: &TargetPlatform) -> PathBuf {
    match target.os {
        TargetOs::Win32 => version_dir.join("node.exe"),
        _ => version_dir.join("bin").join("node"),
    }
}

/// Produce the runnable Node bytes to embed, applying the target's strip+sign
/// policy. Falls back to the untouched original on any failure — an unstripped
/// Node costs ~4 MB post-zstd, a broken one costs the user a binary that cannot
/// start.
///
/// The policy, and why it is per-TARGET:
/// - **macOS** — stripping invalidates the Mach-O signature, and arm64 refuses to
///   execute an unsigned image, so a strip is only safe if we can re-sign. That
///   needs `codesign`, which exists only on a macOS host. Cross-compiling to
///   macOS therefore embeds Node unstripped rather than shipping something that
///   cannot launch. (A pure-Rust re-signer would lift this; libsui signs the
///   ARTIFACT that way already, but not an arbitrary inner binary.)
/// - **Linux / Windows** — nothing is signed, so a strip can never invalidate
///   anything. Only `llvm-strip` is used for a foreign format: GNU `strip` and
///   Apple's `strip` each handle only their own platform's format and would fail
///   (or, worse, mangle) the other's.
/// - **Verification by execution** happens only when target == host; a foreign
///   binary cannot be run, so the check degrades to "is it still a well-formed
///   image of the expected format".
fn prepare_node_bytes(node_bin: &Path, target: &TargetPlatform) -> Result<Vec<u8>> {
    let original = fs::read(node_bin).with_context(|| format!("reading {}", node_bin.display()))?;
    let format = target.format();

    let needs_resign = format == ContainerFormat::MachO;
    if needs_resign && which_first(&["codesign"]).is_none() {
        eprintln!(
            "note: no codesign on PATH — a stripped macOS Node could not be re-signed, \
             so it is embedded unstripped"
        );
        return Ok(original);
    }

    // A native-format target may use whichever stripper is around; a foreign
    // format needs the multi-format llvm-strip.
    let native = TargetPlatform::host().is_some_and(|h| h.format() == format);
    let candidates: &[&str] = if native {
        &["llvm-strip", "strip"]
    } else {
        &["llvm-strip"]
    };
    let Some(strip) = which_first(candidates) else {
        eprintln!(
            "note: no {} on PATH — embedding the Node binary unstripped",
            candidates.join("/")
        );
        return Ok(original);
    };

    let tmp = std::env::temp_dir().join(format!("nub-compile-node-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, &original).with_context(|| format!("staging Node at {}", tmp.display()))?;
    // fs::write lands 0644; the post-strip `--version` verification must be able to
    // EXEC the staged binary, so restore the executable bit before strip/verify.
    set_executable(&tmp)?;
    let _guard = FileGuard(tmp.clone());

    let mut ok = run_ok(&strip, &[tmp.as_os_str()]);
    if ok && needs_resign {
        ok = run_ok(
            "codesign",
            &[
                "--force".as_ref(),
                "-s".as_ref(),
                "-".as_ref(),
                tmp.as_os_str(),
            ],
        );
    }
    if ok {
        ok = if target.is_host() {
            node_runs(&tmp)
        } else {
            // Can't execute a foreign binary — settle for "still the right kind
            // of image, and not obviously truncated".
            fs::read(&tmp)
                .is_ok_and(|b| b.len() > 1_000_000 && inject::detect_format(&b) == Some(format))
        };
    }
    if ok {
        let how = if needs_resign {
            "Stripped + ad-hoc re-signed the embedded Node"
        } else {
            "Stripped the embedded Node"
        };
        eprintln!("{how}");
        return fs::read(&tmp).with_context(|| format!("reading stripped {}", tmp.display()));
    }

    eprintln!("note: strip failed verification — embedding Node unstripped");
    Ok(original)
}

/// Does this Node binary still execute? Asks it for `--version` with the ambient
/// Node configuration REMOVED.
///
/// The env scrub is load-bearing, not hygiene. A developer machine routinely
/// carries a `NODE_OPTIONS` aimed at a different Node than the one being embedded
/// — nub's own dev shell exports one — and Node rejects the whole invocation when
/// a flag in it is unknown to that binary. Inheriting it made a perfectly good
/// stripped Node look broken, and the fallback then silently shipped the
/// unstripped one (~27 MB heavier) with only a note. The check must test the
/// BINARY, not the environment it happens to run in.
fn node_runs(node: &Path) -> bool {
    std::process::Command::new(node)
        .arg("--version")
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_REPL_EXTERNAL_MODULE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---- launcher template --------------------------------------------------------

/// Find the `nub-launcher` template built FOR `target`. A launcher is
/// target-specific twice over — it is that platform's executable format, and it
/// carries the container-format reader and nub-core runtime logic the payload
/// depends on — so a foreign target needs that triple's own prebuilt template and
/// there is nothing to fall back to.
///
/// Lookup order: the `NUB_LAUNCHER_TEMPLATE` override, then a `nub-launcher-<triple>`
/// sibling of the running `nub`, then a plain `nub-launcher` sibling for the host
/// target.
///
/// SIBLING-OF-`nub` IS THE DISTRIBUTION CONTRACT. Every release channel puts the
/// host's template in the same directory as the binary it shipped with — the npm
/// platform package's `bin/`, the release archive's `bin/` (so `~/.nub/bin` and the
/// Windows install dir), and the Homebrew keg's `bin`. `--platform` for a FOREIGN
/// triple still needs `NUB_LAUNCHER_TEMPLATE`: a package carries one platform's
/// launcher, not all eight.
///
/// The exe path is canonicalized first so a channel that exposes `nub` through a
/// symlink (winget's portable command alias) still anchors the sibling lookup to
/// the real install dir. `current_exe` resolves symlinks on Linux
/// (`/proc/self/exe`) but NOT on macOS, where it returns the path used to exec.
fn locate_launcher_template(target: &TargetPlatform) -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| fs::canonicalize(&p).unwrap_or(p));
    locate_launcher_template_in(
        target,
        std::env::var_os("NUB_LAUNCHER_TEMPLATE").map(PathBuf::from),
        exe.as_deref().and_then(Path::parent),
    )
}

/// [`locate_launcher_template`] with the environment made explicit — the seam the
/// tests drive, so they never mutate process-global env or depend on where the
/// test binary happens to live.
fn locate_launcher_template_in(
    target: &TargetPlatform,
    override_path: Option<PathBuf>,
    nub_dir: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(p) = override_path {
        if p.is_file() {
            return Ok(p);
        }
        bail!(
            "NUB_LAUNCHER_TEMPLATE points at a missing file: {}",
            p.display()
        );
    }

    // Both the suffixed and bare spellings, because a Windows template may be
    // published either way; on a non-Windows target they coincide, so dedupe.
    let triple = target.triple();
    let suffix = target.exe_suffix();
    let mut names = vec![format!("nub-launcher-{triple}{suffix}")];
    if !suffix.is_empty() {
        names.push(format!("nub-launcher-{triple}"));
    }
    if target.is_host() {
        names.push(format!("nub-launcher{suffix}"));
        if !suffix.is_empty() {
            names.push("nub-launcher".to_string());
        }
    }

    if let Some(dir) = nub_dir {
        for name in &names {
            let cand = dir.join(name);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }

    bail!(
        "no nub-launcher template for --platform {triple}.\n\
         \x20\x20Expected one of {} next to the nub binary, or NUB_LAUNCHER_TEMPLATE\n\
         \x20\x20pointing at a launcher built for {triple}.",
        names.join(", ")
    )
}

/// Check the artifact before handing it to the user. Two layers, the second
/// available only natively:
///
/// 1. **Static scan (always).** Locate the payload in the produced file the way
///    the target's loader will, and decode it. Catches a malformed injection on
///    every target, including the cross ones that cannot be run here.
/// 2. **`__probe` self-check (target == host only).** Executes the artifact so it
///    reads its own section and touches a heap allocation — the exact path an
///    under-padded Mach-O injection corrupts into a SIGILL trap, which no static
///    check can see. Cross-compiling SKIPS this, loudly: an artifact that passes
///    the scan but was never executed is a weaker guarantee, and the user should
///    know which one they got.
fn verify_artifact(bin: &Path, target: &TargetPlatform) -> Result<()> {
    let bytes = fs::read(bin).with_context(|| format!("reading {}", bin.display()))?;
    let payload = inject::find_payload(target.format(), &bytes)
        .with_context(|| format!("scanning {} for its payload", bin.display()))?
        .context("the produced executable carries no payload — the injection did not take")?;
    nub_core::compile::decode(payload)
        .context("the produced executable's payload does not decode")?;

    if !target.is_host() {
        eprintln!(
            "note: cross-compiled for {} — payload verified statically; the run-it \
             self-check needs a {} host",
            target.triple(),
            target.triple()
        );
        return Ok(());
    }

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

    fn opts(install_message: Option<&str>) -> CompileOptions {
        CompileOptions {
            entry: "main.ts".into(),
            out: None,
            smol: false,
            target: None,
            platform: None,
            include: Vec::new(),
            exclude: Vec::new(),
            install_message: install_message.map(str::to_string),
            bundle: BundleOptions {
                minify: true,
                keep_names: true,
                sourcemap: SourcemapMode::Inline,
                sources_content: true,
                define: Vec::new(),
                auto_define: Vec::new(),
                tree_shake: true,
                ignore_annotations: false,
                alias: Vec::new(),
                conditions: Vec::new(),
                external: Vec::new(),
                tsconfig: None,
            },
        }
    }

    #[test]
    fn auto_defines_describe_the_target_not_the_build_host() {
        let win = TargetPlatform::parse("win32-x64").expect("known triple");
        let defs = target_defines(&win);
        assert_eq!(
            defs,
            vec![
                ("process.platform".to_string(), "\"win32\"".to_string()),
                ("process.arch".to_string(), "\"x64\"".to_string()),
                (
                    "process.env.NODE_ENV".to_string(),
                    "\"production\"".to_string()
                ),
            ],
            "cross-compiled platform checks must fold against the TARGET, and the \
             values must be quoted so they land as string literals, not identifiers"
        );
    }

    // The launcher treats `None` in the MANIFEST as "print nothing", and a first
    // run unpacks ~100 MB of Node — so omitting the flag must reach the manifest
    // as the default line, never as `None`. The flag customizes the text; it
    // cannot silence it.
    #[test]
    fn install_message_defaults_when_omitted_and_is_overridable() {
        assert_eq!(install_message(&opts(None)), "Initializing...");
        assert_eq!(install_message(&opts(Some("Warming up"))), "Warming up");
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

    #[test]
    fn platform_defaults_to_the_host_and_rejects_unknown_triples() {
        let host = TargetPlatform::host().unwrap();
        assert_eq!(resolve_platform(None).unwrap(), host);
        assert_eq!(
            resolve_platform(Some("linux-x64-musl")).unwrap().triple(),
            "linux-x64-musl"
        );
        let err = resolve_platform(Some("linux-riscv")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("linux-x64"),
            "should list what IS supported: {msg}"
        );
    }

    /// A cross target must never be provisioned into the host's Node store — the
    /// store is keyed by version alone, so a foreign binary there would be picked
    /// up as runnable. Asserted on the path, since exercising it needs a download.
    #[test]
    fn a_foreign_node_is_stored_under_a_triple_scoped_root() {
        let cache = Path::new("/tmp/nub-cache-probe");
        let host = TargetPlatform::host().unwrap();
        let foreign = SUPPORTED_TRIPLES
            .iter()
            .map(|t| TargetPlatform::parse(t).unwrap())
            .find(|t| *t != host)
            .unwrap();
        assert_eq!(node_store_root(cache, &host), cache.to_path_buf());
        assert_eq!(
            node_store_root(cache, &foreign),
            cache.join("compile-dist").join(foreign.triple())
        );
    }

    #[test]
    fn the_node_binary_sits_where_each_dist_archive_puts_it() {
        let dir = Path::new("/store/24.10.0");
        let at = |t: &str| node_binary_in(dir, &TargetPlatform::parse(t).unwrap());
        assert_eq!(at("win32-x64"), dir.join("node.exe"));
        assert_eq!(at("linux-x64"), dir.join("bin").join("node"));
        assert_eq!(at("darwin-arm64"), dir.join("bin").join("node"));
    }

    /// A release ships only the HOST's template, so this error is still the whole
    /// cross-compile UX — it must name the triple and the exact filenames.
    #[test]
    fn a_missing_foreign_template_names_what_is_missing() {
        let dir = fresh_dir("no-template");
        let target = TargetPlatform::parse("win32-arm64").unwrap();
        let err = locate_launcher_template_in(&target, None, Some(&dir)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("win32-arm64"), "should name the triple: {msg}");
        assert!(
            msg.contains("nub-launcher-win32-arm64.exe"),
            "should name the file it looked for: {msg}"
        );
        assert!(
            msg.contains("NUB_LAUNCHER_TEMPLATE"),
            "should name the override: {msg}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The name gate must read the TARGET's path rules, not the build host's.
    /// `a\..\..\x` is one ordinary filename on Unix, so a host-parsed gate lets a
    /// Unix→win32 cross-compile bake an escaping name — which only surfaces as an
    /// abort on the Windows user's machine. (The predicate itself is covered in
    /// nub-core; this pins that the target actually reaches it.)
    #[test]
    fn the_payload_name_gate_dispatches_on_the_target_not_the_host() {
        let dir = fresh_dir("winsafe");
        let source = dir.join("asset.bin");
        fs::write(&source, b"x").unwrap();

        let bundled = bundle::BundleResult {
            entry: "main.js".into(),
            files: vec![bundle::BundledFile {
                name: "main.js".into(),
                bytes: b"export {}".to_vec(),
            }],
            detached_maps: Vec::new(),
        };
        let layout = assets::Layout {
            entry_prefix: String::new(),
            assets: vec![assets::Asset {
                source,
                rel: "a\\..\\..\\escaped".into(),
            }],
        };

        let win = TargetPlatform::parse("win32-x64").unwrap();
        let err = assemble_app(&bundled, &layout, &win).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cannot be embedded"), "{msg}");
        assert!(msg.contains("win32-x64"), "should name the target: {msg}");

        // The same name on a Unix target is a legal single-component filename.
        let linux = TargetPlatform::parse("linux-x64").unwrap();
        assert!(assemble_app(&bundled, &layout, &linux).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Pins the Rust half of the release↔lookup contract: the name
    /// `.github/workflows/release.yml` must copy the template to. Renaming it here
    /// fails this test; renaming it in the workflow does not, so the two have to be
    /// changed together.
    #[test]
    fn the_release_shipped_filename_is_what_the_host_lookup_accepts() {
        let dir = fresh_dir("shipped");
        let host = TargetPlatform::host().unwrap();
        let shipped = dir.join(format!(
            "nub-launcher-{}{}",
            host.triple(),
            host.exe_suffix()
        ));
        fs::write(&shipped, b"template").unwrap();
        assert_eq!(
            locate_launcher_template_in(&host, None, Some(&dir)).unwrap(),
            shipped
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A foreign target resolves its OWN triple's template. The unsuffixed
    /// `nub-launcher` sibling is the HOST's — substituting it for a foreign target
    /// would inject into the wrong format, so only the host may fall back to it.
    #[test]
    fn only_the_host_falls_back_to_the_unsuffixed_template() {
        let dir = fresh_dir("templates");
        fs::write(dir.join("nub-launcher"), b"host").unwrap();
        let foreign = SUPPORTED_TRIPLES
            .iter()
            .map(|t| TargetPlatform::parse(t).unwrap())
            .find(|t| !t.is_host())
            .unwrap();

        assert_eq!(
            locate_launcher_template_in(&TargetPlatform::host().unwrap(), None, Some(&dir))
                .unwrap(),
            dir.join("nub-launcher")
        );
        assert!(
            locate_launcher_template_in(&foreign, None, Some(&dir)).is_err(),
            "{} must not borrow the host's template",
            foreign.triple()
        );

        // Its own triple-suffixed template IS accepted.
        let own = dir.join(format!("nub-launcher-{}", foreign.triple()));
        fs::write(&own, b"foreign").unwrap();
        assert_eq!(
            locate_launcher_template_in(&foreign, None, Some(&dir)).unwrap(),
            own
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
