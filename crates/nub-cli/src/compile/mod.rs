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
//! physically able to do: execute the produced artifact (the probe-mode smoke
//! check) and shell out to `codesign`. A cross-compile therefore differs from a
//! native one in exactly two places, and both degrade to a weaker check rather
//! than an error — see [`verify_artifact`] and [`prepare_node_bytes`].

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::{
    AppFile, ContainerFormat, Manifest, SUPPORTED_TRIPLES, Shape, TargetArch, TargetOs,
    TargetPlatform, encode_with_license,
};
use nub_core::node::discovery;
use nub_core::node::version::{NodeVersion, VersionPin};
use nub_core::version_management::{self, NodeArch, NodeOs};
use sha2::{Digest, Sha256};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

mod assets;
pub mod bundle;
mod closure;
mod external;
mod inject;
mod launcher;
mod loaders;
mod metafile;
mod native;
mod native_layout;
mod unbundlable;
mod version_info;

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
    /// Custom first-run line; `None` takes [`DEFAULT_INSTALL_MESSAGE`]. An EMPTY
    /// string suppresses the notice — the launcher's `plan()` reads an empty
    /// message as silent — so a publisher who wants a quiet first run has a
    /// spelling. The default stays on because the alternative is a multi-second
    /// silent hang while Node is unpacked.
    pub install_message: Option<String>,
    /// `--define-file KEY=PATH`. Folded into [`BundleOptions::define`] before the
    /// bundle runs, so there is one substitution mechanism rather than two that
    /// could drift.
    pub define_file: Vec<String>,
    /// `--node-options`: NODE_OPTIONS-style strings the compiled binary starts its
    /// Node with, applied before whatever the end user sets at run time.
    pub node_options: Vec<String>,
    /// `--icon`: a Windows `.ico` to show on the executable. Windows-only because
    /// it is the only target whose format carries the icon in the file itself —
    /// macOS reads one from the surrounding `.app` bundle and Linux from a
    /// `.desktop` entry, and neither is part of a single-file artifact.
    pub icon: Option<PathBuf>,
    /// `--metadata KEY=VALUE`: Windows version-resource fields, overriding what
    /// the nearest `package.json` supplies. Windows-only for the same reason as
    /// [`CompileOptions::icon`] — no other container format carries them.
    pub metadata: Vec<String>,
    /// `--metafile`: where to write the build report. `None` collects nothing.
    pub metafile: Option<PathBuf>,
    /// The bundler-flag surface, shared verbatim with `nub build`.
    pub bundle: BundleOptions,
}

/// The app files to embed — entry + chunks, any shipped source map, native
/// package islands, and every `--include`d asset. Names are `/`-separated and
/// relative to the extracted app dir.
type AppFiles = Vec<AppFile<Vec<u8>>>;

