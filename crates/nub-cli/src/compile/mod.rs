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
mod icu;
mod inject;
mod inline;
mod launcher;
mod loaders;
mod metafile;
mod native;
mod native_layout;
mod sea;
mod unbundlable;
mod version_info;

pub use bundle::{BundleOptions, SourcemapMode};
pub use icu::parse_locales as parse_icu_locales;

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
    /// `--icu=<locales>`: the languages to keep in the embedded Node's ICU data.
    /// `None` — no flag, or a bare `--icu`, or `--icu=full` — keeps all ~700, which
    /// is the only setting that formats identically to the user's own `node`.
    /// Ignored under `--smol`, which embeds no Node to trim.
    pub icu: Option<Vec<String>>,
    /// `--icon`: a Windows `.ico` to show on the executable. Windows-only because
    /// it is the only target whose format carries the icon in the file itself —
    /// macOS reads one from the surrounding `.app` bundle and Linux from a
    /// `.desktop` entry, and neither is part of a single-file artifact.
    pub icon: Option<PathBuf>,
    /// `--metadata KEY=VALUE`: Windows version-resource fields, overriding what
    /// the nearest `package.json` supplies. Windows-only for the same reason as
    /// [`CompileOptions::icon`] — no other container format carries them.
    pub metadata: Vec<String>,
    /// `--hide-console`: give the Windows executable the GUI subsystem, and tell
    /// its launcher to spawn Node with no console. Windows-only for the same
    /// reason as [`CompileOptions::icon`] — the subsystem is a PE header field,
    /// and neither Mach-O nor ELF has anything corresponding to it.
    pub hide_console: bool,
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
    // Started before anything that can touch the network or the disk, so the
    // closing success line reports the wait the user actually sat through rather
    // than the part of it this function happens to bracket.
    let started = std::time::Instant::now();
    let target = resolve_platform(opts.platform.as_deref())?;

    // A typo'd entry costs one stat, so it is checked before anything that can
    // touch the network — a foreign target's launcher fetch would otherwise
    // download a template only to report the entry does not exist.
    let entry_path = Path::new(&opts.entry);
    if !entry_path.is_file() {
        bail!("entry file not found: {}", opts.entry);
    }

    // Refused rather than ignored, matching `--icon` and `--metadata`: `--smol`
    // embeds no Node, so there is no ICU data to trim and the artifact would run on
    // whatever the host supplies. Accepting the flag would report a saving the
    // build never made.
    if opts.smol && opts.icu.is_some() {
        bail!(
            "--icu trims the ICU data out of the EMBEDDED Node, and --smol embeds none.\n\
             \x20\x20A --smol artifact uses whichever Node it finds or provisions at startup."
        );
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
    reject_non_windows_hide_console(opts.hide_console, &target)?;

    // Printed here — after the cheap rejections, before the first step that can
    // take a visible amount of time. Everything above fails in the first second,
    // so an intro above them would announce a build that never started.
    eprintln!(
        "{}",
        intro_line(
            &opts.entry,
            &out_path,
            crate::cli::color_enabled(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        )
    );

    // 1. Resolve `--include`/`--exclude` BEFORE bundling: a typo'd include is a
    //    sub-second failure, and paying for a full bundle first would hide that
    //    behind the slowest step in the pipeline.
    let entry_dir = entry_abs.parent().unwrap_or(Path::new("."));
    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let layout = assets::plan(entry_dir, &cwd, &opts.include, &opts.exclude)?;
    // A bundled CommonJS module's `__dirname` has to land where the extraction dir
    // actually puts that module's directory, which only the layout knows.
    //
    // Both paths are `canonicalize`d, which on Windows yields the VERBATIM
    // `\\?\C:\...` spelling while rolldown's module ids carry the ordinary one.
    // `Path::strip_prefix` compares prefixes by variant and `VerbatimDisk != Disk`,
    // so leaving them verbatim makes every offset silently `None` on a Windows
    // build host. `canonicalize_for_bundler` exists for the same mismatch.
    let mut mirror = bundle::ModuleMirror {
        anchor: bundle::strip_verbatim_prefix(layout.anchor.clone(), cfg!(windows)),
        entry_dir: bundle::strip_verbatim_prefix(entry_dir.to_path_buf(), cfg!(windows)),
        materialized: Default::default(),
    };
    // Bundle output lands under the entry prefix, and each asset creates its own
    // parents; nothing else in the payload makes a directory.
    mirror.materialize(&layout.entry_prefix);
    for asset in &layout.assets {
        mirror.materialize(asset.rel.rsplit_once('/').map_or("", |(dir, _)| dir));
    }
    opts.bundle.module_mirror = mirror;

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
    opts.bundle.eager_startup =
        bundle::eager_startup_compilation_supported(opts.bundle.target_node);

    // Resolved AND verified before any real work: a cross-compile whose launcher
    // template is missing, or is not that platform's executable, must fail in the
    // first second — not after downloading and recompressing a ~100 MB Node for
    // the target. For a foreign target this may fetch the template from this
    // release, a few hundred KB, still the cheapest step to fail on.
    //
    // A single-executable application opens no template at all — it IS a Node —
    // so this asks the same question the container decision below asks, minus the
    // payload-shape half that is not known yet. Being conservative here only costs
    // a lookup nothing reads; being eager would fail a perfectly good SEA build on
    // a platform whose launcher this release never published, and would fetch a
    // template over the network to do it.
    let template = if opts.smol || !sea::supports_blob_exec_argv(&gate_version) {
        Some(launcher::locate(&target)?)
    } else {
        None
    };

    // 2. Bundle (Rolldown, in-process). The target's platform/arch are baked in
    //    as defines UNDER the user's, so a cross-compiled `process.platform`
    //    branch dead-code-eliminates for the machine the artifact will run on,
    //    not the one it was built on.
    opts.bundle.auto_define = target_defines(&target);
    // Native addons are embedded for the TARGET, and those same defines are what
    // make resolution pick the target's platform package rather than the host's.
    opts.bundle.native_target = Some(target);
    // Held for the rest of `run`. Its `Drop` clears the line on every exit, the
    // error paths included, so nothing can leave a spinner frozen above a report.
    let live = LiveLine::start();
    live.phase("bundling", &opts.entry);
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
    // The payload names those wrappers land under, which is the only spelling a
    // `new Worker(...)` inside the artifact can name. `assemble_app` applies the
    // same `bundle_path`, so this reads the layout rather than predicting it.
    let worker_entries: Vec<String> = worker_wrappers
        .iter()
        .map(|(name, _)| layout.bundle_path(name))
        .collect();
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
    }
    // Computed here rather than beside the manifest because the no-extract decision
    // below needs it: only the VERBATIM payload sets can carry a file Node parses
    // that the bundler did not — emitted asset copies, native-island contents, and
    // `--include`s. Their PRESENCE is the predicate, not their extensions, since the
    // CommonJS loader parses an exact-path `require()` of any unknown or absent
    // extension with its `.js` handler, so no name-based allowlist can prove a
    // shipped file is not runtime JavaScript.
    let carries_verbatim_files =
        bundled.assets.len() + bundled.native_files.len() + layout.assets.len() > 0;
    let sealed_module_graph = !shim_plan.needed() && !carries_verbatim_files;

    // Can this payload run WITHOUT being written to disk? `sealed_module_graph` is
    // necessary and not sufficient: a statically traced worker chunk and a
    // `--sourcemap=linked` map are both files the bundler itself parsed, so the
    // graph is sealed with either present, and neither can be reached from a
    // `data:` URL. See `compile::inline`.
    let no_extract_inputs = inline::Inputs {
        sealed_module_graph,
        worker_roots: bundled.worker_roots.len(),
        worker_wrappers: worker_wrappers.len(),
        // The mode, not whether one was asked for. The single-executable container
        // carries an inline or a detached map; the no-extract launcher carries
        // neither, because it rewrites its chunks after the map is made. Only
        // `linked` is out of reach for both.
        sourcemap: opts.bundle.sourcemap,
        embeds_node: !opts.smol,
        computes_module_specifier: bundled.app_computes_module_specifier,
        entry: &entry_name,
    };
    // WHICH CONTAINER: a Node single-executable application, or nub's launcher
    // with a payload appended to it?
    //
    // The SEA is the default for an embedding build, and it is the same payload in
    // a different container — the bundle, the bootstrap, the flags and the virtual
    // root are all shared, and only the delivery differs. It wins because it drops
    // the two terms the launcher shape cannot: the launcher process itself, and
    // unpacking ~110 MB of Node to the cache on first run. Measured on Linux
    // against plain `node` running the same source, 300-run minimums with a 1.49 ms
    // baseline spread: the launcher artifact +7.20 ms, the single-executable one
    // +0.53 — at parity with plain Node, and 1.2 ms from a SEA carrying no nub code
    // at all.
    //
    // It is declined in exactly two situations. A Node too old to read `execArgv`
    // out of the blob cannot be handed nub's flags at all
    // (`sea::supports_blob_exec_argv`), and `--smol` embeds no Node — a SEA IS a
    // Node, so a shape whose whole point is not carrying one cannot be one. Beyond
    // those, a payload that needs real filesystem paths (`--external`, a surviving
    // computed `import()`, `--include`d files, a native addon, a traced worker
    // chunk, a linked source map) still extracts, and the eligibility pass that
    // decides is the one the inline shape already uses.
    let sea_capable = !opts.smol && sea::supports_blob_exec_argv(&gate_version);
    let sea_decline = if sea_capable {
        inline::classify(&app_files, &no_extract_inputs, inline::Mode::Sea)?.err()
    } else {
        None
    };
    let use_sea = sea_capable && sea_decline.is_none();

    // The lookup above was skipped for anything that COULD be a SEA, and a decline
    // is how that guess turns out wrong: the payload needs real paths after all, so
    // the launcher is back and its template has to be fetched now. Still ahead of
    // the Node download, which is the step worth failing before.
    let template = match template {
        _ if use_sea => Vec::new(),
        Some(template) => template,
        None => launcher::locate(&target)?,
    };

    // A SEA takes the chunks VERBATIM, so it skips this rewrite entirely: its
    // loader serves them from `module.registerHooks` at real `file:` URLs, where
    // `import.meta.url` and every relative specifier already mean what they mean in
    // the extracted tree. See `compile::sea::payload`.
    let (app_files, inline_app, app_delivery) = if use_sea {
        (app_files, false, AppDelivery::Sea)
    } else {
        match inline::rewrite(app_files, &no_extract_inputs)? {
            inline::Rewritten::Inline(files) => (files, true, AppDelivery::Inline),
            inline::Rewritten::Extract(files, why) => {
                // An embedding build reports why it is not a SEA, which is a
                // different question from why it is not inline and has a better
                // answer. `Decline::EmbeddedNode` — "it extracts its embedded Node
                // anyway" — is the inline answer, and it is both uninformative and
                // now premise-free: an embedding artifact is a SEA by default and
                // extracts nothing. The version gate has no inline counterpart at
                // all, so without this the artifact silently loses the SEA shape
                // and the build says something true about a different question.
                let why = if opts.smol {
                    why.reason()
                } else if let Some(sea_decline) = sea_decline {
                    sea_decline.reason()
                } else {
                    "its Node predates single-executable flag support"
                };
                (files, false, AppDelivery::Extracted(why))
            }
        }
    };
    let app_sha = sha256_of_app(&app_files);
    if !layout.assets.is_empty() {
        live.phase("embedding", &format!("{} files", layout.assets.len()));
    }

    // What did NOT get sealed into the bundle, collected for the closing block.
    //
    // These were three separate sentences printed at three points in the build —
    // `External (must be installed where the binary runs): …`, `Native addons: …`,
    // and a `Dynamic import: …` line. They are three shapes for one question, and
    // a reader asks it once, about the finished artifact, not spread across a
    // scrolling build log. Worth saying at all because a binary carrying machine
    // code for one platform, or depending on something installed elsewhere, is
    // otherwise indistinguishable from one that is fully self-contained.
    let mut shipped: Vec<(String, &'static str)> = Vec::new();
    for addon in &bundled.native_addons {
        shipped.push((addon.clone(), "native addon"));
    }
    for package in &opts.bundle.external {
        shipped.push((package.clone(), "--external"));
    }

    // 3. Resolve the Node version through nub run's SAME pin chain (so compile
    //    can't drift from run); --target overrides it. The pin context is the
    //    entry's project dir (walk up from there).
    live.phase("runtime", "");
    let (node_version, provision_version, node, runtime_summary) = if opts.smol {
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
        let summary = RuntimeSummary {
            fact: format!(
                "Node {}, not embedded",
                non_exact_spec(&pin, &raw).unwrap_or_else(|| floor.to_string())
            ),
            provenance: format!(
                "{source}; {}{}",
                smol_runtime_policy(&pin, &floor),
                newest
                    .as_ref()
                    .map(|n| format!(", provisioning {n}"))
                    .unwrap_or_default()
            ),
        };
        (floor, newest, EmbeddedNode::default(), summary)
    } else {
        // Embed bakes ONE exact version — a range/major/alias collapses to the
        // newest satisfying release at compile time. (`build_node_blob` →
        // provisioning prints the `Using Node.js … (resolved from …)` line, the
        // same surface nub run uses.)
        let (os, arch, musl) = dist_platform(&target);
        let exact =
            version_management::resolve_pin_for_platform(&pin, os, arch, musl, &cache_root)?;
        external::check_node_support(&exact, &source, &shim_plan)?;
        let node = build_node_blob(
            &exact,
            &target,
            &cache_root,
            &source,
            opts.icu.as_deref(),
            if use_sea {
                NodeDelivery::Verbatim
            } else {
                NodeDelivery::Compressed
            },
        )?;
        let summary = RuntimeSummary {
            fact: format!("Node {exact}, embedded"),
            provenance: source.to_string(),
        };
        (exact, None, node, summary)
    };

    // Compress AFTER `sha256_of_app` above: that hash is the extraction cache key
    // and must stay over the semantic content, not over whatever this zstd version
    // happens to emit. Per file rather than per region because `nub_core::compile`
    // is a pure container that does no compression of its own — the same split the
    // Node blob already uses, and it lets the launcher decode only the files it
    // actually extracts. Level 19 matches the Node blob; the app region is small
    // enough that the time is not noticeable next to the ~107 MB one.
    // An inline payload takes BROTLI instead, because the code that decompresses it
    // is the artifact's own JavaScript: `zlib.zstdDecompressSync` is missing on Node
    // 23.5 and 23.6 while brotli is on every supported version. Nothing in Rust ever
    // decompresses these bytes, which is why the manifest's `app_compressed` — the
    // launcher's per-file zstd flag — stays false for them.
    let app_files: Vec<_> = if use_sea {
        // Raw. A SEA's assets are mapped from the executable and handed to Node as
        // the `ArrayBuffer` `getRawAsset` returns; compressing them would force a
        // JavaScript decode into a string on every start, which is the copy the
        // loader exists to avoid. They sit beside a ~110 MB Node either way.
        app_files
            .into_iter()
            .map(|file| nub_core::compile::AppFile {
                plain_size: Some(file.bytes.len() as u64),
                name: file.name,
                bytes: file.bytes,
                executable: file.executable,
            })
            .collect()
    } else {
        app_files
            .into_iter()
            .map(|file| {
                // The length the LAUNCHER will find on disk, so it is read before
                // anything below can compress or move these bytes.
                let plain_size = file.bytes.len() as u64;
                let bytes = if inline_app {
                    // The bootstrap alone is stored VERBATIM. The launcher reads it out
                    // of the payload to build the `-e` argument, and it carries no
                    // decompressor for this codec by design — nub-launcher is a
                    // deliberately minimal binary. Storing ~13 KB raw is what buys that.
                    if file.name == nub_core::compile::COMPILE_BOOTSTRAP_NAME {
                        file.bytes
                    } else {
                        brotli_encode(&file.bytes)
                            .with_context(|| format!("brotli-compressing {}", file.name))?
                    }
                } else {
                    zstd::encode_all(&file.bytes[..], 19)
                        .with_context(|| format!("zstd-compressing {}", file.name))?
                };
                Ok::<_, anyhow::Error>(nub_core::compile::AppFile {
                    name: file.name,
                    plain_size: Some(plain_size),
                    bytes,
                    executable: file.executable,
                })
            })
            .collect::<Result<_>>()?
    };

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
        node_icu: node.icu,
        app_compressed: !inline_app,
        app_sha256: app_sha,
        minify: opts.bundle.minify,
        install_message: Some(install_message(&opts)),
        node_flags: node_flags(&opts)?,
        // Sealed only when Node can parse no module the bundler did not. Two
        // channels can hand runtime Node a file it never saw: the shim plan
        // (`--external` packages, surviving computed `import()`) and ANY
        // verbatim payload file — reachable through the real `createRequire`
        // every CJS chunk carries, and parsed as JS by the CJS loader whatever
        // its extension. Generated chunks and wrappers are exempt by
        // construction: the bundler parsed them. Computed require of a path
        // OUTSIDE the artifact stays out of the predicate on the
        // plain-Node-baseline argument in the Manifest field's doc comment.
        sealed_module_graph,
        // The launcher's half of `--hide-console`. The PE subsystem flip below
        // only stops Windows giving the LAUNCHER a console; this is what stops it
        // giving one to the Node it spawns.
        hide_console: opts.hide_console,
        inline_app,
        // An inline payload runs the bootstrap as `-e` and has no preload to save.
        standalone_preamble: !inline_app
            && bundled.bootstrap_optional
            && supports_standalone_preamble(opts.bundle.target_node),
    };
    // A SEA carries no nub payload at all: the manifest above describes the
    // launcher's container, and the launcher is not in this artifact. Everything
    // the manifest would have told the launcher is either already true in a SEA
    // (the entry, the exact Node) or is baked into the blob (the flags).
    let payload = if use_sea {
        Vec::new()
    } else {
        encode_with_license(&manifest, &app_files, &node.blob, &node.license)
    };

    // 5. Build and verify a staged artifact before replacing the requested
    // destination. A late signing/permission/static/native-probe failure must
    // never truncate a previously good executable at `--out`.
    live.phase("linking", &target.triple());
    let staged_maps = stage_detached_maps(&bundled, &out_path)?;
    let staged = StagedArtifact::new(&out_path, "artifact")?;
    let sea_blob_len = if use_sea {
        let blob = sea::build_blob(&sea::Inputs {
            app_files: &app_files,
            entry: &manifest.entry,
            workers: &worker_entries,
            app_sha: &manifest.app_sha256,
            node_license: &node.license,
            node_version: &node_version,
            node_flags: &manifest.node_flags,
        })?;
        sea::inject(
            &target,
            &node.blob,
            &blob,
            icon.as_deref(),
            version_info.as_deref(),
            opts.hide_console,
            staged.path(),
        )
        .with_context(|| format!("writing {}", staged.path().display()))?;
        blob.len() as u64
    } else {
        inject::inject(
            &target,
            &template,
            &payload,
            icon.as_deref(),
            version_info.as_deref(),
            opts.hide_console,
            staged.path(),
        )
        .with_context(|| format!("writing {}", staged.path().display()))?;
        0
    };
    set_executable(staged.path())?;
    sync_file(staged.path())?;
    live.phase("verifying", "");
    if use_sea {
        // The entry's own length, which the self-probe compares against what the
        // artifact's `sea.getRawAsset` hands back. `build_blob` has already
        // refused a payload whose entry is not among the emitted chunks, so the
        // lookup cannot miss — and a zero would simply fail the probe, which is
        // the safe way for an impossible case to land.
        let entry_len = app_files
            .iter()
            .find(|file| file.name == manifest.entry)
            .map_or(0, |file| file.bytes.len());
        sea::verify_artifact(
            staged.path(),
            &manifest.entry,
            entry_len,
            &target,
            version_info.as_deref(),
            opts.hide_console,
        )?;
    } else {
        verify_artifact(
            staged.path(),
            &target,
            version_info.as_deref(),
            opts.hide_console,
        )?;
    }
    staged.publish(&out_path)?;

    // Detached maps are optional debugging companions rather than part of the
    // executable's atomic replacement. They were staged before the binary became
    // visible, then each is atomically published afterwards. A map publish failure
    // warns (the verified executable remains usable) and removes its temp file.
    publish_detached_maps(staged_maps);

    // Cleared HERE, explicitly, rather than left to the end of the scope. `Drop`
    // would run after the block below has already printed, so the spinner would
    // still be repainting over the rows it is supposed to hand off to.
    drop(live);

    let size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    let facts = BuildFacts {
        size,
        // The COMPRESSED contribution of each region, which is what occupies the
        // bytes being split. `node.size` is the DECOMPRESSED length — the
        // launcher's warm-start check — so using it here would report a Node
        // component four times the space it takes in the file.
        node_bytes: (node.blob.len() + node.license.len()) as u64,
        // For a SEA the app's contribution is the whole blob — the assets plus the
        // generated main that serves them, which is the payload's real cost.
        app_bytes: if use_sea {
            sea_blob_len
        } else {
            app_files.iter().map(|f| f.bytes.len() as u64).sum()
        },
        // Injection re-signs the whole image, so the template's own ad-hoc
        // signature never reaches the artifact and must come off its size.
        // Zero for a SEA, whose container is the Node binary itself rather than a
        // launcher template — the `node` part above already accounts for it.
        launcher_bytes: if use_sea {
            0
        } else {
            (template.len() as u64)
                .saturating_sub(inject::code_signature_size(target.format(), &template))
        },
        // Measured off the published file rather than predicted: the signature's
        // size is a function of the final image, which nothing before the write
        // knows. A failure to read it back is not worth failing a verified build
        // over — the component simply reports zero and goes unnamed.
        signature_bytes: inject::code_signature_size_of(target.format(), &out_path).unwrap_or(0),
        shipped,
        deferred: bundled.dynamic_import_sites,
        app_delivery,
        report: opts.metafile.clone(),
        elapsed: started.elapsed(),
    };
    report_resolved_build(&out_path, &facts, &runtime_summary, &target);
    Ok(0)
}