pub fn run(mut opts: CompileOptions) -> Result<i32> {
    let target = resolve_platform(opts.platform.as_deref())?;

    // A typo'd entry costs one stat, so it is checked before anything that can
    // touch the network — a foreign target's launcher fetch would otherwise
    // download a template only to report the entry does not exist.
    let entry_path = Path::new(&opts.entry);
    if !entry_path.is_file() {
        bail!("entry file not found: {}", opts.entry);
    }

    // Read here for the same reason the entry is stat'd here: a mistyped path is
    // one failed open, not a launcher download away. Appended AFTER the argv
    // defines, so a key given both ways takes the file's value.
    let from_files = read_define_files(&opts.define_file)?;
    opts.bundle.define.extend(from_files);

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
        .unwrap_or_else(|| default_output_path(&stem, &target));
    reject_entry_output_alias(&entry_abs, &out_path)?;
    reject_missing_output_parent(&out_path)?;
    reject_directory_output(&out_path)?;
    // Read before the expensive work, so a bad path or a mislabelled file fails in
    // the first second rather than after a ~100 MB Node download.
    let icon = load_icon(opts.icon.as_deref(), &target)?;
    let version_info = load_version_info(
        &opts.metadata,
        entry_abs.parent().unwrap_or(Path::new(".")),
        &out_path,
        &target,
    )?;

    // Resolved AND verified before any real work: a cross-compile whose launcher
    // template is missing, or is not that platform's executable, must fail in the
    // first second — not after downloading and recompressing a ~100 MB Node for
    // the target. For a foreign target this may fetch the template from this
    // release, a few hundred KB, still the cheapest step to fail on.
    let template = launcher::locate(&target)?;

    // 1. Resolve `--include`/`--exclude` BEFORE bundling: a typo'd include is a
    //    sub-second failure, and paying for a full bundle first would hide that
    //    behind the slowest step in the pipeline.
    let entry_dir = entry_abs.parent().unwrap_or(Path::new("."));
    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let layout = assets::plan(entry_dir, &cwd, &opts.include, &opts.exclude)?;

    // The exact target Node must be known BEFORE bundling: it decides which
    // polyfills the preamble carries, and a static import cannot be tree-shaken
    // away after the fact. Resolved once here and reused by step 3.
    let shape = if opts.smol { Shape::Smol } else { Shape::Embed };
    let cache_root = discovery::cache_dir()
        .context("no writable cache dir for compile-time Node provisioning")?;
    let pin_cwd = entry_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let (pin, raw, source) = determine_target(opts.target.as_deref(), &pin_cwd)?;
    let (os, arch, musl) = dist_platform(&target);
    // Smol gates on the FLOOR, the oldest runtime it will accept, so a polyfill is
    // dropped only when every Node the artifact can run on already has the global.
    let gate_version = if opts.smol {
        version_management::resolve_pin_floor_for_platform(&pin, os, arch, musl, &cache_root)?
    } else {
        version_management::resolve_pin_for_platform(&pin, os, arch, musl, &cache_root)?
    };
    opts.bundle.target_node = Some((
        gate_version.0.major,
        gate_version.0.minor,
        gate_version.0.patch,
    ));

    // 2. Bundle (Rolldown, in-process). The target's platform/arch are baked in
    //    as defines UNDER the user's, so a cross-compiled `process.platform`
    //    branch dead-code-eliminates for the machine the artifact will run on,
    //    not the one it was built on.
    opts.bundle.auto_define = target_defines(&target);
    // Native addons are embedded for the TARGET, and those same defines are what
    // make resolution pick the target's platform package rather than the host's.
    opts.bundle.native_target = Some(target);
    eprintln!("Bundling {} …", opts.entry);
    let bundled = bundle::bundle_for_compile(&entry_abs, &opts.bundle, &cwd)?;
    // Written as soon as the bundle exists, not at the end: everything after this
    // point can fail on the network or on the target's launcher, and a report of
    // what the bundler produced is exactly what someone diagnosing a size problem
    // wants to keep from a run that did not finish.
    if let (Some(path), Some(report)) = (&opts.metafile, &bundled.metafile) {
        write_metafile(path, report)?;
    }
    // The runtime resolve hook is decided AFTER the bundle: `--external` always
    // needs it, but `--allow-dynamic-import` only earns it if a computed
    // `import()` actually survived — the flag is cheap to pass defensively and a
    // build that never uses it should ship no wrapper.
    let shim_plan = external::ShimPlan {
        external: &opts.bundle.external,
        external_imports: &bundled.external_imports,
        dynamic: bundled.dynamic_import_sites > 0,
    };
    // Every static worker enters through a tiny generated module. When a resolver
    // hook is needed it imports that hook and only THEN dynamically imports the
    // prelude-bearing worker chunk; `module.registerHooks()` is not inherited by
    // workers registered programmatically in the main realm. Without a hook the
    // wrapper is still useful: it keeps the public worker URL stable while the
    // real bundle chunk has a private filename.
    let worker_wrappers = external::worker_wrappers(
        &bundled.worker_roots,
        shim_plan.needed(),
        &layout.entry_prefix,
    )?;
    let mut entry_name = layout.bundle_path(&bundled.entry);
    let mut app_files = assemble_app(&bundled, &layout, &worker_wrappers, &target)?;
    if shim_plan.needed() {
        // These land AFTER assemble_app's payload-name and collision gates. Safe
        // for the NAME gate because both are fixed constants legal on every
        // target; a shim name derived from user input would have to move it here.
        // Safe for the COLLISION gate because `external::shim` refuses a payload
        // entry matching either shim name case-insensitively, on every target —
        // it reserves the two names rather than relying on the later gate.
        let shim = external::shim(&app_files, &entry_name, &shim_plan)?;
        entry_name = shim.entry;
        app_files.extend(shim.files);
        if !opts.bundle.external.is_empty() {
            eprintln!(
                "External (must be installed where the binary runs): {}",
                opts.bundle.external.join(", ")
            );
        }
        if shim_plan.dynamic {
            eprintln!(
                "Dynamic import: {} site(s) resolved where the binary runs, not at build time",
                bundled.dynamic_import_sites
            );
        }
    }
    let app_sha = sha256_of_app(&app_files);
    if !layout.assets.is_empty() {
        eprintln!("Embedding {} file(s) …", layout.assets.len());
    }
    // Worth saying out loud: the binary now carries machine code for one platform,
    // and an addon that silently failed to resolve would otherwise look identical
    // to one that embedded correctly.
    if !bundled.native_addons.is_empty() {
        eprintln!("Native addons: {}", bundled.native_addons.join(", "));
    }

    // 3. Resolve the Node version through nub run's SAME pin chain (so compile
    //    can't drift from run); --target overrides it. The pin context is the
    //    entry's project dir (walk up from there).
    let (node_version, provision_version, node) = if opts.smol {
        // Smol bakes the oldest acceptable runtime as its bundling floor. The
        // manifest also retains an explicit range so discovery can enforce both
        // ends; other non-exact targets retain floor behavior.
        let floor = gate_version;
        external::check_node_support(&floor, &source, &shim_plan)?;
        // What to DOWNLOAD when discovery finds nothing. Provisioning the floor
        // would fetch the oldest acceptable release — `--target 26` giving
        // 26.0.0 — so resolve the newest satisfying one here, where the dist
        // index is already reachable, and leave acceptance on the floor alone.
        // A resolution failure is not fatal: the launcher still has the floor.
        let (os, arch, musl) = dist_platform(&target);
        let newest =
            version_management::resolve_pin_for_platform(&pin, os, arch, musl, &cache_root)
                .ok()
                .filter(|newest| {
                    provision_preference_is_usable(newest, &floor, shim_plan.needed())
                });
        eprintln!(
            "Using Node.js {} (resolved from {source}; {}{})",
            non_exact_spec(&pin, &raw).unwrap_or_else(|| floor.to_string()),
            smol_runtime_policy(&pin, &floor),
            newest
                .as_ref()
                .map(|n| format!(", provisioning {n}"))
                .unwrap_or_default()
        );
        (floor, newest, EmbeddedNode::default())
    } else {
        // Embed bakes ONE exact version — a range/major/alias collapses to the
        // newest satisfying release at compile time. (`build_node_blob` →
        // provisioning prints the `Using Node.js … (resolved from …)` line, the
        // same surface nub run uses.)
        let (os, arch, musl) = dist_platform(&target);
        let exact =
            version_management::resolve_pin_for_platform(&pin, os, arch, musl, &cache_root)?;
        external::check_node_support(&exact, &source, &shim_plan)?;
        let node = build_node_blob(&exact, &target, &cache_root, &source)?;
        (exact, None, node)
    };

    // Compress AFTER `sha256_of_app` above: that hash is the extraction cache key
    // and must stay over the semantic content, not over whatever this zstd version
    // happens to emit. Per file rather than per region because `nub_core::compile`
    // is a pure container that does no compression of its own — the same split the
    // Node blob already uses, and it lets the launcher decode only the files it
    // actually extracts. Level 19 matches the Node blob; the app region is small
    // enough that the time is not noticeable next to the ~107 MB one.
    let app_files: Vec<_> = app_files
        .into_iter()
        .map(|file| {
            let bytes = zstd::encode_all(&file.bytes[..], 19)
                .with_context(|| format!("zstd-compressing {}", file.name))?;
            Ok::<_, anyhow::Error>(nub_core::compile::AppFile {
                name: file.name,
                bytes,
                executable: file.executable,
            })
        })
        .collect::<Result<_>>()?;

    // 4. Manifest + payload.
    let manifest = Manifest {
        shape,
        entry: entry_name,
        node_version: node_version.to_string(),
        provision_version: provision_version.map(|v| v.to_string()).unwrap_or_default(),
        smol_exact_target: opts.smol && smol_requires_exact_target(&pin),
        // `node_version` IS the gate the bundle was stripped against, so it is
        // the only correct thing to judge the range against here.
        smol_version_range: if opts.smol {
            smol_version_range(&pin, &node_version)
        } else {
            String::new()
        },
        // Only smol discovers its runtime, so only smol can be handed one that
        // cannot run the shim; embed carries an exact Node already gated above.
        requires_augmentation: opts.smol && shim_plan.needed(),
        triple: target.triple(),
        node_sha256: node.sha256,
        node_blake3: node.blake3,
        node_size: node.size,
        app_compressed: true,
        app_sha256: app_sha,
        minify: opts.bundle.minify,
        install_message: Some(install_message(&opts)),
        node_flags: node_flags(&opts)?,
    };
    let payload = encode_with_license(&manifest, &app_files, &node.blob, &node.license);

    // 5. Build and verify a staged artifact before replacing the requested
    // destination. A late signing/permission/static/native-probe failure must
    // never truncate a previously good executable at `--out`.
    let staged_maps = stage_detached_maps(&bundled, &out_path)?;
    let staged = StagedArtifact::new(&out_path, "artifact")?;
    inject::inject(
        &target,
        &template,
        &payload,
        icon.as_deref(),
        version_info.as_deref(),
        staged.path(),
    )
    .with_context(|| format!("writing {}", staged.path().display()))?;
    set_executable(staged.path())?;
    sync_file(staged.path())?;
    verify_artifact(staged.path(), &target, version_info.as_deref())?;
    staged.publish(&out_path)?;

    // Detached maps are optional debugging companions rather than part of the
    // executable's atomic replacement. They were staged before the binary became
    // visible, then each is atomically published afterwards. A map publish failure
    // warns (the verified executable remains usable) and removes its temp file.
    publish_detached_maps(staged_maps);

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

/// Write the `--metafile` report, pretty-printed because it is read by people at
/// least as often as by a tool.
fn write_metafile(path: &Path, report: &metafile::Metafile) -> Result<()> {
    let json = serde_json::to_string_pretty(report).context("serializing the build report")?;
    fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("Wrote {}", path.display());
    Ok(())
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

fn default_output_path(stem: &str, target: &TargetPlatform) -> PathBuf {
    PathBuf::from(format!("{stem}{}", target.exe_suffix()))
}

/// Refuse a `--out` whose parent directory does not exist, here rather than at
/// the write. Reaching the artifact write means bundling, stripping + re-signing
/// Node, and zstd-19 compressing ~100 MB have already been paid for — and the
/// error that surfaced there named an internal staging directory
/// (`.nub-compile-artifact-<pid>-<n>`) instead of the output path the user chose.
fn reject_missing_output_parent(out: &Path) -> Result<()> {
    let parent = match out.parent() {
        // A bare filename is written into the current directory, which exists.
        Some(p) if p.as_os_str().is_empty() => return Ok(()),
        Some(p) => p,
        None => return Ok(()),
    };
    if parent.is_dir() {
        return Ok(());
    }
    if parent.exists() {
        bail!(
            "the --out parent {} is not a directory. Pass --out under an existing directory.",
            parent.display()
        );
    }
    bail!(
        "the --out directory {} does not exist. Create it first, or pass --out under an existing directory.",
        parent.display()
    )
}

/// Refuse an `--out` that names an existing DIRECTORY, alongside its sibling
/// guards rather than at the rename.
///
/// The staged artifact is moved into place last, so without this the whole build
/// runs first — bundle, provision Node, strip and re-sign it, compress — and then
/// fails on the rename with `Is a directory (os error 21)` over a staging path the
/// user never chose and cannot act on. `--out dist` on a tree that already has a
/// `dist/` reaches it, which is an ordinary typo, and the cost was a 30 MB Node
/// download to learn about it.
fn reject_directory_output(out: &Path) -> Result<()> {
    if out.is_dir() {
        bail!(
            "the --out path {} is a directory. Pass the file to write, such as {}.",
            out.display(),
            out.join("app").display()
        );
    }
    Ok(())
}

/// Read `--icon`, refusing anything that would produce a Windows executable with
/// a broken resource directory.
///
/// The ICO magic is checked rather than the extension: an icon that is really a
/// PNG is the ordinary mistake — every image tool will happily save one under a
/// `.ico` name — and libsui would embed it into a resource Windows then declines
/// to draw, which is invisible until someone opens Explorer on another machine.
///
/// Windows is the only target that carries an icon inside the executable at all,
/// so the flag is refused elsewhere instead of being accepted and ignored. Unlike
/// Bun, which documents its icon flag as unusable when cross-compiling because it
/// calls Windows APIs, this is byte editing and works from any host.
fn load_icon(icon: Option<&Path>, target: &TargetPlatform) -> Result<Option<Vec<u8>>> {
    let Some(path) = icon else { return Ok(None) };
    if target.format() != ContainerFormat::Pe {
        bail!(
            "--icon applies to Windows executables, and this build targets {}.\n\
             \x20\x20macOS takes an icon from the surrounding .app bundle and Linux from a \
             .desktop entry, neither of which is part of a single-file artifact.",
            target.triple()
        );
    }
    let bytes =
        fs::read(path).with_context(|| format!("reading the --icon file {}", path.display()))?;
    // Reserved=0, type=1 (icon). A cursor file is type 2 and is otherwise identical.
    if bytes.get(..4) != Some(&[0, 0, 1, 0]) {
        bail!(
            "the --icon file {} is not an ICO.\n\
             \x20\x20Windows needs a real .ico here; a PNG or JPEG renamed to .ico embeds \
             but does not draw. Convert it first.",
            path.display()
        );
    }
    Ok(Some(bytes))
}

/// Build the Windows version resource — the fields Explorer's Details tab shows
/// and `(Get-Item app.exe).VersionInfo` reads.
///
/// The defaults are the point. A compiled binary with no version resource is what
/// installers and antivirus heuristics treat as anonymous, and the information
/// they want is already in `package.json`, so it is taken from there and the
/// common case needs no flags. `--metadata` only overrides: `Key=value` sets a
/// field and `Key=` drops one, the same spelling by which an empty
/// `--install-message` suppresses the first-run notice.
///
/// Refused rather than ignored on a non-Windows target, matching `--icon`: no
/// other container format carries these fields, so accepting the flag would
/// silently produce a binary without them. The package.json DEFAULTS are not
/// refused — they are implicit, and erroring on a Linux build because a project
/// has a `name` would be absurd.
fn load_version_info(
    metadata: &[String],
    entry_dir: &Path,
    out_path: &Path,
    target: &TargetPlatform,
) -> Result<Option<Vec<u8>>> {
    if !metadata.is_empty() && target.format() != ContainerFormat::Pe {
        bail!(
            "--metadata sets Windows executable metadata, and this build targets {}.\n\
             \x20\x20Only the PE format carries these fields inside the executable.",
            target.triple()
        );
    }
    if target.format() != ContainerFormat::Pe {
        return Ok(None);
    }

    let mut strings = nearest_package_metadata(entry_dir);
    // The field means the name the file was built under, so it is derived from
    // the output rather than defaulted from the manifest — a rename is exactly
    // what it lets a program detect.
    if let Some(name) = out_path.file_name().and_then(|n| n.to_str()) {
        strings.insert("OriginalFilename".to_string(), name.to_string());
    }
    for assignment in metadata {
        let (key, value) = version_info::parse_assignment(assignment)?;
        if value.is_empty() {
            strings.remove(key);
        } else {
            strings.insert(key.to_string(), value);
        }
    }

    // The numeric block is derived from the strings so the two can never
    // disagree; a prerelease tag survives in the string and is truncated only in
    // the four-u16 block, which has nowhere to put it.
    let file_version =
        version_info::parse_version(strings.get("FileVersion").map(String::as_str).unwrap_or(""))?;
    let product_version = version_info::parse_version(
        strings
            .get("ProductVersion")
            .map(String::as_str)
            .unwrap_or(""),
    )?;
    let info = version_info::VersionInfo {
        file_version,
        product_version,
        strings,
    };
    // OriginalFilename alone is not worth a resource: it would put an otherwise
    // blank Details tab on every Windows build that never asked for one.
    if info.strings.len() <= 1 && info.file_version == [0; 4] && info.product_version == [0; 4] {
        return Ok(None);
    }
    Ok(Some(info.encode()?))
}

/// Version-resource defaults from the nearest `package.json`, walking up from the
/// entry's directory — the same boundary Node itself resolves against.
fn nearest_package_metadata(entry_dir: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut dir = Some(entry_dir);
    while let Some(current) = dir {
        let manifest = current.join("package.json");
        if let Ok(text) = fs::read_to_string(&manifest)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
        {
            let field = |name: &str| {
                json.get(name)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            let mut set = |key: &str, value: Option<String>| {
                if let Some(value) = value {
                    out.insert(key.to_string(), value);
                }
            };
            set("ProductName", field("name"));
            set("InternalName", field("name"));
            set(
                "FileDescription",
                field("description").or_else(|| field("name")),
            );
            set("FileVersion", field("version"));
            set("ProductVersion", field("version"));
            set("CompanyName", package_author(&json));
            return out;
        }
        dir = current.parent();
    }
    out
}

/// The `author` field's display name. npm allows either a `{ name, email, url }`
/// object or the shorthand `"Name <email> (url)"`, and only the name belongs in
/// CompanyName — an email address in Explorer's Company column is noise.
fn package_author(json: &serde_json::Value) -> Option<String> {
    let author = json.get("author")?;
    let name = match author {
        serde_json::Value::String(s) => s
            .split(['<', '('])
            .next()
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        serde_json::Value::Object(_) => author
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        _ => return None,
    };
    (!name.is_empty()).then_some(name)
}

/// Refuse an output that names the source entry before fetching a launcher,
/// bundling, or provisioning Node. `canonicalize` catches ordinary spellings and
/// symlinks; Unix metadata additionally recognizes hard links.
fn reject_entry_output_alias(entry: &Path, out: &Path) -> Result<()> {
    if paths_alias(entry, out)? {
        bail!(
            "the compile output {} aliases the source entry {}. Pass --out with a different path.",
            out.display(),
            entry.display()
        );
    }
    Ok(())
}

fn paths_alias(entry: &Path, out: &Path) -> Result<bool> {
    let out = absolute_normalized(out)?;
    if out == entry {
        return Ok(true);
    }
    if fs::canonicalize(&out).is_ok_and(|resolved| resolved == entry) {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(entry_meta), Ok(out_meta)) = (fs::metadata(entry), fs::metadata(&out)) {
            return Ok(entry_meta.dev() == out_meta.dev() && entry_meta.ino() == out_meta.ino());
        }
    }
    Ok(false)
}

/// Make an output comparable with the already-canonical entry without requiring
/// the output to exist (the usual case). This is lexical only; canonicalization
/// above is still required to follow existing symlinks.
fn absolute_normalized(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving the current directory for --out")?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
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

/// Split one `NODE_OPTIONS`-style string into the arguments Node would see.
///
/// Node's own parser is the specification here, and it is not `split_whitespace`:
/// a quoted run is ONE argument so a path with a space survives, and a backslash
/// escapes the next character. Anything else is a spelling that works when the
/// author tests it in a shell and breaks once it is baked, which is the whole
/// failure this flag exists to prevent.
fn split_node_options(raw: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = raw.chars();

    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(next) => {
                    cur.push(next);
                    started = true;
                }
                None => {
                    bail!("--node-options {raw:?} ends in a lone backslash, which escapes nothing.")
                }
            },
            c if Some(c) == quote => quote = None,
            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                // An empty quoted run is still an argument.
                started = true;
            }
            c if c.is_whitespace() && quote.is_none() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("--node-options {raw:?} has an unclosed quote.");
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// Validate `--node-options` and hand back the arguments to bake into the
/// manifest.
///
/// Syntactic only. The target's Node is not available to ask — a foreign
/// platform's is not on this machine at all — so an option Node rejects fails at
/// startup rather than at build time. What IS checked is the shape that would
/// otherwise fail confusingly: a bare word, which Node would read as a script
/// path rather than an option.
///
/// The launcher applies these AFTER the set it computes for the target's Node
/// version, and whoever runs the binary can still set `NODE_OPTIONS` on top —
/// the three are additive, which is why nothing here tries to deduplicate.
fn node_flags(opts: &CompileOptions) -> Result<Vec<String>> {
    let mut flags = Vec::new();
    for raw in &opts.node_options {
        for arg in split_node_options(raw)? {
            if !arg.starts_with('-') {
                bail!(
                    "--node-options {raw:?} contains {arg:?}, which is not an option.\n\
                     \x20\x20Node reads a bare word as a script path. If it is a VALUE, attach it \
                     to its option with an equals sign: --node-options \"--max-old-space-size=4096\""
                );
            }
            flags.push(arg);
        }
    }
    Ok(flags)
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

/// Resolve this before the smol floor: after resolution, a major/minor pin,
/// alias, or range may be indistinguishable from an exact literal.
fn smol_requires_exact_target(pin: &VersionPin) -> bool {
    matches!(pin, VersionPin::Exact(_))
}

/// Preserve an explicit range in the manifest so the runtime can enforce its
/// complete constraint. Other non-exact forms intentionally retain floor mode.
///
/// ONLY a range whose minimum IS `gate` may be carried. The launcher enforces a
/// stored range in place of the floor, while the bundle is stripped against
/// `gate` — so anything satisfying the range below `gate` would run on an
/// artifact missing the polyfills it needs. Two forms fail that test and keep
/// floor-only acceptance, which can never be wider than the gate: one with no
/// representable lower bound (`<23`), where floor resolution falls back to the
/// NEWEST matching release, and one whose semver minimum is published but does
/// not carry the target's artifact, where the resolved gate lands above it.
fn smol_version_range(pin: &VersionPin, gate: &NodeVersion) -> String {
    match pin {
        VersionPin::Range(alternatives) if version_management::range_minimum_is(pin, gate) => {
            alternatives
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" || ")
        }
        _ => String::new(),
    }
}

/// Is `newest` worth recording as the version to DOWNLOAD when discovery finds
/// nothing? Two ways it is not.
///
/// Equal to the floor, and the preference says nothing the launcher does not
/// already know. Or — the case this exists for — newer than the floor, satisfying
/// the pin, and still unable to run the payload: `--target ">=22.15 <23.5"`
/// resolves to 23.4, which sorts above the floor and matches the range yet
/// predates `registerHooks` on the 23.x line. Recording it produced a binary that
/// built clean and then bailed on the user's machine, which is the same class the
/// launcher's discovery check closes, reached through the other door.
///
/// Refusing is safe rather than merely conservative: no preference means the
/// launcher provisions the FLOOR, and `check_node_support` has already failed the
/// build if a shim-bearing floor lacks the API. So whenever `shim_needed` holds
/// here, the floor is known to run the payload.
fn provision_preference_is_usable(
    newest: &NodeVersion,
    floor: &NodeVersion,
    shim_needed: bool,
) -> bool {
    newest != floor && (!shim_needed || newest.supports_augmentation())
}

/// The runtime contract printed beside the original target. Kept in step with
/// [`smol_version_range`] through the same predicate and the same `gate`, so the
/// line cannot promise an enforcement the manifest does not carry.
fn smol_runtime_policy(pin: &VersionPin, gate: &NodeVersion) -> &'static str {
    match pin {
        VersionPin::Exact(_) => "required exactly at runtime",
        VersionPin::Range(_) if version_management::range_minimum_is(pin, gate) => {
            "range enforced at runtime"
        }
        // The range's minimum is not what the gate resolved to, so only the gate
        // is enforceable: an upper-only range has no representable floor at all,
        // and a sparse artifact index can push the gate above the range's own.
        VersionPin::Range(_) => "floor enforced at runtime; upper bounds are not enforced",
        _ => "floor enforced at runtime",
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

/// Turn `--define-file KEY=PATH` into the `KEY=VALUE` strings `--define` already
/// takes. The two flags are one feature with two value sources, and this is where
/// they become one: argv caps a value at ARG_MAX, which a real payload — a
/// multi-megabyte JSON snapshot baked in as a build constant — exceeds outright.
///
/// The file holds the value EXACTLY as the command line would: a JavaScript
/// expression, so a bare string still needs its own quotes and raw JSON is already
/// an object literal. One trailing line ending is dropped, because every editor
/// writes one and the argv form has none — a user moving a value between the two
/// flags must get the same substitution.
fn read_define_files(raw: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let (key, path) = item
            .split_once('=')
            .filter(|(k, p)| !k.is_empty() && !p.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "--define-file expects KEY=PATH, got {item:?}\n\
                     \x20\x20The file holds the value, a JavaScript expression:\n\
                     \x20\x20--define-file MODELS=./models.json"
                )
            })?;
        let bytes = fs::read(path)
            .with_context(|| format!("reading {path}, the --define-file for {key}"))?;
        let mut value = String::from_utf8(bytes).map_err(|e| {
            anyhow!(
                "{path}, the --define-file for {key}, is not valid UTF-8 (first bad byte at \
                 offset {}).\n\
                 \x20\x20A define value is substituted into the bundle as source text. To ship \
                 binary data, embed it with --include instead.",
                e.utf8_error().valid_up_to()
            )
        })?;
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        // Same trap as the argv form, caught here so the advice can name the FILE.
        // `defines()` sees only the merged `KEY=VALUE` strings and cannot tell which
        // flag a value came from, and telling someone to retype it as `--define`
        // is the one fix that does not apply: this flag exists precisely for values
        // that do not fit on a command line.
        //
        // The remedy is shown as the required FILE CONTENTS, not as a shell command.
        // Both halves would be interpolated user input: a path with a space breaks the
        // redirect and sends the write somewhere else, and an apostrophe anywhere in the
        // value ends the quoting. A pasteable command that silently writes the wrong
        // file is worse than no command.
        if let Some(s) = bundle::swallowed_define(&value) {
            let (kept, suggested, outcome) =
                (&s.kept, &s.suggested, bundle::swallowed_define_outcome(&s));
            bail!(
                "{path}, the --define-file for {key}, is not a complete JavaScript expression: {suggested}\n\
                 \x20\x20The file holds an EXPRESSION, and JavaScript keeps only `{kept}` here —\n\
                 \x20\x20the rest is discarded (`//` opens a comment). Accepted, it would\n\
                 \x20\x20{outcome}\n\
                 \x20\x20Put the quotes inside the file, so it holds a string literal:\n\
                 \x20\x20\"{suggested}\""
            );
        }
        out.push(format!("{key}={value}"));
    }
    Ok(out)
}