/// The runtime row, split where its two tiers are.
///
/// `fact` is what was resolved; `provenance` is where the version came from and,
/// for `--smol`, what the launcher will enforce at run time. Kept apart rather
/// than joined with an em dash because the row is drawn in two weights — the
/// provenance is why this row exists, but not what a reader checks first.
struct RuntimeSummary {
    fact: String,
    provenance: String,
}

/// The build's resolved configuration, as a labelled block.
///
/// It replaces a single comma-separated line, and the reason is not only that
/// five unlabelled facts in a row are hard to read. The Node version's
/// PROVENANCE was missing entirely on the default path: a `--smol` build said
/// where its pin came from and an embed build never did, because the embed
/// path's only mention rode the provisioning line, which prints when a Node is
/// downloaded and stays silent when one is cached. Where the runtime came from
/// is the fact a reader most often wants, so it is now stated on every build.
///
/// Four fixed rows and three that appear only when they have something to say.
/// The optional ones — `shipped`, `deferred`, `report` — used to be standalone
/// sentences printed at three different points in the build, and moving them here
/// is the point rather than a side effect: `External (must be installed where the
/// binary runs): …`, `Native addons: …` and `Dynamic import: … site(s)` were three
/// sentence shapes for one question, which a reader asks once, at the end, about
/// the artifact in front of them. The bundler's `Shipping N packages unbundled`
/// list stays where it is, because it carries the REASON each package earned, and
/// a row here would trade that reason for a second, worse copy.
///
/// The word `shape` is gone from user-facing output with it. It is nub's internal
/// name for the embed/`--smol` split; a reader knows the flag, not the noun.
fn report_resolved_build(
    out_path: &Path,
    facts: &BuildFacts,
    runtime_summary: &RuntimeSummary,
    target: &TargetPlatform,
) {
    let color = crate::cli::color_enabled(std::io::IsTerminal::is_terminal(&std::io::stderr()));
    let rows = resolved_build_rows(out_path, facts, runtime_summary, target);
    // Sized from the widest label PRESENT rather than from a constant: the
    // optional rows mean the set differs between builds, so a fixed width would
    // either strand a gap on a narrow block or collide on a wide one. That is what
    // `install_report.rs::render_block` does, for the same reason.
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let cols = stderr_cols();
    eprintln!();
    for (label, spans) in rows {
        for line in render_row(label, &spans, width, cols, color) {
            eprintln!("{line}");
        }
    }
    eprintln!();
    eprintln!("{}", success_line(out_path, facts.elapsed, color));
}

/// The line that opens a compile, and the one thing here that survives in the
/// scrollback from before the build.
///
/// The live line cannot do this job: it is `ProgressJobDoneBehavior::Hide`, so
/// the only pre-block line a compile printed erased itself, and a build that had
/// scrolled past left no record of what was even being compiled. It is also off
/// entirely when stderr is not a terminal, which is exactly the run — a CI log —
/// where the record matters most. So this is a plain `eprintln!` rather than a
/// kept final frame: it prints identically in both modes, and the spinner keeps
/// being purely transient.
///
/// `entry → out` and nothing else. The platform and the size are on the block
/// eight lines below, and a cross-compile carries its target in the default
/// output name anyway; an intro repeating the block is a second, worse copy of
/// it. What the block cannot say is what the reader is waiting FOR, because it
/// prints when the waiting is over.
fn intro_line(entry: &str, out_path: &Path, color: bool) -> String {
    format!(
        "{} {} compiling {entry} {} {}",
        banner(color),
        paint("·", Ink::Muted, color),
        paint("→", Ink::Muted, color),
        paint(&out_path.display().to_string(), Ink::Accent, color),
    )
}