/// Bundle output + embedded assets, in the payload's write order.
///
/// `--include`d files remain ordinary verbatim payload entries. Compiled chunks
/// carry their own `.mjs` extension, so they do not need a synthesized manifest
/// to establish their module format.
fn assemble_app(
    bundled: &bundle::BundleResult,
    layout: &assets::Layout,
    worker_wrappers: &[(String, Vec<u8>)],
    target: &TargetPlatform,
) -> Result<AppFiles> {
    // Root support files are compiler/launcher-private bootstrap inputs. They
    // stay at the payload root even when the entry and ordinary bundle output
    // live under a nested layout prefix.
    let mut files: AppFiles = bundled
        .root_support_files
        .iter()
        .map(|f| AppFile::plain(f.name.clone(), f.bytes.clone()))
        .chain(
            bundled
                .files
                .iter()
                .chain(&bundled.assets)
                .map(|f| AppFile::plain(layout.bundle_path(&f.name), f.bytes.clone())),
        )
        // Spliced in at the position islands have always occupied so the payload's
        // write order is unchanged. They are separate only because an island is a
        // verbatim copy of an installed package and so is the one bundler output
        // that can carry an executable — `esbuild`'s Go binary, a vendored helper.
        .chain(bundled.native_files.iter().map(|f| AppFile {
            name: layout.bundle_path(&f.name),
            bytes: f.bytes.clone(),
            executable: f.executable,
        }))
        .chain(
            bundled
                .support_files
                .iter()
                .map(|f| AppFile::plain(layout.bundle_path(&f.name), f.bytes.clone())),
        )
        .collect();

    files.extend(
        worker_wrappers
            .iter()
            .map(|(name, bytes)| AppFile::plain(layout.bundle_path(name), bytes.clone())),
    );

    // Tracked POSITIONALLY, not by looking a name up in a set: in a collision the
    // two sides share a name, so a name-keyed lookup labels both of them the same
    // and the message loses the one thing that makes it actionable — which side is
    // the file the user can rename.
    let mut origins: Vec<Origin> = vec![Origin::Generated; bundled.root_support_files.len()];
    origins.extend(std::iter::repeat_n(Origin::Compiled, bundled.files.len()));
    origins.resize(files.len(), Origin::Generated);
    for asset in &layout.assets {
        // Bytes and mode come from ONE open handle: a `--include`d path is
        // user-supplied, and a separate stat could describe a different file than
        // the one whose bytes were embedded.
        let file = fs::File::open(&asset.source)
            .with_context(|| format!("reading {}", asset.source.display()))?;
        let mode = source_mode(
            &file
                .metadata()
                .with_context(|| format!("reading {}", asset.source.display()))?,
        );
        let mut bytes = Vec::new();
        (&file)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {}", asset.source.display()))?;
        files.push(AppFile::from_source_mode(asset.rel.clone(), bytes, mode));
        origins.push(Origin::Included);
    }

    reject_colliding_names(&files, &origins, target)?;

    // The launcher refuses any payload name that could escape its extraction dir.
    // Names are partly user-derived since `--include`, so check the SAME predicate
    // on the WHOLE set here — rather than shipping an executable that aborts on
    // someone else's machine. Checked against the TARGET's rules, never the host's:
    // `a\..\..\x` is one ordinary filename on Linux and a traversal on Windows, so
    // a host-parsed gate lets a cross-compile bake a name its own launcher refuses.
    let rules = target.name_rules();
    // A chunk's name carries `layout.entry_prefix`, so a rejection is not always
    // an --include's fault — an entry under `src/aux/` trips the Windows device
    // rule on the directory the source tree already has.
    let windows_rules = if rules == nub_core::compile::NameRules::Windows {
        "\n\x20\x20On Windows that rules out `\\`, `<>:\"|?*`, a trailing dot or space, and the \
         reserved\n\x20\x20device names (CON, PRN, AUX, NUL, COM0-9, LPT0-9) — including as a \
         directory\n\x20\x20component, or before an extension."
    } else {
        ""
    };
    for name in files.iter().map(|f| &f.name) {
        // The launcher owns this one ROOT name as the durable app-extraction
        // completion marker. It is deliberately an exact match: nested paths
        // are ordinary user files and cannot collide with the root marker.
        if name == ".nub-complete" {
            bail!(
                "this path cannot be embedded: {name:?} is reserved for the compiled app's \
                 extraction completion marker. Rename the source or include it under a directory."
            );
        }
        if !nub_core::compile::is_safe_relative_name_for(rules, name) {
            bail!(
                "this path cannot be embedded for {}: {name:?}.\n\
                 \x20\x20Every file in a compiled binary needs a plain relative name the target \
                 can create.{windows_rules}",
                target.triple()
            );
        }
    }
    Ok(files)
}

/// Where a payload entry came from. Carried alongside the names so a collision
/// can say which of the two the user is able to change.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Origin {
    /// A bundle chunk, or a source map shipped with one.
    Compiled,
    /// Bytes the bundler embedded for a `file` import or a `new URL(…)`.
    Generated,
    /// A file the user named with `--include`.
    Included,
}

impl Origin {
    fn label(self) -> &'static str {
        match self {
            Self::Compiled => "compiled output",
            Self::Generated => "an embedded asset",
            Self::Included => "an --included file",
        }
    }
}

/// Refuse two payload entries the target's filesystem cannot keep apart.
///
/// The launcher writes entries in payload order, so a later one with the same
/// name silently REPLACES an earlier one — and on a case-insensitive filesystem
/// "the same name" includes a case variant. `--include Main.js` alongside an
/// emitted chunk `main.js` therefore ships a binary whose compiled code has been
/// overwritten by the asset, with nothing failing at build time. Same class as
/// the trailing dot/space that Win32 strips (see `is_safe_windows_component` in
/// nub-core), one level up: there the NAME is unusable, here the PAIR is.
///
/// Folding is applied only where the TARGET folds, never the host — Win32 folds
/// case; default macOS APFS folds case and canonically equivalent Unicode
/// spellings; Linux preserves both. No working Linux build is rejected.
///
/// Nub's OWN generated names cannot reach here as a fold-collision: `asset_name`
/// lowercases them, so a case-variant pair either dedupes (same bytes) or
/// separates on the content hash. Every collision this reports therefore has a
/// name the user wrote on at least one side.
fn reject_colliding_names(
    files: &AppFiles,
    origins: &[Origin],
    target: &TargetPlatform,
) -> Result<()> {
    let mut seen: std::collections::HashMap<String, usize> =
        std::collections::HashMap::with_capacity(files.len());
    for (i, file) in files.iter().enumerate() {
        let name = &file.name;
        let key = collision_key(name, target.os);
        let Some(first) = seen.insert(key, i) else {
            continue;
        };
        let (prev, prev_origin) = (&files[first].name, origins[first]);
        let why = collision_explanation(prev, name, target);
        // Only an --include has a spelling the user chose; a chunk name follows the
        // entry's, so pointing at --include when neither side is one would send
        // them to a flag they never passed.
        let fix = if [prev_origin, origins[i]].contains(&Origin::Included) {
            "\x20\x20Rename the file, or drop it from --include."
        } else {
            "\x20\x20Rename the source file one of them is named after."
        };
        bail!(
            "two files would collide in the compiled binary: {prev:?} ({}) and {name:?} ({}), \
             so one would overwrite the other where it is extracted.{why}\n{fix}",
            prev_origin.label(),
            origins[i].label()
        );
    }
    Ok(())
}

fn collision_key(name: &str, os: TargetOs) -> String {
    match os {
        TargetOs::Darwin => name.nfd().case_fold().collect(),
        TargetOs::Win32 => name.to_lowercase(),
        _ => name.to_string(),
    }
}

fn collision_explanation(prev: &str, name: &str, target: &TargetPlatform) -> String {
    if prev == name {
        return String::new();
    }
    let case_equivalent = prev.chars().case_fold().eq(name.chars().case_fold());
    let normalization_equivalent = prev.nfd().eq(name.nfd());
    let difference = match (case_equivalent, normalization_equivalent) {
        (true, false) => "differ only in case",
        (false, true) => "use canonically equivalent Unicode spellings",
        (false, false) if target.os == TargetOs::Darwin => {
            "differ in case and Unicode normalization"
        }
        _ => "use filesystem-equivalent spellings",
    };
    format!(
        "\n\x20\x20The names {difference}, and {}'s filesystem does not distinguish them.",
        target.triple()
    )
}

/// `--sourcemap=external` maps land BESIDE the executable, deliberately outside
/// it: the point of the mode is to keep source text out of what you ship while
/// still having a map to hand an error tracker. Their bytes are staged before
/// the executable is published so a write failure preserves the prior output.
fn stage_detached_maps(
    bundled: &bundle::BundleResult,
    out_path: &Path,
) -> Result<Vec<StagedDetachedMap>> {
    let dir = out_path.parent().unwrap_or_else(|| Path::new("."));
    let mut staged = Vec::with_capacity(bundled.detached_maps.len());
    for map in &bundled.detached_maps {
        let path = dir.join(&map.name);
        let staged_map = StagedArtifact::new(&path, "map")?;
        fs::write(staged_map.path(), &map.bytes).with_context(|| {
            format!("writing staged source map {}", staged_map.path().display())
        })?;
        sync_file(staged_map.path())?;
        staged.push(StagedDetachedMap {
            staged: staged_map,
            destination: path,
        });
    }
    Ok(staged)
}

/// Map publication cannot be one transaction with the executable (they are
/// separate files), so the executable wins: after it is atomically published a
/// failed map replacement is reported as a warning and its stage is cleaned up.
fn publish_detached_maps(staged_maps: Vec<StagedDetachedMap>) {
    for map in staged_maps {
        let destination = map.destination;
        match map.staged.publish(&destination) {
            Ok(()) => eprintln!("Wrote {}", destination.display()),
            Err(error) => eprintln!(
                "note: compiled executable was written, but its detached source map {} was not: {error:#}",
                destination.display()
            ),
        }
    }
}

struct StagedDetachedMap {
    staged: StagedArtifact,
    destination: PathBuf,
}

// ---- Node blob (default/embed shape) ------------------------------------------

/// Provision the official Node for `target`, strip it per the target's policy,
/// and zstd-19 compress. Returns the compressed Node blob, the hash of the
/// DECOMPRESSED (runnable) bytes, and the compressed aggregate root `LICENSE`
/// from that exact official distribution.
/// The embedded Node, compressed, alongside every field the manifest carries about
/// it. A struct rather than a tuple because these five travel together and three of
/// them are derived from the same decompressed bytes — `sha256` keys the extraction
/// directory, `blake3` is retained as manifest format headroom, and `size` is what
/// the launcher checks on a warm start.
///
/// `Default` is the `smol` shape: no embedded Node, so no blob, no digests, no size.
#[derive(Default)]
struct EmbeddedNode {
    /// zstd-19 compressed Node binary.
    blob: Vec<u8>,
    /// SHA-256 of the DECOMPRESSED bytes — the extraction cache key.
    sha256: String,
    /// BLAKE3 of the same bytes, retained as manifest format headroom.
    blake3: String,
    /// Length of the same bytes — the launcher's warm-start check.
    size: u64,
    /// zstd-19 compressed Node LICENSE.
    license: Vec<u8>,
}

fn build_node_blob(
    version: &NodeVersion,
    target: &TargetPlatform,
    cache_root: &Path,
    resolved_from: &str,
) -> Result<EmbeddedNode> {
    let (os, arch, musl) = dist_platform(target);
    // Provisioning prints the `Using Node.js <v> (resolved from <source>)` line +
    // downloads (verified against SHASUMS256.txt before it commits).
    let (dir, license) = version_management::provision_node_with_license_for_platform(
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
    let sha = crate::cli::sha256_hex(&bytes);
    // Retained as manifest format headroom; `sha` stays the cache key. Both are
    // over the same decompressed bytes.
    let b3 = blake3::hash(&bytes).to_hex().to_string();
    // Compressing a ~113 MB Node at zstd-19 takes ~20 s, and it was paid on every
    // single compile even though the input never changes for a given Node and
    // target. Keyed by the hash of the bytes being compressed, so a stale entry
    // is not expressible: different bytes are a different key.
    let cached = cache_root
        .join("compile-node-blob")
        .join(format!("{sha}.zst"));
    let blob = match std::fs::read(&cached) {
        Ok(blob) => blob,
        Err(_) => {
            eprintln!(
                "Compressing Node ({:.0} MB) with zstd-19 …",
                bytes.len() as f64 / 1_000_000.0
            );
            let blob = zstd::encode_all(&bytes[..], 19).context("zstd-19 compressing Node")?;
            // Written through a temporary so a killed compile cannot leave a
            // truncated blob behind for the next one to read as complete.
            if let Some(dir) = cached.parent() {
                let _ = std::fs::create_dir_all(dir);
                let tmp = cached.with_extension(format!("zst.{}.tmp", std::process::id()));
                if std::fs::write(&tmp, &blob).is_ok() && std::fs::rename(&tmp, &cached).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            blob
        }
    };
    let license = zstd::encode_all(&license[..], 19).context("zstd-19 compressing Node LICENSE")?;
    Ok(EmbeddedNode {
        blob,
        sha256: sha,
        blake3: b3,
        size: bytes.len() as u64,
        license,
    })
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
///   image of the expected format". Execution alone is NOT sufficient — see
///   `retains_node_api_exports`.
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

    // Capture the entitlements BEFORE stripping, because the stripper may destroy
    // them and `--preserve-metadata` would then have nothing to read. Measured:
    // Apple `strip -x` leaves the entitlement blob intact, `llvm-strip -x` removes
    // it outright — and the stripper is whichever of the two is on PATH, so without
    // this the artifact's posture depends on whether the BUILD MACHINE has LLVM
    // installed. Same shape as the napi-export defect: a host-dependent artifact.
    let entitlements = tmp.with_extension("entitlements.plist");
    let captured = needs_resign
        && run_ok(
            "codesign",
            &[
                "-d".as_ref(),
                "--entitlements".as_ref(),
                entitlements.as_os_str(),
                "--xml".as_ref(),
                tmp.as_os_str(),
            ],
        )
        && fs::metadata(&entitlements).is_ok_and(|m| m.len() > 0);
    let _ent_guard = FileGuard(entitlements.clone());

    let mut argv: Vec<&std::ffi::OsStr> = strip_flags(format).iter().map(AsRef::as_ref).collect();
    argv.push(tmp.as_os_str());
    let mut ok = run_ok(&strip, &argv);
    if ok && needs_resign {
        // `-i node` is load-bearing, not cosmetic. Without it `codesign` derives the
        // CodeDirectory identifier from the file's BASENAME, and this file is staged
        // as `nub-compile-node-<pid>` — so the same Node signed by two compiles gets
        // two identifiers, two signatures, and two `node_sha256` values. That keys a
        // different `compile-node/<version>-<hash>` extraction dir every time, so a
        // compile-and-run loop leaves a fresh ~107 MB tree behind on each pass and
        // nothing collects them. It also makes a byte-identical rebuild impossible.
        // `node` is what the official distribution signs with.
        //
        // `--preserve-metadata=entitlements,flags` keeps what a bare re-sign throws
        // away. Official Node ships six entitlements under the hardened runtime;
        // signing without this yields `flags=0x2(adhoc)` and NONE of them. The two
        // must move together: preserving the runtime flag without the entitlements
        // would turn library validation back ON without
        // `disable-library-validation`, and a third-party native addon would then
        // fail to load. `get-task-allow` (debug attach) rides along — that is Node's
        // own choice on the binary we are re-signing, and dropping it would be us
        // silently changing the runtime's posture rather than preserving it.
        let mut sign: Vec<&std::ffi::OsStr> =
            vec!["--force".as_ref(), "-i".as_ref(), "node".as_ref()];
        if captured {
            // `--options runtime` restores the hardened-runtime flag the re-sign
            // would otherwise drop. It must travel WITH the entitlements: the
            // runtime enforces library validation, and only
            // `disable-library-validation` (one of the six) lets a third-party
            // native addon load under it.
            sign.extend_from_slice(&[
                "--entitlements".as_ref(),
                entitlements.as_os_str(),
                "--options".as_ref(),
                "runtime".as_ref(),
            ]);
        }
        sign.extend_from_slice(&["-s".as_ref(), "-".as_ref(), tmp.as_os_str()]);
        ok = run_ok("codesign", &sign);
    }

    let stripped = if ok {
        fs::read(&tmp).with_context(|| format!("reading stripped {}", tmp.display()))?
    } else {
        Vec::new()
    };
    // A foreign binary cannot be executed — settle for "still the right kind of
    // image, and not obviously truncated".
    let intact = ok
        && if target.is_host() {
            node_runs(&tmp)
        } else {
            stripped.len() > 1_000_000 && inject::detect_format(&stripped) == Some(format)
        };
    let reject = if !ok {
        Some("strip failed")
    } else if !intact {
        Some("the stripped Node failed verification")
    } else if !retains_node_api_exports(&original, &stripped) {
        Some("stripping dropped the Node-API exports that native addons resolve against")
    } else {
        None
    };
    if let Some(why) = reject {
        eprintln!("note: {why} — embedding the Node binary unstripped");
        return Ok(original);
    }

    if needs_resign {
        eprintln!("Signing embedded Node.js");
    } else {
        eprintln!("Stripped the embedded Node");
    }
    Ok(stripped)
}

/// Flags this format's stripper needs to leave a usable Node behind.
///
/// Mach-O gets `-x` (discard local symbols only). Apple's `/usr/bin/strip` with
/// NO flags rewrites an executable's dyld export trie, leaving Node exporting 4
/// of its 159 `napi_*` symbols — and a macOS addon binds Node-API against the
/// host executable's flat namespace, so every addon then dies at `dlopen` with
/// `symbol not found in flat namespace '_napi_create_error'`. `-x` is spelled
/// the same by Apple strip, GNU strip and llvm-strip, keeps all 159, and still
/// recovers 36 of the 38 MB the default flags do (measured, Node 26 darwin-arm64:
/// 145.3 MB original → 104.6 MB default → 107.0 MB with `-x`).
///
/// ELF and PE keep their default flags: `strip` never touches ELF `.dynsym` or
/// the PE export directory, and `-x` there would only retain `.symtab` globals
/// for no gain.
fn strip_flags(format: ContainerFormat) -> &'static [&'static str] {
    match format {
        ContainerFormat::MachO => &["-x"],
        _ => &[],
    }
}

/// Did the strip preserve the Node-API symbols a native addon resolves against?
///
/// This is the check `node_runs` structurally cannot make: a Node with no
/// exported Node-API answers `--version` perfectly and then fails every single
/// `dlopen`. The defect that motivated it surfaced only on hosts without
/// `llvm-strip` — a stock macOS runner, never a dev box with Homebrew LLVM —
/// because execution was the only gate.
///
/// Differential, never absolute: an original that does not export Node-API, or
/// an image this build cannot read, passes. The check can therefore only fire on
/// a real regression, and no future Node layout can silently veto every strip.
///
/// Mach-O only, deliberately. That is where the defect exists and where the
/// answer is subtle; `strip` leaves ELF `.dynsym` and the PE export directory
/// alone, so there is nothing to catch and no second parser to get wrong.
fn retains_node_api_exports(original: &[u8], stripped: &[u8]) -> bool {
    !exports_node_api(original) || exports_node_api(stripped)
}

/// Does this Mach-O export any `napi_*` symbol?
///
/// Reads the dyld EXPORT TRIE, which is what resolves an addon's flat-namespace
/// references — NOT the classic symbol table. The two disagree exactly where it
/// matters: `llvm-strip` drops the symbol table and keeps the trie (addons still
/// load), Apple's `strip` rewrites the trie itself (addons no longer load). A
/// symbol-table reader calls the working case broken.
fn exports_node_api(image: &[u8]) -> bool {
    macho_export_trie(image)
        .and_then(|trie| trie_has_export_prefix(trie, b"_napi_"))
        .unwrap_or(false)
}

/// The `LC_DYLD_EXPORTS_TRIE` (or legacy `LC_DYLD_INFO`) payload of a 64-bit
/// little-endian Mach-O. Anything else — fat, 32-bit, big-endian, truncated —
/// reads as "no trie", which the differential above treats as "cannot verify".
fn macho_export_trie(image: &[u8]) -> Option<&[u8]> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_DYLD_INFO: u32 = 0x22;
    const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
    const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;

    fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
        let raw = bytes.get(offset..offset.checked_add(4)?)?;
        Some(u32::from_le_bytes(raw.try_into().ok()?))
    }

    if u32_at(image, 0)? != MH_MAGIC_64 {
        return None;
    }
    let ncmds = u32_at(image, 16)?;
    let mut offset = 32usize;
    for _ in 0..ncmds {
        let cmd = u32_at(image, offset)?;
        let cmdsize = u32_at(image, offset + 4)? as usize;
        if cmdsize < 8 {
            return None;
        }
        // `linkedit_data_command` carries its (offset, size) pair at +8; in
        // `dyld_info_command` the export pair is the sixth, at +40.
        let pair = match cmd {
            LC_DYLD_EXPORTS_TRIE => Some(offset + 8),
            LC_DYLD_INFO | LC_DYLD_INFO_ONLY => Some(offset + 40),
            _ => None,
        };
        if let Some(at) = pair {
            let start = u32_at(image, at)? as usize;
            let len = u32_at(image, at + 4)? as usize;
            return image.get(start..start.checked_add(len)?);
        }
        offset = offset.checked_add(cmdsize)?;
    }
    None
}

/// Does any exported name in this trie start with `prefix`?
///
/// The trie splits a name across edge labels, so a plain byte search cannot
/// answer this. Descent is pruned to the one path that still spells `prefix`,
/// making the walk O(prefix length) rather than O(exports). A trie has no dead
/// branches, so spelling out `prefix` proves a real export sits below it.
fn trie_has_export_prefix(trie: &[u8], prefix: &[u8]) -> Option<bool> {
    // (node offset, how much of `prefix` the path to it has already spelled)
    let mut stack = vec![(0usize, 0usize)];
    // Child offsets are unvalidated file data, so bound the walk rather than
    // trusting them to form a DAG.
    let mut budget = 10_000u32;

    while let Some((node, matched)) = stack.pop() {
        budget = budget.checked_sub(1)?;
        let (terminal_size, width) = uleb128(trie, node)?;
        let mut at = node
            .checked_add(width)?
            .checked_add(usize::try_from(terminal_size).ok()?)?;
        let children = *trie.get(at)?;
        at += 1;
        for _ in 0..children {
            let start = at;
            while *trie.get(at)? != 0 {
                at += 1;
            }
            let label = trie.get(start..at)?;
            at += 1;
            let (child, width) = uleb128(trie, at)?;
            at = at.checked_add(width)?;

            let want = prefix.get(matched..)?;
            let common = want.len().min(label.len());
            if label.get(..common)? != want.get(..common)? {
                continue;
            }
            if label.len() >= want.len() {
                return Some(true);
            }
            stack.push((usize::try_from(child).ok()?, matched + label.len()));
        }
    }
    Some(false)
}

/// Returns the decoded value and the number of bytes it occupied.
fn uleb128(bytes: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    let mut at = offset;
    loop {
        let byte = *bytes.get(at)?;
        at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, at - offset));
        }
        shift += 7;
        if shift > 56 {
            return None;
        }
    }
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

// ---- artifact verification ----------------------------------------------------

/// Check the artifact before handing it to the user. Two layers, the second
/// available only natively:
///
/// 1. **Static scan (always).** Locate the payload in the produced file the way
///    the target's loader will, and decode it. Catches a malformed injection on
///    every target, including the cross ones that cannot be run here.
/// 2. **Probe-mode self-check (target == host only).** Executes the artifact so it
///    reads its own section and touches a heap allocation — the exact path an
///    under-padded Mach-O injection corrupts into a SIGILL trap, which no static
///    check can see. Cross-compiling SKIPS this, loudly: an artifact that passes
///    the scan but was never executed is a weaker guarantee, and the user should
///    know which one they got.
fn verify_artifact(bin: &Path, target: &TargetPlatform, version_info: Option<&[u8]>) -> Result<()> {
    let bytes = fs::read(bin).with_context(|| format!("reading {}", bin.display()))?;
    let payload = inject::find_payload(target.format(), &bytes)
        .with_context(|| format!("scanning {} for its payload", bin.display()))?
        .context("the produced executable carries no payload — the injection did not take")?;
    let view = nub_core::compile::decode(payload)
        .context("the produced executable's payload does not decode")?;
    verify_payload_shape(&view)?;

    // The version resource is re-read here for the same reason the payload is,
    // and it needs it more. A cross-compiled Windows binary cannot be executed
    // on this host, so this parse is the only evidence the resource is REACHABLE
    // rather than merely written — and an unreachable one fails silently, with
    // Explorer showing nothing and no error anywhere. The concrete way to lose it
    // is the resource directory's ascending-id rule (see `set_version_info` in
    // vendor/libsui), which takes the icon down with it.
    if let Some(encoded) = version_info {
        let found = inject::find_version_resource(&bytes)
            .with_context(|| format!("scanning {} for its version resource", bin.display()))?
            .context(
                "the produced executable carries no version resource, so its metadata \
                 would not appear in Explorer — the injection did not take",
            )?;
        // Compared as PARSED values rather than as bytes. The walk is the point:
        // it navigates by the declared lengths and the alignment rule, so a
        // resource whose root header survived while its StringTable or Var
        // children were truncated compares unequal here, where a byte compare
        // would only catch a change and a bare parse would accept the header
        // alone. Both sides go through the same reader so the comparison is of
        // what Windows would see, not of what nub meant.
        let intended = version_info::parse(encoded)
            .context("the version resource nub encoded does not parse")?;
        let carried = version_info::parse(found)
            .context("the produced executable's version resource does not parse")?;
        if carried != intended {
            bail!(
                "the produced executable's version resource is not the one that was \
                 encoded — {} fields and {} translations survived, out of {} and {}",
                carried.strings.len(),
                carried.translations.len(),
                intended.strings.len(),
                intended.translations.len()
            );
        }
    }

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
    let out = match probe_once(&bin) {
        Ok(out) => out,
        // A deep enough `--out` yields a binary every FILE api can read and sign
        // but that Windows will not spawn. MEASURED with this crate's own
        // artifact on Windows Server 2022: `--out` at 285 characters compiles
        // clean, at 351 the staged image fails to start with os error 3. The
        // threshold between them is not pinned, so the retry keys on the error
        // rather than on a length.
        //
        // Do NOT "fix" this by prefixing the path. `\\?\` does not rescue a
        // spawn the way it rescues the publish rename in `windows_verbatim_path`
        // — measured from Rust, it turns a WORKING spawn into os error 123.
        // Long-path support does not decide it either: the failure reproduces
        // both where `LongPathsEnabled` is 0 (a default Windows box) and where
        // it is 1 (GitHub's windows runners).
        //
        // The probe asks whether the produced BYTES run, which is a property of
        // the file rather than of its directory, so retry from a short copy. The
        // staged artifact itself must NOT move: its directory is chosen to share
        // a filesystem with the destination so the publish stays an atomic
        // rename.
        #[cfg(windows)]
        Err(error) if is_path_too_long_to_spawn(&error) => {
            // Carry the original code into both failures. os 3 is
            // ERROR_PATH_NOT_FOUND, which is not exclusively a length signal, so
            // a genuinely missing image must keep naming itself instead of
            // surfacing as an unexplained copy error.
            let short = ShortProbeCopy::new(&bin)
                .with_context(|| format!("{} did not spawn in place: {error}", bin.display()))?;
            probe_once(short.path()).with_context(|| {
                format!(
                    "running the self-probe on a short copy of {}, which did not spawn in place: {error}",
                    bin.display()
                )
            })?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("running the self-probe on {}", bin.display()));
        }
    };
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