/// The line that closes a successful compile.
///
/// A build that worked used to end on its own last fact — `elapsed  3.5s`, or
/// whichever optional row happened to sort last — so nothing in the output said
/// the thing had actually succeeded. This says it, in the vocabulary the rest of
/// the CLI already spends on exactly that: a green bold check mark, then the
/// noun, then a dim duration. It is the same line `nub install` signs off with
/// (`✓ installed 2 packages in 1.2s`), which is what makes it legible without
/// being read.
///
/// It carries the elapsed time, and the `elapsed` block row was removed when it
/// did. One fact stated twice, three lines apart, is worse than either placement
/// on its own — and the duration belongs with the success cue, where it answers
/// "that worked, and it took this long" as one sentence.
///
/// No banner, unlike the install summary. That summary is frequently the ONLY
/// persistent line an install prints, so it has to identify who is speaking; a
/// compile always prints [`intro_line`] first, and stamping the version twice
/// into a seven-line output is noise.
fn success_line(out_path: &Path, elapsed: std::time::Duration, color: bool) -> String {
    format!(
        "{} compiled {} in {}",
        paint_success(color),
        paint(&out_path.display().to_string(), Ink::Accent, color),
        paint(&format_elapsed(elapsed), Ink::Muted, color),
    )
}

/// The product banner: magenta-bold `nub`, then the dim version.
///
/// One definition, because it is drawn on two surfaces — the live line's header
/// and [`intro_line`] — and the two drifting apart would be invisible until
/// someone put them side by side. Byte-compatible with the banner the engine
/// prints, which is what makes an install and a compile look like one CLI.
fn banner(color: bool) -> String {
    let version = env!("CARGO_PKG_VERSION");
    if !color {
        return format!("nub {version}");
    }
    format!("\x1b[35m\x1b[1mnub\x1b[22m\x1b[39m \x1b[2m{version}\x1b[22m")
}

/// The green check mark, in the one place a build says it worked.
///
/// Not an [`Ink`], deliberately: `Ink` is the block's three-tier vocabulary and
/// adding a fourth for a glyph one line uses would put a color in it that the
/// block itself never draws.
fn paint_success(color: bool) -> String {
    if color {
        "\x1b[32m\x1b[1m✓\x1b[22m\x1b[39m".to_string()
    } else {
        "✓".to_string()
    }
}

/// An elapsed build, in the three bands the engine's install summary uses
/// (`aube::progress::ci::format_duration`): sub-second `240ms`, sub-minute
/// `4.0s`, otherwise `3m12s`.
///
/// Reimplemented rather than called because that function is private to the
/// engine. The bands matter here more than they do for an install: a `--target`
/// that has to download and recompress a ~100 MB Node routinely runs past a
/// minute, and the flat `{:.1}s` this replaces rendered that as `92.4s`.
fn format_elapsed(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let total = d.as_secs();
        format!("{}m{:02}s", total / 60, total % 60)
    }
}

/// Assumed terminal width when stderr cannot be measured — a pipe, a log file, a
/// CI runner. Matches `install_report.rs`.
const FALLBACK_COLS: usize = 80;

/// A string's width in TERMINAL CELLS, which is what a wrap decision needs.
///
/// `chars().count()` counts Unicode scalar values, and the two disagree on
/// exactly the text a user controls: a path, a package name or a `--platform`
/// value carrying CJK or emoji is two cells per scalar, so counting scalars
/// wraps a full column late and the line the tier just indented soft-wraps at
/// column zero anyway. `console` is already a dependency here — it is what
/// measures the terminal — and it strips SGR while measuring, so this stays
/// correct if it is ever handed painted text.
fn cells(text: &str) -> usize {
    console::measure_text_width(text)
}

/// The width everything this module prints wraps to. Read once per surface
/// rather than threaded down, and named so the block and the diagnostics
/// demonstrably wrap to the same number.
fn stderr_cols() -> usize {
    console::Term::stderr()
        .size_checked()
        .map_or(FALLBACK_COLS, |(_, cols)| cols as usize)
}

/// One row: the label right-aligned in its gutter, then each span in its own ink.
///
/// Split out so a test can assert on the line this actually prints. The gutter is
/// computed from the VISIBLE label and the escapes added afterwards — pad an
/// already-painted string instead and its escape bytes count toward the field
/// width, so every styled label silently loses the column the block exists to
/// give it, and only once color is on.
///
/// The label is RIGHT-aligned, which is the one thing that makes this block read
/// differently from the one `install_report.rs` prints. Both put every value in
/// the same column; the choice is only which side the slack falls on. Left-aligned
/// it lands inside the row, so a short label sits further from its own value than
/// a long one does. Right-aligned it lands in the margin, on dim text, and no
/// label is ever separated from the thing it labels.
///
/// That holds only while the labels stay close in length: the ragged margin is
/// exactly as wide as the spread between the shortest and longest, so a label far
/// longer than the rest turns the margin into something that reads as broken
/// indentation. Keep them short and near-uniform — the current spread is two
/// columns, `output` to `platform`.
/// Wraps at the terminal's width, with every continuation line hung at the value
/// column. Without that a long row — the `--smol` runtime row is 112 columns on a
/// real build — wraps wherever the terminal happens to break it, and the
/// remainder restarts at column 0. That destroys the aligned value column the
/// block exists for, and it does it on the widest, most informative row.
fn render_row(
    label: &str,
    spans: &[(String, Ink)],
    width: usize,
    cols: usize,
    color: bool,
) -> Vec<String> {
    let value_col = INDENT + width + GAP;
    // A pathologically narrow terminal still has to make progress rather than
    // emit one word per line forever.
    let limit = cols.max(value_col + 20);

    let mut lines = Vec::new();
    let mut line = format!(
        "{}{}{}{}",
        " ".repeat(INDENT),
        " ".repeat(width.saturating_sub(label.len())),
        paint(label, Ink::Muted, color),
        " ".repeat(GAP),
    );
    let mut col = value_col;
    let mut at_line_start = true;

    // Wrapping happens word by word, but PAINTING happens per run of one ink.
    // `run` is the text accumulated since the ink last changed or the line last
    // broke; it is flushed through `paint` once. Painting each word as it lands
    // renders identically and is what shipped first — but it wraps every word in
    // its own escape pair, so `(node 28.2 MB · app 57 KB · launcher 851 KB)`
    // leaves the terminal as eleven separate dim spans and roughly ten times the
    // bytes, which is what anyone piping the output to a file or a doc gets.
    let mut run = String::new();
    let mut run_ink = Ink::Plain;
    macro_rules! flush {
        () => {
            if !run.is_empty() {
                line.push_str(&paint(&run, run_ink, color));
                run.clear();
            }
        };
    }

    for (text, ink) in spans {
        if *ink != run_ink {
            flush!();
            run_ink = *ink;
        }
        for (lead, word) in words(text) {
            // The separator belongs to whichever line the word lands on, so a
            // wrapped word sheds it and no continuation opens with stray spaces.
            let needed = if at_line_start { 0 } else { lead } + cells(word);
            if !at_line_start && col + needed > limit {
                flush!();
                lines.push(std::mem::take(&mut line));
                line = " ".repeat(value_col);
                col = value_col;
                at_line_start = true;
            }
            if !at_line_start {
                run.push_str(&" ".repeat(lead));
                col += lead;
            }
            run.push_str(word);
            col += cells(word);
            at_line_start = false;
        }
    }
    flush!();
    lines.push(line);
    lines
}

/// Split into `(leading spaces, word)` pairs, so the two-space gap that separates
/// a fact from its aside survives on the line it lands on and is dropped on one
/// it wraps to. Splitting on whitespace and rejoining with single spaces instead
/// would silently close that gap, which is the block's only separator now that no
/// dash is.
fn words(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let lead = rest.len() - rest.trim_start_matches(' ').len();
        rest = &rest[lead..];
        if rest.is_empty() {
            break;
        }
        let end = rest.find(' ').unwrap_or(rest.len());
        out.push((lead, &rest[..end]));
        rest = &rest[end..];
    }
    out
}

/// The block's left margin, and the space between the label column and the value
/// column. Both match `install_report.rs`, so nub's two labelled blocks sit on the
/// same grid even though they align their labels to opposite edges.
const INDENT: usize = 2;
const GAP: usize = 2;

/// How one segment of the block is drawn.
///
/// Three tiers and no more, because the block only has three jobs: point at the
/// artifact, state the facts, and say where they came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ink {
    /// The facts themselves, at the terminal's own default weight.
    Plain,
    /// Secondary detail — a label, a size, a provenance, a parenthetical aside.
    /// Present when wanted, out of the way when not.
    Muted,
    /// The one token the reader acts on next: the path they are about to run.
    Accent,
}

/// Apply an [`Ink`], or return the text untouched when color is off.
///
/// Bright cyan is deliberate rather than free choice: it is what `nub run`
/// already spends on a script name, so an artifact path drawn in it reuses a
/// color the CLI has taught instead of introducing a second identifier color.
fn paint(text: &str, ink: Ink, color: bool) -> String {
    match ink {
        _ if !color => text.to_string(),
        Ink::Plain => text.to_string(),
        Ink::Muted => format!("\x1b[2m{text}\x1b[22m"),
        Ink::Accent => format!("\x1b[96m{text}\x1b[39m"),
    }
}

/// The rows themselves, split from the printing so they can be asserted on
/// without a terminal — including their styling, which is the half most likely
/// to be wrong in a way no plain-text assertion would catch.
fn resolved_build_rows(
    out_path: &Path,
    facts: &BuildFacts,
    runtime_summary: &RuntimeSummary,
    target: &TargetPlatform,
) -> Vec<(&'static str, Vec<(String, Ink)>)> {
    let mut rows = vec![
        (
            "output",
            vec![
                (out_path.display().to_string(), Ink::Accent),
                (format!("  {}", mb(facts.size)), Ink::Plain),
                (facts.size_split(), Ink::Muted),
            ],
        ),
        (
            "runtime",
            vec![
                (runtime_summary.fact.clone(), Ink::Plain),
                (format!("  ({})", runtime_summary.provenance), Ink::Muted),
            ],
        ),
        (
            // `platform`, not `target`. `--target` selects the NODE VERSION and
            // `--platform` the os-arch pair, so a row labelled `target` showing a
            // triple names one flag while answering for the other — and the Node
            // version `--target` really does set is on the row above. The internal
            // binding is `let target = resolve_platform(opts.platform)`, which is
            // where the confusion came from.
            "platform",
            if target.is_host() {
                // No aside. Building for the host is the overwhelming majority of
                // builds, so a note saying so is a note on the default: it is paid
                // for on every build and tells the reader nothing they did not
                // already assume. The cross-compile is the surprising case, and it
                // is the one that gets the words.
                vec![(target.triple().to_string(), Ink::Plain)]
            } else {
                vec![
                    (target.triple().to_string(), Ink::Plain),
                    (
                        match TargetPlatform::host() {
                            Some(host) => format!("  cross-compiled from {}", host.triple()),
                            None => "  cross-compiled".to_string(),
                        },
                        Ink::Muted,
                    ),
                ]
            },
        ),
    ];

    // The three optional rows. Each replaces a standalone sentence printed earlier
    // in the build, and the reason to move them is that they were three sentence
    // shapes for one question — what is NOT sealed inside this binary — which a
    // reader asks once, at the end, about the artifact in front of them.
    if !facts.shipped.is_empty() {
        let mut spans = Vec::new();
        for (i, (name, why)) in facts.shipped.iter().enumerate() {
            if i > 0 {
                spans.push((", ".to_string(), Ink::Plain));
            }
            spans.push((name.clone(), Ink::Plain));
            spans.push((format!(" ({why})"), Ink::Muted));
        }
        rows.push(("shipped", spans));
    }
    if facts.deferred > 0 {
        rows.push((
            "deferred",
            vec![
                (
                    format!(
                        "{} dynamic import site{}",
                        facts.deferred,
                        if facts.deferred == 1 { "" } else { "s" }
                    ),
                    Ink::Plain,
                ),
                ("  resolved where the binary runs".to_string(), Ink::Muted),
            ],
        ));
    }
    // Always shown, because whether the binary needs a writable directory is the
    // question a self-contained artifact exists to answer — and the reason is what
    // makes the answer actionable when it is the wrong one.
    rows.push((
        "app",
        match facts.app_delivery {
            AppDelivery::Inline => vec![
                ("run from the executable".to_string(), Ink::Plain),
                ("  nothing is written to disk".to_string(), Ink::Muted),
            ],
            // Deliberately not "nothing is written to disk": a single-executable
            // artifact unpacks nothing, but it still writes Node's compile cache,
            // exactly as `node app.js` does. Claiming otherwise would be the one
            // sentence in this block a reader could act on and be wrong about.
            AppDelivery::Sea => vec![
                ("run from the executable".to_string(), Ink::Plain),
                ("  a single-executable application".to_string(), Ink::Muted),
            ],
            AppDelivery::Extracted(why) => vec![
                ("extracted on first run".to_string(), Ink::Plain),
                (format!("  {why}"), Ink::Muted),
            ],
        },
    ));

    if let Some(report) = &facts.report {
        rows.push((
            "report",
            vec![
                (report.display().to_string(), Ink::Plain),
                ("  esbuild schema".to_string(), Ink::Muted),
            ],
        ));
    }

    // No `elapsed` row: the closing success line carries the duration, and
    // stating it in both places three lines apart reads as two measurements.
    rows
}

/// Bytes as the size a reader compares against a disk quota — decimal, the unit
/// convention every other size in this output and on the docs page already uses.
///
/// Under a megabyte it says kilobytes instead, because a fixed `{:.1} MB` prints
/// `0.0 MB` for anything below 50 KB — and the app region of a small binary is
/// exactly that. A component of a real artifact reported as `0.0 MB` reads as
/// "nothing is there", which is both wrong and the opposite of what the split
/// exists to tell someone trying to shrink their binary. Measured on a two-line
/// program: `app 0.0 MB`, beside a 24 KB region.
fn mb(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{:.0} KB", bytes as f64 / 1_000.0)
    }
}

// ---- the live line -------------------------------------------------------------

/// The one status line a compile shows while it runs.
///
/// It is deliberately the SAME surface `nub install` draws — the magenta `nub`
/// token, the dim version, clx's `mini_dot` braille spinner, a cyan phase verb —
/// because a user who has watched an install already knows how to read it. The
/// spinner is clx's own `{{ spinner() }}`, whose default set is `mini_dot`: the
/// exact frames aube spins during an install and the compiled binary's launcher
/// spins on first run, so all three animate identically without sharing code.
///
/// Progress erases itself. Every fact worth keeping is restated by the closing
/// block, so a scrollback full of phase lines would be a second, worse copy of
/// it — and the phases a compile passes through are not something a reader needs
/// after the build is over.
///
/// Off entirely when stderr is not a terminal or color is off. clx's `Text` mode
/// would append a line per update instead, which is the scrollback this exists to
/// avoid; a redirected build gets the notes and the closing block, which is all a
/// log needs.
struct LiveLine(Option<std::sync::Arc<clx::progress::ProgressJob>>);

/// The phase column's width, so the payload after it does not jitter as the verb
/// changes. Sized to the longest verb below.
const PHASE_WIDTH: usize = 11;

/// The active line, reachable from anywhere in the compile path.
///
/// A module-level slot rather than a `&LiveLine` threaded through every
/// signature, because the places that need to print during a build are the
/// deepest ones — the Node stripper, the launcher fetch, the bundler's own
/// warnings — and none of them is otherwise given anything about how this command
/// reports. Threading a reporter down to `prepare_node_bytes` to let it say one
/// sentence buys nothing but churn in the signatures in between. aube reaches for
/// the same shape and for the same reason (`progress::safe_eprintln`).
///
/// Only `run` writes it, once, and only through [`LiveLine::start`].
static LIVE: std::sync::Mutex<Option<std::sync::Arc<clx::progress::ProgressJob>>> =
    std::sync::Mutex::new(None);

/// Move the live line to a new phase, from anywhere in the compile path.
///
/// A free function for the same reason [`note`] is one: the code that knows a
/// step has started is often deep — the Node stripper, the re-signer — and
/// threading a reporter down through every signature in between to let one of
/// them change a word buys nothing.
///
/// This is where a step that used to print a standalone sentence goes.
/// `Signing embedded Node.js` was one: it is not a fact about the artifact and
/// not something a reader can act on, so it does not belong in the scrollback
/// the closing block is handed — but it IS the slowest part of an embed build,
/// so saying nothing while it runs is worse. A phase says it and then takes it
/// back, which is what a phase is for.
///
/// With no live line this is a no-op, deliberately: the redirected build already
/// gets the warnings and the closing block, which is everything a log needs.
pub(crate) fn phase(verb: &str, detail: &str) {
    if let Ok(guard) = LIVE.lock()
        && let Some(job) = guard.as_ref()
    {
        job.prop("phase", &LiveLine::phase_field(verb));
        job.prop("detail", &detail.to_string());
    }
}

/// Print a line without tearing an animated status line.
///
/// A bare `eprintln!` while a job is repainting interleaves with it and leaves
/// the line's remains in the scrollback, so every note the compile path emits has
/// to come through here. With no live line — redirected output, `NO_COLOR`, a
/// call from a test — it is exactly `eprintln!`.
pub(crate) fn note(line: &str) {
    match LIVE.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(job) => job.println(line),
            None => eprintln!("{line}"),
        },
        // A poisoned lock means another thread panicked while holding it. The
        // message still matters more than the tearing does.
        Err(_) => eprintln!("{line}"),
    }
}

/// How loud a diagnostic is.
///
/// Separate from [`Ink`], which is the closing block's vocabulary and
/// deliberately has three tiers and no more. A diagnostic answers a different
/// question — how much of the reader's attention this deserves — so it gets its
/// own two-value answer rather than a fourth `Ink` that only one surface uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    /// Worth acting on; the build still produced an artifact.
    Warn,
    /// The build produced nothing.
    Error,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Tier::Warn => "warn",
            Tier::Error => "error",
        }
    }

    /// Yellow and red, which are the two colors nub already spends on exactly
    /// these meanings: yellow on an install's `latest X` advisory, red on the
    /// engine's `ERR_NUB_*` line. Neither is a new color.
    fn sgr(self) -> &'static str {
        match self {
            Tier::Warn => "\x1b[33m",
            Tier::Error => "\x1b[31m",
        }
    }
}

/// A warning, drawn in the two weights it has.
///
/// Everything the compile path wants to say used to be one weight and one
/// prefix — a `note:` in front of a paragraph — so a warning a reader had to act
/// on looked exactly like one they did not. This splits it: a yellow `warn` label
/// and the headline at full weight, then the explanation dimmed underneath.
///
/// The reason to dim the body rather than drop it is that these explanations are
/// the useful part. The data-asset warning has to say what a data asset cannot
/// do; the dropped-edge warning has to say what will fail at run time and where.
/// Dimming keeps them skimmable for a reader who already knows, without making
/// the one who does not go looking.
///
/// A plain [`note`] is still the right call for something a reader cannot act on
/// — that a cross-compiled payload was verified statically is a fact about the
/// build, not a problem with it.
pub(crate) fn warn(headline: &str, body: &[&str]) {
    let color = crate::cli::color_enabled(std::io::IsTerminal::is_terminal(&std::io::stderr()));
    for line in diagnostic_lines(Tier::Warn, headline, body, stderr_cols(), color) {
        note(&line);
    }
}

/// Report a failed build, and hand back the exit code it should carry.
///
/// `main` returns a `Result`, so an error escaping [`run`] reaches Rust's
/// `Termination` and prints an unstyled `Error: …` — the same framing a panic
/// gets, and indistinguishable at a glance from a warning that let the build
/// finish. An error is a diagnostic one step louder than a warning, so it is
/// drawn as one rather than left to the runtime.
///
/// Scoped to `nub compile` deliberately. nub's error surface is already split —
/// the PM engine prints a red `ERR_NUB_*` line while everything reaching
/// `Termination` prints a plain `Error:` — and closing that gap is a change to
/// every command's output rather than this one's to make.
pub(crate) fn report_error(err: &anyhow::Error) -> i32 {
    let color = crate::cli::color_enabled(std::io::IsTerminal::is_terminal(&std::io::stderr()));
    for line in error_lines(err, stderr_cols(), color) {
        note(&line);
    }
    1
}

/// Decompose an error into the lines [`report_error`] prints.
///
/// Split out for the same reason [`render_row`] is: the decomposition is the
/// part with judgment in it, and a test that rebuilt it would not be testing
/// what ships.
///
/// The rule that matters: **everything after the first line is the author's, and
/// is reproduced byte for byte.** A compile error is not a sentence, it is a
/// formatted block — a `file:line`, the offending source nested one level under
/// it, a blank line, then a paragraph hard-wrapped by whoever wrote it. Both
/// obvious treatments destroy it, and both were tried: re-indenting to hang
/// under the headline flattens the nesting, so the location and the source at it
/// become two unrelated lines; re-wrapping breaks each authored line one word
/// early, because the hanging indent pushes a 74-column line two columns past
/// the terminal. The tier's job is to label the error, not to lay it out.
fn error_lines(err: &anyhow::Error, cols: usize, color: bool) -> Vec<String> {
    let rendered = err.to_string();
    let mut source = rendered.lines();
    let headline = source.next().unwrap_or_default();

    // The headline is one sentence with no structure of its own, and the tier
    // put a label in front of it, so this is the part the tier owns and wraps.
    let mut lines = headline_lines(Tier::Error, headline, cols, color);
    lines.extend(source.map(|line| {
        // An empty line stays empty rather than becoming a pair of escapes.
        if line.is_empty() {
            String::new()
        } else {
            paint(line, Ink::Muted, color)
        }
    }));

    // A cause is the tier's own composition — anyhow gives it as a bare string
    // with no formatting around it — so it does hang under the headline.
    //
    // Per PHYSICAL line, though, not once per cause. A cause is frequently
    // multiline itself: `inject::inject` builds `setting the executable icon:
    // …\n  The container parsed, so one of the images inside it did not. …`,
    // and `run` wraps that with `writing <staged path>`, so the icon message
    // arrives here as a chained cause carrying its own newlines. Prefixing the
    // whole string once indents its first line and leaves every later one back
    // at the column it was authored in — the same defect the message body had,
    // one level down. The line's own relative indent is kept on top of the
    // hanging one, so the cause's internal structure survives.
    let indent = Tier::Error.label().len() + GAP;
    let pad = " ".repeat(indent);
    lines.extend(err.chain().skip(1).flat_map(|cause| {
        cause
            .to_string()
            .lines()
            .map(|line| {
                if line.is_empty() {
                    String::new()
                } else {
                    format!("{pad}{}", paint(line, Ink::Muted, color))
                }
            })
            .collect::<Vec<_>>()
    }));
    lines
}

/// The label and its headline: the one part both tiers compose themselves, so
/// the label is painted in exactly one place.
fn headline_lines(tier: Tier, headline: &str, cols: usize, color: bool) -> Vec<String> {
    let label = tier.label();
    let painted = if color {
        format!("{}\x1b[1m{label}\x1b[22m\x1b[39m", tier.sgr())
    } else {
        label.to_string()
    };
    let indent = label.len() + GAP;
    wrapped(headline, indent, cols)
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            // The label sits on the first line only; the rest align under it.
            let lead = if i == 0 {
                format!("{painted}{}", " ".repeat(GAP))
            } else {
                " ".repeat(indent)
            };
            format!("{lead}{chunk}")
        })
        .collect()
}

/// The lines a diagnostic prints, split from the printing so a test can read
/// them.
///
/// Shared by both tiers so they cannot drift into two different shapes — the
/// point of a tier is that the reader learns one layout and reads severity off
/// the label.
///
/// Same reason [`render_row`] is split out: the body's hanging indent has to be
/// computed from the label's VISIBLE width, and a test that rebuilt the
/// arithmetic itself would stay green while production indented by the escape
/// bytes instead.
fn diagnostic_lines(
    tier: Tier,
    headline: &str,
    body: &[&str],
    cols: usize,
    color: bool,
) -> Vec<String> {
    // Everything hangs under the headline rather than under the margin, so an
    // explanation reads as belonging to the thing it explains — and so does a
    // headline long enough to wrap, which is not rare: `unknown --platform …`
    // enumerates all eight supported triples and runs to 155 columns.
    let indent = tier.label().len() + GAP;
    let mut lines = headline_lines(tier, headline, cols, color);
    // A `warn` body is written AT the call site FOR this indent — short
    // fragments, no nesting — so unlike an error's, it is the tier's to lay out.
    for line in body {
        for chunk in wrapped(line, indent, cols) {
            lines.push(format!(
                "{}{}",
                " ".repeat(indent),
                paint(&chunk, Ink::Muted, color)
            ));
        }
    }
    lines
}

/// Break `text` into chunks that each fit in `cols` once `indent` is added.
///
/// Painted AFTER this runs, never before — wrapping a string that already
/// carries escapes counts those bytes as columns, which is the same trap
/// [`render_row`]'s label gutter has.
fn wrapped(text: &str, indent: usize, cols: usize) -> Vec<String> {
    // A pathologically narrow terminal still has to make progress rather than
    // emit one word per line forever.
    let limit = cols.max(indent + 20);
    let mut out = Vec::new();
    let mut line = String::new();
    let mut col = indent;
    for (lead, word) in words(text) {
        let needed = if line.is_empty() { 0 } else { lead } + cells(word);
        if !line.is_empty() && col + needed > limit {
            out.push(std::mem::take(&mut line));
            col = indent;
        }
        if !line.is_empty() {
            line.push_str(&" ".repeat(lead));
            col += lead;
        }
        line.push_str(word);
        col += cells(word);
    }
    out.push(line);
    out
}