fn probe_once(bin: &Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new(bin)
        .env("__NUB_COMPILED_LAUNCHER_MODE", "probe")
        .output()
}

/// Whether a spawn failed because Windows would not accept the image path's
/// length: `ERROR_PATH_NOT_FOUND` (3), which is what `CreateProcess` reports for
/// an over-long path that plainly exists, or `ERROR_FILENAME_EXCED_RANGE` (206).
///
/// Test the RAW code, never `ErrorKind`: what std maps these to is std's
/// business and has changed, whereas which codes mean "too long" is this
/// predicate's contract. `aube-linker`'s `is_transient_fs_error` documents the
/// same trap for os 32, which no `matches!` arm could name.
#[cfg(windows)]
fn is_path_too_long_to_spawn(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(3 | 206))
}

/// A copy of the artifact at a path short enough to spawn, removed on drop.
///
/// Copying the artifact is real work — a measured embed shape is ~25 MB — which
/// is why this exists only on the retry rather than as the default path. An
/// ordinary `--out` spawns in place and copies nothing.
#[cfg(windows)]
struct ShortProbeCopy {
    path: PathBuf,
    _guard: FileGuard,
}

#[cfg(windows)]
impl ShortProbeCopy {
    fn new(bin: &Path) -> Result<Self> {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // `.exe` is required: Windows decides executability by extension.
        let path = std::env::temp_dir().join(format!(
            "nub-compile-probe-{}-{seq}.exe",
            std::process::id()
        ));
        fs::copy(bin, &path).with_context(|| {
            format!(
                "copying {} to {} so the self-probe can spawn it",
                bin.display(),
                path.display()
            )
        })?;
        Ok(Self {
            path: path.clone(),
            _guard: FileGuard(path),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Static verification's redistribution invariant: an embed artifact always
/// includes the compressed aggregate root `LICENSE` for its exact Node dist.
/// `smol` carries neither Node nor its notice.
fn verify_payload_shape(view: &nub_core::compile::PayloadView<'_>) -> Result<()> {
    match view.manifest.shape {
        Shape::Embed if view.node_license_blob.is_empty() => {
            bail!("the produced executable embeds Node but carries no Node LICENSE")
        }
        Shape::Smol if !view.node_blob.is_empty() || !view.node_license_blob.is_empty() => {
            bail!("the produced smol executable carries embedded Node payload data")
        }
        _ => Ok(()),
    }
}

// ---- helpers ------------------------------------------------------------------

/// A private staging container in a writable same-filesystem ancestor when one
/// is available. Its file can be atomically renamed into the destination. If
/// that ancestor is not writable, the output directory is the compatibility
/// fallback: the private container still protects and cleans its payload bytes,
/// although an empty container can remain until a late chmod is reversed.
struct StagedArtifact {
    path: PathBuf,
    container: PathBuf,
}

impl StagedArtifact {
    fn new(destination: &Path, kind: &str) -> Result<Self> {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let parents = staging_dirs(destination)?;
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output");
        for parent in parents {
            for _ in 0..128 {
                let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let container =
                    parent.join(format!(".nub-compile-{kind}-{}-{seq}", std::process::id()));
                match create_private_staging_dir(&container) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    // The output directory is intentionally the second choice:
                    // users commonly own it while not owning its parent.
                    //
                    // A read-only parent is the same situation and must fall
                    // through the same way. `--out /tmp/app` reaches it on every
                    // current macOS: the parent of /tmp is /, which the sealed
                    // system volume mounts read-only, so staging there fails with
                    // EROFS rather than EACCES and used to abort the whole build
                    // instead of trying the output directory that would have
                    // worked. Writing to a directory under /tmp hid it, because
                    // then the parent is an ordinary writable directory.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::PermissionDenied
                                | std::io::ErrorKind::ReadOnlyFilesystem
                        ) =>
                    {
                        break;
                    }
                    Err(error) => {
                        return Err(anyhow::Error::new(error).context(format!(
                            "creating private staging directory {}",
                            container.display()
                        )));
                    }
                }
                let path = container.join(format!("{name}.tmp"));
                if let Err(error) = create_private_staging_file(&path) {
                    let _ = fs::remove_dir(&container);
                    return Err(anyhow::Error::new(error)
                        .context(format!("creating staged output {}", path.display())));
                }
                return Ok(Self { path, container });
            }
        }
        bail!(
            "could not reserve a unique staged output beside {}",
            destination.display()
        )
    }

    fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    fn container(&self) -> &Path {
        &self.container
    }

    fn publish(self, destination: &Path) -> Result<()> {
        publish_staged_with(&self.path, destination, |staged, destination| {
            atomic_replace(staged, destination)
        })?;
        let _ = fs::remove_dir(&self.container);
        sync_parent_directory(destination);
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(staged: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(staged, destination)
}

#[cfg(windows)]
fn atomic_replace(staged: &Path, destination: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let staged = windows_verbatim_path(staged)?;
    let destination = windows_verbatim_path(destination)?;
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    // SAFETY: both buffers are live, NUL-terminated UTF-16 paths. No destination
    // is removed first, so an API failure leaves the previous artifact intact.
    if unsafe { MoveFileExW(staged.as_ptr(), destination.as_ptr(), flags) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_verbatim_path(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("output path has no file name: {}", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // `canonicalize` returns Windows' absolute verbatim (`\\?\`) spelling. Only
    // the existing parent is resolved: the destination may not exist yet, and a
    // final symlink must be replaced rather than followed.
    let path = fs::canonicalize(parent)?.join(name);
    Ok(path.as_os_str().encode_wide().chain(Some(0)).collect())
}

#[cfg(unix)]
fn create_private_staging_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)
}

#[cfg(not(unix))]
fn create_private_staging_dir(path: &Path) -> std::io::Result<()> {
    // `create_dir` is create-new and never follows a final path component. The
    // platform ACL inherited by a private user directory is the narrowest
    // portable policy available without introducing a Windows ACL dependency.
    fs::create_dir(path)
}

#[cfg(unix)]
fn create_private_staging_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map(|_| ())
}

#[cfg(not(unix))]
fn create_private_staging_file(path: &Path) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
}

fn staging_dirs(destination: &Path) -> Result<Vec<PathBuf>> {
    let output_dir = destination.parent().unwrap_or_else(|| Path::new("."));
    let output_dir = absolute_normalized(output_dir)
        .context("resolving the output directory for transactional staging")?;
    let mut dirs = Vec::with_capacity(2);
    if let Some(ancestor) = output_dir.parent()
        && same_filesystem(&output_dir, ancestor)
    {
        dirs.push(ancestor.to_path_buf());
    }
    dirs.push(output_dir);
    Ok(dirs)
}

#[cfg(unix)]
fn same_filesystem(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_filesystem(_a: &Path, _b: &Path) -> bool {
    // A lexical ancestor is normally on the same Windows volume. `rename` still
    // reports any unusual mount boundary rather than degrading to copy+delete.
    true
}

fn publish_staged_with<F>(staged: &Path, destination: &Path, rename: F) -> Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    rename(staged, destination).with_context(|| {
        format!(
            "atomically replacing {} with staged output {}",
            destination.display(),
            staged.display()
        )
    })
}

impl Drop for StagedArtifact {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.container);
    }
}

/// Flush an artifact before the rename that makes it visible. Write access is
/// deliberate: Windows `FlushFileBuffers` requires it.
fn sync_file(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("flushing staged output {}", path.display()))
}

/// POSIX directories must be synced for a rename to survive a power loss. Rust's
/// portable file open cannot acquire the Windows directory handle access needed
/// for its equivalent, so that platform remains best effort.
#[cfg(unix)]
fn sync_parent_directory(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fs::File::open(dir).and_then(|dir| dir.sync_all());
    }
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) {}

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The first of `names` on PATH, as the matched path — which is what gets spawned,
/// so discovery and execution cannot disagree.
///
/// Thin on purpose: the lookup lives in nub-core because the launcher needs the
/// same one, and because nub-core's tests run on every OS leg while the
/// compile-feature tests run only on Ubuntu. The rule it carries is that on
/// Windows the strippers are on disk as `llvm-strip.exe`, so a bare
/// `dir.join("llvm-strip")` matched nothing and `prepare_node_bytes` took its
/// unstripped early return on every compile run on a Windows host.
fn which_first(names: &[&str]) -> Option<PathBuf> {
    nub_core::find_on_path(names)
}