impl LiveLine {
    fn start() -> Self {
        if !crate::cli::color_enabled(std::io::IsTerminal::is_terminal(&std::io::stderr())) {
            return Self(None);
        }
        // clx redraws every 200 ms by default, which reads as a stutter rather
        // than a spinner. 80 ms is ora's cadence. The setting is process-global,
        // and `nub compile` owns the only progress job in this process.
        clx::progress::set_interval(std::time::Duration::from_millis(80));
        let job = clx::progress::ProgressJobBuilder::new()
            .body("{{header}}  {{ spinner() }} {{phase}}{{detail}}")
            .prop("header", &banner(true))
            .prop("phase", &Self::phase_field("bundling"))
            .prop("detail", &String::new())
            // The line is transient by construction: at teardown the job flips to
            // `Done`, which under `Hide` renders empty, so there is no frame left
            // behind for the closing block to be printed underneath.
            .on_done(clx::progress::ProgressJobDoneBehavior::Hide)
            .start();
        if let Ok(mut slot) = LIVE.lock() {
            *slot = Some(job.clone());
        }
        Self(Some(job))
    }

    /// Move to a phase, with an optional detail after it.
    fn phase(&self, verb: &str, detail: &str) {
        if self.0.is_some() {
            phase(verb, detail);
        }
    }

    /// Pad on the PLAIN verb, so the trailing spaces land outside the color span
    /// and the field's visible width is what it claims. Padding the styled string
    /// counts escape bytes as columns and collapses the field — the same trap the
    /// block's label gutter has, and the reason both compute width before paint.
    fn phase_field(verb: &str) -> String {
        format!(
            "\x1b[36m{verb}\x1b[39m{}",
            " ".repeat(PHASE_WIDTH.saturating_sub(verb.len()))
        )
    }
}

impl Drop for LiveLine {
    /// Clears the line however the build ended, an error path included: a bail
    /// out between two phases would otherwise leave a spinner frozen mid-frame
    /// above the error report.
    fn drop(&mut self) {
        if let Some(job) = &self.0 {
            job.set_status(clx::progress::ProgressStatus::Done);
        }
        // Cleared before the block prints, and before any later command in the
        // same process could reach `note` and push a line at a job that is gone.
        if let Ok(mut slot) = LIVE.lock() {
            *slot = None;
        }
    }
}

/// What the build produced, gathered where it is known rather than recomputed.
///
/// Every field is already in hand at the point the artifact is finished. Deriving
/// one again at report time — re-reading the blob, re-stat'ing a payload — is how
/// a report starts describing something other than the file that was written.
/// How a finished artifact hands its app to Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppDelivery {
    /// Written to the cache on first run, for the reason given. A borrowed
    /// reason rather than a [`inline::Decline`] because the answer comes from
    /// whichever no-extract shape was actually refused — the single-executable
    /// one for an embedding build, the inline one for `--smol` — and from the
    /// Node version gate, which is neither.
    Extracted(&'static str),
    /// Served from the executable's own bytes as `data:` URLs, writing nothing.
    Inline,
    /// Served out of a Node single-executable blob.
    Sea,
}