fn run_ok(program: impl AsRef<std::ffi::OsStr>, args: &[&std::ffi::OsStr]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Content hash of the app payload — name, length, bytes, and executable bit of
/// each file, in order.
///
/// This is the extraction cache key, so the mode belongs in it: two artifacts
/// whose files differ only in executability must not share one extracted tree.
fn sha256_of_app(files: &[AppFile<Vec<u8>>]) -> String {
    let mut h = Sha256::new();
    for file in files {
        h.update(file.name.as_bytes());
        h.update((file.bytes.len() as u64).to_le_bytes());
        h.update(&file.bytes);
        h.update([u8::from(file.executable)]);
    }
    format!("{:x}", h.finalize())
}

/// The source file's Unix mode, or `None` where the platform has none — see
/// [`AppFile::from_source_mode`].
#[cfg(unix)]
fn source_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    Some(metadata.permissions().mode())
}
#[cfg(not(unix))]
fn source_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
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
    use nub_core::compile::COMPILE_BOOTSTRAP_NAME;

    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nub-compile-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Only the two path-length codes may divert the probe to a copy. The
    /// mapping Rust gives these codes is deliberately not asserted: that is
    /// std's business and it has changed, whereas which codes mean "too long" is
    /// this predicate's contract.
    #[cfg(windows)]
    #[test]
    fn only_the_path_length_spawn_errors_take_the_short_copy() {
        use std::io::Error;

        assert!(is_path_too_long_to_spawn(&Error::from_raw_os_error(3)));
        assert!(is_path_too_long_to_spawn(&Error::from_raw_os_error(206)));
        // A missing image and a denied one must surface as themselves rather
        // than sending a ~100 MB copy after a file that is not the problem.
        assert!(!is_path_too_long_to_spawn(&Error::from_raw_os_error(2)));
        assert!(!is_path_too_long_to_spawn(&Error::from_raw_os_error(5)));
    }

    /// The `--out` parent is validated up front because the write happens only
    /// after bundling, re-signing Node, and compressing ~100 MB — and the error
    /// that surfaced there named an internal staging directory, not `--out`.
    #[test]
    fn a_missing_out_parent_is_refused_but_a_bare_filename_is_not() {
        let dir = fresh_dir("outparent");

        let missing = dir.join("nope").join("app");
        let err = reject_missing_output_parent(&missing)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(
            err.contains(&dir.join("nope").display().to_string()),
            "{err}"
        );

        let file = dir.join("afile");
        fs::write(&file, b"x").unwrap();
        let err = reject_missing_output_parent(&file.join("app"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a directory"), "{err}");

        reject_missing_output_parent(&dir.join("app")).expect("an existing parent is accepted");
        // A bare filename lands in the cwd, which exists by definition.
        reject_missing_output_parent(Path::new("app")).expect("a bare filename is accepted");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The parent guard above passes when `--out` names a directory whose parent
    /// `--icon` is checked before anything expensive runs, and by CONTENT rather
    /// than by extension — a PNG saved under a `.ico` name is the ordinary mistake
    /// and would embed into a resource Windows silently declines to draw.
    #[test]
    fn an_icon_is_refused_unless_it_is_an_ico_on_a_windows_target() {
        let dir = fresh_dir("icon");
        let ico = dir.join("app.ico");
        // Reserved=0, type=1 (icon); the rest never gets read on these paths.
        std::fs::write(&ico, [0u8, 0, 1, 0, 1, 0]).unwrap();
        let win = TargetPlatform {
            os: TargetOs::Win32,
            arch: TargetArch::X64,
            musl: false,
        };
        let mac = TargetPlatform {
            os: TargetOs::Darwin,
            arch: TargetArch::Arm64,
            musl: false,
        };

        assert!(load_icon(Some(&ico), &win).unwrap().is_some());
        // The control: without it the ICO check above could pass on any input.
        let png = dir.join("fake.ico");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
        let err = load_icon(Some(&png), &win).unwrap_err().to_string();
        assert!(err.contains("is not an ICO"), "{err}");

        let err = load_icon(Some(&ico), &mac).unwrap_err().to_string();
        assert!(
            err.contains("darwin-arm64"),
            "must name the target, got: {err}"
        );
        assert!(load_icon(None, &mac).unwrap().is_none());
    }

    /// The defaults are the feature: a Windows build with no version resource is
    /// what installers and antivirus heuristics treat as anonymous, and everything
    /// they want is already in `package.json`. So the manifest path is asserted
    /// first, then the two override spellings, then the target gate.
    ///
    /// Read back through [`version_info::parse`] rather than compared against the
    /// map that went in — the bytes are what ships, and a resource that encodes
    /// but does not walk is the failure mode nothing else here would catch.
    #[test]
    fn version_metadata_defaults_to_the_manifest_and_metadata_overrides_it() {
        let dir = fresh_dir("versioninfo");
        fs::write(
            dir.join("package.json"),
            r#"{"name":"acme-tool","version":"2.5.1","description":"Does a thing","author":{"name":"Acme Inc."}}"#,
        )
        .unwrap();
        let win = TargetPlatform {
            os: TargetOs::Win32,
            arch: TargetArch::X64,
            musl: false,
        };
        let out = dir.join("acme.exe");

        let bytes = load_version_info(&[], &dir, &out, &win)
            .unwrap()
            .expect("a manifest with a name and a version earns a resource");
        let parsed = version_info::parse(&bytes).unwrap();
        assert_eq!(
            parsed.strings.get("ProductName").map(String::as_str),
            Some("acme-tool")
        );
        assert_eq!(
            parsed.strings.get("CompanyName").map(String::as_str),
            Some("Acme Inc.")
        );
        assert_eq!(
            parsed.strings.get("FileDescription").map(String::as_str),
            Some("Does a thing")
        );
        assert_eq!(parsed.file_version, [2, 5, 1, 0]);
        // Derived from --out, not the manifest: the field means the name the file
        // was built under, which is exactly what a rename should change.
        assert_eq!(
            parsed.strings.get("OriginalFilename").map(String::as_str),
            Some("acme.exe")
        );

        // `Key=value` overrides; `Key=` drops, the same spelling by which an empty
        // --install-message suppresses the first-run notice.
        let overridden = load_version_info(
            &[
                "ProductName=Renamed".to_string(),
                "CompanyName=".to_string(),
            ],
            &dir,
            &out,
            &win,
        )
        .unwrap()
        .unwrap();
        let parsed = version_info::parse(&overridden).unwrap();
        assert_eq!(
            parsed.strings.get("ProductName").map(String::as_str),
            Some("Renamed")
        );
        assert!(
            !parsed.strings.contains_key("CompanyName"),
            "an empty value drops the field rather than writing a blank one"
        );

        // A project with nothing to say earns no resource at all: OriginalFilename
        // alone would put a near-blank Details tab on every Windows build. The
        // empty manifest is what makes this deterministic — the lookup walks UP
        // from the entry, so a bare directory would otherwise inherit whatever
        // package.json happens to sit above the temp dir on the runner.
        let bare = fresh_dir("versioninfo-bare");
        fs::write(bare.join("package.json"), "{}").unwrap();
        assert!(load_version_info(&[], &bare, &out, &win).unwrap().is_none());

        // Refused on a target whose container cannot carry the fields — accepting
        // it would ship a binary silently missing them. The manifest DEFAULTS are
        // not refused, because they are implicit: erroring on a Linux build
        // because the project has a name would be absurd.
        let linux = TargetPlatform {
            os: TargetOs::Linux,
            arch: TargetArch::X64,
            musl: false,
        };
        let err = load_version_info(&["ProductName=x".to_string()], &dir, &out, &linux)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("linux-x64"),
            "must name the target, got: {err}"
        );
        assert!(
            load_version_info(&[], &dir, &out, &linux)
                .unwrap()
                .is_none()
        );
    }

    /// exists, so the build used to run to completion and die on the rename with
    /// `Is a directory (os error 21)` over an internal staging path.
    #[test]
    fn an_out_path_that_is_a_directory_is_refused() {
        let dir = fresh_dir("outdir");

        let err = reject_directory_output(&dir).unwrap_err().to_string();
        assert!(err.contains("is a directory"), "{err}");
        // Names a usable path rather than only diagnosing: the whole point is that
        // the user learns the fix here instead of after a 30 MB Node download.
        assert!(
            err.contains(&dir.join("app").display().to_string()),
            "the message must suggest a file to write, got: {err}"
        );

        // The ordinary cases the guard must not touch: a path that does not exist
        // yet is exactly what a normal compile passes, and so is a plain file being
        // overwritten by a rebuild.
        reject_directory_output(&dir.join("app")).expect("a not-yet-existing output is accepted");
        let file = dir.join("existing");
        fs::write(&file, b"x").unwrap();
        reject_directory_output(&file).expect("overwriting an existing file is accepted");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Apple's `strip` needs `-x` on Mach-O or it rewrites the export trie and
    /// leaves a Node that runs but cannot `dlopen` a single native addon. The
    /// end-to-end proof is `tests/compile-native-islands/` on a host whose only
    /// stripper is `/usr/bin/strip` — no unit test can strip a real Node image.
    #[test]
    fn only_mach_o_carries_a_strip_flag() {
        assert_eq!(strip_flags(ContainerFormat::MachO), &["-x"]);
        assert!(strip_flags(ContainerFormat::Elf).is_empty());
        assert!(strip_flags(ContainerFormat::Pe).is_empty());
    }

    /// A Mach-O export trie holding exactly one name, one edge per segment —
    /// the shape the real encoder produces when a name shares a prefix with a
    /// sibling.
    fn export_trie(segments: &[&[u8]]) -> Vec<u8> {
        // Leaf: a terminal payload of (flags, address), no children.
        let mut nodes = vec![vec![0x02, 0x00, 0x00, 0x00]];
        for segment in segments.iter().rev() {
            let mut node = vec![0x00, 0x01];
            node.extend_from_slice(segment);
            node.push(0);
            node.push(0); // child offset, patched once the layout is known
            nodes.push(node);
        }
        nodes.reverse();

        let mut offsets = Vec::new();
        let mut at = 0usize;
        for node in &nodes {
            offsets.push(at);
            at += node.len();
        }
        assert!(
            at < 0x80,
            "this builder only emits single-byte ULEB offsets"
        );

        let mut trie = Vec::new();
        for (index, node) in nodes.iter().enumerate() {
            let mut node = node.clone();
            if let Some(child) = offsets.get(index + 1) {
                let last = node.len() - 1;
                node[last] = *child as u8;
            }
            trie.extend_from_slice(&node);
        }
        trie
    }

    /// The trie splits a name across edges wherever it shares a prefix with a
    /// sibling, so `_napi_` routinely straddles an edge boundary — which is why
    /// this cannot be a byte search. `_nanosleep` is the adversarial neighbour:
    /// it shares `_na` and must not count.
    #[test]
    fn the_export_scan_follows_a_name_split_across_trie_edges() {
        let split = export_trie(&[b"_na", b"pi_create_error"]);
        assert_eq!(trie_has_export_prefix(&split, b"_napi_"), Some(true));

        let whole = export_trie(&[b"_napi_create_error"]);
        assert_eq!(trie_has_export_prefix(&whole, b"_napi_"), Some(true));

        let neighbour = export_trie(&[b"_nanosleep"]);
        assert_eq!(trie_has_export_prefix(&neighbour, b"_napi_"), Some(false));

        // Truncated trie data reads as unanswerable, never as a panic.
        assert_eq!(trie_has_export_prefix(&whole[..6], b"_napi_"), None);
    }

    /// The export check is a DIFFERENTIAL so it can only ever fire on a real
    /// regression: an original that exports no Node-API, or bytes this parser
    /// does not understand, must never veto a strip that is actually fine.
    #[test]
    fn the_node_api_export_check_never_vetoes_what_it_cannot_read() {
        assert!(retains_node_api_exports(b"not a mach-o", b"still not"));
        assert!(!exports_node_api(b"not a mach-o"));
        assert!(!exports_node_api(&[]));
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
    fn an_extensionless_default_output_cannot_overwrite_its_source_entry() {
        let dir = fresh_dir("entry-output-default");
        let source = dir.join("app");
        fs::write(&source, "console.log('source')").unwrap();
        let source = fs::canonicalize(&source).unwrap();
        let target = TargetPlatform::parse("linux-x64").unwrap();
        assert_eq!(default_output_path("app", &target), PathBuf::from("app"));
        assert!(reject_entry_output_alias(&source, &source).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalized_and_existing_link_output_aliases_are_refused() {
        let dir = fresh_dir("entry-output-alias");
        let source = dir.join("app.js");
        fs::write(&source, "console.log('source')").unwrap();
        let source = fs::canonicalize(&source).unwrap();
        let normalized = dir.join("nested").join("..").join("app.js");
        assert!(paths_alias(&source, &normalized).unwrap());

        #[cfg(unix)]
        {
            let hard_link = dir.join("hard-link.js");
            fs::hard_link(&source, &hard_link).unwrap();
            assert!(paths_alias(&source, &hard_link).unwrap());

            let symlink = dir.join("sym-link.js");
            std::os::unix::fs::symlink(&source, &symlink).unwrap();
            assert!(paths_alias(&source, &symlink).unwrap());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_atomic_publish_preserves_the_existing_destination() {
        let dir = fresh_dir("atomic-publish-failure");
        let destination = dir.join("app");
        fs::write(&destination, b"known-good").unwrap();
        let staged = StagedArtifact::new(&destination, "test").unwrap();
        fs::write(staged.path(), b"new-artifact").unwrap();

        let error = publish_staged_with(staged.path(), &destination, |_staged, _destination| {
            Err(std::io::Error::other("injected late publish failure"))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("late publish failure"));
        assert_eq!(fs::read(&destination).unwrap(), b"known-good");
        drop(staged);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Staging must survive an output directory whose PARENT is a read-only
    /// mount, by falling back to the output directory itself.
    ///
    /// macOS-only because that is where the condition genuinely exists: the
    /// parent of /tmp is /, which the sealed system volume mounts read-only, so
    /// creating the preferred staging container there fails EROFS. Only EACCES
    /// fell through, so `nub compile --out /tmp/app` — an output path people use
    /// constantly — aborted outright.
    ///
    /// A chmod-ed directory does NOT reproduce it and must not be substituted:
    /// that yields EACCES, which the broken code already handled, so the test
    /// passes with the bug present. Verified by reverting the fix and watching
    /// this go red.
    #[cfg(target_os = "macos")]
    #[test]
    fn staging_survives_a_read_only_parent_mount() {
        let destination = Path::new("/tmp").join(format!(
            "nub-staging-rofs-{}-{}",
            std::process::id(),
            line!()
        ));
        let staged = StagedArtifact::new(&destination, "test")
            .expect("staging must fall back to /tmp when / is read-only");
        assert_eq!(
            staged.container().parent(),
            // Lexical, not canonicalised: `absolute_normalized` does not resolve
            // the /tmp -> private/tmp symlink, so the container keeps the path the
            // caller asked for.
            Some(Path::new("/tmp")),
            "expected the fallback to stage inside the output directory"
        );
        drop(staged);
    }

    #[cfg(unix)]
    #[test]
    fn permission_revocation_after_staging_preserves_output_and_cleans_the_stage() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fresh_dir("atomic-publish-permissions");
        let output_dir = dir.join("output");
        fs::create_dir(&output_dir).unwrap();
        let destination = output_dir.join("app");
        fs::write(&destination, b"known-good").unwrap();
        let staged = StagedArtifact::new(&destination, "test").unwrap();
        let staged_path = staged.path().to_path_buf();
        let container = staged.container().to_path_buf();
        assert_eq!(container.parent(), Some(dir.as_path()));
        assert_eq!(
            fs::metadata(&container).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&staged_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&staged_path, b"new-artifact").unwrap();

        fs::set_permissions(&output_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let error = publish_staged_with(&staged_path, &destination, |staged, destination| {
            fs::rename(staged, destination)
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("atomically replacing"));
        assert_eq!(fs::read(&destination).unwrap(), b"known-good");
        drop(staged);
        assert!(
            !staged_path.exists(),
            "the staging ancestor stays writable, so cleanup cannot leak a temp artifact"
        );
        assert!(
            !container.exists(),
            "cleanup must remove the private container too"
        );
        fs::set_permissions(&output_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn parent_permission_fallback_cleans_payload_but_can_leave_an_empty_container() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fresh_dir("atomic-publish-parent-fallback");
        let output_dir = dir.join("output");
        fs::create_dir(&output_dir).unwrap();
        let destination = output_dir.join("app");
        fs::write(&destination, b"known-good").unwrap();

        // A normal user may own `output/` but not its parent. The fallback must
        // still compile, using a private container in `output/`.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let staged = StagedArtifact::new(&destination, "test").unwrap();
        let staged_path = staged.path().to_path_buf();
        let container = staged.container().to_path_buf();
        assert_eq!(container.parent(), Some(output_dir.as_path()));
        fs::write(&staged_path, b"new-artifact").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        // A late output-directory chmod prevents removing the container itself,
        // but its 0700 mode still lets its owner delete the staged payload.
        fs::set_permissions(&output_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let _ = publish_staged_with(&staged_path, &destination, |staged, destination| {
            fs::rename(staged, destination)
        })
        .unwrap_err();
        drop(staged);
        assert!(!staged_path.exists(), "the staged payload must never leak");
        assert!(
            container.is_dir(),
            "only the empty private container can remain"
        );
        assert!(fs::read_dir(&container).unwrap().next().is_none());
        assert_eq!(fs::read(&destination).unwrap(), b"known-good");

        fs::set_permissions(&output_dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir(&container).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn successful_publish_removes_the_private_staging_container() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fresh_dir("atomic-publish-private-container");
        let output_dir = dir.join("output");
        fs::create_dir(&output_dir).unwrap();
        let destination = output_dir.join("app");
        let staged = StagedArtifact::new(&destination, "test").unwrap();
        let container = staged.container().to_path_buf();
        assert_eq!(
            fs::metadata(&container).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(staged.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(staged.path(), b"new-artifact").unwrap();

        staged.publish(&destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new-artifact");
        assert!(
            !container.exists(),
            "publish must remove its private container"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn repeated_windows_publish_atomically_replaces_the_existing_destination() {
        let dir = fresh_dir("atomic-publish-windows-replace");
        let output_dir = dir.join("output");
        fs::create_dir(&output_dir).unwrap();
        let destination = output_dir.join("app.exe");
        fs::write(&destination, b"known-good").unwrap();
        let staged = StagedArtifact::new(&destination, "test").unwrap();
        let container = staged.container().to_path_buf();
        fs::write(staged.path(), b"replacement").unwrap();

        staged.publish(&destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        assert!(!container.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_publish_replaces_an_existing_destination_beyond_max_path() {
        use std::os::windows::ffi::OsStrExt;

        let dir = fresh_dir("atomic-publish-windows-long-path");
        let mut output_dir = dir.join("output");
        for index in 0..12 {
            output_dir.push(format!("segment-{index:02}-0123456789abcdef"));
        }
        fs::create_dir_all(&output_dir).unwrap();
        let destination = output_dir.join("app.exe");
        assert!(destination.as_os_str().encode_wide().count() > 260);
        fs::write(&destination, b"known-good").unwrap();
        let staged = StagedArtifact::new(&destination, "test").unwrap();
        fs::write(staged.path(), b"replacement").unwrap();

        staged.publish(&destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        let relative = windows_verbatim_path(Path::new("future-output.exe")).unwrap();
        assert_eq!(
            &relative[..4],
            &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]
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
    fn node_options_split_into_arguments_in_order() {
        let mut o = opts(None);
        // One string carrying several options is the whole point of the
        // NODE_OPTIONS spelling, and a second use appends rather than replaces.
        o.node_options = vec![
            "--experimental-vm-modules   --max-old-space-size=4096".into(),
            "  --enable-source-maps  ".into(),
        ];
        assert_eq!(
            node_flags(&o).expect("all three are well-formed options"),
            vec![
                "--experimental-vm-modules",
                "--max-old-space-size=4096",
                "--enable-source-maps"
            ],
            "order decides which of two conflicting options Node honours, so it is preserved"
        );
    }

    /// Node's parser, not `split_whitespace`: a quoted run is ONE argument, so a
    /// path containing a space survives being baked.
    #[test]
    fn a_quoted_run_stays_one_argument() {
        assert_eq!(
            split_node_options(r#"--import="/a b/x.mjs" --no-warnings"#).unwrap(),
            vec!["--import=/a b/x.mjs", "--no-warnings"]
        );
        assert_eq!(
            split_node_options(r"--title=one\ two").unwrap(),
            vec!["--title=one two"],
            "a backslash escapes the space rather than ending the argument"
        );
    }

    #[test]
    fn an_unterminated_quote_is_refused() {
        let err = split_node_options("--import=\"/a b/x.mjs").expect_err("the quote never closes");
        assert!(
            err.to_string().contains("unclosed quote"),
            "the error must name the problem: {err}"
        );
    }

    #[test]
    fn a_bare_word_is_refused() {
        let mut o = opts(None);
        // Node reads a bare word as a script path, so this would silently change
        // what the binary runs rather than fail.
        o.node_options = vec!["--experimental-vm-modules 4096".into()];
        let err = node_flags(&o).expect_err("a bare word is not an option");
        let err = err.to_string();
        assert!(
            err.contains("not an option") && err.contains("equals sign"),
            "the error must name the offender and the spelling that works: {err}"
        );
    }

    fn opts(install_message: Option<&str>) -> CompileOptions {
        CompileOptions {
            entry: "main.ts".into(),
            out: None,
            icon: None,
            metadata: Vec::new(),
            smol: false,
            target: None,
            platform: None,
            include: Vec::new(),
            exclude: Vec::new(),
            install_message: install_message.map(str::to_string),
            node_options: Vec::new(),
            define_file: Vec::new(),
            metafile: None,
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
                unbundled: Vec::new(),
                bundled: Vec::new(),
                allow_dynamic_import: false,
                tsconfig: None,
                loaders: Vec::new(),
                native_target: None,
                drop_console: false,
                drop_debugger: false,
                metafile: false,
                target_node: None,
            },
        }
    }

    #[test]
    fn static_payload_verification_requires_license_for_embed_only() {
        let manifest = Manifest {
            shape: Shape::Embed,
            entry: "main.js".into(),
            node_version: "24.10.0".into(),
            provision_version: String::new(),
            smol_exact_target: false,
            smol_version_range: String::new(),
            requires_augmentation: false,
            triple: "darwin-arm64".into(),
            node_sha256: "node".into(),
            node_blake3: String::new(),
            node_size: 0,
            app_compressed: false,
            app_sha256: "app".into(),
            minify: false,
            install_message: None,
            node_flags: Vec::new(),
        };
        let app = vec![AppFile::plain("main.js", b"app".to_vec())];
        let missing = nub_core::compile::encode_with_license(&manifest, &app, b"node", &[]);
        let missing = nub_core::compile::decode(&missing).unwrap();
        assert!(verify_payload_shape(&missing).is_err());

        let present = nub_core::compile::encode_with_license(&manifest, &app, b"node", b"license");
        let present = nub_core::compile::decode(&present).unwrap();
        assert!(verify_payload_shape(&present).is_ok());

        let smol = Manifest {
            shape: Shape::Smol,
            node_sha256: String::new(),
            node_blake3: String::new(),
            node_size: 0,
            app_compressed: false,
            ..manifest
        };
        let smol = nub_core::compile::encode_with_license(&smol, &app, &[], &[]);
        let smol = nub_core::compile::decode(&smol).unwrap();
        assert!(verify_payload_shape(&smol).is_ok());
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

    /// A file holding a bare URL is the same shipped-`ReferenceError` trap as the argv
    /// form, and it is caught here rather than in `defines()` so the advice can name the
    /// FILE. By the time the two flags merge, the source is unrecoverable — and the one
    /// remedy that does not apply to this flag is "retype it as `--define`", since
    /// `--define-file` exists for values that do not fit on a command line.
    #[test]
    fn a_define_file_holding_a_bare_url_is_rejected_and_the_advice_names_the_file() {
        let dir = fresh_dir("definefile-url");
        let f = dir.join("api.txt");
        fs::write(&f, "https://api.example.com\n").unwrap();

        let err = read_define_files(&[format!("API={}", f.display())])
            .expect_err("a file holding an unquoted URL must be rejected at build time");
        let m = format!("{err:#}");
        assert!(
            m.contains("ReferenceError: https is not defined"),
            "the error must name the run-time failure it prevents: {m}"
        );
        assert!(
            m.contains(&f.display().to_string()),
            "the advice must name the file that has to change, not a flag to retype: {m}"
        );
        assert!(
            !m.contains("--define '"),
            "it must NOT tell a --define-file user to switch to --define: {m}"
        );

        // The quoted form is what the error tells the user to write, so it has to work.
        fs::write(&f, "\"https://api.example.com\"\n").unwrap();
        assert_eq!(
            read_define_files(&[format!("API={}", f.display())]).unwrap(),
            vec!["API=\"https://api.example.com\"".to_string()]
        );
    }

    /// `--define-file` must be indistinguishable from typing the same value as
    /// `--define`, so the contents pass through as the JS expression they are —
    /// no quoting, no JSON re-encoding — minus the newline an editor adds.
    #[test]
    fn a_define_file_value_is_the_file_text_without_its_trailing_newline() {
        let dir = fresh_dir("definefile");
        let json = dir.join("models.json");
        fs::write(&json, "{\"a\":1}\n").unwrap();
        assert_eq!(
            read_define_files(&[format!("MODELS={}", json.display())]).unwrap(),
            vec!["MODELS={\"a\":1}".to_string()]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_define_file_names_the_key_and_the_path() {
        let dir = fresh_dir("definefile-bad");
        let msg = |e: anyhow::Error| format!("{e:#}");

        let m = msg(read_define_files(&["MODELS=./nope.json".to_string()]).unwrap_err());
        assert!(
            m.contains("nope.json") && m.contains("MODELS"),
            "a missing file must name both the path and the key it was for: {m}"
        );

        let m = msg(read_define_files(&["JUST_A_KEY".to_string()]).unwrap_err());
        assert!(
            m.contains("KEY=PATH"),
            "a malformed argument must show the expected form: {m}"
        );

        let binary = dir.join("blob.bin");
        fs::write(&binary, [0x7b, 0xff, 0x7d]).unwrap();
        let m = msg(read_define_files(&[format!("BLOB={}", binary.display())]).unwrap_err());
        assert!(
            m.contains("UTF-8") && m.contains("--include"),
            "non-UTF-8 must say why it cannot be a define and point at the flag that ships \
             bytes: {m}"
        );
        let _ = fs::remove_dir_all(&dir);
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

    /// The build end of the same rule the launcher enforces on discovery: a version
    /// can be newer than the floor, satisfy the pin, and still be unable to run the
    /// payload. `--target ">=22.15 <23.5"` is the shape that resolves to one.
    #[test]
    fn a_provision_preference_that_cannot_run_the_shim_is_dropped() {
        let floor = NodeVersion::new(22, 15, 0);
        let in_band = NodeVersion::new(23, 4, 0);
        let above_band = NodeVersion::new(23, 5, 0);
        let same_line = NodeVersion::new(22, 20, 0);

        assert!(
            !provision_preference_is_usable(&in_band, &floor, true),
            "23.4 predates registerHooks on the 23.x line, so provisioning it would \
             hand the launcher a Node it must then refuse"
        );
        // The control that keeps this from being a blanket 23.x ban: without a shim
        // the payload does not need the API, so 23.4 is a perfectly good download.
        assert!(
            provision_preference_is_usable(&in_band, &floor, false),
            "a payload with no shim has no claim on registerHooks"
        );
        assert!(
            provision_preference_is_usable(&above_band, &floor, true),
            "23.5 is where the 23.x line gained registerHooks"
        );
        assert!(
            provision_preference_is_usable(&same_line, &floor, true),
            "a newer version on the floor's own line stays preferred"
        );
        assert!(
            !provision_preference_is_usable(&floor, &floor, true),
            "a preference equal to the floor tells the launcher nothing new"
        );
    }

    #[test]
    fn smol_manifest_preserves_only_explicit_ranges() {
        // Each gate is the range's own semver minimum, which is what a complete
        // index resolves to; the sparse-index case is the next test's subject.
        for (range, gate) in [
            (">=22 <23", NodeVersion::new(22, 0, 0)),
            ("22.x", NodeVersion::new(22, 0, 0)),
            ("^22 || >=24 <25", NodeVersion::new(22, 0, 0)),
        ] {
            let pin = version_management::parse_target_spec(range).unwrap();
            let normalized = smol_version_range(&pin, &gate);
            let reparsed = version_management::parse_target_spec(&normalized).unwrap();
            for version in [
                NodeVersion::new(21, 23, 0),
                NodeVersion::new(22, 0, 0),
                NodeVersion::new(22, 23, 1),
                NodeVersion::new(23, 0, 0),
                NodeVersion::new(24, 14, 0),
                NodeVersion::new(25, 0, 0),
            ] {
                assert_eq!(
                    version.satisfies(&pin),
                    version.satisfies(&reparsed),
                    "normalizing {range:?} as {normalized:?} changed its meaning for {version}"
                );
            }
        }
        for non_range in ["22", "22.15", "22.15.0", "lts"] {
            let pin = version_management::parse_target_spec(non_range).unwrap();
            assert!(smol_version_range(&pin, &NodeVersion::new(22, 0, 0)).is_empty());
        }
    }

    /// The launcher enforces a stored range INSTEAD of the floor, while the bundle
    /// is stripped against the gate — so a stored range must never accept a Node
    /// below the gate. The wildcard half of the fix (that `24.x` RESOLVES its gate
    /// to `24.0.0` rather than to the newest 24) is pinned in nub-core's
    /// `range_floor` tests; resolving a real gate here would need the release index.
    #[test]
    fn smol_stores_a_range_only_when_the_gate_is_the_range_minimum() {
        // spec, the gate a complete index resolves to, a version below it, and a
        // gate a SPARSE index would resolve to instead.
        for (spec, gate, below, raised) in [
            (
                ">=22 <23",
                NodeVersion::new(22, 0, 0),
                NodeVersion::new(21, 9, 9),
                NodeVersion::new(22, 4, 0),
            ),
            (
                "22.x",
                NodeVersion::new(22, 0, 0),
                NodeVersion::new(21, 9, 9),
                NodeVersion::new(22, 4, 0),
            ),
            (
                "24.1.x",
                NodeVersion::new(24, 1, 0),
                NodeVersion::new(24, 0, 9),
                NodeVersion::new(24, 1, 3),
            ),
            (
                "^22 || >=24 <25",
                NodeVersion::new(22, 0, 0),
                NodeVersion::new(21, 9, 9),
                NodeVersion::new(22, 4, 0),
            ),
        ] {
            let pin = version_management::parse_target_spec(spec).unwrap();
            let stored = smol_version_range(&pin, &gate);
            assert!(
                !stored.is_empty(),
                "{spec} resolves its gate to its own minimum, so the runtime must enforce it in full"
            );
            let reparsed = version_management::parse_target_spec(&stored).unwrap();
            assert!(
                gate.satisfies(&reparsed),
                "{spec}: the version the bundle is gated on must itself be accepted"
            );
            assert!(
                !below.satisfies(&reparsed),
                "{spec}: {below} sits below the gate, and its polyfills were stripped at build time"
            );

            // A sparse artifact index resolves the gate ABOVE the range's minimum
            // (the musl case). Enforcing the range then readmits everything between
            // the two, so the range must not be stored at all.
            assert!(
                smol_version_range(&pin, &raised).is_empty(),
                "{spec}: gate {raised} is above the range minimum, so only the gate is enforceable"
            );
        }

        // No representable lower bound, so the gate resolves to the NEWEST matching
        // release. Enforcing the range would accept every Node beneath it.
        for spec in ["<23", "<=24"] {
            let pin = version_management::parse_target_spec(spec).unwrap();
            assert!(
                smol_version_range(&pin, &NodeVersion::new(22, 21, 0)).is_empty(),
                "{spec} has no representable floor, so it must keep floor-only acceptance"
            );
        }
    }

    #[test]
    fn smol_exact_target_preserves_only_original_exact_pins() {
        for literal in ["26.0.0", "v26.0.0"] {
            let pin = version_management::parse_target_spec(literal).unwrap();
            assert!(
                smol_requires_exact_target(&pin),
                "{literal} must preserve exact-target semantics"
            );
        }

        for spec in ["26", "26.0", "latest", ">=26"] {
            let pin = version_management::parse_target_spec(spec).unwrap();
            assert!(
                !smol_requires_exact_target(&pin),
                "{spec} must retain floor semantics"
            );
        }
    }

    #[test]
    fn smol_runtime_policy_states_what_the_launcher_enforces() {
        let exact = version_management::parse_target_spec("26.0.0").unwrap();
        let gate = NodeVersion::new(26, 0, 0);
        assert_eq!(
            smol_runtime_policy(&exact, &gate),
            "required exactly at runtime"
        );

        let bounded = version_management::parse_target_spec(">=22 <23").unwrap();
        assert_eq!(
            smol_runtime_policy(&bounded, &NodeVersion::new(22, 0, 0)),
            "range enforced at runtime"
        );

        // The SAME range, gated above its own minimum, must not claim enforcement
        // the manifest no longer carries — this is the line that would otherwise
        // tell a user their upper bound is live when it was dropped.
        assert_eq!(
            smol_runtime_policy(&bounded, &NodeVersion::new(22, 4, 0)),
            "floor enforced at runtime; upper bounds are not enforced"
        );

        let major = version_management::parse_target_spec("22").unwrap();
        assert_eq!(
            smol_runtime_policy(&major, &NodeVersion::new(22, 0, 0)),
            "floor enforced at runtime"
        );
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
            assets: Vec::new(),
            native_files: Vec::new(),
            support_files: Vec::new(),
            root_support_files: Vec::new(),
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: Vec::new(),
            metafile: None,
        };
        let layout = assets::Layout {
            entry_prefix: String::new(),
            assets: vec![assets::Asset {
                source,
                rel: "a\\..\\..\\escaped".into(),
            }],
        };

        let win = TargetPlatform::parse("win32-x64").unwrap();
        let err = assemble_app(&bundled, &layout, &[], &win).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cannot be embedded"), "{msg}");
        assert!(msg.contains("win32-x64"), "should name the target: {msg}");

        // The same name on a Unix target is a legal single-component filename.
        let linux = TargetPlatform::parse("linux-x64").unwrap();
        assemble_app(&bundled, &layout, &[], &linux)
            .expect("a backslash name is one legal component on a Unix target");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_root_completion_marker_is_reserved_but_nested_names_are_not() {
        let dir = fresh_dir("complete-marker");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let (bundled, reserved) = app_with_included(&dir, ".nub-complete");
        let err = assemble_app(&bundled, &reserved, &[], &target).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(".nub-complete"),
            "must name the reserved path: {msg}"
        );
        assert!(
            msg.contains("completion marker"),
            "must explain why compilation refused it: {msg}"
        );

        let (bundled, nested) = app_with_included(&dir, "dir/.nub-complete");
        assemble_app(&bundled, &nested, &[], &target)
            .expect("only the root completion marker is reserved");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A user manifest is a verbatim asset, not compile-time metadata. Chunks
    /// now name their ESM format directly with `.mjs`, so no entry-adjacent
    /// manifest is needed to alter Node's package-type lookup.
    #[test]
    fn included_commonjs_manifest_is_verbatim_and_no_manifest_is_synthesized() {
        let dir = fresh_dir("included-manifest");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let manifest = b"{\n  \"type\": \"commonjs\",\n  \"private\": true\n}\n";
        let source = dir.join("package.json");
        fs::write(&source, manifest).unwrap();
        let bundled = bundle::BundleResult {
            entry: "main.mjs".into(),
            files: vec![bundle::BundledFile {
                name: "main.mjs".into(),
                bytes: b"export default 1;".to_vec(),
            }],
            detached_maps: Vec::new(),
            assets: Vec::new(),
            native_files: Vec::new(),
            support_files: Vec::new(),
            root_support_files: Vec::new(),
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: Vec::new(),
            metafile: None,
        };
        let layout = assets::Layout {
            entry_prefix: "dist/bun".into(),
            assets: vec![assets::Asset {
                source,
                rel: "package.json".into(),
            }],
        };

        let files = assemble_app(&bundled, &layout, &[], &target)
            .expect("a CommonJS-typed included manifest is ordinary asset data");
        assert_eq!(
            files,
            vec![
                AppFile::plain("dist/bun/main.mjs", b"export default 1;".to_vec()),
                AppFile::plain("package.json", manifest.to_vec()),
            ],
            "the manifest must retain its exact bytes and the compiled chunk its distinct extension"
        );
        assert!(
            !files.iter().any(|f| f.name == "dist/bun/package.json"),
            "compile must not synthesize an entry-adjacent package.json"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_support_files_remain_at_the_payload_root_under_a_nested_entry_layout() {
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let bundled = bundle::BundleResult {
            entry: "main.mjs".into(),
            files: vec![bundle::BundledFile {
                name: "main.mjs".into(),
                bytes: b"export default 1;".to_vec(),
            }],
            detached_maps: Vec::new(),
            assets: Vec::new(),
            native_files: Vec::new(),
            support_files: vec![bundle::BundledFile {
                name: "worker-blob-url.cjs".into(),
                bytes: b"module.exports = {};".to_vec(),
            }],
            root_support_files: vec![bundle::BundledFile {
                name: COMPILE_BOOTSTRAP_NAME.into(),
                bytes: b"require('module');".to_vec(),
            }],
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: Vec::new(),
            metafile: None,
        };
        let files = assemble_app(
            &bundled,
            &assets::Layout {
                entry_prefix: "src/app".into(),
                assets: Vec::new(),
            },
            &[],
            &target,
        )
        .expect("the fixed bootstrap is a legal root payload file");
        assert_eq!(
            files,
            vec![
                AppFile::plain(COMPILE_BOOTSTRAP_NAME, b"require('module');".to_vec()),
                AppFile::plain("src/app/main.mjs", b"export default 1;".to_vec()),
                AppFile::plain(
                    "src/app/worker-blob-url.cjs",
                    b"module.exports = {};".to_vec()
                ),
            ]
        );
    }

    #[test]
    fn root_support_file_collision_names_the_user_renamable_include() {
        let dir = fresh_dir("root-support-collision");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let (mut bundled, layout) = app_with_included(&dir, COMPILE_BOOTSTRAP_NAME);
        bundled.root_support_files.push(bundle::BundledFile {
            name: COMPILE_BOOTSTRAP_NAME.into(),
            bytes: b"bootstrap".to_vec(),
        });

        let err = assemble_app(&bundled, &layout, &[], &target).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(COMPILE_BOOTSTRAP_NAME), "{msg}");
        assert!(
            msg.contains("an embedded asset") && msg.contains("an --included file"),
            "the generated root file and user-renamable include must both be identified: {msg}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_support_file_names_are_validated_for_the_target() {
        let target = TargetPlatform::parse("win32-x64").unwrap();
        let bundled = bundle::BundleResult {
            entry: "main.mjs".into(),
            files: Vec::new(),
            detached_maps: Vec::new(),
            assets: Vec::new(),
            native_files: Vec::new(),
            support_files: Vec::new(),
            root_support_files: vec![bundle::BundledFile {
                name: "a\\..\\escaped.cjs".into(),
                bytes: Vec::new(),
            }],
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: Vec::new(),
            metafile: None,
        };

        let err = assemble_app(
            &bundled,
            &assets::Layout {
                entry_prefix: String::new(),
                assets: Vec::new(),
            },
            &[],
            &target,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("cannot be embedded"));
    }

    #[test]
    fn artifact_payload_carries_worker_bootstrap_and_private_chunk_together() {
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let bundled = bundle::BundleResult {
            entry: "main.mjs".into(),
            files: vec![
                bundle::BundledFile {
                    name: "main.mjs".into(),
                    bytes: b"new Worker(new URL('./worker-a.mjs', import.meta.url));".to_vec(),
                },
                bundle::BundledFile {
                    name: "worker-a-code.mjs".into(),
                    bytes: b"// prelude-bearing worker chunk".to_vec(),
                },
            ],
            detached_maps: Vec::new(),
            assets: Vec::new(),
            native_files: Vec::new(),
            support_files: Vec::new(),
            root_support_files: Vec::new(),
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: vec![bundle::WorkerRoot {
                entry: "worker-a.mjs".into(),
                chunk: "worker-a-code.mjs".into(),
            }],
            metafile: None,
        };
        let wrappers = external::worker_wrappers(&bundled.worker_roots, true, "").unwrap();
        let files = assemble_app(
            &bundled,
            &assets::Layout {
                entry_prefix: String::new(),
                assets: Vec::new(),
            },
            &wrappers,
            &target,
        )
        .expect("the extracted payload contains every worker execution file");
        let names = files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>();
        assert!(names.contains(&"worker-a.mjs") && names.contains(&"worker-a-code.mjs"));
        let wrapper = files
            .iter()
            .find(|f| f.name == "worker-a.mjs")
            .map(|f| String::from_utf8_lossy(&f.bytes))
            .unwrap();
        assert!(wrapper.contains("__nub_external.mjs") && wrapper.contains("worker-a-code.mjs"));
    }

    /// A one-chunk bundle plus one `--include`d file named `rel`, which is the
    /// whole setup both collision tests need.
    fn app_with_included(dir: &Path, rel: &str) -> (bundle::BundleResult, assets::Layout) {
        let source = dir.join("included");
        fs::write(&source, b"asset").unwrap();
        (
            bundle::BundleResult {
                entry: "main.js".into(),
                files: vec![bundle::BundledFile {
                    name: "main.js".into(),
                    bytes: b"export {}".to_vec(),
                }],
                detached_maps: Vec::new(),
                assets: Vec::new(),
                native_files: Vec::new(),
                support_files: Vec::new(),
                root_support_files: Vec::new(),
                dynamic_import_sites: 0,
                native_addons: Vec::new(),
                external_imports: Vec::new(),
                worker_roots: Vec::new(),
                metafile: None,
            },
            assets::Layout {
                entry_prefix: String::new(),
                assets: vec![assets::Asset {
                    source,
                    rel: rel.to_string(),
                }],
            },
        )
    }

    /// A case-only collision is invisible on the build host and destroys the
    /// binary on a machine whose filesystem folds: the asset lands ON the chunk at
    /// extraction and the compiled code is simply gone. Refused where the TARGET
    /// folds — Win32 always, macOS because APFS defaults to case-insensitive —
    /// and allowed on Linux, where the two names really are two files.
    #[test]
    fn a_case_only_collision_is_refused_exactly_where_the_target_folds() {
        let dir = fresh_dir("casefold");
        let (bundled, layout) = app_with_included(&dir, "Main.js");

        for triple in ["win32-x64", "darwin-arm64"] {
            let target = TargetPlatform::parse(triple).unwrap();
            let Err(err) = assemble_app(&bundled, &layout, &[], &target) else {
                panic!("{triple} folds case and must refuse the pair");
            };
            let msg = format!("{err:#}");
            assert!(
                msg.contains("\"Main.js\"") && msg.contains("\"main.js\""),
                "{triple}: both colliding paths must be named: {msg}"
            );
            assert!(
                msg.contains("an --included file") && msg.contains("compiled output"),
                "{triple}: the message must say which side is the asset: {msg}"
            );
            assert!(
                msg.contains(triple),
                "{triple}: the target whose filesystem folds must be named: {msg}"
            );
        }

        let linux = TargetPlatform::parse("linux-x64").unwrap();
        assemble_app(&bundled, &layout, &[], &linux)
            .expect("Main.js and main.js are two distinct files on Linux");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The compile half of the esbuild case: a `--include`d platform binary has to
    /// reach the payload marked executable, or the artifact spawns it with EACCES.
    #[cfg(unix)]
    #[test]
    fn an_included_files_executable_bit_reaches_the_payload() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fresh_dir("include-exec-bit");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let (bundled, layout) = app_with_included(&dir, "bin/helper");
        let source = &layout.assets[0].source;

        fs::set_permissions(source, fs::Permissions::from_mode(0o644)).unwrap();
        let plain = assemble_app(&bundled, &layout, &[], &target).unwrap();
        assert!(
            !plain.iter().any(|f| f.executable),
            "nothing in an ordinary payload is executable"
        );

        fs::set_permissions(source, fs::Permissions::from_mode(0o755)).unwrap();
        let files = assemble_app(&bundled, &layout, &[], &target).unwrap();
        let helper = files.iter().find(|f| f.name == "bin/helper").unwrap();
        assert!(helper.executable);
        assert_eq!(helper.bytes, b"asset", "the bytes are still verbatim");
        assert!(
            files.iter().filter(|f| f.executable).count() == 1,
            "the compiled chunk must not inherit the include's mode"
        );
        assert_ne!(
            sha256_of_app(&plain),
            sha256_of_app(&files),
            "the mode is part of the extraction cache key"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_islands_use_the_same_payload_collision_gate_as_includes() {
        let dir = fresh_dir("native-island-collision");
        let island_name = "__nub_native/deadbeef/node_modules/addon/addon.node";
        let (mut bundled, layout) = app_with_included(&dir, island_name);
        bundled.native_files.push(native_layout::IslandFile {
            name: island_name.into(),
            bytes: b"native".to_vec(),
            executable: false,
        });
        let err = assemble_app(
            &bundled,
            &layout,
            &[],
            &TargetPlatform::parse("linux-x64").unwrap(),
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("would collide"), "{message}");
        assert!(message.contains("--included file"), "{message}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonically_equivalent_unicode_names_collide_only_on_darwin() {
        let dir = fresh_dir("unicode-normalization");
        let (bundled, mut layout) = app_with_included(&dir, "caf\u{e9}.txt");
        let decomposed_source = dir.join("decomposed");
        fs::write(&decomposed_source, b"decomposed").unwrap();
        layout.assets.push(assets::Asset {
            source: decomposed_source,
            rel: "cafe\u{301}.txt".into(),
        });

        let darwin = TargetPlatform::parse("darwin-arm64").unwrap();
        let err = assemble_app(&bundled, &layout, &[], &darwin).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("canonically equivalent Unicode spellings"),
            "the diagnostic must identify normalization, not mislabel it as case: {msg}"
        );

        let linux = TargetPlatform::parse("linux-x64").unwrap();
        assemble_app(&bundled, &layout, &[], &linux)
            .expect("Linux preserves canonically distinct filename bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn darwin_collision_keys_use_full_default_case_folding() {
        assert_eq!(
            collision_key("stra\u{df}e.txt", TargetOs::Darwin),
            collision_key("STRASSE.TXT", TargetOs::Darwin),
            "full folding expands sharp-s to ss"
        );
        assert_eq!(
            collision_key("\u{3bf}\u{3c2}.txt", TargetOs::Darwin),
            collision_key("\u{39f}\u{3a3}.TXT", TargetOs::Darwin),
            "full folding maps Greek final sigma to ordinary sigma"
        );
        assert_ne!(
            collision_key("stra\u{df}e.txt", TargetOs::Win32),
            collision_key("STRASSE.TXT", TargetOs::Win32),
            "the existing Win32 lowercase-only model is intentionally unchanged"
        );
    }

    /// The exact-name case predates the fold-aware one and must survive it: an
    /// `--include` whose name IS a chunk's replaces compiled code on every target.
    #[test]
    fn an_include_that_shadows_a_chunk_is_refused_on_every_target() {
        let dir = fresh_dir("exactdup");
        let (bundled, layout) = app_with_included(&dir, "main.js");
        for triple in ["linux-x64", "win32-x64", "darwin-arm64"] {
            let target = TargetPlatform::parse(triple).unwrap();
            let Err(err) = assemble_app(&bundled, &layout, &[], &target) else {
                panic!("{triple} must refuse an exact duplicate");
            };
            // Both sides share the name here, so a name-keyed lookup would label
            // them identically and lose the only fact that makes the message
            // actionable — which of the two the user can rename.
            let msg = format!("{err:#}");
            assert!(
                msg.contains("compiled output") && msg.contains("an --included file"),
                "{triple}: each side must be named by where it came from: {msg}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