struct BuildFacts {
    /// The finished file, from `metadata` on what was published.
    size: u64,
    /// The embedded runtime's contribution: the COMPRESSED Node blob plus the
    /// compressed `LICENSE` shipped beside it, which exists only because that
    /// runtime does. Zero under `--smol`, which embeds no runtime at all.
    node_bytes: u64,
    /// The compressed app files' contribution.
    app_bytes: u64,
    /// The launcher template's contribution, which is its file size MINUS its own
    /// ad-hoc signature: injection re-signs the whole image, so the template's
    /// signature is discarded rather than carried.
    launcher_bytes: u64,
    /// The finished artifact's ad-hoc code signature, measured off the file that
    /// was written. Zero on ELF and PE, which nub never signs.
    signature_bytes: u64,
    /// Everything that did not get sealed into the bundle, with the reason each
    /// one earned. Ordered as the build discovered them.
    shipped: Vec<(String, &'static str)>,
    /// Surviving computed `import()` sites, from `--allow-dynamic-import`.
    deferred: usize,
    /// How the app reaches Node at run time.
    app_delivery: AppDelivery,
    /// Where `--metafile` wrote the build report, if it was asked for.
    report: Option<PathBuf>,
    elapsed: std::time::Duration,
}

impl BuildFacts {
    /// Where the megabytes went, as the aside on the `output` row.
    ///
    /// This is the number someone shrinking a binary actually wants; the total on
    /// its own cannot tell them whether their code or the runtime is the problem.
    /// Every part named here is MEASURED, so they deliberately do NOT sum to the
    /// total. What is left over is the payload container's header, the manifest,
    /// the per-file framing, and the alignment of the payload region to a 64 KiB
    /// boundary — a few tens of kilobytes nobody can act on. Naming it took the
    /// row past 100 columns on a real build, which wraps on an 80-column terminal,
    /// so it is left out and the parts that answer the question stay on one line.
    ///
    /// `launcher` used to BE the remainder — `size - node - app` — so it swallowed
    /// the ad-hoc code signature as well. That is not a rounding error: the
    /// CodeDirectory carries one SHA-256 per 4 KiB page, so on a 29 MB embed build
    /// a fixed 851 KB launcher was reported as 1.2 MB, and the row said the
    /// launcher alone outweighed a whole `--smol` binary.
    fn size_split(&self) -> String {
        let mut parts = Vec::new();
        if self.node_bytes > 0 {
            parts.push(format!("node {}", mb(self.node_bytes)));
        }
        parts.push(format!("app {}", mb(self.app_bytes)));
        // Zero for a single-executable artifact, which has no launcher at all —
        // the container is the Node binary already named on the `node` part.
        if self.launcher_bytes > 0 {
            parts.push(format!("launcher {}", mb(self.launcher_bytes)));
        }
        if self.signature_bytes > 0 {
            parts.push(format!("signature {}", mb(self.signature_bytes)));
        }
        format!("  ({})", parts.join(" · "))
    }
}

/// Write the `--metafile` report, pretty-printed because it is read by people at
/// least as often as by a tool.
/// The path is NOT announced here. It is one of the build's outputs, so it
/// belongs beside the others in the closing block rather than ten lines above
/// it, in the middle of the progress the block summarises.
fn write_metafile(path: &Path, report: &metafile::Metafile) -> Result<()> {
    let json = serde_json::to_string_pretty(report).context("serializing the build report")?;
    fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
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

/// Refuse `--hide-console` for a target whose format has no subsystem field.
///
/// Refused rather than ignored, matching `--icon` and `--metadata`: the whole
/// point of the flag is that nothing is shown, so accepting it on a target that
/// cannot honor it would be indistinguishable from it working right up until
/// someone ran the binary.
///
/// The HOST is not checked, only the target. Everything the flag does is byte
/// editing plus one payload field, so a hidden Windows binary cross-compiles
/// from macOS or Linux exactly like an icon does.
/// Whether every Node this artifact accepts has `process.getBuiltinModule`, which
/// is how a standalone preamble reaches builtins without the bootstrap's early CJS
/// `require` (`runtime/compile-record.mjs`). Landed at 22.3.0 with a 20.16 backport;
/// the 20.x band is left out because a `--smol` floor there admits 21.x, which
/// never had it, and the bootstrap preload is only ~1 ms.
fn supports_standalone_preamble(target: Option<(u64, u64, u64)>) -> bool {
    matches!(target, Some((major, minor, _)) if major > 22 || (major == 22 && minor >= 3))
}

fn reject_non_windows_hide_console(hide_console: bool, target: &TargetPlatform) -> Result<()> {
    if hide_console && target.format() != ContainerFormat::Pe {
        bail!(
            "--hide-console applies to Windows executables, and this build targets {}.\n\
             \x20\x20A console window is a Windows concept: macOS and Linux start a program \
             from a terminal that already exists, and neither Mach-O nor ELF carries anything \
             that would suppress one.",
            target.triple()
        );
    }
    Ok(())
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
            plain_size: Some(f.bytes.len() as u64),
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

    // A ROOT MANIFEST, SO A WALK-UP STOPS INSIDE THE APP.
    //
    // The `getRoot` idiom — climb from `__dirname` until a directory holds
    // `package.json` or `node_modules`, throw at the filesystem root — is how
    // `bindings` and a long tail of packages find their own installed root. A pure
    // bundle's extraction dir held neither, so the climb walked straight out of it:
    // with the cache under `$HOME` it returned the user's home directory, silently
    // and at exit 0, and with the cache elsewhere it threw. Verified on macOS and
    // Linux against two independently built binaries, and `--include package.json`
    // was already the accidental cure — which is the evidence that the absent
    // manifest is the whole cause.
    //
    // NO `"type"` FIELD, deliberately. The chunks carry `.mjs`/`.cjs` and settle
    // their own format, but a bare `.js` `--include`d at the root is loaded by
    // Node's module-syntax detection, and detection only runs while no nearer
    // manifest names a type. A synthesized `"type": "commonjs"` would break exactly
    // that file in a `type: module` project; omitting the field changes nothing.
    // (A user who `--include`s their real `package.json` gets theirs — this only
    // fills a gap, and never overwrites.)
    // Keyed the way the collision gate keys, not by bytes: on darwin and win32
    // `Package.json` and `package.json` are the SAME file, so a byte compare would
    // miss an included one, synthesize a second, and fail a build that used to
    // succeed -- with a message telling the user to rename a file they did not
    // duplicate.
    let manifest_key = collision_key("package.json", target.os);
    if !files
        .iter()
        .any(|f| collision_key(&f.name, target.os) == manifest_key)
    {
        files.push(AppFile::plain(
            "package.json".to_string(),
            b"{\"private\":true}\n".to_vec(),
        ));
        origins.push(Origin::Generated);
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
            Ok(()) => note(&format!("Wrote {}", destination.display())),
            Err(error) => warn(
                &format!(
                    "the detached source map {} was not written",
                    destination.display()
                ),
                &[
                    "The compiled executable itself is complete and usable.",
                    &format!("{error:#}"),
                ],
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
/// How the target's Node travels inside the artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeDelivery {
    /// zstd-19 into the launcher's payload, decompressed to the cache on first
    /// run. The Node is a passenger; the artifact is nub's launcher.
    Compressed,
    /// Verbatim, because the artifact IS this binary — the single-executable
    /// shape writes its blob into it rather than carrying it.
    Verbatim,
}

#[derive(Default)]
struct EmbeddedNode {
    /// The Node bytes as they go into the artifact: zstd-19 compressed under
    /// [`NodeDelivery::Compressed`], the prepared image itself under
    /// [`NodeDelivery::Verbatim`].
    blob: Vec<u8>,
    /// SHA-256 of the DECOMPRESSED bytes — the extraction cache key.
    sha256: String,
    /// BLAKE3 of the same bytes, retained as manifest format headroom.
    blake3: String,
    /// Length of the same bytes — the launcher's warm-start check.
    size: u64,
    /// The Node LICENSE, compressed or plain to match `blob`.
    license: Vec<u8>,
    /// The locales `--icu` kept, comma-joined, or empty for an untrimmed Node.
    /// Reaches the manifest, where it gates the launcher's official-Node dedup.
    icu: String,
}

fn build_node_blob(
    version: &NodeVersion,
    target: &TargetPlatform,
    cache_root: &Path,
    resolved_from: &str,
    icu: Option<&[String]>,
    delivery: NodeDelivery,
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

    let bytes = prepare_node_bytes(&node_bin, target, icu)?;
    let sha = crate::cli::sha256_hex(&bytes);
    // Retained as manifest format headroom; `sha` stays the cache key. Both are
    // over the same decompressed bytes.
    let b3 = blake3::hash(&bytes).to_hex().to_string();
    // Compressing a ~113 MB Node at zstd-19 takes ~20 s, and it was paid on every
    // single compile even though the input never changes for a given Node and
    // target. Keyed by the hash of the bytes being compressed, so a stale entry
    // is not expressible: different bytes are a different key.
    // A single-executable artifact IS this binary, so there is nothing to
    // compress and nothing to decompress at start — which is also what removes
    // the ~20 s zstd-19 pass on a cache miss below.
    if delivery == NodeDelivery::Verbatim {
        return Ok(EmbeddedNode {
            size: bytes.len() as u64,
            blob: bytes,
            sha256: sha,
            blake3: b3,
            license,
            icu: icu.map(|l| l.join(",")).unwrap_or_default(),
        });
    }
    let cached = cache_root
        .join("compile-node-blob")
        .join(format!("{sha}.zst"));
    let blob = match std::fs::read(&cached) {
        Ok(blob) => blob,
        Err(_) => {
            // Only on a cache MISS, which is the slow path — ~20 s for a
            // ~113 MB Node. Worth a line of its own rather than a phase alone,
            // because it names WHY this build is slower than the last one.
            note(&format!(
                "Compressing Node ({:.0} MB) with zstd-19 …",
                bytes.len() as f64 / 1_000_000.0
            ));
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
        icu: icu.map(|l| l.join(",")).unwrap_or_default(),
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
fn prepare_node_bytes(
    node_bin: &Path,
    target: &TargetPlatform,
    icu: Option<&[String]>,
) -> Result<Vec<u8>> {
    let original = fs::read(node_bin).with_context(|| format!("reading {}", node_bin.display()))?;
    let format = target.format();

    // Every fallback below ships the ORIGINAL Node, which is correct but larger.
    // That trade is fine for a strip nobody asked for and wrong for an explicit
    // `--icu`: silently shipping ~700 locales when the caller asked for two is the
    // build lying about what it produced. So a requested trim turns each fallback
    // into an error instead.
    let needs_resign = format == ContainerFormat::MachO;
    if needs_resign && which_first(&["codesign"]).is_none() {
        if icu.is_some() {
            bail!(
                "--icu needs codesign on PATH: trimming rewrites the Node binary, and macOS will \
                 not launch one whose signature no longer matches"
            );
        }
        warn(
            "no codesign on PATH, so the embedded Node is not stripped",
            &["A stripped macOS Node could not be re-signed, and an unsigned one cannot launch."],
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
    // Optional, because an ICU trim is worth staging for on its own: a cross-format
    // target needs llvm-strip specifically, so a Mac compiling for Windows routinely
    // has no usable stripper and must still be able to honour `--icu`.
    let strip = which_first(candidates);
    if strip.is_none() && icu.is_none() {
        warn(
            &format!(
                "no {} on PATH, so the embedded Node is not stripped",
                candidates.join("/")
            ),
            &["The artifact is larger than it needs to be; installing the tool shrinks it."],
        );
        return Ok(original);
    }

    // The trim lands on the STAGED copy, never on `original`, so every fallback path
    // below still has pristine bytes to return.
    let staged = match icu {
        Some(locales) => {
            let mut bytes = original.clone();
            let report = icu::trim(&mut bytes, locales)?;
            // Resource counts, not the bytes freed. The rewrite vacates ~19 MB of a
            // ~107 MB binary, but most of that is zeros by the time zstd sees it, so
            // reporting the raw figure would promise an artifact four times smaller
            // than the one this build is about to produce. The `output` row below
            // states the size that actually ships.
            note(&format!(
                "Trimmed ICU to {} ({} of {} locale resources kept)",
                locales.join(", "),
                report.kept,
                report.total
            ));
            bytes
        }
        None => original.clone(),
    };

    let tmp = std::env::temp_dir().join(format!("nub-compile-node-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, &staged).with_context(|| format!("staging Node at {}", tmp.display()))?;
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

    let mut ok = match &strip {
        Some(strip) => {
            let mut argv: Vec<&std::ffi::OsStr> =
                strip_flags(format).iter().map(AsRef::as_ref).collect();
            argv.push(tmp.as_os_str());
            run_ok(strip, &argv)
        }
        // Nothing to strip with, but a trim still has to be signed and verified.
        None => true,
    };
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
        // Announced BEFORE the call, not after it. Signing a ~107 MB binary takes
        // real time, and a progress line printed once it finished left that whole
        // stretch labelled by the previous phase.
        phase("signing", "embedded Node");
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
    } else if icu.is_some() && target.is_host() && !node_formats_dates(&tmp) {
        // `node_runs` is a `--version` check, and a broken ICU package passes it:
        // a package whose per-tree indexes no longer resolve boots fine and then
        // FATAL-aborts inside the first `Intl.DateTimeFormat`. So a trim earns a
        // probe that actually reaches ICU.
        Some("the trimmed Node could not format a date through Intl")
    } else {
        None
    };
    if let Some(why) = reject {
        if icu.is_some() {
            bail!("--icu could not produce a working Node: {why}");
        }
        // The same class as the two missing-tool warnings above: the artifact is
        // correct but larger than it should be, and the reader can act on why.
        warn(
            "the embedded Node is not stripped",
            &[why, "The artifact is larger than it needs to be."],
        );
        return Ok(original);
    }

    if !needs_resign && strip.is_some() {
        note("Stripped the embedded Node");
    }
    Ok(stripped)
}

/// Can this Node reach its ICU data and format through it?
///
/// Constructs an `Intl.DateTimeFormat` and a segmenter and formats with both, which
/// is what separates a package that merely PARSES from one ICU can navigate. Host
/// targets only, for the same reason `node_runs` is: a foreign binary cannot run
/// here. A non-zero exit or a crash is a failure; the OUTPUT is deliberately not
/// asserted on, because a trimmed Node is expected to answer in a fallback locale.
fn node_formats_dates(node: &Path) -> bool {
    run_ok(
        node.to_string_lossy().as_ref(),
        &[
            "-e".as_ref(),
            "new Intl.DateTimeFormat('en',{dateStyle:'full'}).format(0);\
             [...new Intl.Segmenter('en',{granularity:'word'}).segment('a b')];"
                .as_ref(),
        ],
    )
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

/// The two things about a Windows artifact that only a read-back can establish,
/// shared by both containers.
///
/// A Windows binary is routinely cross-compiled and so is never executed on the
/// build host — and both of these fail SILENTLY on the target machine: an
/// un-hidden console window, or an Explorer Details tab showing nothing at all.
fn verify_windows_dressing(
    bytes: &[u8],
    version_info: Option<&[u8]>,
    hide_console: bool,
) -> Result<()> {
    // The version resource is re-read here for the same reason the payload is,
    // and it needs it more. A cross-compiled Windows binary cannot be executed
    // on this host, so this parse is the only evidence the resource is REACHABLE
    // rather than merely written — and an unreachable one fails silently, with
    // Explorer showing nothing and no error anywhere. The concrete way to lose it
    // is the resource directory's ascending-id rule (see `set_version_info` in
    // vendor/libsui), which takes the icon down with it.
    // Same argument as the version resource below, and the same failure shape: a
    // Windows artifact built on macOS is never executed here, so reading the
    // subsystem back is the only thing standing between a silently un-hidden
    // binary and the user discovering it on the target machine.
    if hide_console {
        let subsystem = inject::pe_subsystem(bytes).context(
            "the produced executable's PE optional header is unreadable, so \
             --hide-console cannot be confirmed",
        )?;
        if subsystem != inject::SUBSYSTEM_WINDOWS_GUI {
            bail!(
                "the produced executable's subsystem is {subsystem}, not \
                 {} (GUI) — --hide-console did not take",
                inject::SUBSYSTEM_WINDOWS_GUI
            );
        }
    }

    if let Some(encoded) = version_info {
        let found = inject::find_version_resource(bytes)
            .context("scanning the produced executable for its version resource")?
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
    Ok(())
}

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
fn verify_artifact(
    bin: &Path,
    target: &TargetPlatform,
    version_info: Option<&[u8]>,
    hide_console: bool,
) -> Result<()> {
    let bytes = fs::read(bin).with_context(|| format!("reading {}", bin.display()))?;
    let payload = inject::find_payload(target.format(), &bytes)
        .with_context(|| format!("scanning {} for its payload", bin.display()))?
        .context("the produced executable carries no payload — the injection did not take")?;
    let view = nub_core::compile::decode(payload)
        .context("the produced executable's payload does not decode")?;
    verify_payload_shape(&view)?;

    verify_windows_dressing(&bytes, version_info, hide_console)?;

    if !target.is_host() {
        note(&format!(
            "note: cross-compiled for {} — payload verified statically; the run-it \
             self-check needs a {} host",
            target.triple(),
            target.triple()
        ));
        return Ok(());
    }

    let out = run_self_probe(bin)?;
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

/// Run an artifact in probe mode and hand back what it printed.
///
/// Shared by both containers: the launcher answers on its own probe path, the
/// single-executable shape from inside its blob's main, and neither caller cares
/// which of the two spawn hazards below it was saved from.
fn run_self_probe(bin: &Path) -> Result<std::process::Output> {
    // `Command::new` PATH-searches a bare name, so the default `--out` (the entry
    // stem, no directory component) would probe a stray PATH binary or fail to
    // spawn. Anchor a relative path to the cwd the file was just written to.
    let bin = if bin.is_absolute() || bin.components().count() > 1 {
        bin.to_path_buf()
    } else {
        Path::new(".").join(bin)
    };
    match probe_once(&bin) {
        Ok(out) => Ok(out),
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
            })
        }
        Err(error) => {
            Err(error).with_context(|| format!("running the self-probe on {}", bin.display()))
        }
    }
}

fn probe_once(bin: &Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new(bin)
        .env("__NUB_COMPILED_LAUNCHER_MODE", "probe")
        // The same scrub, and the same reason, as [`node_runs`] — and it binds
        // harder here, because a single-executable artifact IS a Node and reads
        // this itself. A developer machine routinely carries a `NODE_OPTIONS`
        // aimed at some other Node (nub's own dev shell exports one), and Node
        // rejects the whole invocation over one flag it does not know, so the
        // probe would fail a perfectly good artifact. A preload named there is
        // worse than that: `--require`/`--import` runs BEFORE the blob's main, so
        // anything it prints lands on stdout ahead of the probe's reply and the
        // response no longer matches. Either way the check would be reading the
        // build machine's environment rather than the bytes just written.
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_REPL_EXTERNAL_MODULE")
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

    // Windows releases an executable's image section ASYNCHRONOUSLY, so the move
    // can lose a race with a process that has already exited. The build's own
    // self-probe RUNS the staged artifact immediately before this call, and
    // `Command::output` returning means the child was reaped — not that the
    // section is gone. Measured on the win32-arm64 CI leg: `a-env-strict` failed
    // with `The process cannot access the file because it is being used by another
    // process. (os error 32)`, intermittently, while every other fixture in the
    // same run published fine.
    //
    // A bounded retry is the fix rather than a workaround: there is no handle to
    // wait on, because the holder is the kernel finishing with a process that no
    // longer exists. Anti-malware scanning a freshly written binary produces the
    // same code, and the same answer. ~1.3s total, which is invisible next to a
    // build and far short of hanging a broken publish.
    //
    // The retry is deliberately NOT extended to other errors. A denied or missing
    // path fails on the first attempt, where the message still describes what went
    // wrong.
    const SHARING_VIOLATION: i32 = 32;
    let mut delay = std::time::Duration::from_millis(10);
    for attempt in 0..8 {
        // SAFETY: both buffers are live, NUL-terminated UTF-16 paths. No destination
        // is removed first, so an API failure leaves the previous artifact intact.
        if unsafe { MoveFileExW(staged.as_ptr(), destination.as_ptr(), flags) } != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(SHARING_VIOLATION) || attempt == 7 {
            return Err(error);
        }
        std::thread::sleep(delay);
        delay *= 2;
    }
    unreachable!("the loop returns on its last attempt")
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
/// Brotli at the maximum quality, for an inline payload's chunks.
///
/// Quality 11 with a 24-bit window, matching what the zstd-19 it replaces is
/// reaching for: this runs once per build on a few tens of kilobytes, and the bytes
/// it produces are shipped to every user of the artifact.
fn brotli_encode(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut reader = brotli::CompressorReader::new(bytes, 4096, 11, 24);
    std::io::copy(&mut reader, &mut out).context("brotli-compressing an inline payload chunk")?;
    Ok(out)
}

fn sha256_of_app(files: &[AppFile<Vec<u8>>]) -> String {
    let mut h = Sha256::new();
    for file in files {
        h.update(file.name.as_bytes());
        h.update((file.bytes.len() as u64).to_le_bytes());
        h.update(&file.bytes);
        h.update([u8::from(file.executable)]);
    }
    hex::encode(h.finalize())
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

    /// `--hide-console` is refused on a target that cannot honor it, for the same
    /// reason `--icon` is: the flag's whole promise is that nothing appears, so
    /// accepting it and doing nothing looks identical to it working until someone
    /// runs the binary. The HOST is deliberately not part of the gate — the flip
    /// is byte editing, so a hidden Windows binary cross-compiles from anywhere.
    #[test]
    fn hide_console_is_refused_for_a_target_with_no_subsystem_field() {
        let win = TargetPlatform {
            os: TargetOs::Win32,
            arch: TargetArch::X64,
            musl: false,
        };
        let linux = TargetPlatform {
            os: TargetOs::Linux,
            arch: TargetArch::Arm64,
            musl: false,
        };

        reject_non_windows_hide_console(true, &win).expect("a Windows target accepts the flag");
        let err = reject_non_windows_hide_console(true, &linux)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("linux-arm64"),
            "must name the target, got: {err}"
        );
        // The control: the gate must key on the FLAG, not on the target alone, or
        // it would break every ordinary Linux build.
        reject_non_windows_hide_console(false, &linux)
            .expect("a Linux build without the flag is untouched");
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

    /// The parent guard above passes when `--out` names a directory whose parent
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
            hide_console: false,
            icu: None,
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
                module_mirror: Default::default(),
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
                allow_dynamic_import: Vec::new(),
                tsconfig: None,
                loaders: Vec::new(),
                native_target: None,
                drop_console: false,
                drop_debugger: false,
                metafile: false,
                target_node: None,
                eager_startup: false,
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
            node_icu: String::new(),
            app_compressed: false,
            app_sha256: "app".into(),
            minify: false,
            install_message: None,
            node_flags: Vec::new(),
            sealed_module_graph: false,
            hide_console: false,
            inline_app: false,
            standalone_preamble: false,
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
            node_icu: String::new(),
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

    /// A build with everything present, so the optional rows all appear at once.
    fn full_facts() -> BuildFacts {
        BuildFacts {
            // Every size here is measured off a real darwin-arm64 embed build, so
            // the components add up the way a reader's own build will: 851 KB of
            // launcher template, a 228 KB signature that scales with the file, and
            // 66 KB of container header, manifest and 64 KiB payload alignment.
            size: 29_390_370,
            node_bytes: 28_189_460,
            app_bytes: 56_571,
            launcher_bytes: 850_864,
            signature_bytes: 227_954,
            shipped: vec![
                ("@napi-rs/nice".to_string(), "native addon"),
                ("sharp".to_string(), "--external"),
            ],
            deferred: 3,
            // Consistent with the rest of these facts rather than an arbitrary
            // pick: `--external` and a surviving computed import are exactly what
            // leaves the graph unsealed, and this build has both.
            app_delivery: AppDelivery::Extracted(inline::Decline::UnsealedGraph.reason()),
            report: Some(PathBuf::from("report.json")),
            elapsed: std::time::Duration::from_millis(8_880),
        }
    }

    fn embedded_summary() -> RuntimeSummary {
        RuntimeSummary {
            fact: "Node 26.8.1, embedded".to_string(),
            provenance: "package.json#engines.node".to_string(),
        }
    }

    /// Every row the block can carry, in order, with its text.
    ///
    /// The `platform` row is the one worth reading closely. It is named for what
    /// `--platform` sets; the row above it carries what `--target` sets. Getting
    /// those two the wrong way round is the defect this label fixes.
    #[test]
    fn the_resolved_build_block_states_every_output_fact_once() {
        let host = TargetPlatform::host().unwrap();
        assert_eq!(
            plain_rows(&resolved_build_rows(
                Path::new("acme"),
                &full_facts(),
                &embedded_summary(),
                &host,
            )),
            vec![
                "output=acme  29.4 MB  (node 28.2 MB · app 57 KB · launcher 851 KB · \
                 signature 228 KB)"
                    .to_string(),
                "runtime=Node 26.8.1, embedded  (package.json#engines.node)".to_string(),
                // No aside: building for the host is the default, and a note on
                // the default is paid for by every build and earned by none.
                format!("platform={}", host.triple()),
                "shipped=@napi-rs/nice (native addon), sharp (--external)".to_string(),
                "deferred=3 dynamic import sites  resolved where the binary runs".to_string(),
                "app=extracted on first run  it resolves modules at run time".to_string(),
                "report=report.json  esbuild schema".to_string(),
            ]
        );
    }

    /// Every part of the split is measured, and the container's own bytes are not
    /// named at all — so the parts stay under the total rather than reaching it.
    ///
    /// The regression this pins: `launcher` used to BE the remainder, so it
    /// swallowed the ad-hoc code signature — which grows at one SHA-256 per 4 KiB
    /// page — and reported a fixed 851 KB template as 1.2 MB on a 29 MB build.
    #[test]
    fn the_split_names_the_signature_apart_from_the_launcher() {
        let signed = full_facts();
        assert_eq!(
            signed.size_split(),
            "  (node 28.2 MB · app 57 KB · launcher 851 KB · signature 228 KB)",
        );
        let components =
            signed.node_bytes + signed.app_bytes + signed.launcher_bytes + signed.signature_bytes;
        assert!(
            components < signed.size,
            "the measured parts must stay under the total, since the container's \
             header, manifest and alignment padding go unnamed: {components} vs {}",
            signed.size
        );

        // ELF and PE are never signed, so there is no component to name and the
        // bytes it would have covered do not exist rather than moving elsewhere.
        let unsigned = BuildFacts {
            size: signed.size - signed.signature_bytes,
            signature_bytes: 0,
            ..signed
        };
        assert_eq!(
            unsigned.size_split(),
            "  (node 28.2 MB · app 57 KB · launcher 851 KB)",
        );
    }

    /// A cross-compile is the case that earns words, and it is the only one.
    #[test]
    fn only_a_cross_compile_annotates_the_platform_row() {
        let host = TargetPlatform::host().unwrap();
        let foreign = SUPPORTED_TRIPLES
            .iter()
            .map(|t| TargetPlatform::parse(t).unwrap())
            .find(|t| *t != host)
            .unwrap();
        let rows = resolved_build_rows(
            Path::new("acme"),
            &full_facts(),
            &embedded_summary(),
            &foreign,
        );
        assert_eq!(
            plain_rows(&rows)[2],
            format!(
                "platform={}  cross-compiled from {}",
                foreign.triple(),
                host.triple()
            ),
            "a cross-compile has to say what it was built ON, since the artifact \
             cannot be run here to find out"
        );
    }

    /// The optional rows are optional. A plain build states four facts, and a
    /// reader of that block should not have to skip three empty ones to find them.
    #[test]
    fn a_build_with_nothing_deferred_or_external_prints_no_row_for_it() {
        let host = TargetPlatform::host().unwrap();
        let bare = BuildFacts {
            size: 3_433_512,
            // `--smol` embeds no runtime, so the split names only what is there.
            node_bytes: 0,
            app_bytes: 2_500_000,
            launcher_bytes: 850_864,
            signature_bytes: 26_744,
            shipped: Vec::new(),
            deferred: 0,
            app_delivery: AppDelivery::Inline,
            report: None,
            elapsed: std::time::Duration::from_millis(2_400),
        };
        assert_eq!(
            plain_rows(&resolved_build_rows(
                Path::new("acme"),
                &bare,
                &RuntimeSummary {
                    fact: "Node >=22 <23, not embedded".to_string(),
                    provenance: "--target".to_string(),
                },
                &host,
            )),
            vec![
                "output=acme  3.4 MB  (app 2.5 MB · launcher 851 KB · signature 27 KB)".to_string(),
                "runtime=Node >=22 <23, not embedded  (--target)".to_string(),
                format!("platform={}", host.triple()),
                "app=run from the executable  nothing is written to disk".to_string(),
            ]
        );
    }

    /// Render the rows the way [`report_resolved_build`] does, minus the styling,
    /// so a text assertion reads what a user without color sees.
    fn plain_rows(rows: &[(&'static str, Vec<(String, Ink)>)]) -> Vec<String> {
        rows.iter()
            .map(|(label, spans)| {
                let joined: String = spans.iter().map(|(text, _)| text.as_str()).collect();
                format!("{label}={joined}")
            })
            .collect()
    }

    /// Which tier each segment is drawn in.
    ///
    /// Worth its own test because it is invisible to every text assertion above:
    /// the block could lose all of its styling, or paint the whole line one color,
    /// and the rendered text would be byte-identical.
    ///
    /// The accent is the claim being pinned. It belongs to the artifact path and
    /// nothing else — it marks the one thing the reader runs next, so a second
    /// accented token would spend the distinction it exists to make.
    #[test]
    fn only_the_artifact_path_is_accented_and_every_aside_is_muted() {
        let host = TargetPlatform::host().unwrap();
        let rows =
            resolved_build_rows(Path::new("acme"), &full_facts(), &embedded_summary(), &host);
        let inks: Vec<(&str, Vec<Ink>)> = rows
            .iter()
            .map(|(label, spans)| (*label, spans.iter().map(|(_, ink)| *ink).collect()))
            .collect();

        assert_eq!(
            inks,
            vec![
                // The size is Plain and only its breakdown is Muted: the total is
                // a fact the reader came for, the split is why it is that size.
                ("output", vec![Ink::Accent, Ink::Plain, Ink::Muted]),
                ("runtime", vec![Ink::Plain, Ink::Muted]),
                ("platform", vec![Ink::Plain]),
                (
                    "shipped",
                    vec![Ink::Plain, Ink::Muted, Ink::Plain, Ink::Plain, Ink::Muted,],
                ),
                ("deferred", vec![Ink::Plain, Ink::Muted]),
                ("app", vec![Ink::Plain, Ink::Muted]),
                ("report", vec![Ink::Plain, Ink::Muted]),
            ],
            "exactly one Accent in the whole block, on the path the reader runs next"
        );
        assert_eq!(
            inks.iter()
                .flat_map(|(_, i)| i)
                .filter(|i| **i == Ink::Accent)
                .count(),
            1
        );
    }

    /// The two lines that bracket the block, in both the modes they print in.
    ///
    /// The colorless spelling is the load-bearing half. Both lines print on a
    /// redirected build — where the live line is off entirely — so a CI log is
    /// the one place they are the ONLY record that a compile started and that it
    /// worked, and an escape sequence leaking into that log is exactly what a
    /// TTY-only assertion would miss.
    #[test]
    fn the_intro_and_success_lines_bracket_the_block_in_both_modes() {
        let out = Path::new("dist/cli");
        let took = std::time::Duration::from_millis(3_540);

        assert_eq!(
            intro_line("cli.ts", out, false),
            format!(
                "nub {} · compiling cli.ts → dist/cli",
                env!("CARGO_PKG_VERSION")
            ),
            "the intro names what the reader is waiting for, which is the one \
             thing the closing block cannot say"
        );
        assert_eq!(
            success_line(out, took, false),
            "✓ compiled dist/cli in 3.5s",
            "a build that worked has to say so in words, not by ending"
        );

        // The artifact path is the block's Accent and stays it on both lines, so
        // the path a reader runs next is one color from the first line to the
        // last. The green check mark is the only ink here the block never draws.
        let intro = intro_line("cli.ts", out, true);
        let success = success_line(out, took, true);
        assert!(
            intro.contains(&paint("dist/cli", Ink::Accent, true))
                && success.contains(&paint("dist/cli", Ink::Accent, true)),
            "intro={intro:?} success={success:?}"
        );
        assert!(
            success.starts_with("\x1b[32m\x1b[1m✓"),
            "the success cue is the engine's green bold check mark: {success:?}"
        );
        assert_eq!(
            strip_sgr(&success),
            success_line(out, took, false),
            "color must add nothing but color"
        );
    }

    /// The bands exist because an embed build that downloads a ~100 MB Node
    /// routinely runs past a minute, and the flat `{:.1}s` this replaced rendered
    /// that as `92.4s`.
    #[test]
    fn an_elapsed_build_is_reported_in_the_band_it_lands_in() {
        let ms = |n| format_elapsed(std::time::Duration::from_millis(n));
        assert_eq!(ms(240), "240ms");
        assert_eq!(ms(999), "999ms");
        assert_eq!(ms(1_000), "1.0s");
        assert_eq!(ms(59_940), "59.9s");
        assert_eq!(ms(92_400), "1m32s");
    }

    /// A row too wide for the terminal wraps to the value column, not to zero.
    ///
    /// The input is the real one that exposed this: a `--smol` build's runtime
    /// row measured 112 columns on an 80-column terminal, so the terminal broke
    /// it wherever it liked and the remainder restarted at the left margin —
    /// destroying the aligned value column on the widest row in the block. Every
    /// other test here passes on the unwrapped implementation.
    #[test]
    fn a_row_wider_than_the_terminal_wraps_to_the_value_column() {
        let spans = vec![
            ("Node >=22, not embedded".to_string(), Ink::Plain),
            (
                "  (package.json#engines.node; range enforced at runtime, provisioning 26.8.1)"
                    .to_string(),
                Ink::Muted,
            ),
        ];
        let lines = render_row("runtime", &spans, 8, 80, false);
        assert!(lines.len() > 1, "this row does not fit in 80 columns");
        for line in &lines {
            assert!(
                line.chars().count() <= 80,
                "{} columns: {line:?}",
                line.chars().count()
            );
        }
        let value_col = INDENT + 8 + GAP;
        for continuation in &lines[1..] {
            assert_eq!(
                continuation.len() - continuation.trim_start().len(),
                value_col,
                "a continuation hung anywhere but the value column is the defect: {continuation:?}"
            );
        }
        // Wrapping moves words between lines; it must not lose or duplicate one.
        let words_out: Vec<&str> = lines
            .iter()
            .flat_map(|l| l.split_whitespace())
            .skip(1) // the label
            .collect();
        let words_in: Vec<&str> = spans
            .iter()
            .flat_map(|(t, _)| t.split_whitespace())
            .collect();
        assert_eq!(words_out, words_in, "wrapping dropped or reordered a word");
    }

    /// A diagnostic's body hangs under its headline, not under the margin.
    ///
    /// The indent is the whole reason a tier reads as two things: line the body
    /// up with the left edge instead and the explanation stops looking like it
    /// belongs to the headline above it. Both tiers are asserted, because they
    /// have different label widths and a shared implementation is exactly where
    /// one of them would silently pick up the other's indent.
    ///
    /// Color is asserted too — that is the case where an implementation padding
    /// the PAINTED label would indent by the escape bytes instead of the letters.
    #[test]
    fn a_diagnostic_hangs_its_body_under_its_headline() {
        let body = ["first explanation line", "second"];
        assert_eq!(
            diagnostic_lines(
                Tier::Warn,
                "the embedded Node is not stripped",
                &body,
                80,
                false
            ),
            vec![
                "warn  the embedded Node is not stripped".to_string(),
                "      first explanation line".to_string(),
                "      second".to_string(),
            ]
        );
        assert_eq!(
            diagnostic_lines(
                Tier::Error,
                "entry file not found: app.ts",
                &body,
                80,
                false
            ),
            vec![
                "error  entry file not found: app.ts".to_string(),
                "       first explanation line".to_string(),
                "       second".to_string(),
            ],
            "the wider label carries a wider hanging indent"
        );

        for (tier, sgr) in [(Tier::Warn, "\x1b[33m"), (Tier::Error, "\x1b[31m")] {
            let colored = diagnostic_lines(tier, "headline", &body, 80, true);
            assert_eq!(
                colored.iter().map(|l| strip_sgr(l)).collect::<Vec<_>>(),
                diagnostic_lines(tier, "headline", &body, 80, false),
                "color must change nothing about which columns the lines occupy"
            );
            assert!(
                colored[0].starts_with(&format!("{sgr}\x1b[1m{}", tier.label())),
                "the label carries the whole of the diagnostic's weight: {:?}",
                colored[0]
            );
        }
    }

    /// A headline too long for the terminal wraps under the label, not to the
    /// margin.
    ///
    /// Uses the real one that forced this: `--platform` enumerates all eight
    /// supported triples and runs to 155 columns, so on any ordinary terminal
    /// the label's own line is the one that wraps. Left to the terminal, the
    /// remainder restarts at column zero and the hanging indent the tier exists
    /// for is honored only on lines short enough not to need it.
    #[test]
    fn a_diagnostic_too_wide_for_the_terminal_wraps_under_its_label() {
        let headline = "unknown --platform \"sunos-sparc\". Supported: darwin-arm64, darwin-x64, \
             linux-arm64, linux-arm64-musl, linux-x64, linux-x64-musl, win32-arm64, win32-x64";
        let lines = diagnostic_lines(Tier::Error, headline, &[], 80, false);

        assert!(
            lines.len() > 1,
            "a 155-column headline must wrap: {lines:?}"
        );
        for line in &lines {
            assert!(line.len() <= 80, "line runs past the terminal: {line:?}");
        }
        let indent = "error".len() + GAP;
        for line in &lines[1..] {
            assert_eq!(
                line.len() - line.trim_start().len(),
                indent,
                "a continuation hangs under the headline, not at the margin: {line:?}"
            );
        }
        let words_in: Vec<&str> = headline.split_whitespace().collect();
        let words_out: Vec<&str> = lines.iter().flat_map(|l| l.split_whitespace()).collect();
        assert_eq!(
            words_out[1..],
            words_in[..],
            "wrapping dropped or reordered a word (the leading `error` label aside)"
        );
    }

    /// An error's own formatting survives verbatim, and its causes hang.
    ///
    /// The verbatim half is the load-bearing one, and it is asserted with a
    /// message shaped like the real ones: a `file:line`, the offending source
    /// nested one level under it, a blank line, then a hard-wrapped paragraph.
    /// Re-indenting it collapses the nesting so the location and the source at
    /// it become two unrelated lines; re-wrapping breaks each authored line one
    /// word early. Both shipped, briefly, and both are what this pins shut.
    ///
    /// The causes are different — anyhow hands those over as bare strings with
    /// no formatting of their own, so the tier lays them out.
    #[test]
    fn an_error_keeps_its_own_formatting_and_hangs_its_causes() {
        let formatted = anyhow::anyhow!(
            "1 import could not be resolved at build time:\n  /src/plugins.ts:6:22\n    import(pluginName)\n\n  A compiled binary carries no node_modules, so an unresolved import fails at\n  runtime on the machine you ship to."
        );
        assert_eq!(
            error_lines(&formatted, 80, false),
            vec![
                "error  1 import could not be resolved at build time:".to_string(),
                "  /src/plugins.ts:6:22".to_string(),
                "    import(pluginName)".to_string(),
                String::new(),
                "  A compiled binary carries no node_modules, so an unresolved import fails at"
                    .to_string(),
                "  runtime on the machine you ship to.".to_string(),
            ],
            "only the first line is the tier's; the rest is the author's, byte for byte"
        );

        let chained = anyhow::anyhow!("no such file or directory")
            .context("reading app.ts")
            .context("bundling app.ts");
        assert_eq!(
            error_lines(&chained, 80, false),
            vec![
                "error  bundling app.ts".to_string(),
                "       reading app.ts".to_string(),
                "       no such file or directory".to_string(),
            ],
            "every cause is stated; none is dropped and none repeats the headline"
        );

        // A cause is frequently multiline itself. This is the real one, shortened:
        // `inject::inject` builds the icon message with its own second line, and
        // `run` wraps it with `writing <staged path>`. Indenting the cause once
        // leaves that second line back at the column it was authored in.
        let multiline_cause = anyhow::anyhow!(
            "setting the executable icon: PngNotRgba\n  The container parsed, so one of the images inside it did not."
        )
        .context("writing /tmp/app.staged");
        assert_eq!(
            error_lines(&multiline_cause, 200, false),
            vec![
                "error  writing /tmp/app.staged".to_string(),
                "       setting the executable icon: PngNotRgba".to_string(),
                "         The container parsed, so one of the images inside it did not."
                    .to_string(),
            ],
            "every physical line of a cause hangs, keeping the relative indent it came with"
        );
    }

    /// A wide character occupies two terminal cells, and the wrap has to know.
    ///
    /// `chars().count()` and the rendered width agree on ASCII, which is why
    /// every other test here would pass with either. They disagree on exactly
    /// the text a user controls — a path, a package name, a `--platform` value —
    /// so counting scalars wraps a column late and the line lands past the edge,
    /// where the terminal breaks it back to column zero and the hanging indent
    /// the tier just applied is lost.
    #[test]
    fn a_wide_character_counts_as_the_two_cells_it_occupies() {
        // Eight per word, so a scalar count says 8 where the terminal says 16.
        let headline = "unknown --platform \"日本語テスト\". Supported: 日本語テスト, 日本語テスト, 日本語テスト";
        let lines = diagnostic_lines(Tier::Error, headline, &[], 40, false);

        assert!(lines.len() > 1, "must wrap at 40 cells: {lines:?}");
        for line in &lines {
            assert!(
                cells(line) <= 40,
                "line renders {} cells, over the 40 asked for: {line:?}",
                cells(line)
            );
        }
        let words_in: Vec<&str> = headline.split_whitespace().collect();
        let words_out: Vec<&str> = lines.iter().flat_map(|l| l.split_whitespace()).collect();
        assert_eq!(
            words_out[1..],
            words_in[..],
            "wrapping dropped or reordered a word (the leading `error` label aside)"
        );
    }

    /// `NO_COLOR` and a redirected stream both reach [`paint`] as `color = false`,
    /// and the block has to survive it on alignment alone — so nothing may depend
    /// on an escape being present, and none may be emitted.
    #[test]
    fn paint_emits_no_escapes_when_color_is_off() {
        for ink in [Ink::Plain, Ink::Muted, Ink::Accent] {
            assert_eq!(paint("dist/cli", ink, false), "dist/cli");
        }
        assert_eq!(paint("dist/cli", Ink::Plain, true), "dist/cli");
        assert_eq!(
            paint("dist/cli", Ink::Muted, true),
            "\x1b[2mdist/cli\x1b[22m"
        );
        assert_eq!(
            paint("dist/cli", Ink::Accent, true),
            "\x1b[96mdist/cli\x1b[39m"
        );
    }

    /// A colored row must occupy exactly the columns its plain twin does.
    ///
    /// Asserted through [`render_row`] — the function that prints — rather than a
    /// copy of its arithmetic, or the test would pass while production padded the
    /// painted label and collapsed the very column this guards. Verified by
    /// breaking it: padding the painted string turns the color-on `output` row
    /// into `  outputdist/cli`, and this goes red.
    #[test]
    fn a_styled_row_occupies_the_same_columns_as_a_plain_one() {
        let spans = vec![
            ("acme".to_string(), Ink::Accent),
            ("  (29.5 MB)".to_string(), Ink::Muted),
        ];
        for label in ["output", "runtime", "platform"] {
            assert_eq!(
                render_row(label, &spans, 8, 80, true)
                    .iter()
                    .map(|l| strip_sgr(l))
                    .collect::<Vec<_>>(),
                render_row(label, &spans, 8, 80, false),
                "the {label} row must not lose its gutter to escape bytes"
            );
        }
        assert_eq!(
            render_row("runtime", &spans, 8, 80, false),
            vec!["   runtime  acme  (29.5 MB)".to_string()],
            "the label column is the block's whole structure — pin it literally"
        );
    }

    /// Right alignment is the block's whole visual claim, so pin the thing it
    /// promises: every value starts in the same column whatever its label is, and
    /// the slack lands in the left margin instead of between a label and its own
    /// value. A left-aligned implementation passes every other test in this file.
    #[test]
    fn every_value_starts_in_one_column_and_the_slack_is_on_the_left() {
        let spans = vec![("x".to_string(), Ink::Plain)];
        let width = 8;
        let rendered: Vec<String> = ["output", "runtime", "platform", "shipped"]
            .iter()
            .map(|label| render_row(label, &spans, width, 80, false).join(""))
            .collect();

        let value_columns: Vec<usize> = rendered
            .iter()
            .map(|line| line.find('x').expect("every row carries its value"))
            .collect();
        assert_eq!(
            value_columns,
            vec![INDENT + width + GAP; 4],
            "a value that does not start where every other value starts is the \
             one defect right alignment exists to prevent"
        );

        assert_eq!(
            rendered[0], "    output  x",
            "the shortest label carries the widest left margin"
        );
        assert_eq!(
            rendered[2], "  platform  x",
            "the longest label sits flush against the indent"
        );
    }

    /// Drop every SGR sequence, so a painted row can be compared against a plain
    /// one column for column. Written generically rather than as a list of the
    /// codes [`paint`] emits today, so a fourth [`Ink`] cannot slip past it.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(start) = rest.find('\x1b') {
            out.push_str(&rest[..start]);
            let Some(end) = rest[start..].find('m') else {
                return out;
            };
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        out
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
            bootstrap_optional: false,
            app_computes_module_specifier: false,
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: Vec::new(),
            metafile: None,
        };
        let layout = assets::Layout {
            anchor: PathBuf::new(),
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

    /// A user manifest is a verbatim asset, not compile-time metadata: it is
    /// embedded exactly as written and never rewritten or replaced. Chunks name
    /// their ESM format directly with `.mjs`, so nothing here is trying to alter
    /// Node's package-type lookup. Distinct from the ROOT manifest `assemble_app`
    /// synthesizes when the payload has none, which exists to stop a `getRoot`
    /// walk-up climbing out of the extraction dir and carries no `"type"` field
    /// for exactly the reason above.
    #[test]
    fn an_included_manifest_is_embedded_verbatim_rather_than_replaced() {
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
            bootstrap_optional: false,
            app_computes_module_specifier: false,
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: Vec::new(),
            metafile: None,
        };
        let layout = assets::Layout {
            anchor: PathBuf::new(),
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
            bootstrap_optional: false,
            app_computes_module_specifier: false,
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: Vec::new(),
            metafile: None,
        };
        let files = assemble_app(
            &bundled,
            &assets::Layout {
                anchor: PathBuf::new(),
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
                // Synthesized because this payload includes no manifest of its own,
                // and at the ROOT even though the entry sits under `src/app/` --
                // which is the point: a walk-up from a chunk has to terminate at the
                // app dir, not at the entry's directory.
                AppFile::plain("package.json", b"{\"private\":true}\n".to_vec()),
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
            bootstrap_optional: false,
            app_computes_module_specifier: false,
            dynamic_import_sites: 0,
            native_addons: Vec::new(),
            external_imports: Vec::new(),
            worker_roots: Vec::new(),
            metafile: None,
        };

        let err = assemble_app(
            &bundled,
            &assets::Layout {
                anchor: PathBuf::new(),
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
            bootstrap_optional: false,
            app_computes_module_specifier: false,
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
                anchor: PathBuf::new(),
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
                bootstrap_optional: false,
                app_computes_module_specifier: false,
                dynamic_import_sites: 0,
                native_addons: Vec::new(),
                external_imports: Vec::new(),
                worker_roots: Vec::new(),
                metafile: None,
            },
            assets::Layout {
                anchor: PathBuf::new(),
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
