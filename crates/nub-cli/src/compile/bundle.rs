//! The shared compiler front end: transpile + bundle an entry with Rolldown,
//! in-process.
//!
//! Split out of `compile::run` because the bundler-flag surface is COMMON to
//! `nub compile` and the planned `nub build` (design record:
//! wiki/commands/compile.md, "Shared compiler block") — compile is build plus a
//! launcher wrap. Everything here is therefore expressed against
//! [`BundleOptions`] and a target *description*, never against the compile
//! pipeline's own state, so `nub build` can drive it unchanged.
//!
//! One deliberate non-delegation: an unresolvable dynamic `import(expr)` FAILS
//! the build rather than warning. A compiled artifact carries no `node_modules`,
//! so a specifier that cannot be resolved at build time is an
//! `ERR_MODULE_NOT_FOUND` on the deploy machine — with no stack-trace clue and
//! nothing obvious for the operator to install.
//!
//! There are two ways past it, and neither is the default, because a binary
//! whose plugin loads fail on the target machine must be something the author
//! chose. `--external` REMOVES a named package from the graph (matched on its
//! raw specifier before resolution, so its source is never loaded and its
//! unanalyzable sites are never scanned). `--allow-dynamic-import` keeps the
//! `import(expr)` in the output and lets the artifact's runtime hook resolve it
//! against the launch directory — the case `--external` structurally cannot
//! serve, since a plugin path the user supplies has no package to name. See
//! [`crate::compile::external`] for the hook.
//!
//! The same reasoning drives the two gates that bracket the bundle. BEFORE it,
//! [`jsx_override`] refuses to let a tsconfig `jsx: "preserve"` through, because
//! preserved JSX is not JavaScript and cannot run anywhere. AFTER it,
//! [`reject_invalid_chunks`] re-parses every emitted chunk, so ANY future path
//! that emits something unparseable fails the compile instead of producing a
//! binary that dies with `Unexpected token` on first run. The emit check is the
//! backstop of last resort: it costs one parse per chunk and makes "compiled
//! successfully" mean the output is at least loadable.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use oxc_ast::ast::{BinaryExpression, BinaryOperator, Expression, NewExpression, Program};
use rolldown::plugin::__inner::SharedPluginable;
use rolldown::plugin::{
    HookAddonArgs, HookInjectionOutputReturn, HookLoadArgs, HookLoadOutput, HookLoadReturn,
    HookRenderChunkArgs, HookRenderChunkOutput, HookRenderChunkReturn, HookResolveIdArgs,
    HookResolveIdOutput, HookResolveIdReturn, HookTransformArgs, HookTransformOutput,
    HookTransformOutputMap, HookTransformReturn, HookUsage, Plugin, PluginContext,
    SharedLoadPluginContext, SharedTransformPluginContext,
};
use rolldown::{BundlerBuilder, BundlerOptions, InputItem};
use rolldown_common::bundler_options::{BundlerTransformOptions, Either, JsxOptions};
use rolldown_common::{
    CodeSplittingMode, EmittedChunk, InnerOptions, IsExternal, ManualCodeSplittingOptions,
    MatchGroup, MatchGroupName, ModuleType, Output, OutputFormat, Platform, RawMinifyOptions,
    ResolveOptions, ResolvedExternal, SourceMapType, StrOrBytes, TreeshakeOptions, TsConfig,
};
use rolldown_error::{BuildDiagnostic, DiagnosticOptions, EventKind};
use rolldown_utils::indexmap::FxIndexMap;
use rolldown_utils::url::clean_url;
use serde::Serialize;
use sha2::{Digest, Sha256};
use string_wizard::{Hires, MagicString, SourceMapOptions};

use super::{closure, loaders, native, native_layout};

/// Where the source map goes. `Linked` and `External` both emit a real `.map`;
/// they differ only in whether the bundle references it — which for a compiled
/// artifact also decides whether the map ships INSIDE the executable or lands
/// beside it for an error tracker to consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcemapMode {
    /// A `.map` travelling with the bundle, referenced by a `sourceMappingURL`.
    Linked,
    /// A base64 `sourceMappingURL` data URI inside the bundle. Self-contained.
    Inline,
    /// A `.map` emitted but NOT referenced or shipped.
    External,
    None,
}

/// Everything the bundler front end needs, and nothing about the artifact shape.
pub struct BundleOptions {
    pub minify: bool,
    /// Preserve `fn.name` / `Class.name` under minification. Default ON: minify
    /// silently renames a class, and the frameworks that key on `Class.name`
    /// (NestJS DI, TypeORM/MikroORM entities, class-keyed registries) then fail
    /// at RUNTIME, inside a frozen binary, with nothing in the stack trace
    /// pointing at the cause.
    pub keep_names: bool,
    pub sourcemap: SourcemapMode,
    /// Embed the original source text in the map. `--sourcemap-exclude-sources`
    /// off.
    pub sources_content: bool,
    /// `K=V` from `--define`. Values are JS EXPRESSIONS (esbuild/Rolldown
    /// semantics), so a string constant is written `K='"v"'`.
    pub define: Vec<String>,
    /// Compile-driven defines applied UNDER `define`, so an explicit `--define`
    /// of the same key wins.
    pub auto_define: Vec<(String, String)>,
    pub tree_shake: bool,
    /// Ignore `/*@__PURE__*/` and `/*@__NO_SIDE_EFFECTS__*/` annotations.
    pub ignore_annotations: bool,
    /// `FROM=TO` specifier remaps.
    pub alias: Vec<String>,
    /// Extra `exports` conditions, ADDED to the platform defaults (Rolldown
    /// unions them; a condition set is matched, not ranked).
    pub conditions: Vec<String>,
    /// Packages to leave OUT of the bundle, resolved from disk at run time.
    /// Package-scoped, not specifier-exact: `--external prettier` also covers
    /// `prettier/plugins/babel`, which is what makes the flag usable on a
    /// package whose subpaths are imported separately.
    pub external: Vec<String>,
    /// Packages the user forced to ship unbundled, beyond what detection found.
    pub unbundled: Vec<String>,
    /// Packages the user forced INTO the bundle, overriding detection.
    pub bundled: Vec<String>,
    /// Let a dynamic `import()` whose specifier is not statically analyzable
    /// survive into the output instead of failing the build. Off by default.
    pub allow_dynamic_import: bool,
    /// Explicit tsconfig; `None` keeps Rolldown's auto-discovery.
    pub tsconfig: Option<PathBuf>,
    /// `EXT=TYPE` from `--loader`, applied over nub's defaults. See
    /// [`crate::compile::loaders`].
    pub loaders: Vec<String>,
    /// The platform a `.node` addon must be loadable on, which turns native-addon
    /// embedding ON. Compile-driven, like [`Self::auto_define`].
    ///
    /// `None` leaves `.node` to Rolldown, which is the right answer for a
    /// `nub build` that emits into a real project: the addon is already beside its
    /// `node_modules` there, so embedding a copy would only duplicate it. Only a
    /// self-contained artifact — running from a cache dir with no `node_modules`
    /// in sight — needs the file carried along.
    pub native_target: Option<nub_core::compile::TargetPlatform>,
    /// The exact Node this artifact targets, as (major, minor, patch) — the input to
    /// [`strip_native_polyfills`]. `None` for `--smol`, whose runtime is not known
    /// until launch, so every polyfill ships.
    pub target_node: Option<(u64, u64, u64)>,
}

/// One emitted file: a chunk, or a source map that travels with it.
#[derive(Debug)]
pub struct BundledFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

pub struct BundleResult {
    /// The entry chunk's filename within [`Self::files`].
    pub entry: String,
    /// Chunks plus any map the bundle REFERENCES — the set that must ship.
    pub files: Vec<BundledFile>,
    /// `--sourcemap=external` maps: emitted, unreferenced, not shipped.
    pub detached_maps: Vec<BundledFile>,
    /// Files embedded beside the chunks: the `file` loader's payload and the
    /// targets of [`NewUrlAssets`]. Kept APART from [`Self::files`] because those
    /// are re-parsed as JavaScript by [`reject_invalid_chunks`], which a `.wasm`
    /// would rightly fail.
    pub assets: Vec<BundledFile>,
    /// Reached native-addon package islands. These are nested regular files and
    /// deliberately do not share the generic flat asset collector — nor
    /// [`BundledFile`], because an island is a verbatim copy of a package
    /// directory and so is the only bundler output that can be executable.
    pub native_files: Vec<native_layout::IslandFile>,
    /// Compile-runtime helpers which must remain real files beside the emitted
    /// chunks because runtime `createRequire(import.meta.url)` loads them by
    /// relative path.
    pub support_files: Vec<BundledFile>,
    /// Compile bootstrap which must be extracted at the payload root before any
    /// generated chunk can read the private builtin registry it installs.
    pub root_support_files: Vec<BundledFile>,
    /// Computed `import()` sites `--allow-dynamic-import` let through. Zero
    /// unless the flag is set; the build would otherwise have failed. This is
    /// what decides whether the artifact needs a runtime resolve hook at all.
    ///
    /// Counted as the graph LOADED, so a site in a module tree-shaking later
    /// empties still counts. The over-approximation can only ship a hook nothing
    /// uses; it can never omit one something needs.
    pub dynamic_import_sites: usize,
    /// Owning package identities plus `.node` names, for the compile summary.
    pub native_addons: Vec<String>,
    /// Static `--external` imports with the authored importer retained for a
    /// compiled artifact's runtime hook. Empty for the generic build path.
    pub external_imports: Vec<ExternalImport>,
    /// The public worker entry named from application code and its private
    /// bundled implementation chunk.  Compile writes a tiny public wrapper for
    /// each pair after it knows whether a runtime resolve hook is needed.
    pub worker_roots: Vec<WorkerRoot>,
}

/// One statically traceable worker. `entry` is the URL rewritten into the
/// program; `chunk` is the prelude-bearing Rolldown output it must import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRoot {
    pub entry: String,
    pub chunk: String,
}

/// One external import Rolldown must keep distinct in generated output. The
/// synthetic id is an internal compiler identity, while `specifier` is what
/// Node must resolve at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalImport {
    pub id: String,
    pub specifier: String,
    pub package: String,
    /// Importer filename relative to the directory in which the binary will be
    /// started. `None` is a virtual/non-file importer and preserves cwd lookup.
    pub importer: Option<String>,
}

#[cfg(test)]
pub fn bundle(entry_abs: &Path, opts: &BundleOptions) -> Result<BundleResult> {
    bundle_inner(entry_abs, opts, None)
}

/// Compile's artifact hook needs the original importer, whereas `nub build`
/// deliberately leaves ordinary external specifiers in its output.
pub fn bundle_for_compile(
    entry_abs: &Path,
    opts: &BundleOptions,
    launch_root: &Path,
) -> Result<BundleResult> {
    bundle_inner(entry_abs, opts, Some(launch_root))
}

fn bundle_inner(
    entry_abs: &Path,
    opts: &BundleOptions,
    launch_root: Option<&Path>,
) -> Result<BundleResult> {
    let cwd = entry_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // Rolldown resolves files through the real filesystem. On macOS `/tmp` is
    // commonly a `/private/tmp` symlink; keep its output cwd in that same
    // spelling or relative region/map ids fall back to the build-machine path.
    let cwd = canonicalize_for_bundler(&cwd);
    let stem = entry_abs
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main".into());

    let loader_plan = loaders::plan(&opts.loaders)?;

    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some(stem),
            // A virtual root statically imports the compile prelude before the
            // program.  Putting the import into the authored file would turn a
            // CommonJS entry into an ESM-classified module before Rolldown has
            // wrapped it, which changes `module.exports` semantics.  The wrapper
            // keeps the source's format intact while still making both imports
            // part of the module graph (and therefore its source map).
            import: COMPILE_ROOT_ID.into(),
        }]),
        module_types: (!loader_plan.module_types.is_empty()).then(|| {
            loader_plan
                .module_types
                .iter()
                .map(|(ext, ty)| (ext.clone(), ty.clone()))
                .collect()
        }),
        cwd: Some(cwd.clone()),
        // THESE TWO ARE A PRECONDITION, NOT A DEFAULT. Two things below assume
        // `Esm` + `Node` and would go quietly wrong if `nub build` makes either a
        // knob. [`reject_invalid_chunks`] parses every emitted chunk as `mjs`, so a
        // CJS chunk would go UNVALIDATED — key its `SourceType` on the format.
        // [`CjsPathGlobals`] splices `import.meta.url`, which Rolldown polyfills
        // ONLY for `(Node, Cjs)` and leaves verbatim everywhere else — and under a
        // CJS format Rolldown already declares both globals, so the plugin is
        // redundant there and should simply be skipped.
        format: Some(OutputFormat::Esm),
        platform: Some(Platform::Node),
        // An authored ESM module has no `require` binding. Rolldown's Node ESM
        // default installs `createRequire(import.meta.url)` for every unbound
        // `require` reference, which changes `require.main` from a ReferenceError
        // into an ordinary runtime value. Compiled artifacts preserve Node's ESM
        // semantics here. CommonJS inputs are forced into their own chunk below,
        // where [`CompilePreamble::intro`] supplies the loader their remaining
        // external/dynamic require calls need without exposing it to ESM chunks.
        polyfill_require: Some(false),
        code_splitting: Some(compile_code_splitting(entry_abs)),
        // The manual CommonJS boundary deliberately creates CJS↔ESM cross-chunk
        // edges. Rolldown's default fast ordering does not promise cycle/order
        // fidelity for that shape; its supported correctness mode does.
        strict_execution_order: Some(true),
        // Compiled chunks always execute as ESM, regardless of the source
        // package's `type` field. Keeping that fact in their extension lets Node
        // load extracted artifacts without a synthetic package boundary.
        entry_filenames: Some("[name].mjs".to_string().into()),
        chunk_filenames: Some("[name]-[hash].mjs".to_string().into()),
        minify: Some(RawMinifyOptions::Bool(opts.minify)),
        // The ONLY keep-names switch we touch. Rolldown threads this single flag
        // into both the finalizer's `__name` helper and the minifier's
        // mangle/compress keep-names, so applying it a second time (e.g. via
        // `RawMinifyOptions::Object`) would run the name-preserving transform
        // twice and break tree-shaking (vitejs/vite#9164).
        keep_names: Some(opts.keep_names),
        treeshake: treeshake_options(opts),
        define: Some(defines(opts)?),
        sourcemap: sourcemap_type(opts.sourcemap),
        sourcemap_exclude_sources: (!opts.sources_content).then_some(true),
        // Rolldown evaluates `external` before plugins. Compile needs a plugin
        // to replace each raw request with an importer-specific synthetic id,
        // while build keeps the ordinary matcher/output behavior.
        external: if launch_root.is_some() {
            validate_external_packages(&opts.external)?;
            None
        } else {
            external_matcher(&opts.external)?
        },
        resolve: Some(ResolveOptions {
            alias: alias_entries(&opts.alias)?,
            condition_names: (!opts.conditions.is_empty()).then(|| opts.conditions.clone()),
            // `module` BEFORE `main`, inverting Rolldown's node-platform default
            // (`["main", "module"]`) to Rollup's order. A legacy dual package
            // with no `exports` map points `main` at a UMD build whose factory
            // takes `require` as a PARAMETER; those `require()` calls are
            // ordinary function calls no bundler can rewrite, so they survive
            // into the artifact and throw MODULE_NOT_FOUND against the extracted
            // app dir. `module` is by definition the ESM build and has none.
            // `exports` still outranks both, so this only moves legacy packages.
            main_fields: Some(vec!["module".to_string(), "main".to_string()]),
            ..Default::default()
        }),
        transform: jsx_override(entry_abs, opts.tsconfig.as_deref()),
        // Left unset for auto-discovery. An explicit path is made absolute
        // first: Rolldown resolves a relative tsconfig against the bundler's
        // `cwd`, which is the ENTRY's directory, not the shell's.
        tsconfig: opts
            .tsconfig
            .as_ref()
            .map(|p| TsConfig::Manual(absolutize(p))),
        ..Default::default()
    };

    let scan = Arc::new(DynamicImportScan::default());
    // One collector behind both emitting plugins, so a file reached by an
    // `import` AND by a `new URL(…)` ships once.
    let collected = Arc::new(loaders::Assets::default());
    let files_plugin = Arc::new(loaders::FilePlugin::new(
        &loader_plan,
        Arc::clone(&collected),
    ));
    let prelude = Arc::new(CompilePreamble::new(entry_abs, opts.target_node)?);
    let new_urls = Arc::new(NewUrlAssets {
        collected: Arc::clone(&collected),
        files: Arc::clone(&files_plugin),
        prelude: Arc::clone(&prelude),
        project_root: cwd.clone(),
        workers: Mutex::new(BTreeMap::new()),
        modules: Mutex::new(BTreeMap::new()),
        worker_refusals: Mutex::new(Vec::new()),
    });
    let native_plugin = opts.native_target.map(|target| {
        Arc::new(native::NativeAddons::new(
            target,
            loader_plan.claims_extension("node"),
            opts.unbundled.clone(),
            opts.bundled.clone(),
        ))
    });
    let external_plugin = launch_root
        .filter(|_| !opts.external.is_empty())
        .map(|root| {
            Arc::new(ExternalImports::new(
                &opts.external,
                root,
                prelude.private_importers(),
            ))
        });
    // `CjsPathGlobals` runs LAST among the transforms that rewrite user source, so the
    // scanners ahead of it see the module as its author wrote it. `NativeAddons` is
    // pushed after it but cannot disturb that: its transform is gated on the source
    // containing `createRequire`, which `CjsPathGlobals` never emits (it splices
    // `__dirname`/`__filename` from the virtual `\0nub-path-globals` module), and its
    // rewrite blanks bytes in place without moving any position.
    let mut plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(plugin) = &external_plugin {
        // Claim raw package requests before aliases and resolver plugins.
        plugins.push(Arc::clone(plugin) as SharedPluginable);
    }
    plugins.extend([
        Arc::clone(&scan) as SharedPluginable,
        Arc::clone(&files_plugin) as SharedPluginable,
        Arc::clone(&new_urls) as SharedPluginable,
        Arc::clone(&prelude) as SharedPluginable,
        Arc::new(CjsPathGlobals) as SharedPluginable,
    ]);
    if let Some(plugin) = &native_plugin {
        plugins.push(Arc::clone(plugin) as SharedPluginable);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the bundler runtime")?;
    let output = rt.block_on(async move {
        let mut bundler = BundlerBuilder::default()
            .with_options(options)
            .with_plugins(plugins)
            .build()
            .map_err(|e| anyhow!("rolldown init:\n{}", render_diagnostics(&e)))?;
        bundler
            .generate()
            .await
            .map_err(|e| anyhow!("the bundler failed:\n{}", render_diagnostics(&e)))
    });
    // A case-mismatched extension fails inside Rolldown with a diagnostic that
    // names neither loaders nor the flag that fixes it, and a plugin cannot
    // improve that from the hook — Rolldown replaces a plugin error with its own
    // `UNLOADABLE_DEPENDENCY`. So the hint is attached here, to the failure the
    // user actually sees.
    // A rejected native addon is the same shape and needs the same treatment:
    // Rolldown reports only "plugin `nub:native-addons` threw an error", which
    // names neither the platform mismatch nor what to do about it (measured
    // 2026-07-30 — the message really is dropped, not merely reformatted).
    let output = output.map_err(|e| {
        let mut hints = files_plugin.case_hints();
        if let Some(plugin) = &native_plugin {
            hints.extend(plugin.rejections());
        }
        if hints.is_empty() {
            e
        } else {
            anyhow!("{e}\n\n{}", hints.join("\n"))
        }
    })?;

    // A plugin transform error loses its useful text when Rolldown renders the
    // diagnostic.  Collect the syntactically provable unsupported worker forms
    // while scanning, then refuse here so the author sees the source and the
    // precise v1 boundary rather than merely a plugin name.
    new_urls.reject_unsupported_workers()?;

    let sites = scan.take();
    reject_unresolved(
        &sites,
        &output.warnings,
        opts.allow_dynamic_import,
        uses_plug_n_play(entry_abs),
        nothing_installed(entry_abs),
    )?;
    let dynamic_import_sites = if opts.allow_dynamic_import {
        // Both `import()` shapes are excused by the flag and both are served by
        // the same runtime hook, so both are counted — omitting the variable-held
        // one would ship an artifact whose hook the build decided it did not need.
        let deferred: Vec<&DynamicSite> = sites
            .iter()
            .filter(|s| s.kind != SiteKind::Indirect)
            .collect();
        warn_deferred_imports(&deferred);
        deferred.len()
    } else {
        0
    };

    let mut entry = None;
    let mut files = Vec::new();
    let mut maps = Vec::new();
    let mut assets: Vec<BundledFile> = collected
        .take()
        .into_iter()
        .map(|a| BundledFile {
            name: a.name,
            bytes: a.bytes,
        })
        .collect();
    // Rolldown marks an emitted worker chunk `is_entry` exactly like the program's
    // own, so the filenames are the only thing telling them apart here.
    let worker_names = new_urls.worker_names();
    for asset in &output.assets {
        match asset {
            Output::Chunk(c) => {
                if c.is_entry && !worker_names.contains(c.filename.as_str()) {
                    entry = Some(c.filename.to_string());
                }
                files.push(BundledFile {
                    name: c.filename.to_string(),
                    bytes: c.code.as_bytes().to_vec(),
                });
            }
            Output::Asset(a) => {
                let bytes = match &a.source {
                    StrOrBytes::Str(s) => s.as_bytes().to_vec(),
                    StrOrBytes::Bytes(b) => b.clone(),
                };
                let file = BundledFile {
                    name: a.filename.to_string(),
                    bytes,
                };
                // Everything Rolldown emits that is NOT a source map is a file
                // the chunks reference by name, so dropping it (as this arm did
                // before loaders existed) would ship a chunk naming a file that
                // is not in the payload — a runtime ENOENT with nothing failing
                // at build time.
                //
                // Nothing reaches this branch today: both of nub's asset paths —
                // the `file` loader and [`NewUrlAssets`] — collect their own
                // bytes, and Rolldown's own asset path is gated on
                // `experimental.resolve_new_url_to_asset`, which defaults off and
                // nub never sets (see [`NewUrlAssets`] for why it is not the
                // mechanism used). This is kept as the correct handling rather
                // than a silent drop, so enabling that flag — or any future
                // emitting plugin — cannot quietly produce a broken artifact.
                // `--include` never routes through here at all: it embeds bytes
                // straight from disk, which is what makes it verbatim.
                if a.filename.ends_with(".map") {
                    maps.push(file);
                } else {
                    assets.push(file);
                }
            }
        }
    }
    let entry = entry.context("the bundler emitted no entry chunk")?;
    if files.is_empty() {
        bail!("the bundler produced no chunks");
    }
    // The load hook only collected cheap `.node` seeds. Package traversal and
    // copying starts here, after tree-shaking: a foreign optional variant whose
    // wrapper disappeared cannot affect this target's payload or fail the build.
    let planned_native = if let Some(plugin) = &native_plugin {
        plugin.plan_survivors(&mut files)?
    } else {
        native::PlannedNative {
            files: Vec::new(),
            summaries: Vec::new(),
            closure: closure::Plan::default(),
        }
    };
    let mut native_files = planned_native.files;
    // A second, much smaller bundle: the packages inside an eject closure that
    // did not have to be ejected. It runs here rather than inside the plugin
    // because the flags it has to match — minify, keep_names — are the caller's.
    native_files.extend(closure::bundle(
        &planned_native.closure,
        opts.minify,
        opts.keep_names,
    )?);
    let native_addons = planned_native.summaries;
    // Reported AFTER tree-shaking so a package the program never actually reaches
    // is not named. These bundled clean — a package that finds its addon through
    // `node-gyp-build` or `bindings` never puts a `.node` in the import graph — so
    // without this the build succeeds and the binary fails on the user's machine,
    // which is precisely the failure bun ships (measured: silent build, runtime
    // error naming the package's own fallback path rather than the real one).
    if let Some(plugin) = &native_plugin {
        refuse_unreached_unbundled(&plugin.unreached_forced_unbundled())?;
        report_unbundled_packages(&plugin.unbundled_summaries());
    }
    // `files` is still chunks-only here — maps are merged in below.
    reject_invalid_chunks(&files)?;
    // Also while it is chunks-only, because reachability is a property of CODE
    // and a source map is debug metadata. Scanning maps as well happens to give
    // the same answer today — measured, on every `--sourcemap` mode — but only
    // because a shaken-out module leaves nothing behind for its map to describe.
    // That is a coincidence of Rolldown's map generation, not a rule, so the
    // narrower input is the one worth depending on.
    retain_referenced(&files, &mut assets);

    // A referenced map has to travel with the chunk that names it; an external
    // one must NOT, or `--sourcemap=external` would ship the sources it exists
    // to keep out of the artifact.
    let detached_maps = if opts.sourcemap == SourcemapMode::External {
        std::mem::take(&mut maps)
    } else {
        Vec::new()
    };
    files.extend(maps);

    reject_nested_chunks(&files, &assets)?;

    // Read AFTER `retain_referenced`, so the report names what the payload
    // actually carries rather than everything the hooks happened to see.
    {
        let kept: BTreeSet<&str> = assets.iter().map(|a| a.name.as_str()).collect();
        warn_module_data_assets(&new_urls.module_assets(&kept));
    }

    Ok(BundleResult {
        entry,
        files,
        detached_maps,
        assets,
        native_files,
        support_files: prelude.support_files().collect(),
        root_support_files: prelude.root_support_files().collect(),
        dynamic_import_sites,
        native_addons,
        external_imports: external_plugin.map_or_else(Vec::new, |p| p.take()),
        worker_roots: new_urls.worker_roots(),
    })
}

/// Drop assets that no emitted chunk names.
///
/// Both asset paths record their bytes while the graph is being built — the
/// `file` loader in `load`, [`NewUrlAssets`] in `transform` — which is before
/// anything is tree-shaken. So a reference whose result goes unused still
/// collects its bytes, and without this pass a stray `import splash from
/// "./splash.mp4"` would embed the file in every artifact forever.
///
/// It prunes what the SHAKER left, so how much it recovers differs by path. The
/// `file` loader marks its synthesized module side-effect-free, so an unused
/// import is shaken and its asset dropped. A `new URL(…)` is a constructor call
/// Rolldown cannot prove pure, so an unused binding in a REACHED module keeps its
/// bytes; only an unreachable module drops them. (This is entry-language-dependent and therefore easy to
/// miss: a `.ts` entry never reaches the hook at all, because the TypeScript
/// transform elides an unused import as possibly-type-only, while the identical
/// `.js` entry does. The payload should not depend on which one you wrote.)
///
/// Reachability is decided by a substring scan for the asset's name, which is
/// exact here rather than approximate: the name carries a full SHA-256 content hash,
/// and the only way user code can reach the file is through the path string the
/// loader emitted — a name absent from every chunk is unreachable by
/// construction. Minification preserves it, since it lives in a string literal.
fn retain_referenced(chunks: &[BundledFile], assets: &mut Vec<BundledFile>) {
    if assets.is_empty() {
        return;
    }
    let code: Vec<String> = chunks
        .iter()
        .map(|c| String::from_utf8_lossy(&c.bytes).into_owned())
        .collect();
    assets.retain(|asset| code.iter().any(|c| c.contains(&asset.name)));
}

/// Assert the flat-output invariant both asset paths rest on.
///
/// A `file` import and a rewritten [`NewUrlAssets`] reference both evaluate to
/// `new URL("./<name>", import.meta.url)`, resolved against whichever CHUNK the
/// referencing module was bundled into. That is only the asset's location while
/// every chunk and every nub-emitted asset sit in one directory. Rolldown's
/// defaults do put them there, but `chunkFileNames` is configurable and a future
/// flag could nest them — at which point every asset reference would silently
/// resolve to a path that does not exist, on the deploy machine, with no
/// build-time signal. Checking is a string scan; the failure it prevents is a
/// shipped-broken binary.
///
/// Rolldown's OWN assets are exempt: it rewrites those references itself
/// (`compute_relative_path`), so a nested `assets/x-HASH.png` stays correct.
fn reject_nested_chunks(chunks: &[BundledFile], assets: &[BundledFile]) -> Result<()> {
    let nested: Vec<&str> = chunks
        .iter()
        .map(|f| f.name.as_str())
        .filter(|n| n.contains('/'))
        .collect();
    if !nested.is_empty() {
        bail!(
            "the bundler emitted chunks in a subdirectory ({}), which breaks the \
             path a `file` import resolves to — assets are emitted as siblings of \
             the chunks and located relative to them.",
            nested.join(", ")
        );
    }
    // nub's own asset names are single components by construction; anything else
    // here came from a code path that bypassed `loaders::asset_name`.
    for a in assets {
        if !nub_core::compile::is_safe_relative_name(&a.name) {
            bail!(
                "the bundler emitted an asset with an unusable name: {:?}",
                a.name
            );
        }
    }
    Ok(())
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
}

// ---- CommonJS `__dirname` / `__filename` ---------------------------------------

/// Gives a bundled CommonJS module the `__dirname` / `__filename` it had on plain
/// Node, anchored — like every other path this compiler emits — on
/// `import.meta.url`.
///
/// THE DEFECT. The output format is ESM, and Rolldown pre-declares
/// `module`/`require`/`__filename`/`__dirname` only for a CJS *output* format
/// (`rolldown::utils::renamer`). A CJS dependency wrapped into an ESM bundle
/// therefore keeps `exports` and `module` but loses the two path globals, and
/// dies on first use with `ReferenceError: __dirname is not defined in ES module
/// scope` — a runtime failure, inside a frozen binary, in third-party code the app
/// author cannot edit. A large share of real CJS packages reference `__dirname`,
/// so this silently broke ordinary dependencies.
///
/// WHY A PER-MODULE TRANSFORM AND NOT `inject` OR A BANNER. Rolldown's `inject`
/// option rewrites free references bundle-wide, so it cannot tell a CJS dependency
/// apart from a genuine ES module — and an ES module has no `__dirname` on Node,
/// so handing it one would make nub run code that plain Node rejects. A chunk
/// `banner` is worse: its declarations are raw text the renamer never sees, so a
/// user module with its own top-level `__dirname` would emit a duplicate `const`
/// and fail to parse. The transform hook is the only one of the three that can
/// scope the shim to CJS-origin code.
///
/// WHAT THE VALUE IS, AND WHY THAT IS THE HONEST ANSWER. `__dirname` resolves to
/// the directory of the running chunk — the extracted app dir — not to the
/// module's old `node_modules` path. Bundling fuses every module into one chunk,
/// so a per-module directory no longer exists at runtime, and the entry's own
/// directory is the one every other runtime path here already resolves against —
/// `import.meta.dirname` in bundled code lands in exactly the same place. Deriving
/// it from `import.meta.url` rather than `process.cwd()` is what keeps it correct
/// from any cwd and inside a content-hashed cache dir, exactly as the `file` loader
/// and [`NewUrlAssets`] already do.
///
/// WHY THE URL COMES FROM A VIRTUAL MODULE INSTEAD OF `import.meta.url` INLINE.
/// Rolldown parses a `.cjs`/`.cts` module with `with_commonjs(true)`
/// (`rolldown::utils::parse_to_ecma_ast`), and `import.meta` is a SYNTAX ERROR in a
/// non-module source — so splicing it straight into the module turns a build that
/// used to succeed into a hard failure, on the very extension that most reliably
/// means CommonJS. `.js` hid this, because an unknown or `type: "commonjs"` format
/// is parsed `with_module(true)`. Reading the URL out of an ES module that the CJS
/// module `require`s puts `import.meta` where it is legal and leaves a plain call
/// behind, which parses under either source type — one path for every CJS shape.
///
/// ESM-OUTPUT-ONLY, and [`bundle`]'s `format`/`platform` fields carry the note for
/// whoever makes either a knob. The shim is merely REDUNDANT under a CJS format
/// rather than wrong — Rolldown declares both names there, the splice shadows them
/// inside the `__commonJS` closure, and the shadowed value is identical because the
/// virtual module's `import.meta.url` becomes `pathToFileURL(__filename).href`,
/// which `fileURLToPath` turns straight back into `__filename`. That module is
/// emitted at chunk ROOT, outside every closure, so its own `__filename` is always
/// Rolldown's and never a shadow — there is no cycle.
#[derive(Debug)]
struct CjsPathGlobals;

/// The module both spliced declarations read from. Rollup's `\0` prefix marks an id
/// as plugin-owned, so it can never collide with a package a user could install.
const PATH_GLOBALS_ID: &str = "\0nub-path-globals";

/// Its source. An ES module, so `import.meta` is legal HERE — which is the whole
/// point. It carries neither `__dirname` nor `__filename` as text, so the transform
/// hook's own cheap reject skips it.
const PATH_GLOBALS_SOURCE: &str = concat!(
    "const { fileURLToPath: __nubToPath } = process[Symbol.for(\"nub.compile.bootstrap\")].getBuiltin(\"node:url\");\n",
    "const { dirname: __nubDirname } = process[Symbol.for(\"nub.compile.bootstrap\")].getBuiltin(\"node:path\");\n",
    "export const file = __nubToPath(import.meta.url);\n",
    "export const dir = __nubDirname(file);\n",
);

impl Plugin for CjsPathGlobals {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:cjs-path-globals")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load | HookUsage::Transform
    }

    fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let claimed = args.specifier == PATH_GLOBALS_ID;
        async move { Ok(claimed.then(|| HookResolveIdOutput::from_id(PATH_GLOBALS_ID))) }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let ours = args.id == PATH_GLOBALS_ID;
        async move {
            Ok(ours.then(|| HookLoadOutput {
                code: PATH_GLOBALS_SOURCE.into(),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }))
        }
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let scannable = matches!(
            args.module_type,
            ModuleType::Js | ModuleType::Jsx | ModuleType::Ts | ModuleType::Tsx
        );
        let inserts = if scannable {
            commonjs_source_inserts(clean_url(args.id), args.code)
        } else {
            Vec::new()
        };
        let source = (!inserts.is_empty()).then(|| args.code.to_string());
        async move {
            let Some(source) = source else {
                return Ok(None);
            };
            Ok(Some(HookTransformOutput {
                // Spliced WITHOUT a newline, so every line of the module still
                // starts on the line it started on and `Null` stays honest about
                // line positions — the same bargain [`rewrite_new_urls`] makes, and
                // for the same reason: `Omitted` would make Rolldown drop the
                // module's mapping entirely and warn on every build. Only the
                // columns after a splice, on its own line, shift.
                code: Some(apply_source_inserts(&source, inserts)),
                map: HookTransformOutputMap::Null,
                ..Default::default()
            }))
        }
    }
}

/// Every correction a module needs before Rolldown scans it, as `(byte offset,
/// text)` pairs. Both are pure insertions and neither reads the other's output, so
/// they are independent and order-free.
fn commonjs_source_inserts(path: &str, source: &str) -> Vec<(usize, String)> {
    let mut inserts = concise_arrow_require_inserts(path, source);
    if let Some((at, decls)) = cjs_path_globals_edit(path, source) {
        inserts.push((at, decls));
    }
    inserts
}

/// Applying from the highest offset down keeps every lower offset valid, so no
/// insertion has to be rebased against the ones before it.
fn apply_source_inserts(source: &str, mut inserts: Vec<(usize, String)>) -> String {
    inserts.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let mut out = source.to_string();
    for (at, text) in inserts {
        out.insert_str(at, &text);
    }
    out
}

/// Give a `require()` written as the entire concise body of an arrow function an
/// explicit block body, so its VALUE survives bundling.
///
/// This works around a Rolldown scanner misread rather than a nub defect.
/// `process_global_require_call` decides a require's result is discarded by walking
/// outward to the nearest `ExpressionStatement` — and oxc represents a concise
/// arrow body as a `FunctionBody` holding exactly that node. So `() => require(x)`
/// is flagged `IsRequireUnused`, and the finalizer emits a bare `init_xxx()` in
/// place of `(init_xxx(), __toCommonJS(xxx_exports))`. Where the required module is
/// ESM that call evaluates to `undefined` — silently, with no error, no warning and
/// a successful compile. A CommonJS importee is unaffected because its wrapper
/// returns `module.exports` either way, and any surrounding expression (`() =>
/// require(x).y`) escapes the misread by ending the walk early. Verified against
/// rolldown 1.2.0 and still present on upstream `main`.
///
/// `() => e` and `() => { return e; }` are the same program for every arrow, async
/// included, so the rewrite needs no CommonJS gating: on a module Rolldown
/// classifies as ESM it is inert rather than wrong.
fn concise_arrow_require_inserts(path: &str, source: &str) -> Vec<(usize, String)> {
    use oxc_allocator::Allocator;
    use oxc_ast_visit::Visit;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    // Cheap reject first: this runs on every module in the graph.
    if !source.contains("require") || !source.contains("=>") {
        return Vec::new();
    }
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    let mut scan = ConciseArrowRequireScan::default();
    scan.visit_program(&parsed.program);
    scan.bodies
        .into_iter()
        .flat_map(|(start, end)| [(start, "{return ".to_owned()), (end, ";}".to_owned())])
        .collect()
}

/// The span of every concise arrow body that is exactly a build-time `require`.
#[derive(Default)]
struct ConciseArrowRequireScan {
    bodies: Vec<(usize, usize)>,
}

impl<'a> oxc_ast_visit::Visit<'a> for ConciseArrowRequireScan {
    fn visit_arrow_function_expression(&mut self, it: &oxc_ast::ast::ArrowFunctionExpression<'a>) {
        if it.expression {
            if let Some(oxc_ast::ast::Statement::ExpressionStatement(stmt)) =
                it.body.statements.first()
            {
                if is_static_require_call(stmt.expression.get_inner_expression()) {
                    let span = oxc_span::GetSpan::span(&stmt.expression);
                    self.bodies.push((span.start as usize, span.end as usize));
                }
            }
        }
        oxc_ast_visit::walk::walk_arrow_function_expression(self, it);
    }
}

/// A `require("literal")` Rolldown resolves at build time — the only shape its
/// scanner records, and so the only one this correction may touch.
fn is_static_require_call(expr: &Expression<'_>) -> bool {
    let Expression::CallExpression(call) = expr else {
        return false;
    };
    let Expression::Identifier(callee) = call.callee.get_inner_expression() else {
        return false;
    };
    callee.name == "require"
        && matches!(
            call.arguments.first(),
            Some(
                oxc_ast::ast::Argument::StringLiteral(_)
                    | oxc_ast::ast::Argument::TemplateLiteral(_)
            )
        )
}

/// Where to splice the declarations a CJS-origin module needs, and what they are —
/// or `None` to leave the module alone.
///
/// `require` rather than an `import`: adding an import statement to a module that
/// uses `module.exports` flips Rolldown's own ESM/CommonJS classification and
/// breaks its exports. `import.meta.url` survives verbatim because Rolldown
/// rewrites it only for a CJS output format, and is NOT one of the signals that
/// classification reads — both confirmed against a compiled binary.
fn cjs_path_globals_edit(path: &str, source: &str) -> Option<(usize, String)> {
    use oxc_allocator::Allocator;
    use oxc_ast_visit::Visit;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    // Cheap reject first: this hook sees every module in the graph and almost none
    // of them name either global.
    if !source.contains("__dirname") && !source.contains("__filename") {
        return None;
    }
    // Node settles these by extension before looking at anything else, and so does
    // Rolldown (`ModuleDefFormat::from_path`).
    let ext = Path::new(path).extension().and_then(|e| e.to_str());
    if matches!(ext, Some("mjs" | "mts")) {
        return None;
    }
    let extension_is_commonjs = matches!(ext, Some("cjs" | "cts"));

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || has_esm_syntax(&parsed.program) {
        return None;
    }

    let mut scan = PathGlobalScan::default();
    scan.visit_program(&parsed.program);
    // A module that binds either name owns it — a UMD wrapper declaring its own
    // `var __dirname` is the common shape — and prepending a `const` of the same
    // name would be a redeclaration the chunk cannot parse. Skipping on a binding
    // ANYWHERE, not just at the top level, costs only the exotic module that
    // shadows the name in an inner scope while relying on the global outside it.
    if scan.binds {
        return None;
    }
    // POSITIVE evidence of CommonJS is required, never merely the absence of ESM
    // keywords. Absence is not enough because Rolldown's last classification step
    // reads the package's `"type"` field, which no transform-hook argument carries
    // — so a `"type": "module"` file with no imports, no exports and no
    // `module`/`exports` is a genuine ES module that "has no ESM syntax", and
    // shimming it handed `__dirname` to code plain Node answers with a
    // `ReferenceError`. The cost of requiring evidence is the mirror-image miss: a
    // bare `.js` SCRIPT in a CommonJS package that reads `__dirname` and touches
    // nothing else is left alone. That direction is safe — it is exactly what this
    // compiler did before — while the other direction invents behavior Node does
    // not have.
    if !scan.commonjs && !extension_is_commonjs {
        return None;
    }
    let bindings = match (scan.filename, scan.dirname) {
        (true, true) => "{ file: __filename, dir: __dirname }",
        (true, false) => "{ file: __filename }",
        (false, true) => "{ dir: __dirname }",
        (false, false) => return None,
    };
    let decls = format!(
        ";const {bindings} = require(\"\\0{}\");",
        &PATH_GLOBALS_ID[1..]
    );
    Some((splice_point(&parsed.program, source), decls))
}

/// The byte offset at which the declarations may be spliced without changing what
/// the module means or which line anything is on.
///
/// Byte 0 is WRONG in two shapes that real packages have. A `"use strict"`
/// directive only counts while it is still in the directive prologue, so text in
/// front of it silently makes a strict module sloppy; and a hashbang is only a
/// hashbang at byte 0, so text in front of THAT is a parse error. Splicing after
/// the prologue fixes both. It also has to land after the last directive rather
/// than on the next line, because `const` has a temporal dead zone — a module with
/// `"use strict"; …__dirname…` all on one line would otherwise reference the
/// binding above its own declaration. The leading `;` makes the splice correct
/// whether or not the directive's own span already covered its semicolon.
fn splice_point(program: &Program<'_>, source: &str) -> usize {
    if let Some(last) = program.directives.last() {
        return last.span.end as usize;
    }
    let Some(hashbang) = program.hashbang.as_ref() else {
        return 0;
    };
    // A hashbang runs to end of line and would swallow anything appended to it, so
    // this is the one case that starts the next line instead.
    let end = hashbang.span.end as usize;
    source[end..]
        .find('\n')
        .map_or(source.len(), |i| end + i + 1)
}

/// Whether the module carries ESM syntax, mirroring the two keyword signals
/// Rolldown's own scanner reads (`ast_scanner`: an `export` keyword forces ESM, an
/// `import` keyword decides an otherwise-unclassified module). A module with
/// neither is CommonJS or a bare script, and on Node both have `__dirname`.
fn has_esm_syntax(program: &Program<'_>) -> bool {
    use oxc_ast::ast::Statement;

    program.body.iter().any(|stmt| {
        matches!(
            stmt,
            Statement::ImportDeclaration(_)
                | Statement::ExportNamedDeclaration(_)
                | Statement::ExportDefaultDeclaration(_)
                | Statement::ExportAllDeclaration(_)
        )
    })
}

/// Which of the two globals a module reads, whether it binds any of the three names
/// the splice depends on, and whether it shows evidence of being CommonJS.
#[derive(Default)]
struct PathGlobalScan {
    dirname: bool,
    filename: bool,
    binds: bool,
    /// Reads `module`, `exports` or `require` — the same identifiers Rolldown's own
    /// classifier treats as proof of CommonJS, one rung above the package `"type"`
    /// field that a transform hook cannot see.
    commonjs: bool,
}

impl<'a> oxc_ast_visit::Visit<'a> for PathGlobalScan {
    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        match it.name.as_str() {
            "__dirname" => self.dirname = true,
            "__filename" => self.filename = true,
            "module" | "exports" | "require" => self.commonjs = true,
            _ => {}
        }
    }

    fn visit_binding_identifier(&mut self, it: &oxc_ast::ast::BindingIdentifier<'a>) {
        // `require` joins the two globals here because the splice CALLS it. A UMD
        // factory takes `require` as a parameter, so inside one the name is the
        // caller's function, not the module loader — splicing a call to it would
        // hand a browser shim an id it has never heard of.
        if matches!(it.name.as_str(), "__dirname" | "__filename" | "require") {
            self.binds = true;
        }
    }
}

// ---- `new URL(…, import.meta.url)` asset embedding -----------------------------

// ---- Compile prelude roots ------------------------------------------------------

/// Virtual module ids owned by [`CompilePreamble`].  The leading NUL puts them in
/// Rollup's plugin-only namespace, so neither a package nor a user file can
/// collide with the compiler's roots.
const COMPILE_ROOT_ID: &str = "\0nub:compile-root";
const COMPILE_PREAMBLE_ID: &str = "\0nub:compile-preamble";
const COMPILE_COMMONJS_CHUNK: &str = "_nub_commonjs";
const COMPILE_COMMONJS_REQUIRE_MARKER: &str = "var __nubRequireCache; function __nubRequire() { return (__nubRequireCache ??= process[Symbol.for(\"nub.compile.bootstrap\")].createRequire(import.meta.url)); } function require(id) { return __nubRequire()(id); }";

/// Keep CommonJS inputs in a lexical scope application ESM can never share.
///
/// `polyfill_require: false` is bundle-global, while the desired semantics are
/// not: authored ESM must keep `require` unbound, but bundled CommonJS needs a
/// real Node loader for builtins, externals, and unanalyzable calls Rolldown
/// intentionally leaves in the output. Rolldown exposes its final per-module
/// classification to manual chunk naming, after parsing and package-boundary
/// resolution have settled ambiguities which a source transform cannot settle.
/// Grouping exactly those inputs creates a reliable lexical boundary; dependency
/// recursion is deliberately off so an authored ESM dependency can never be
/// pulled in. The one explicit ESM member is Nub's own path-globals bridge: it
/// must evaluate `import.meta.url` in the chunk whose `__filename` it supplies.
/// Placement does not make CommonJS eager: Rolldown retains each input's
/// `__commonJS` wrapper and cross-chunk links invoke that cached wrapper at the
/// original import/require site. `strict_execution_order` above enables its
/// cycle/order-preserving linker path, while each wrapper's early module cache
/// preserves partial exports through CommonJS cycles. The shared chunk evaluates
/// only wrapper declarations.
fn compile_code_splitting(entry_abs: &Path) -> CodeSplittingMode {
    // Resolved ONCE here rather than per module: it is the same path on every
    // call, and the predicate below runs across the whole graph.
    let entry: Arc<Path> =
        Arc::from(std::fs::canonicalize(entry_abs).unwrap_or_else(|_| entry_abs.to_path_buf()));
    CodeSplittingMode::Advanced(ManualCodeSplittingOptions {
        groups: Some(vec![MatchGroup {
            name: MatchGroupName::Dynamic(Arc::new(move |id, ctx| {
                let commonjs_scope = id == PATH_GLOBALS_ID
                    || authored_entry_is_node_commonjs(id, &entry)
                    || ctx
                        .get_module_info(id)
                        .is_some_and(|module| is_node_commonjs_module(&module))
                    || is_commonjs_only_data_module(id, ctx);
                Box::pin(
                    async move { Ok(commonjs_scope.then(|| COMPILE_COMMONJS_CHUNK.to_string())) },
                )
            })),
            include_dependencies_recursively: Some(false),
            ..Default::default()
        }]),
        ..Default::default()
    })
}

/// The authored entry, classified the way NODE would rather than the way
/// Rolldown's scanner does.
///
/// Rolldown decides CommonJS from `module`/`exports` markers, so a `.js` entry
/// that only CALLS `require` — with no marker anywhere — is scanned as ESM. Node
/// disagrees: absent a `"type"` field the nearest package.json makes it
/// CommonJS, and it runs. Without this the entry misses the CommonJS chunk, its
/// `require` never gets the chunk's lexical binding, and the artifact dies with
/// `require is not defined in ES module scope` — after a build that exited 0. It
/// only surfaces when the entry requires a BUILTIN or an ejected package, since a
/// require of a bundlable package is inlined and leaves nothing behind.
///
/// Scoped to the one authored entry ON PURPOSE. Widening
/// [`is_node_commonjs_module`] itself to ignore Rolldown's verdict also fixes
/// this and then breaks six unrelated tests: virtual roots, worker roots and
/// loader-emitted modules all reach that predicate, Node's extension rule claims
/// them too, and a worker root pulled into this chunk stops being emitted as its
/// own. The entry is the only module whose format Node has already decided and
/// whose `require` the user wrote.
fn authored_entry_is_node_commonjs(id: &str, entry: &Path) -> bool {
    let id = clean_url(id);
    let path = Path::new(id);
    // Extension first, deliberately. This predicate is called for EVERY module in
    // the graph and at most one of them can match, so the cheap test has to come
    // before anything that touches the filesystem — otherwise a large application
    // pays two `canonicalize` syscalls per module to answer "no" a few thousand
    // times. `.js` is also the only extension that can qualify (see below), so
    // nothing is lost by checking it up front.
    if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
        return false;
    }
    // Compared through `canonicalize` because a raw `Path` equality misses the
    // same file reached by a different prefix — /tmp vs /private/tmp on macOS is
    // the everyday case, and the cost of a miss is the silent runtime failure
    // this whole predicate exists to prevent. `entry` arrives already canonical
    // (resolved once when the closure was built), so only the id is resolved
    // here. Falls back to the lexical compare when the id names no real file.
    let same = match std::fs::canonicalize(path) {
        Ok(resolved) => resolved == entry,
        Err(_) => path == entry,
    };
    if !same {
        return false;
    }
    // ONLY `.js`. That is the whole gap: `.cjs`/`.cts` already reach the chunk
    // because Rolldown classifies them from the extension, and `.mjs`/`.mts` are
    // ESM to both. TypeScript is deliberately excluded even though Node applies
    // the same package-type rule to it — nub transpiles `.ts` through the ESM
    // path, and claiming it here pulls worker roots and loader-emitted modules
    // into the CommonJS chunk, which stops their chunks being emitted at all
    // (measured: 4 tests red, all of them worker/loader/new-URL cases).
    node_package_defaults_to_commonjs(id)
}

/// Keep a JSON module in the CommonJS chunk when only CommonJS reaches it.
///
/// Without this the group splits a package across two chunks and the halves
/// import each other. A `require("./data.json")` inside a dynamically imported
/// CommonJS package is the shape that shows it: the package's own module is
/// CommonJS so the group claims it, while the JSON stays in the dynamic import's
/// chunk. That chunk then imports the wrapper it needs from `_nub_commonjs`, and
/// `_nub_commonjs` imports the JSON wrapper back out of it.
///
/// The cycle is not survivable, because a dynamic import of a CommonJS module
/// emits an EAGER `export default require_pkg()` at the top of its chunk. ESM
/// evaluates one side of a cycle to completion while the other is still partway
/// through its own body, so that call runs before `var require_pkg` has been
/// assigned and throws `require_pkg is not a function`. Co-locating the JSON
/// removes the edge that closes the cycle.
///
/// Only JSON qualifies, and deliberately so. Extending this to modules in
/// general would move authored ESM — which a CommonJS module can `require()` on
/// Node 22+ — into a chunk carrying a lexical `require`, exactly the binding
/// `compile_code_splitting`'s group exists to keep away from ESM. A JSON module
/// is data with no user code, so it cannot observe that binding.
fn is_commonjs_only_data_module(id: &str, ctx: &rolldown_common::ChunkingContext) -> bool {
    if Path::new(clean_url(id))
        .extension()
        .and_then(|ext| ext.to_str())
        != Some("json")
    {
        return false;
    }
    let Some(module) = ctx.get_module_info(id) else {
        return false;
    };
    // An entry, or a module something imports dynamically, is a chunk root in its
    // own right; moving it would change what the graph loads and when.
    if module.is_entry || !module.dynamic_importers.is_empty() || module.importers.is_empty() {
        return false;
    }
    module.importers.iter().all(|importer| {
        ctx.get_module_info(importer.as_str())
            .is_some_and(|importer| is_node_commonjs_module(&importer))
    })
}

/// Apply the chunk boundary only when Node and Rolldown agree on CommonJS.
///
/// Rolldown's scanner lets CommonJS `module`/`exports` markers outweigh even an
/// `.mjs` suffix; Node does not. Trusting `input_format` alone would therefore
/// move authored ESM into the loader-bearing chunk and silently make code run
/// where plain Node throws. The scanner result is still the first gate because
/// only a module Rolldown will wrap can safely consume the chunk's lexical
/// `require`.
fn is_node_commonjs_module(module: &rolldown_common::ModuleInfo) -> bool {
    if !module.input_format.is_commonjs() {
        return false;
    }
    let id = clean_url(module.id.as_str());
    match Path::new(id).extension().and_then(|ext| ext.to_str()) {
        Some("cjs" | "cts") => true,
        Some("mjs" | "mts") => false,
        // Rolldown has settled whether it will emit a CommonJS wrapper. Node's
        // explicit extension/package classification is the remaining gate.
        _ => node_package_defaults_to_commonjs(id),
    }
}

/// A synchronous loader intro for CommonJS chunks. Its lexical `require` is
/// isolated by the manual chunk boundary, and the payload-root bootstrap has
/// already installed the private builtin registry before any chunk evaluates.
///
/// `require` is a FUNCTION DECLARATION, not a `const`, for the same reason
/// [`hoist_module_wrappers`] rewrites Rolldown's wrappers: a chunk in an import
/// cycle can be re-entered before its own body has run, and a `const` is in its
/// temporal dead zone then — `Cannot access 'require' before initialization`
/// before any application code. A function declaration is initialized when the
/// module is instantiated, so the loader is callable from the first moment any
/// chunk can reach it.
///
/// The properties are attached in body order, so a cycle that reaches
/// `require.resolve` before this chunk's body runs still sees it missing. That
/// window is far narrower than the one this closes: the intro is the chunk's
/// first statement, and reaching a property means calling into the loader
/// earlier than the loader's own chunk starts.
fn compile_commonjs_require_intro() -> String {
    format!(
        "{COMPILE_COMMONJS_REQUIRE_MARKER}\n\
         require.resolve = (id, options) => __nubRequire().resolve(id, options);\n\
         Object.defineProperty(require, \"cache\", {{ get: () => __nubRequire().cache }});\n\
         Object.defineProperty(require, \"main\", {{ get: () => __nubRequire().main }});\n\
         Object.defineProperty(require, \"extensions\", {{ get: () => __nubRequire().extensions }});\n"
    )
}

/// Rolldown's lazy module wrappers, as function declarations instead of `var`s.
///
/// Rolldown emits one wrapper per non-inlined module:
///
/// ```js
/// var require_pkg = /* @__PURE__ */ __commonJSMin(((exports, module) => { … }));
/// var init_mod    = /* @__PURE__ */ __esmMin((() => { … }));
/// ```
///
/// A `var` is assigned when the statement runs, but a chunk's `import`s are
/// hoisted above every statement in it. So when two chunks import each other,
/// ESM finishes one side while the other is still partway through its body, and
/// whichever wrapper the finished side calls is still `undefined`. Real graphs
/// hit this constantly: an eager `export default require_pkg()` at the head of a
/// dynamic import's chunk throws `require_pkg is not a function`, and an `async`
/// init deadlocks into an unsettled top-level await instead — no error, exit 13.
///
/// A function declaration is initialized at INSTANTIATION, before any body in the
/// cycle runs, so rewriting each wrapper to
///
/// ```js
/// var __nub_lazy_require_pkg;
/// function require_pkg() { return (__nub_lazy_require_pkg ??= __commonJSMin(((exports, module) => { … }))).apply(this, arguments) }
/// ```
///
/// makes the call safe from the first moment the binding is reachable. The
/// wrapper is still built on first call and `__commonJSMin` still memoizes it, so
/// evaluation ORDER is unchanged — only the window in which the name is callable
/// widens. Chunk membership is untouched, and so is the CommonJS/ESM `require`
/// isolation that `compile_code_splitting` exists to protect.
///
/// This runs in `render_chunk`, which Rolldown drives BEFORE `minify_chunks`, so
/// the helper names are still the readable ones matched below rather than mangled
/// single letters.
///
/// The forwarder takes no parameters and reads `arguments` instead. A rest
/// parameter would introduce a binding INSIDE the wrapper, shadowing any
/// same-named module-scope variable the wrapped body closes over — and bundled
/// output is full of one-letter names, so `(...a)` silently rebound `a` for a
/// whole module. That failed as `a is not a function` deep inside a command
/// handler, long after the build reported success.
const ROLLDOWN_MODULE_WRAPPERS: [&str; 4] = ["__commonJS", "__commonJSMin", "__esm", "__esmMin"];

/// Rewrite every top-level Rolldown module wrapper in one chunk. Returns `None`
/// when the chunk has none, so an untouched chunk keeps its original bytes.
fn hoist_module_wrappers(code: &str) -> Option<String> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{Expression, Statement};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, SourceType::mjs()).parse();
    if parsed.panicked {
        // An unparseable chunk is already fatal further down the pipeline
        // (`reject_invalid_chunks`); reporting it there keeps one error path.
        return None;
    }

    let mut magic = MagicString::new(code.to_owned());
    let mut rewrote = false;
    for statement in &parsed.program.body {
        let Statement::VariableDeclaration(decl) = statement else {
            continue;
        };
        if !decl.kind.is_var() || decl.declarations.len() != 1 {
            continue;
        }
        let declarator = &decl.declarations[0];
        let Some(name) = declarator.id.get_binding_identifier() else {
            continue;
        };
        let Some(Expression::CallExpression(call)) = declarator.init.as_ref() else {
            continue;
        };
        let Expression::Identifier(callee) = &call.callee else {
            continue;
        };
        if !ROLLDOWN_MODULE_WRAPPERS.contains(&callee.name.as_str()) {
            continue;
        }

        let name = name.name.as_str();
        let lazy = format!("__nub_lazy_{name}");
        // `var <lazy>; function <name>(...a) { return (<lazy> ??= ` replaces
        // everything up to the wrapper call, dropping the `/* @__PURE__ */` with
        // it — tree-shaking has already run by render_chunk, so the annotation
        // has no reader left.
        // A failed range abandons the WHOLE chunk rather than emitting a
        // half-rewritten one: `magic` is discarded with the `None`, so the chunk
        // ships exactly as Rolldown rendered it. The spans come from this same
        // parse, so this is a guard, not an expected path.
        if magic
            .update(
                decl.span.start,
                call.span.start,
                format!("var {lazy}; function {name}() {{ return ({lazy} ??= "),
            )
            .is_err()
            || magic
                .update(
                    call.span.end,
                    decl.span.end,
                    ").apply(this, arguments) }".to_string(),
                )
                .is_err()
        {
            return None;
        }
        rewrote = true;
    }
    rewrote.then(|| magic.to_string())
}

/// Supplies the program and worker root wrappers plus the prelude source itself.
///
/// A wrapper, rather than a textual import prepended to every authored root, is
/// intentional.  A static ESM import in a `.cjs` root changes Rolldown's module
/// classification before its CommonJS pass sees `module.exports`; importing that
/// source FROM an ESM wrapper preserves CommonJS semantics and still emits ESM.
/// The wrapper's two static imports give the prelude unconditional side effects
/// before any program or worker code can run.
#[derive(Debug)]
struct CompilePreamble {
    /// Virtual root id → the user-authored source it imports.  `BTreeMap` keeps
    /// the generated wrapper ids and test output deterministic.
    roots: Mutex<BTreeMap<String, PathBuf>>,
    /// The public runtime directory found from the running Nub binary. In a
    /// release this is the embedded-runtime extraction cache, never Cargo's
    /// source checkout; in development it is the live `runtime/` directory.
    runtime_dir: PathBuf,
    /// Loaded before Rolldown starts, so a missing runtime gives a normal compile
    /// error rather than an opaque plugin failure.
    source: String,
    /// Physical modules reached from the private prelude. Externalization runs
    /// before this plugin, so both plugins share the set to keep `--external`
    /// from intercepting any transitive prelude dependency.
    private_importers: Arc<Mutex<BTreeSet<String>>>,
    /// Runtime helpers intentionally loaded through `createRequire(import.meta.url)`
    /// rather than an ESM import. They must remain real sibling files after the
    /// prelude is bundled; otherwise the emitted chunk resolves them from the
    /// extraction directory and fails before application code runs.
    support_files: Vec<(String, Vec<u8>)>,
    /// Runtime bootstrap extracted at the payload root rather than beside the
    /// content-addressed bundle layout. The launcher loads it before the entry.
    root_support_files: Vec<(String, Vec<u8>)>,
}

/// Polyfills the compile preamble installs, and the first Node version that ships
/// each one natively.
///
/// `nub compile` bakes an exact target Node, so a polyfill for a global that target
/// already has is dead weight the program still pays to parse and construct. On a
/// hello-world bundle that was ~195 KB of 206 KB and ~18 ms of every launch, almost
/// all of it Temporal.
///
/// Each version here is one this repo has actually RUN and probed, not one read off
/// a changelog — and deliberately the first version confirmed native rather than the
/// first version that plausibly shipped it. Erring high ships a redundant polyfill;
/// erring low ships a binary that references a global its runtime does not have. The
/// two failures are not comparable, so the table stays conservative.
///
/// Probed: 18.19 and 20.19 have none of these. 22.x has `navigator` only. 24.1 adds
/// `URLPattern` and `Float16Array`. 24.9 adds `navigator.locks`. 26.0 adds `Temporal`.
/// `Worker` is native on no supported version, so it has no entry and is never
/// stripped.
const POLYFILL_NATIVE_FROM: &[(&str, (u64, u64, u64))] = &[
    ("navigator", (22, 0, 0)),
    ("urlpattern", (24, 1, 0)),
    ("float16", (24, 1, 0)),
    ("navigatorlocks", (24, 9, 0)),
    ("temporal", (26, 0, 0)),
];

/// Remove `// #region nub:polyfill:<name>` blocks whose global the target Node has
/// natively.
///
/// Stripping the SOURCE rather than dead-code-eliminating later is what keeps the
/// dependency out of the module graph entirely — a static `import` has side effects,
/// so no amount of tree-shaking drops `@js-temporal/polyfill` once it is imported.
/// Every region is written to be independently removable and to leave valid syntax,
/// and the preamble's own comment says so, because that invariant lives across two
/// files.
///
/// `None` (a `--smol` artifact whose runtime is unknown until launch) strips nothing.
fn strip_native_polyfills(source: &str, target: Option<(u64, u64, u64)>) -> String {
    let Some(target) = target else {
        return source.to_string();
    };
    let native: Vec<&str> = POLYFILL_NATIVE_FROM
        .iter()
        .filter(|(_, from)| target >= *from)
        .map(|(name, _)| *name)
        .collect();
    if native.is_empty() {
        return source.to_string();
    }
    let mut out = String::with_capacity(source.len());
    let mut skipping = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("// #region nub:polyfill:") {
            if native.contains(&name) {
                skipping = true;
                continue;
            }
        }
        if skipping {
            if trimmed == "// #endregion" {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

impl CompilePreamble {
    fn new(entry: &Path, target_node: Option<(u64, u64, u64)>) -> Result<Self> {
        let runtime_dir = compile_runtime_dir()?;
        let prelude = runtime_dir.join("compile-preamble.mjs");
        let source = std::fs::read_to_string(&prelude)
            .with_context(|| format!("reading the compile prelude at {}", prelude.display()))?;
        let source = strip_native_polyfills(&source, target_node);
        let worker_blob = runtime_dir.join("worker-blob-url.cjs");
        let worker_blob_bytes = std::fs::read(&worker_blob).with_context(|| {
            format!(
                "reading compile runtime support at {}",
                worker_blob.display()
            )
        })?;
        // Two distinct names, deliberately: the runtime tree ships this as its own
        // source filename, while the payload publishes it under the `__nub_`-prefixed
        // fixed root name so it cannot collide with an application file. Reading by
        // the payload name finds nothing and fails every compile.
        let bootstrap = runtime_dir.join("compile-bootstrap.cjs");
        let bootstrap_bytes = std::fs::read(&bootstrap).with_context(|| {
            format!(
                "reading compile runtime root support at {}",
                bootstrap.display()
            )
        })?;
        let mut prelude = Self::from_source(entry, runtime_dir, source);
        prelude
            .support_files
            .push(("worker-blob-url.cjs".to_string(), worker_blob_bytes));
        prelude.root_support_files.push((
            nub_core::compile::COMPILE_BOOTSTRAP_NAME.to_string(),
            bootstrap_bytes,
        ));
        Ok(prelude)
    }

    fn from_source(entry: &Path, runtime_dir: PathBuf, source: String) -> Self {
        let entry = canonicalize_for_bundler(entry);
        Self {
            roots: Mutex::new(BTreeMap::from([(COMPILE_ROOT_ID.to_string(), entry)])),
            runtime_dir,
            source,
            private_importers: Arc::new(Mutex::new(BTreeSet::from([
                COMPILE_PREAMBLE_ID.to_string()
            ]))),
            support_files: Vec::new(),
            root_support_files: Vec::new(),
        }
    }

    fn private_importers(&self) -> Arc<Mutex<BTreeSet<String>>> {
        Arc::clone(&self.private_importers)
    }

    fn support_files(&self) -> impl Iterator<Item = BundledFile> + '_ {
        self.support_files.iter().map(|(name, bytes)| BundledFile {
            name: name.clone(),
            bytes: bytes.clone(),
        })
    }

    fn root_support_files(&self) -> impl Iterator<Item = BundledFile> + '_ {
        self.root_support_files
            .iter()
            .map(|(name, bytes)| BundledFile {
                name: name.clone(),
                bytes: bytes.clone(),
            })
    }

    /// Register `source` as a static worker root and return the virtual entry id
    /// that [`NewUrlAssets::emit_worker`] must give Rolldown.  Its hash is solely
    /// an internal module identity; the existing emitted filename remains keyed
    /// on the source path and retains its current layout.
    fn worker_root(&self, source: &Path) -> Result<String> {
        let source = canonicalize_for_bundler(source);
        let hash = format!("{:x}", Sha256::digest(source.to_string_lossy().as_bytes()));
        let id = format!("\0nub:compile-worker-{hash}");
        let mut roots = self
            .roots
            .lock()
            .map_err(|_| anyhow!("the compile-prelude roots were poisoned by an earlier panic"))?;
        match roots.entry(id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(source);
            }
            Entry::Occupied(entry) if entry.get() == &source => {}
            Entry::Occupied(_) => {
                bail!("generated worker root id {id:?} identifies different source paths")
            }
        }
        Ok(id)
    }

    fn root_source(&self, id: &str) -> Option<String> {
        let source = self.roots.lock().ok()?.get(id)?.clone();
        Some(compile_root_source(&source))
    }

    fn has_root(&self, id: &str) -> bool {
        self.roots.lock().is_ok_and(|roots| roots.contains_key(id))
    }

    fn is_root_source(&self, id: &str) -> bool {
        let id_path = Path::new(clean_url(id));
        self.roots
            .lock()
            .is_ok_and(|roots| roots.values().any(|source| source == id_path))
    }
}

/// The only code generated for a root.  Both imports are static and
/// side-effectful: prelude first, program second.  JSON quoting keeps Windows
/// separators and every filename character out of JavaScript grammar concerns;
/// Rolldown resolves the absolute id only while building and never emits it.
/// Drop a Windows verbatim (`\\?\`) prefix. Pure over `windows` so both branches
/// test on any host.
fn strip_verbatim_prefix(path: PathBuf, windows: bool) -> PathBuf {
    if !windows {
        return path;
    }
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

/// Canonicalize a path that will reach Rolldown as a module specifier.
///
/// `fs::canonicalize` returns the verbatim spelling on Windows, and Rolldown
/// cannot resolve one: every Windows compile died with
/// `Could not resolve '\\?\C:\...' in \0nub:compile-root`. The ordinary spelling
/// denotes the same file and both Rolldown and Node accept it, so strip the
/// prefix at the point the path stops being a filesystem handle and becomes a
/// specifier.
fn canonicalize_for_bundler(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_verbatim_prefix(canonical, cfg!(windows))
}

fn compile_root_source(source: &Path) -> String {
    let prelude = serde_json::to_string(COMPILE_PREAMBLE_ID).expect("a virtual id serializes");
    let source =
        serde_json::to_string(&source.to_string_lossy()).expect("a source path serializes");
    format!("import {prelude};\nimport {source};\n")
}

/// Locate exactly the public runtime whose dependencies compiled artifacts need.
/// `find_public_preload` is the one runtime-extraction seam used by normal nub
/// launch: it resolves the live source tree in development and materializes the
/// embedded runtime cache in a released single binary. Deriving the sibling
/// directory from that public `preload.mjs` keeps compile from ever reaching back
/// to a non-existent Cargo checkout after distribution.
fn compile_runtime_dir() -> Result<PathBuf> {
    let binary = nub_core::node::spawn::current_nub_binary()
        .context("locating the running nub binary for the compile prelude")?;
    let preload = nub_core::node::spawn::find_public_preload(&binary)
        .context("the nub runtime is unavailable, so compile cannot bundle its required prelude")?;
    let runtime_dir = Path::new(&preload)
        .parent()
        .map(Path::to_path_buf)
        .context("the public nub preload has no runtime directory")?;
    #[cfg(feature = "embed-runtime")]
    if !nub_core::node::verify_or_heal_embedded_runtime_tree(&runtime_dir) {
        bail!("the embedded nub runtime failed integrity verification; refusing to compile")
    }
    Ok(runtime_dir)
}

impl Plugin for CompilePreamble {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:compile-preamble")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId
            | HookUsage::Load
            | HookUsage::Transform
            | HookUsage::Intro
            | HookUsage::RenderChunk
    }

    /// See [`hoist_module_wrappers`]. No source map is returned: the rewrite is
    /// confined to the wrapper's own declaration head and tail, so every line of
    /// module code keeps its position and the incoming map stays accurate.
    fn render_chunk(
        &self,
        _ctx: &PluginContext,
        args: &HookRenderChunkArgs<'_>,
    ) -> impl Future<Output = HookRenderChunkReturn> + Send {
        let hoisted = hoist_module_wrappers(&args.code);
        async move {
            Ok(hoisted.map(|code| HookRenderChunkOutput {
                code,
                map: HookTransformOutputMap::Omitted,
            }))
        }
    }

    fn intro(
        &self,
        ctx: &PluginContext,
        args: &HookAddonArgs,
    ) -> impl Future<Output = HookInjectionOutputReturn> + Send {
        let module_ids = args
            .chunk
            .module_ids
            .iter()
            .map(|id| id.as_str().to_string())
            .collect::<Vec<_>>();
        let commonjs = module_ids.iter().any(|id| {
            ctx.get_module_info(id.as_str())
                .is_some_and(|module| is_node_commonjs_module(&module))
        });
        let intro = commonjs.then(compile_commonjs_require_intro);
        async move { Ok(intro) }
    }

    fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let root = self.has_root(args.specifier);
        let specifier = args.specifier.to_string();
        let prelude_path = self.runtime_dir.join("compile-preamble.mjs");
        let private_importers = Arc::clone(&self.private_importers);
        let private_import = args.importer.is_some_and(|importer| {
            private_importers
                .lock()
                .map_or(true, |ids| ids.contains(importer))
        });
        let importer = (private_import && args.importer == Some(COMPILE_PREAMBLE_ID))
            .then(|| prelude_path.to_string_lossy().into_owned())
            .or_else(|| args.importer.map(str::to_string));
        async move {
            if root || specifier == COMPILE_PREAMBLE_ID {
                return Ok(Some(HookResolveIdOutput::from_id(specifier)));
            }
            if !private_import {
                return Ok(None);
            }
            // The prelude has a virtual graph id so no build-machine path can
            // escape into emitted code, but its imports must resolve from the
            // PUBLIC runtime directory (which carries runtime/node_modules in a
            // release), not from the application's cwd. Re-entering Rolldown's
            // resolver with the physical prelude only as its importer preserves
            // package `exports`/conditions and normal relative-import behavior.
            let importer = importer.expect("a private import has an importer");
            private_importers
                .lock()
                .map_err(|_| anyhow!("the compile-prelude graph was poisoned by an earlier panic"))?
                .insert(importer.clone());
            let resolved = ctx
                .resolve(&specifier, Some(&importer), None)
                .await
                .context("resolving a compile-prelude dependency")?
                .map_err(|err| {
                    anyhow!("resolving compile-prelude dependency {specifier:?}: {err}")
                })?;
            private_importers
                .lock()
                .map_err(|_| anyhow!("the compile-prelude graph was poisoned by an earlier panic"))?
                .insert(resolved.id.to_string());
            Ok(Some(HookResolveIdOutput::from_resolved_id(resolved)))
        }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let prelude = (args.id == COMPILE_PREAMBLE_ID).then(|| self.source.clone());
        let wrapper = (!args.id.eq(COMPILE_PREAMBLE_ID))
            .then(|| self.root_source(args.id))
            .flatten();
        let root = self
            .is_root_source(args.id)
            .then(|| PathBuf::from(clean_url(args.id)));
        async move {
            if let Some(code) = prelude {
                return Ok(Some(HookLoadOutput {
                    code: code.into(),
                    module_type: Some(ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(code) = wrapper {
                return Ok(Some(HookLoadOutput {
                    code: code.into(),
                    module_type: Some(ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(path) = root {
                let source = std::fs::read_to_string(&path).with_context(|| {
                    format!("reading compile root source at {}", path.display())
                })?;
                if let Some(code) =
                    preserve_entry_esm_classification(&path.to_string_lossy(), &source)
                {
                    // Rolldown chooses the module format before transform hooks.
                    // Returning the marker here makes an authored `.mjs` root
                    // unambiguously ESM without changing its physical id, which
                    // is still needed by tsconfig, asset, and worker handling.
                    return Ok(Some(HookLoadOutput {
                        code: code.into(),
                        // Let Rolldown infer the real source type from this
                        // physical id; forcing `Js` would skip `.ts`/`.tsx`.
                        ..Default::default()
                    }));
                }
            }
            Ok(None)
        }
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl Future<Output = HookTransformReturn> + Send {
        let root = self.is_root_source(args.id);
        let rewritten = if root {
            let cjs = rewrite_entry_main_checks(clean_url(args.id), args.code);
            let source = cjs.as_deref().unwrap_or(args.code);
            rewrite_import_meta_main(clean_url(args.id), source, "true")
                .map(|magic| import_meta_transform_output(args.id, magic))
                .or_else(|| cjs.map(|code| (code, HookTransformOutputMap::Null)))
                .or_else(|| {
                    preserve_entry_esm_classification(clean_url(args.id), args.code)
                        .map(|code| (code, HookTransformOutputMap::Null))
                })
        } else {
            // Rolldown can flatten a static dependency into the executable's
            // entry chunk, where a raw `import.meta.main` would accidentally
            // observe the chunk's main-ness. Preserve Node/Deno's per-module
            // rule before chunking: only the executable root may see `true`.
            rewrite_non_root_import_meta_main(clean_url(args.id), args.code)
                .map(|magic| import_meta_transform_output(args.id, magic))
        };
        async move {
            Ok(rewritten.map(|(code, map)| HookTransformOutput {
                code: Some(code),
                map,
                ..Default::default()
            }))
        }
    }
}

/// A bundled static dependency is not the process entry merely because Rolldown
/// places it in the entry chunk. Give each dependency its own non-main metadata
/// object before finalization. Every dynamic chunk is non-main too.
fn rewrite_non_root_import_meta_main(path: &str, source: &str) -> Option<MagicString<'static>> {
    rewrite_import_meta_main(path, source, "false")
}

/// Snapshot a module's `import.meta` before Rolldown can flatten it into a chunk.
///
/// Replacing only `import.meta.main` with a boolean is not semantics-preserving:
/// the property is writable and configurable, so assignment, update, and delete
/// targets must remain references. Instead every authored `import.meta` becomes
/// one module-local object. Object-literal spread gives each copied property —
/// including the explicit `main` override — the ordinary writable, enumerable,
/// configurable data descriptor, while `__proto__: null` prevents inherited
/// properties from appearing on the snapshot. Roots need `true` because worker
/// resolver bootstraps dynamically import their prelude-bearing chunk;
/// dependencies and code-split chunks need `false` even when Rolldown flattens
/// them into main.
///
/// Main-ness is the only thing chunking can leak, so a module whose every
/// `import.meta` is read through a static non-`main` property is left alone: it
/// never holds the metadata object, so there is nothing to correct. That keeps
/// `import.meta.url` — the overwhelmingly common use, and the base every
/// embedded asset resolves against — intact in the emitted chunk instead of
/// paying for a snapshot object in each such module of every compiled binary.
fn rewrite_import_meta_main(
    path: &str,
    source: &str,
    literal: &str,
) -> Option<MagicString<'static>> {
    use oxc_allocator::Allocator;
    use oxc_ast_visit::{Visit, walk};
    use oxc_parser::Parser;
    use oxc_span::{GetSpan, SourceType, Span};

    // Whitespace and comments are legal between the `import`, `.`, and `meta`
    // tokens (`import /* ... */ . meta`), so a contiguous `import.meta` search
    // would skip valid metadata expressions before the AST gets to recognize
    // them. `import` itself cannot be escaped when it is the keyword here; it
    // is only a cheap parser-avoidance sentinel for sources that cannot contain
    // an `import.meta` MetaProperty at all.
    if !source.contains("import") {
        return None;
    }
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return None;
    }

    struct Scan {
        edits: Vec<Span>,
        /// Start offsets of the `import.meta` occurrences that are read through a
        /// static, non-`main` property and so can never reach `main`.
        main_free: BTreeSet<u32>,
        identifiers: BTreeSet<String>,
    }
    impl<'a> Visit<'a> for Scan {
        fn visit_meta_property(&mut self, meta: &oxc_ast::ast::MetaProperty<'a>) {
            if meta.meta.name == "import" && meta.property.name == "meta" {
                self.edits.push(meta.span());
            }
        }

        // A computed key stays out of this set: `import.meta["ma" + "in"]` is a
        // main access spelled dynamically, and only the static form is decidable.
        fn visit_static_member_expression(
            &mut self,
            member: &oxc_ast::ast::StaticMemberExpression<'a>,
        ) {
            if member.property.name != "main"
                && let oxc_ast::ast::Expression::MetaProperty(meta) = &member.object
                && meta.meta.name == "import"
                && meta.property.name == "meta"
            {
                self.main_free.insert(meta.span().start);
            }
            walk::walk_static_member_expression(self, member);
        }

        fn visit_identifier_reference(
            &mut self,
            identifier: &oxc_ast::ast::IdentifierReference<'a>,
        ) {
            self.identifiers.insert(identifier.name.to_string());
        }

        fn visit_binding_identifier(&mut self, identifier: &oxc_ast::ast::BindingIdentifier<'a>) {
            self.identifiers.insert(identifier.name.to_string());
        }
    }

    let mut scan = Scan {
        edits: Vec::new(),
        main_free: BTreeSet::new(),
        identifiers: BTreeSet::new(),
    };
    scan.visit_program(&parsed.program);
    if scan.edits.is_empty()
        || scan
            .edits
            .iter()
            .all(|span| scan.main_free.contains(&span.start))
    {
        return None;
    }

    // Check both raw text and normalized identifier names. The latter catches a
    // nested binding or an unbound reference written with Unicode escapes: the
    // injection would otherwise shadow it despite the alias text itself being
    // absent from the file. Rolldown may independently rename equal aliases from
    // different modules when it flattens them into one chunk.
    let alias = import_meta_alias(source, &scan.identifiers);
    let injection =
        format!("const {alias} = {{ __proto__: null, ...import.meta, main: {literal} }};\n");
    let mut magic = MagicString::new(source.to_owned());
    for span in scan.edits {
        // Spans are disjoint MetaProperty leaves, so updates cannot overlap.
        magic
            .update(span.start, span.end, alias.clone())
            .expect("parsed import.meta spans must be valid MagicString ranges");
    }

    // A hashbang must remain the first bytes in the file, and directives must
    // remain a prologue. Insert immediately after both when present; otherwise
    // a real prepend gives the common case the simplest generated shape.
    let insertion = parsed
        .program
        .directives
        .last()
        .map(|directive| directive.span())
        .or_else(|| {
            parsed
                .program
                .hashbang
                .as_ref()
                .map(|hashbang| hashbang.span())
        })
        .map_or(0, |span| span.end);
    if insertion == 0 {
        magic.prepend(injection);
    } else {
        magic.prepend_right(insertion, format!("\n{injection}"));
    }
    Some(magic)
}

fn import_meta_alias(source: &str, identifiers: &BTreeSet<String>) -> String {
    const STEM: &str = "__nub_import_meta";
    let available =
        |candidate: &str| !source.contains(candidate) && !identifiers.contains(candidate);
    if available(STEM) {
        return STEM.to_string();
    }
    (1u32..)
        .map(|suffix| format!("{STEM}_{suffix}"))
        .find(|candidate| available(candidate))
        .expect("an authored source cannot contain every numeric alias")
}

fn import_meta_transform_output(
    path: &str,
    magic: MagicString<'static>,
) -> (String, HookTransformOutputMap) {
    let code = magic.to_string();
    let map = magic.source_map(SourceMapOptions {
        hires: Hires::Boundary,
        include_content: true,
        source: path.to_string().into(),
    });
    (code, HookTransformOutputMap::from(map))
}

/// Restore the Node main-module check for a CommonJS source used as one of
/// compile's executable roots.
///
/// Compile enters the app and each static worker through an ESM prelude wrapper.
/// That preserves the authored source's CommonJS classification, but it also
/// means Rolldown wraps the source as an imported dependency. Consequently,
/// `require.main === module` is false even though Node would make it true when
/// the same file is launched directly (including as a worker entry). This is a
/// common CLI guard, and leaving it false produces a successful executable that
/// silently does nothing.
///
/// Rewriting only roots is load-bearing. A bundled dependency can carry the
/// identical guard around its own CLI, and plain Node correctly answers false
/// there. A bundle-wide define would execute those dependency CLIs as collateral
/// damage. The comparison itself is replaced rather than either operand, keeping
/// every other `require.main` use on today's conservative semantics.
fn rewrite_entry_main_checks(path: &str, source: &str) -> Option<String> {
    use oxc_allocator::Allocator;
    use oxc_ast_visit::{Visit, walk};
    use oxc_parser::Parser;
    use oxc_span::{GetSpan, SourceType, Span};

    if !source.contains("require") || !source.contains("main") || !source.contains("module") {
        return None;
    }

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || !node_classifies_as_commonjs(path, &parsed.program) {
        return None;
    }
    let semantic = oxc_semantic::SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&parsed.program)
        .semantic;

    #[derive(Clone, Copy)]
    struct Edit {
        span: Span,
        value: bool,
    }

    struct Scan<'a> {
        edits: Vec<Edit>,
        semantic: &'a oxc_semantic::Semantic<'a>,
    }

    impl<'a> Visit<'a> for Scan<'a> {
        fn visit_binary_expression(&mut self, it: &BinaryExpression<'a>) {
            let Some(value) = (match it.operator {
                BinaryOperator::Equality | BinaryOperator::StrictEquality => Some(true),
                BinaryOperator::Inequality | BinaryOperator::StrictInequality => Some(false),
                _ => None,
            }) else {
                walk::walk_binary_expression(self, it);
                return;
            };
            if (is_require_main(&it.left, self.semantic)
                && is_global_identifier(&it.right, "module", self.semantic))
                || (is_global_identifier(&it.left, "module", self.semantic)
                    && is_require_main(&it.right, self.semantic))
            {
                self.edits.push(Edit {
                    span: it.span(),
                    value,
                });
                return;
            }
            walk::walk_binary_expression(self, it);
        }
    }

    fn unparenthesized<'a>(mut expr: &'a Expression<'a>) -> &'a Expression<'a> {
        while let Expression::ParenthesizedExpression(parenthesized) = expr {
            expr = &parenthesized.expression;
        }
        expr
    }

    fn is_global_identifier(
        expr: &Expression<'_>,
        name: &str,
        semantic: &oxc_semantic::Semantic<'_>,
    ) -> bool {
        matches!(
            unparenthesized(expr),
            Expression::Identifier(identifier)
                if identifier.name == name && is_unbound_global(identifier, semantic)
        )
    }

    fn is_require_main(expr: &Expression<'_>, semantic: &oxc_semantic::Semantic<'_>) -> bool {
        match unparenthesized(expr) {
            Expression::StaticMemberExpression(member) => {
                member.property.name == "main"
                    && is_global_identifier(&member.object, "require", semantic)
            }
            Expression::ComputedMemberExpression(member) => {
                matches!(
                    member.expression.get_inner_expression(),
                    Expression::StringLiteral(property) if property.value == "main"
                ) && is_global_identifier(&member.object, "require", semantic)
            }
            _ => false,
        }
    }

    let mut scan = Scan {
        edits: Vec::new(),
        semantic: &semantic,
    };
    scan.visit_program(&parsed.program);
    if scan.edits.is_empty() {
        return None;
    }
    scan.edits.sort_by_key(|edit| edit.span.start);

    let mut output = source.as_bytes().to_vec();
    for edit in scan.edits {
        let start = edit.span.start as usize;
        let end = edit.span.end as usize;
        let literal = if edit.value {
            b"true".as_slice()
        } else {
            b"false".as_slice()
        };
        debug_assert!(end - start >= literal.len());
        output[start..start + literal.len()].copy_from_slice(literal);
        for byte in &mut output[start + literal.len()..end] {
            if !matches!(*byte, b'\r' | b'\n') {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(output).ok()
}

/// Rolldown's CommonJS scan treats an unbound `require`/`module` reference as
/// stronger evidence than an `.mjs` id. An ESM root that merely *mentions* one
/// of those names would therefore be wrapped as CommonJS and gain a fabricated
/// `require` at runtime. An empty export is the smallest standard ESM marker:
/// it changes no bindings or evaluation order, but makes the module format
/// unambiguous before that scan runs.
///
/// This is root-only. A dependency with no ESM syntax still follows its own
/// package/module rules; marking it here would change its exports semantics.
fn preserve_entry_esm_classification(path: &str, source: &str) -> Option<String> {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked
        || has_esm_syntax(&parsed.program)
        || node_classifies_as_commonjs(path, &parsed.program)
    {
        return None;
    }
    Some(format!("{source}\nexport {{}};\n"))
}

fn is_unbound_global(
    identifier: &oxc_ast::ast::IdentifierReference<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
) -> bool {
    identifier
        .reference_id
        .get()
        .is_some_and(|id| semantic.scoping().get_reference(id).symbol_id().is_none())
}

/// Node's module kind for a compiler root. `.js`/`.ts` inherit the nearest
/// package boundary; syntax alone cannot distinguish a type-module script with
/// no import or export from CommonJS.
fn node_classifies_as_commonjs(path: &str, program: &Program<'_>) -> bool {
    match Path::new(path).extension().and_then(|ext| ext.to_str()) {
        Some("cjs" | "cts") => true,
        Some("mjs" | "mts") => false,
        _ if has_esm_syntax(program) => false,
        _ => node_package_defaults_to_commonjs(path),
    }
}

fn node_package_defaults_to_commonjs(path: &str) -> bool {
    let mut dir = Path::new(clean_url(path)).parent();
    while let Some(current) = dir {
        let manifest = current.join("package.json");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
        {
            return json.get("type").and_then(serde_json::Value::as_str) != Some("module");
        }
        dir = current.parent();
    }
    true
}

/// Embeds the file a `new URL("./x", import.meta.url)` names, and repoints the
/// URL at the embedded copy.
///
/// This is the idiomatic ESM way to reach a file sitting beside your module, and
/// it was the one asset form that compiled CLEAN and then died: the artifact
/// carried no copy of `x`, and the source tree the URL pointed at is not on the
/// machine the binary ships to, so the first read was an ENOENT with no
/// build-time signal at all. Embedding makes it work; there is nothing for the
/// user to opt into.
///
/// THE REWRITE IS NUB'S RATHER THAN ROLLDOWN'S, deliberately. Rolldown has
/// `experimental.resolve_new_url_to_asset`, but it routes the referenced file
/// through the module graph as `ModuleType::Asset` — where nub's own extension
/// map and `file` loader already claim it, so a `new URL("./logo.png", …)` would
/// be loaded as JavaScript by one and emitted as an asset by the other — and it
/// names what it emits under Rolldown's `assetFileNames`, a second layout and a
/// second naming scheme beside the flat content-hashed one everything else here
/// depends on. Emitting from nub keeps ONE collector, one naming rule, one
/// reachability pass ([`retain_referenced`]), and the same `import.meta.url` base
/// that makes the path correct from any cwd.
///
/// WHAT IS OUT OF SCOPE, AND WHY IT IS SILENT. Only a statically analyzable
/// specifier can be embedded, so `new URL(name, import.meta.url)` is left exactly
/// as written — no rewrite and no build error. Failing it would be wrong twice
/// over: the same expression is how a program names a file it intends to WRITE,
/// and a computed URL is routinely on a branch the artifact never takes. The same
/// reasoning covers a specifier resolving to nothing on the build machine.
///
/// A `new Worker(new URL(…), …)` is the ONE shape that must NOT be embedded
/// verbatim, and it is handled here rather than in a plugin of its own because
/// the two are the same scan: the worker's URL is an ordinary asset reference
/// syntactically, and only its enclosing `new Worker` tells them apart. What the
/// runtime does with the two differs completely — an asset is opened as DATA,
/// while a worker entry is EXECUTED — so a verbatim copy of a worker entry is
/// never right: it arrives untranspiled if it was TypeScript, and its imports
/// resolve against the extracted app dir, where neither the source tree's
/// `node_modules` nor its tsconfig path aliases exist. That failure is invisible
/// at build time and lands as `ERR_MODULE_NOT_FOUND` inside the worker thread on
/// the user's machine. Emitting it as a real CHUNK is what makes the worker carry
/// its own dependency graph, and it is the behavior Bun and Vite already have.
#[derive(Debug)]
struct NewUrlAssets {
    collected: Arc<loaders::Assets>,
    /// The `file` loader, consulted only to skip the modules IT synthesized: the
    /// module it emits is itself a `new URL(…, import.meta.url)`, so rescanning
    /// one would re-resolve an already-hashed asset name against the original
    /// file's directory.
    files: Arc<loaders::FilePlugin>,
    /// Owns the virtual ESM wrapper each statically emitted worker enters
    /// through, so it receives the same prelude as the primary program without
    /// changing a CommonJS worker's classification.
    prelude: Arc<CompilePreamble>,
    /// Logical root used to name workers independently of checkout location.
    project_root: PathBuf,
    /// Filenames of the worker chunks this build emitted. Read back after the
    /// bundle to keep them out of entry detection — Rolldown marks an emitted
    /// chunk `is_entry`, so without this a worker would be mistaken for the
    /// program's own entry and the launcher would boot the wrong module.
    /// Public worker-entry name → its source and prelude-bearing worker chunk.
    /// The public file is written after bundling, when compile knows whether it
    /// must first install the external/dynamic resolve hook.
    workers: Mutex<BTreeMap<String, WorkerOutput>>,
    /// Payload name → source path, for each embedded asset that is itself a
    /// JavaScript or TypeScript module. See [`warn_module_data_assets`].
    modules: Mutex<BTreeMap<String, PathBuf>>,
    /// Unsupported executable worker sources found while transforming the
    /// module graph.  Kept for [`Self::reject_unsupported_workers`], after
    /// Rolldown has finished rendering its own diagnostics.
    worker_refusals: Mutex<Vec<WorkerRefusal>>,
}

#[derive(Debug, Clone)]
struct WorkerOutput {
    source: PathBuf,
    chunk: String,
}

/// One `new URL(<literal>, import.meta.url)` this build can act on.
#[derive(Debug, Clone)]
struct NewUrlEdit {
    /// Byte range of the FIRST argument within the module's own source.
    start: usize,
    end: usize,
    /// The file on the build machine the URL names.
    source: PathBuf,
    /// This URL is the first argument of a `new Worker(…)`, so the file is a
    /// module to BUNDLE, not data to copy.
    worker: bool,
}

/// A worker source compile can prove has no file-backed module root into which it
/// can install the ESM prelude.  We retain the module path rather than a byte
/// span because Rolldown's normal diagnostic renderer owns source snippets, and
/// it discards text returned from a transform-plugin error.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WorkerRefusal {
    source: PathBuf,
    reason: String,
}

#[derive(Debug, Default)]
struct NewUrlScan {
    edits: Vec<NewUrlEdit>,
    worker_refusals: Vec<WorkerRefusal>,
}

impl NewUrlAssets {
    /// Bundle `source` as its own entry chunk and return the filename to point
    /// the `new URL(…)` at.
    ///
    /// The name is pinned rather than left to Rolldown's hash so it is known HERE,
    /// while the referencing module is still being transformed — `get_file_name`
    /// can only answer once chunks exist, which is after every rewrite has already
    /// had to happen. Hashing the project-relative path keeps same-named workers
    /// in separate directories distinct without baking in the checkout location.
    fn emit_worker(&self, ctx: &PluginContext, source: &Path) -> Result<String> {
        let stem: String = source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let stem = if stem.is_empty() {
            "worker".to_string()
        } else {
            stem
        };
        let entry = worker_output_name(&self.project_root, source, &stem);
        let chunk = worker_chunk_output_name(&entry);
        // Idempotent for the same source: one worker referenced from two modules
        // must emit once, while a generated-name collision must never alias it.
        // Same spelling as `worker_root` below, or the two derive different ids for
        // one file and the worker emits twice.
        let source = canonicalize_for_bundler(source);
        // Scoped so the guard is released before `worker_root` below, which takes
        // its own locks.
        let fresh = {
            let mut workers = self
                .workers
                .lock()
                .map_err(|_| anyhow!("the worker collector was poisoned by an earlier panic"))?;
            record_worker(&mut workers, entry.clone(), chunk.clone(), &source)?
        };
        if fresh {
            let root = self.prelude.worker_root(&source)?;
            ctx.emit_chunk(EmittedChunk {
                name: None,
                file_name: Some(chunk.as_str().into()),
                id: root,
                importer: None,
                preserve_entry_signatures: None,
            })
            .map_err(|e| anyhow!("bundling the worker entry {}: {e}", source.display()))?;
        }
        Ok(entry)
    }

    fn note_worker_refusals(&self, refusals: Vec<WorkerRefusal>) {
        if refusals.is_empty() {
            return;
        }
        if let Ok(mut found) = self.worker_refusals.lock() {
            found.extend(refusals);
        }
    }

    /// Render direct diagnostics for worker construction forms that cannot run
    /// a static ESM prelude in v1.  This happens after the bundle result is
    /// available because Rolldown otherwise reduces plugin errors to an opaque
    /// `plugin threw an error` diagnostic.
    fn reject_unsupported_workers(&self) -> Result<()> {
        let mut refusals = self
            .worker_refusals
            .lock()
            .map_err(|_| anyhow!("the worker refusal collector was poisoned by an earlier panic"))?
            .clone();
        reject_unsupported_workers(&mut refusals)
    }
}

fn worker_output_name(project_root: &Path, source: &Path, stem: &str) -> String {
    let logical = pathdiff::diff_paths(source, project_root)
        .unwrap_or_else(|| source.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/");
    let hash = format!("{:x}", Sha256::digest(logical.as_bytes()));
    worker_output_name_with_hash(stem, &hash)
}

fn worker_output_name_with_hash(stem: &str, hash: &str) -> String {
    format!("{stem}-{hash}.mjs")
}

fn record_worker(
    workers: &mut BTreeMap<String, WorkerOutput>,
    entry: String,
    chunk: String,
    source: &Path,
) -> Result<bool> {
    match workers.entry(entry.clone()) {
        Entry::Vacant(slot) => {
            slot.insert(WorkerOutput {
                source: source.to_path_buf(),
                chunk,
            });
            Ok(true)
        }
        Entry::Occupied(slot) if slot.get().source == source => Ok(false),
        Entry::Occupied(_) => {
            bail!("generated worker entry {entry:?} identifies different source paths")
        }
    }
}

/// Keep the app-visible worker name stable while reserving a distinct bundled
/// module that a generated wrapper can import only after installing a resolver
/// hook. A suffix before `.mjs` avoids a second hash/name scheme and remains a
/// legal flat payload name on every target.
fn worker_chunk_output_name(entry: &str) -> String {
    entry
        .strip_suffix(".mjs")
        .map(|stem| format!("{stem}-code.mjs"))
        .unwrap_or_else(|| format!("{entry}-code.mjs"))
}

fn reject_unsupported_workers(refusals: &mut Vec<WorkerRefusal>) -> Result<()> {
    if refusals.is_empty() {
        return Ok(());
    }
    refusals.sort_by(|a, b| a.source.cmp(&b.source).then(a.reason.cmp(&b.reason)));
    refusals.dedup();
    let lines = refusals
        .iter()
        .map(|refusal| format!("  {}: {}", refusal.source.display(), refusal.reason))
        .collect::<Vec<_>>();
    bail!(
        "nub compile cannot augment these worker sources in v1:\n{}\n\n\
         \x20\x20Compile can inject its ESM prelude into file-backed workers written as\n\
         \x20\x20new Worker(new URL(\"./worker.js\", import.meta.url)). `data:`, `blob:`,\n\
         \x20\x20and `{{ eval: true }}` workers execute code without a module root, so there is\n\
         \x20\x20no safe place to install the prelude. Move the worker body into a file-backed\n\
         \x20\x20module. A constructor compile cannot tie to Node's Worker is refused rather\n\
         \x20\x20than shipped as data: reach it through the global Worker, globalThis.Worker,\n\
         \x20\x20or an import from node:worker_threads, and bind it with a plain const.\n\
         \x20\x20Dynamic or unresolved file worker specifiers keep their existing behavior.",
        lines.join("\n")
    );
}

impl NewUrlAssets {
    /// The worker chunk filenames this build emitted.
    fn worker_names(&self) -> BTreeSet<String> {
        self.workers
            .lock()
            .map(|g| g.values().map(|worker| worker.chunk.clone()).collect())
            .unwrap_or_default()
    }

    fn worker_roots(&self) -> Vec<WorkerRoot> {
        self.workers
            .lock()
            .map(|g| {
                g.iter()
                    .map(|(entry, worker)| WorkerRoot {
                        entry: entry.clone(),
                        chunk: worker.chunk.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record a data asset that is itself a module, for the post-bundle report.
    fn note_if_module(&self, name: &str, source: &Path) -> Result<()> {
        if !is_module_extension(source) {
            return Ok(());
        }
        self.modules
            .lock()
            .map_err(|_| anyhow!("the module-asset collector was poisoned by an earlier panic"))?
            .insert(name.to_string(), source.to_path_buf());
        Ok(())
    }

    /// The module-shaped data assets that reached the payload, named as the user
    /// wrote them.
    ///
    /// Filtered against `kept` for the same reason `native::NativeAddons`
    /// filters its own: bytes are collected while the graph is being built, and
    /// [`retain_referenced`] drops whatever no chunk ends up naming. Reporting the
    /// collection-time set would name a file the binary does not carry.
    fn module_assets(&self, kept: &BTreeSet<&str>) -> Vec<PathBuf> {
        self.modules
            .lock()
            .map(|m| {
                m.iter()
                    .filter(|(payload, _)| kept.contains(payload.as_str()))
                    .map(|(_, source)| source.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Extensions Node and the bundler both read as JavaScript source.
fn is_module_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx"
        )
    })
}

/// Say so when a JavaScript or TypeScript file ships as a DATA asset.
///
/// A `new URL("./x.ts", import.meta.url)` that is not a `new Worker` argument is
/// embedded verbatim, so the artifact carries the file exactly as authored:
/// untranspiled if it was TypeScript, and with every bare import still naming a
/// package the extracted app dir has no `node_modules` for. Anything that then
/// EXECUTES it fails, and until this note nothing about the build said so.
///
/// A WARNING RATHER THAN A REFUSAL, deliberately, and this is the one place in
/// this file that resolves that way. The build cannot tell the two uses apart:
/// reading a script's TEXT — a codegen template, source served to a browser, a
/// snippet handed to `new Worker(src, { eval: true })` — is a correct and
/// currently-working use of the identical expression. Unlike the computed-import
/// refusal there is no flag that means "I meant it", so a refusal would be a wall
/// with nothing on the other side of it, and the only way past would be to stop
/// using `new URL` — which is the idiomatic spelling this compiler went out of
/// its way to support.
///
/// What makes a WARNING sufficient here is that the executed case never reaches
/// this function. Every worker spelling [`classify_worker_callee`] can prove is
/// emitted as a real chunk, and every one it cannot is REFUSED at build time
/// rather than falling through — so what arrives here is data, and the defect it
/// addresses is the absence of a build-time signal about that.
fn warn_module_data_assets(sources: &[PathBuf]) {
    for source in sources {
        eprintln!(
            "note: {} is embedded as a data asset, not as code.\n\
             \x20\x20It ships exactly as written — never transpiled — and its own imports would\n\
             \x20\x20resolve against the extracted app dir, which has no node_modules. Reading\n\
             \x20\x20its text works; executing it does not. A worker entry belongs in\n\
             \x20\x20new Worker(new URL(…)), which is bundled as a real chunk instead.",
            source.display()
        );
    }
}

impl Plugin for NewUrlAssets {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:new-url-assets")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::Transform
    }

    fn transform(
        &self,
        ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        // Real JavaScript sources only. A `text`/`json` module's "code" IS the
        // file's content, so parsing markdown as JavaScript and rewriting what
        // happened to look like a URL inside it would corrupt the user's data.
        let scannable = matches!(
            args.module_type,
            ModuleType::Js | ModuleType::Jsx | ModuleType::Ts | ModuleType::Tsx
        ) && !self.files.claims(clean_url(args.id));
        let scan = if scannable {
            scan_new_urls(args.id, args.code)
        } else {
            NewUrlScan::default()
        };
        self.note_worker_refusals(scan.worker_refusals);
        let edits = scan.edits;
        // Cloned only when there is an edit to make — this hook runs on every
        // module in the graph, and almost none of them have one.
        let source = (!edits.is_empty()).then(|| args.code.to_string());
        async move {
            let Some(source) = source else {
                return Ok(None);
            };
            let mut named = Vec::with_capacity(edits.len());
            for edit in edits {
                let name = if edit.worker {
                    self.emit_worker(&ctx, &edit.source)?
                } else {
                    let bytes = std::fs::read(&edit.source).map_err(|e| {
                        anyhow!("reading {} for new URL(): {e}", edit.source.display())
                    })?;
                    let name = self.collected.add(&edit.source, bytes)?;
                    self.note_if_module(&name, &edit.source)?;
                    name
                };
                named.push((edit, name));
            }
            Ok(Some(HookTransformOutput {
                code: Some(rewrite_new_urls(&source, &named)),
                // NOT `Omitted`, which means "this transform broke the map":
                // Rolldown answers that by dropping the module's mapping entirely
                // and warning on every build. `Null` says the transform does not
                // move positions — true here, since only a string literal's
                // CONTENT changes and every line still starts where it did, so the
                // codegen map still lands on the right line.
                map: HookTransformOutputMap::Null,
                ..Default::default()
            }))
        }
    }
}

/// Splice each embedded asset's name in over the specifier it replaces.
///
/// LINE-PRESERVING, which is what makes the `Null` sourcemap honest: only a
/// string literal's CONTENT changes, and a no-substitution template literal that
/// spanned lines gives those newlines back as ordinary whitespace between
/// `new URL`'s arguments.
fn rewrite_new_urls(source: &str, edits: &[(NewUrlEdit, String)]) -> String {
    let mut out = String::with_capacity(source.len());
    let mut last = 0usize;
    for (edit, name) in edits {
        out.push_str(&source[last..edit.start]);
        out.push_str(
            &serde_json::to_string(&format!("./{name}")).expect("an asset name serializes"),
        );
        out.extend(std::iter::repeat_n(
            '\n',
            source[edit.start..edit.end].matches('\n').count(),
        ));
        last = edit.end;
    }
    out.push_str(&source[last..]);
    out
}

/// Every embeddable `new URL(<literal>, import.meta.url)` in `source`, in source
/// order — [`rewrite_new_urls`] splices in one forward pass. A parse failure
/// yields nothing: the bundler's own parse error is the better diagnostic, and
/// this pass must never be the thing that fails a build.
fn scan_new_urls(id: &str, source: &str) -> NewUrlScan {
    use oxc_allocator::Allocator;
    use oxc_ast_visit::{Visit, walk};
    use oxc_parser::Parser;
    use oxc_span::{GetSpan, SourceType};

    let Some(dir) = Path::new(clean_url(id)).parent().map(Path::to_path_buf) else {
        return NewUrlScan::default();
    };
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(clean_url(id)).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return NewUrlScan::default();
    }
    let semantic = oxc_semantic::SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&parsed.program)
        .semantic;
    let node_builtins = node_builtin_import_bindings(&parsed.program);
    let cjs_worker_bindings = cjs_worker_thread_bindings(&parsed.program, &semantic);
    let worker_aliases = worker_alias_bindings(&parsed.program, &semantic, &node_builtins);

    struct Visitor<'a, 's> {
        dir: PathBuf,
        module: PathBuf,
        code: &'s str,
        semantic: &'a oxc_semantic::Semantic<'a>,
        node_builtins: &'a BTreeMap<oxc_semantic::SymbolId, &'static str>,
        cjs_worker_bindings: &'a BTreeSet<oxc_semantic::SymbolId>,
        worker_aliases: &'a BTreeMap<oxc_semantic::SymbolId, WorkerCallee>,
        found: Vec<NewUrlEdit>,
        worker_refusals: Vec<WorkerRefusal>,
        /// Starts of the `new URL(…)` spans an enclosing `new Worker(…)` already
        /// took. The walk reaches the outer expression first, so claiming there is
        /// what stops the inner one from ALSO being recorded as a data asset — and
        /// embedding it both ways would put the same file in the payload twice
        /// under two names, one of them unrunnable.
        claimed: std::collections::HashSet<u32>,
    }

    impl<'a, 's> Visit<'a> for Visitor<'a, 's> {
        fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
            let callee = classify_worker_callee(
                &it.callee,
                self.semantic,
                self.node_builtins,
                self.worker_aliases,
            );
            if callee == WorkerCallee::Proven
                && let Some(reason) =
                    unsupported_worker_reason(it, self.semantic, self.node_builtins)
            {
                self.worker_refusals.push(WorkerRefusal {
                    source: self.module.clone(),
                    reason,
                });
            }
            if callee == WorkerCallee::Proven
                && let Some(url) = new_url_argument(it)
            {
                if let Some(edit) = self.embeddable(url, true) {
                    self.claimed.insert(url.span().start);
                    self.found.push(edit);
                }
            } else if is_cjs_worker_constructor(it, self.semantic, self.cjs_worker_bindings)
                && let Some(url) = new_url_argument(it)
            {
                // This is a direct `{ Worker: Local } = require(...)` binding,
                // not a text match. Do not silently turn its executable source
                // into a raw data asset: the compiled artifact would then fail
                // only after the source tree has gone away.
                self.worker_refusals.push(WorkerRefusal {
                    source: self.module.clone(),
                    reason: "CommonJS `require(\"node:worker_threads\")` worker bindings are not supported by `nub compile` yet; use `import { Worker } from \"node:worker_threads\"` (aliases are supported) or the global Worker".into(),
                });
                self.claimed.insert(url.span().start);
            } else if callee == WorkerCallee::Unprovable
                && let Some(url) = new_url_argument(it)
                && self.embeddable(url, false).is_some()
            {
                // Same hazard as the CommonJS binding above, reached from the
                // other side: the receiver is unprovable, so this MIGHT be a
                // Worker, and the data-asset fallback is only safe when it is
                // not. Refusing costs a build error on a spelling nobody has to
                // write; falling through costs a worker that runs on bare Node,
                // untranspiled, with no preamble, on the user's machine.
                let spelling = callee_spelling(self.code, it.callee.span());
                self.worker_refusals.push(WorkerRefusal {
                    source: self.module.clone(),
                    reason: format!(
                        "`new {spelling}(new URL(…))` cannot be proven to construct Node's Worker, so its entry would ship as data rather than as code"
                    ),
                });
                self.claimed.insert(url.span().start);
            } else if let Some(url) = spelled_worker_url_argument(it) {
                // A same-named local/custom Worker owns the whole expression;
                // its URL is not a data asset merely because its spelling is
                // otherwise embeddable.
                self.claimed.insert(url.span().start);
            } else if !self.claimed.contains(&it.span().start)
                && let Some(edit) = self.embeddable(it, false)
            {
                self.found.push(edit);
            }
            walk::walk_new_expression(self, it);
        }
    }

    impl Visitor<'_, '_> {
        fn embeddable(&self, expr: &NewExpression<'_>, worker: bool) -> Option<NewUrlEdit> {
            let Expression::Identifier(callee) = &expr.callee else {
                return None;
            };
            if !is_compiler_builtin(callee, "URL", self.semantic, self.node_builtins) {
                return None;
            }
            if !is_import_meta_url(expr.arguments.get(1)?.as_expression()?) {
                return None;
            }
            let arg = expr.arguments.first()?.as_expression()?;
            let source = self.embed_target(&static_specifier(arg)?)?;
            let span = arg.span();
            Some(NewUrlEdit {
                start: span.start as usize,
                end: span.end as usize,
                source,
                worker,
            })
        }

        /// The file on the build machine a specifier names, if this build can
        /// embed it.
        ///
        /// Deliberately narrow, and every exclusion leaves TODAY'S behavior in
        /// place rather than failing. A specifier carrying a SCHEME is already an
        /// absolute URL (`data:`, `https:`, and — the one that bites — a Windows
        /// `C:\…`), so it does not describe a file beside the module; a leading
        /// separator resolves against the URL's root, which is the deploy
        /// machine's, not the build machine's; and `?`/`#` make the tail a query
        /// or fragment rather than part of the path. What is left is resolved
        /// against the importing module's own directory, exactly as the runtime
        /// would, and must EXIST as a file — a specifier naming something that is
        /// not there yet is a path the program WRITES, not one it ships.
        fn embed_target(&self, spec: &str) -> Option<PathBuf> {
            if spec.is_empty() || spec.starts_with(['/', '\\']) || spec.contains(['?', '#']) {
                return None;
            }
            if has_url_scheme(spec) {
                return None;
            }
            let path = self.dir.join(percent_decode(spec)?);
            path.is_file().then_some(path)
        }
    }

    let mut visitor = Visitor {
        module: PathBuf::from(clean_url(id)),
        dir,
        code: source,
        semantic: &semantic,
        node_builtins: &node_builtins,
        cjs_worker_bindings: &cjs_worker_bindings,
        worker_aliases: &worker_aliases,
        found: Vec::new(),
        worker_refusals: Vec::new(),
        claimed: std::collections::HashSet::new(),
    };
    visitor.visit_program(&parsed.program);
    visitor.found.sort_by_key(|e| e.start);
    NewUrlScan {
        edits: visitor.found,
        worker_refusals: visitor.worker_refusals,
    }
}

/// Symbol identities for direct Node CommonJS worker destructuring bindings.
/// This is intentionally narrower than a general CommonJS data-flow analysis:
/// only an unshadowed `require("node:worker_threads")` and a direct `Worker`
/// property prove that a local constructor is Node's Worker. That both avoids
/// comments/string-literal false positives and leaves custom constructors alone.
fn cjs_worker_thread_bindings(
    program: &Program<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
) -> BTreeSet<oxc_semantic::SymbolId> {
    use oxc_ast::ast::{BindingPattern, Expression, VariableDeclarator};
    use oxc_ast_visit::{Visit, walk};

    fn is_worker_threads_require(
        expression: &Expression<'_>,
        semantic: &oxc_semantic::Semantic<'_>,
    ) -> bool {
        let Expression::CallExpression(call) = expression else {
            return false;
        };
        matches!(
            &call.callee,
            Expression::Identifier(callee)
                if callee.name == "require"
                    && is_unbound_global(callee, semantic)
        ) && call.arguments.len() == 1
            && matches!(
                call.arguments[0].as_expression(),
                Some(Expression::StringLiteral(specifier))
                    if matches!(specifier.value.as_str(), "worker_threads" | "node:worker_threads")
            )
    }

    struct Visitor<'a> {
        semantic: &'a oxc_semantic::Semantic<'a>,
        bindings: BTreeSet<oxc_semantic::SymbolId>,
    }

    impl<'a> Visit<'a> for Visitor<'a> {
        fn visit_variable_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
            if declarator
                .init
                .as_ref()
                .is_some_and(|init| is_worker_threads_require(init, self.semantic))
                && let BindingPattern::ObjectPattern(pattern) = &declarator.id
            {
                for property in &pattern.properties {
                    if !property.computed
                        && property.key.is_specific_static_name("Worker")
                        && let BindingPattern::BindingIdentifier(binding) = &property.value
                        && let Some(symbol) = binding.symbol_id.get()
                    {
                        self.bindings.insert(symbol);
                    }
                }
            }
            walk::walk_variable_declarator(self, declarator);
        }
    }

    let mut visitor = Visitor {
        semantic,
        bindings: BTreeSet::new(),
    };
    visitor.visit_program(program);
    visitor.bindings
}

fn spelled_worker_url_argument<'a>(expr: &'a NewExpression<'a>) -> Option<&'a NewExpression<'a>> {
    if !matches!(&expr.callee, Expression::Identifier(callee) if callee.name == "Worker") {
        return None;
    }
    match expr.arguments.first()?.as_expression()? {
        Expression::NewExpression(url) => Some(url),
        _ => None,
    }
}

/// The syntactic URL shape shared by native/global workers and the explicit
/// CommonJS rejection above. This intentionally does not decide WHAT the
/// constructor is; that remains semantic for supported worker roots.
fn new_url_argument<'a>(expr: &'a NewExpression<'a>) -> Option<&'a NewExpression<'a>> {
    match expr.arguments.first()?.as_expression()? {
        Expression::NewExpression(url) => Some(url),
        _ => None,
    }
}

/// A local binding destructured directly from Node's `worker_threads` require.
/// Unlike ESM named imports this is not compiled as a worker root yet, but the
/// shape is explicit enough to reject instead of corrupting it into a data asset.
fn is_cjs_worker_constructor(
    expr: &NewExpression<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
    bindings: &BTreeSet<oxc_semantic::SymbolId>,
) -> bool {
    let Expression::Identifier(callee) = &expr.callee else {
        return false;
    };
    callee
        .reference_id
        .get()
        .and_then(|id| semantic.scoping().get_reference(id).symbol_id())
        .is_some_and(|symbol| bindings.contains(&symbol))
}

/// How strongly a `new` expression's callee can be tied to Node's `Worker`.
///
/// Three-valued rather than a predicate because both ends are wrong for a
/// spelling like `new registry.Worker(new URL("./w.js", import.meta.url))`.
/// Bundling it would emit a chunk for what may be the user's own constructor;
/// ignoring it hands the URL to the data-asset path, which ships an executable
/// module VERBATIM — untranspiled, with its bare imports still naming packages
/// the extracted app dir has no `node_modules` for — so the worker fails only on
/// the deploy machine, long after the source tree is gone. The middle rung is
/// what lets an unprovable spelling be refused at build time instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WorkerCallee {
    No,
    /// Spelled as a `Worker`, on a receiver or binding this pass cannot resolve.
    Unprovable,
    Proven,
}

/// The `new URL(…)` a `new Worker(…)` is constructed from is only recognized in
/// the INLINE form — `new Worker(new URL("./w.ts", import.meta.url))`. A worker
/// whose URL was computed into a variable first is left alone, on the same
/// reasoning the rest of this scan follows: nub rewrites what it can prove, and
/// proving a variable's value needs data flow this pass deliberately does not
/// do. That is the shape every bundler with worker support requires too, and it
/// is what opencode, Vite's docs, and MDN all write.
///
/// The CONSTRUCTOR, unlike the URL, does get followed through bindings — see
/// [`worker_alias_bindings`] for why that asymmetry is not an inconsistency.
fn classify_worker_callee(
    callee: &Expression<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
    node_builtins: &BTreeMap<oxc_semantic::SymbolId, &'static str>,
    aliases: &BTreeMap<oxc_semantic::SymbolId, WorkerCallee>,
) -> WorkerCallee {
    if let Some(access) = worker_property_access(callee, semantic) {
        return access;
    }
    let Expression::Identifier(identifier) = callee.get_inner_expression() else {
        return WorkerCallee::No;
    };
    if is_compiler_builtin(identifier, "Worker", semantic, node_builtins) {
        return WorkerCallee::Proven;
    }
    resolved_symbol(identifier, semantic)
        .and_then(|symbol| aliases.get(&symbol).copied())
        .unwrap_or(WorkerCallee::No)
}

/// `<receiver>.Worker` or `<receiver>["Worker"]`, classified by whether the
/// receiver is provably the platform's global object rather than a user's.
fn worker_property_access(
    expr: &Expression<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
) -> Option<WorkerCallee> {
    let (object, property) = match expr.get_inner_expression() {
        Expression::StaticMemberExpression(member) => {
            (&member.object, member.property.name.to_string())
        }
        Expression::ComputedMemberExpression(member) => {
            (&member.object, static_specifier(&member.expression)?)
        }
        _ => return None,
    };
    (property == "Worker").then(|| {
        if is_global_object(object, semantic) {
            WorkerCallee::Proven
        } else {
            WorkerCallee::Unprovable
        }
    })
}

/// Receivers whose `Worker` property is the platform's, not a user object's.
/// `global` is Node's own spelling; `self` and `window` are what isomorphic code
/// reaches for and both are unbound in a compiled artifact, so a hit here means
/// the author wrote the global they expected nub to supply.
fn is_global_object(expr: &Expression<'_>, semantic: &oxc_semantic::Semantic<'_>) -> bool {
    matches!(
        expr.get_inner_expression(),
        Expression::Identifier(object)
            if matches!(object.name.as_str(), "globalThis" | "global" | "self" | "window")
                && is_unbound_global(object, semantic)
    )
}

fn resolved_symbol(
    identifier: &oxc_ast::ast::IdentifierReference<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
) -> Option<oxc_semantic::SymbolId> {
    identifier
        .reference_id
        .get()
        .and_then(|id| semantic.scoping().get_reference(id).symbol_id())
}

/// A binding initializer reduced to only what decides its Worker identity. Kept
/// as data rather than as an AST borrow so the fixpoint below can iterate
/// without re-walking the program once per round.
enum AliasSource {
    Known(WorkerCallee),
    Binding(oxc_semantic::SymbolId),
    /// `a ?? b`, `a || b`, `a && b`, `c ? a : b` — the value is one of the arms,
    /// so the binding is only proven when EVERY arm is.
    OneOf(Vec<AliasSource>),
}

/// Local bindings that carry Node's `Worker`, and the ones that only look like
/// they might.
///
/// This exists because a spelling this analysis misses is not merely skipped.
/// It falls through to the data-asset path, and the worker's entry then ships
/// verbatim inside a binary that runs it anyway — with bare Node's
/// `process.execPath` and no compiled preamble. So every shape real code puts
/// between the global and the `new` has to resolve here: `const A = Worker`,
/// `const { Worker: W } = globalThis`, and above all the isomorphic
/// `globalThis.Worker ?? NodeWorker`, which is the guard the ecosystem actually
/// writes.
///
/// A binding that is assigned again is capped at `Unprovable` rather than
/// dropped. Its value at the construction site is not this pass's to claim, but
/// dropping it would return that site to the silent data-asset fallback, which
/// is the failure this whole analysis exists to close.
fn worker_alias_bindings(
    program: &Program<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
    node_builtins: &BTreeMap<oxc_semantic::SymbolId, &'static str>,
) -> BTreeMap<oxc_semantic::SymbolId, WorkerCallee> {
    use oxc_ast::ast::{BindingPattern, VariableDeclarator};
    use oxc_ast_visit::{Visit, walk};

    struct Visitor<'a> {
        semantic: &'a oxc_semantic::Semantic<'a>,
        node_builtins: &'a BTreeMap<oxc_semantic::SymbolId, &'static str>,
        candidates: Vec<(oxc_semantic::SymbolId, AliasSource)>,
    }

    impl Visitor<'_> {
        /// A redeclared binding keeps BOTH initializers, so a second `var W =
        /// other` cannot leave the first one's proof standing.
        fn record(&mut self, symbol: oxc_semantic::SymbolId, source: AliasSource) {
            match self
                .candidates
                .iter_mut()
                .find(|(existing, _)| *existing == symbol)
            {
                Some((_, existing)) => {
                    let first = std::mem::replace(existing, AliasSource::Known(WorkerCallee::No));
                    *existing = AliasSource::OneOf(vec![first, source]);
                }
                None => self.candidates.push((symbol, source)),
            }
        }
    }

    impl<'a> Visit<'a> for Visitor<'a> {
        fn visit_variable_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
            if let Some(init) = &declarator.init {
                match &declarator.id {
                    BindingPattern::BindingIdentifier(binding) => {
                        if let Some(symbol) = binding.symbol_id.get() {
                            let source = alias_source(init, self.semantic, self.node_builtins);
                            self.record(symbol, source);
                        }
                    }
                    BindingPattern::ObjectPattern(pattern) => {
                        let receiver = if is_global_object(init, self.semantic) {
                            WorkerCallee::Proven
                        } else {
                            WorkerCallee::Unprovable
                        };
                        for property in &pattern.properties {
                            if !property.computed
                                && property.key.is_specific_static_name("Worker")
                                && let BindingPattern::BindingIdentifier(binding) = &property.value
                                && let Some(symbol) = binding.symbol_id.get()
                            {
                                self.record(symbol, AliasSource::Known(receiver));
                            }
                        }
                    }
                    _ => {}
                }
            }
            walk::walk_variable_declarator(self, declarator);
        }
    }

    let mut visitor = Visitor {
        semantic,
        node_builtins,
        candidates: Vec::new(),
    };
    visitor.visit_program(program);

    // Least fixpoint over `No < Unprovable < Proven`, which terminates because
    // `resolve_alias` is monotone and an entry only ever moves up a lattice of
    // height two. Chained aliases therefore resolve regardless of source order.
    let mut resolved = BTreeMap::new();
    let mut changed = !visitor.candidates.is_empty();
    while changed {
        changed = false;
        for (symbol, source) in &visitor.candidates {
            let mut value = resolve_alias(source, &resolved);
            if semantic.scoping().symbol_is_mutated(*symbol) {
                value = value.min(WorkerCallee::Unprovable);
            }
            if value > resolved.get(symbol).copied().unwrap_or(WorkerCallee::No) {
                resolved.insert(*symbol, value);
                changed = true;
            }
        }
    }
    resolved
}

fn alias_source(
    expr: &Expression<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
    node_builtins: &BTreeMap<oxc_semantic::SymbolId, &'static str>,
) -> AliasSource {
    if let Some(access) = worker_property_access(expr, semantic) {
        return AliasSource::Known(access);
    }
    match expr.get_inner_expression() {
        Expression::Identifier(identifier) => {
            if is_compiler_builtin(identifier, "Worker", semantic, node_builtins) {
                AliasSource::Known(WorkerCallee::Proven)
            } else if let Some(symbol) = resolved_symbol(identifier, semantic) {
                AliasSource::Binding(symbol)
            } else {
                AliasSource::Known(WorkerCallee::No)
            }
        }
        Expression::LogicalExpression(logical) => AliasSource::OneOf(vec![
            alias_source(&logical.left, semantic, node_builtins),
            alias_source(&logical.right, semantic, node_builtins),
        ]),
        Expression::ConditionalExpression(conditional) => AliasSource::OneOf(vec![
            alias_source(&conditional.consequent, semantic, node_builtins),
            alias_source(&conditional.alternate, semantic, node_builtins),
        ]),
        _ => AliasSource::Known(WorkerCallee::No),
    }
}

fn resolve_alias(
    source: &AliasSource,
    resolved: &BTreeMap<oxc_semantic::SymbolId, WorkerCallee>,
) -> WorkerCallee {
    match source {
        AliasSource::Known(callee) => *callee,
        AliasSource::Binding(symbol) => resolved.get(symbol).copied().unwrap_or(WorkerCallee::No),
        AliasSource::OneOf(arms) => {
            let arms: Vec<_> = arms
                .iter()
                .map(|arm| resolve_alias(arm, resolved))
                .collect();
            if arms.iter().all(|arm| *arm == WorkerCallee::Proven) {
                WorkerCallee::Proven
            } else if arms.iter().any(|arm| *arm != WorkerCallee::No) {
                WorkerCallee::Unprovable
            } else {
                WorkerCallee::No
            }
        }
    }
}

/// The callee as the author wrote it, for the refusal that names their spelling.
/// Bounded because the span is arbitrary source: an `await import(…)` receiver
/// can run to several lines, and a diagnostic is not a code listing.
fn callee_spelling(code: &str, span: oxc_span::Span) -> String {
    let text = code
        .get(span.start as usize..span.end as usize)
        .unwrap_or("<constructor>");
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= 48 {
        return flattened;
    }
    flattened.chars().take(47).chain(['…']).collect()
}

/// A recognized constructor must be either an unresolved Node global or a
/// binding imported from Node's corresponding builtin. The imported identity,
/// rather than its local spelling, is what survives `import { Worker as W }`.
/// Semantic references make nested parameters and locals fail this test at their
/// individual use sites rather than disabling every URL in the file.
fn is_compiler_builtin(
    identifier: &oxc_ast::ast::IdentifierReference<'_>,
    name: &str,
    semantic: &oxc_semantic::Semantic<'_>,
    node_builtins: &BTreeMap<oxc_semantic::SymbolId, &'static str>,
) -> bool {
    (identifier.name == name && is_unbound_global(identifier, semantic))
        || identifier
            .reference_id
            .get()
            .and_then(|id| semantic.scoping().get_reference(id).symbol_id())
            .is_some_and(|symbol| {
                node_builtins
                    .get(&symbol)
                    .is_some_and(|imported| *imported == name)
            })
}

/// Name the executable worker forms that have no module root to wrap.  This is
/// deliberately a positive, syntactic check: a computed path or an ordinary
/// unresolved worker without `{ eval: true }` stays on the compiler's existing
/// policy instead of being rejected merely because this pass cannot prove it.
fn unsupported_worker_reason(
    expr: &NewExpression<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
    node_builtins: &BTreeMap<oxc_semantic::SymbolId, &'static str>,
) -> Option<String> {
    if worker_options_enable_eval(expr) {
        return Some("`new Worker(..., { eval: true })` executes arbitrary source text".into());
    }
    let first = expr.arguments.first()?.as_expression()?;
    let specifier = match first.get_inner_expression() {
        Expression::NewExpression(url) if is_url_constructor(url, semantic, node_builtins) => {
            static_specifier(url.arguments.first()?.as_expression()?)?
        }
        other => static_specifier(other)?,
    };
    inline_worker_scheme(&specifier)
        .then(|| format!("`new Worker({specifier:?})` has no file-backed module root"))
}

fn is_url_constructor(
    expr: &NewExpression<'_>,
    semantic: &oxc_semantic::Semantic<'_>,
    node_builtins: &BTreeMap<oxc_semantic::SymbolId, &'static str>,
) -> bool {
    matches!(&expr.callee, Expression::Identifier(callee) if is_compiler_builtin(callee, "URL", semantic, node_builtins))
}

fn inline_worker_scheme(specifier: &str) -> bool {
    specifier
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
        || specifier
            .get(..5)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("blob:"))
}

fn worker_options_enable_eval(expr: &NewExpression<'_>) -> bool {
    use oxc_ast::ast::{ObjectPropertyKind, PropertyKind};

    let Some(Expression::ObjectExpression(options)) =
        expr.arguments.get(1).and_then(|arg| arg.as_expression())
    else {
        return false;
    };
    options.properties.iter().any(|property| {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return false;
        };
        property.kind == PropertyKind::Init
            && !property.computed
            && (property.key.is_specific_id("eval") || property.key.is_specific_string_literal("eval"))
            && matches!(property.value.get_inner_expression(), Expression::BooleanLiteral(value) if value.value)
    })
}

/// `import.meta.url`, exactly — the only base whose resolution nub can predict.
fn is_import_meta_url(expr: &Expression<'_>) -> bool {
    let Expression::StaticMemberExpression(member) = expr else {
        return false;
    };
    member.property.name == "url"
        && matches!(&member.object, Expression::MetaProperty(meta)
            if meta.meta.name == "import" && meta.property.name == "meta")
}

/// Binding symbol ids mapped to their imported Node builtin identity.
/// Every other binding is handled as authored code by semantic reference
/// resolution.  Retaining the imported name makes aliased imports first-class
/// without mistaking an arbitrary local `Worker`/`URL` for a compiler builtin.
fn node_builtin_import_bindings(
    program: &Program<'_>,
) -> BTreeMap<oxc_semantic::SymbolId, &'static str> {
    use oxc_ast::ast::{ImportDeclarationSpecifier, ModuleExportName, Statement};

    program
        .body
        .iter()
        .filter_map(|statement| {
            let Statement::ImportDeclaration(import) = statement else {
                return None;
            };
            Some(import)
        })
        .flat_map(|import| import.specifiers.iter().flatten().filter_map(move |specifier| {
            let ImportDeclarationSpecifier::ImportSpecifier(specifier) = specifier else {
                return None;
            };
            let imported_url = matches!(&specifier.imported, ModuleExportName::IdentifierName(name) if name.name == "URL");
            let identity = (import.source.value == "node:url" && imported_url)
                .then_some("URL")
                .or_else(|| {
                    let imported_worker = matches!(&specifier.imported, ModuleExportName::IdentifierName(name) if name.name == "Worker");
                    (import.source.value == "node:worker_threads" && imported_worker)
                        .then_some("Worker")
                })?;
            specifier.local.symbol_id.get().map(|symbol| (symbol, identity))
        }))
        .collect()
}

/// Resolve a URL specifier's `%XX` escapes into the path bytes the RUNTIME will
/// open, or `None` when the result is not a usable path.
///
/// Skipping this does not merely miss a file — it opens the WRONG one. The
/// specifier is URL text, so `./my%20file.bin` names `my file.bin`; joining the
/// raw string instead looks for a literal `my%20file.bin`, which usually does not
/// exist (so every filename containing a space silently stopped being embeddable)
/// and, when it does, embeds a different file than the program reads, with
/// nothing failing at build time.
///
/// An invalid escape is left verbatim, matching the WHATWG URL parser. Bytes that
/// do not reassemble into UTF-8 yield `None` rather than a lossy path.
fn percent_decode(spec: &str) -> Option<String> {
    if !spec.contains('%') {
        return Some(spec.to_string());
    }
    let raw = spec.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let hex = (i + 2 < raw.len())
            .then(|| std::str::from_utf8(&raw[i + 1..i + 3]).ok())
            .flatten()
            .filter(|_| raw[i] == b'%')
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(raw[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// An RFC 3986 `scheme:` — ALPHA then alphanumerics/`+-.` — which makes a
/// specifier an absolute URL rather than a reference resolved against the base.
fn has_url_scheme(spec: &str) -> bool {
    let mut chars = spec.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return false;
        }
    }
    false
}

/// A template literal with no interpolation is as static as a string literal —
/// Rolldown resolves both, so both are analyzable here.
///
/// TYPE WRAPPERS ARE ERASED FIRST, because the TypeScript transform erases them
/// too: `import("./x" as string)` reaches the resolver as `import("./x")`, so
/// reading the specifier through the wrapper is what keeps this analysis agreeing
/// with what the bundler actually follows. Reading it as written reported a
/// literal as computed and then told the author to make it a static string it had
/// already written — advice that cannot be acted on, which is what sends people
/// to hide the site from the scanner instead. `get_inner_expression` unwraps
/// exactly the set oxc's own transform drops: `as`, `satisfies`, `!`, `<T>`,
/// instantiation, and parentheses.
fn static_specifier(expr: &Expression<'_>) -> Option<String> {
    match expr.get_inner_expression() {
        Expression::StringLiteral(s) => Some(s.value.to_string()),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() => t
            .quasis
            .first()
            .map(|q| q.value.cooked.as_ref().unwrap_or(&q.value.raw).to_string()),
        _ => None,
    }
}

// ---- jsx --------------------------------------------------------------------

/// Neutralize a tsconfig `compilerOptions.jsx: "preserve"`, which Rolldown
/// otherwise honors by emitting raw JSX — syntax no JavaScript engine parses, so
/// the artifact dies on first run. "Preserve" means "a later tool transforms
/// this"; when nub compiles, there IS no later tool, and `nub run` already
/// refuses to preserve (runtime/transform-core.mjs maps everything but `"react"`
/// to the automatic runtime), so honoring it here would also drift compile from
/// run.
///
/// Applied ONLY when the entry's own effective tsconfig says `preserve`, because
/// the only way to defeat it is to name a runtime, and naming one also freezes
/// Rolldown's PER-FILE choice for the rest of the graph — a project on
/// `jsx: "react"` (classic, `jsxFactory`) must keep that. `import_source` is
/// deliberately left unset so Rolldown still fills it per file from each file's
/// own `jsxImportSource`.
fn jsx_override(
    entry_abs: &Path,
    explicit_tsconfig: Option<&Path>,
) -> Option<BundlerTransformOptions> {
    (effective_jsx_setting(entry_abs, explicit_tsconfig)? == "preserve").then(|| {
        BundlerTransformOptions {
            jsx: Some(Either::Right(JsxOptions {
                runtime: Some("automatic".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        }
    })
}

/// `compilerOptions.jsx` for the entry, with the `extends` chain applied.
///
/// Uses the resolver Rolldown itself resolves tsconfigs with, so a package
/// `extends` (`@tsconfig/bun/tsconfig.json` — how the opencode tree inherits its
/// settings) resolves identically to what the bundler will see.
fn effective_jsx_setting(entry_abs: &Path, explicit_tsconfig: Option<&Path>) -> Option<String> {
    let path = match explicit_tsconfig {
        Some(p) => absolutize(p),
        None => entry_abs
            .parent()?
            .ancestors()
            .map(|dir| dir.join("tsconfig.json"))
            .find(|p| p.is_file())?,
    };
    let resolver = oxc_resolver::Resolver::new(oxc_resolver::ResolveOptions::default());
    let tsconfig = resolver.resolve_tsconfig(&path).ok()?;
    tsconfig.compiler_options.jsx.clone()
}

fn sourcemap_type(mode: SourcemapMode) -> Option<SourceMapType> {
    match mode {
        SourcemapMode::Linked => Some(SourceMapType::File),
        SourcemapMode::Inline => Some(SourceMapType::Inline),
        SourcemapMode::External => Some(SourceMapType::Hidden),
        SourcemapMode::None => None,
    }
}

/// `--tree-shake false` disables tree-shaking outright; `--ignore-annotations`
/// keeps it on but stops trusting the `@__PURE__` family, which is the lever for
/// a dependency that annotates a side-effectful call as pure.
fn treeshake_options(opts: &BundleOptions) -> TreeshakeOptions {
    if !opts.tree_shake {
        return TreeshakeOptions::Boolean(false);
    }
    if opts.ignore_annotations {
        return TreeshakeOptions::Option(InnerOptions {
            annotations: Some(false),
            ..Default::default()
        });
    }
    TreeshakeOptions::Boolean(true)
}

fn defines(opts: &BundleOptions) -> Result<FxIndexMap<String, String>> {
    let mut map = FxIndexMap::default();
    for (k, v) in &opts.auto_define {
        map.insert(k.clone(), v.clone());
    }
    for raw in &opts.define {
        let (k, v) = split_once_eq(raw).ok_or_else(|| {
            anyhow!(
                "--define expects KEY=VALUE, got {raw:?}\n\
                 \x20\x20Values are JavaScript expressions, so a string needs its own quotes:\n\
                 \x20\x20--define 'API_URL=\"https://example.com\"'"
            )
        })?;
        map.insert(k, v);
    }
    Ok(map)
}

/// Rolldown's `ResolveOptions::alias` shape: a specifier maps to an ordered list
/// of replacements (`None` meaning "resolve to nothing"). nub only ever emits a
/// single concrete target per alias.
type AliasEntries = Vec<(String, Vec<Option<String>>)>;

fn alias_entries(raw: &[String]) -> Result<Option<AliasEntries>> {
    if raw.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let (from, to) =
            split_once_eq(item).ok_or_else(|| anyhow!("--alias expects FROM=TO, got {item:?}"))?;
        if from.is_empty() || to.is_empty() {
            bail!("--alias expects FROM=TO with both sides non-empty, got {item:?}");
        }
        out.push((from, vec![Some(to)]));
    }
    Ok(Some(out))
}

/// Build the `--external` predicate.
///
/// Rolldown's string form (`IsExternal::StringOrRegex`) compares the specifier
/// for EQUALITY, so `prettier` there would bundle `prettier/plugins/babel`
/// anyway — a silent half-externalization. Rolldown's own predicate variant
/// expresses the package-scoped rule the flag's `<PKG>` value promises, so this
/// stays inside Rolldown's API rather than inventing a matcher on top of it.
///
/// INVARIANT: [`is_package_specifier`] is mirrored by the runtime resolve hook
/// (`compile::external`). If the two rules diverge, a specifier can be left out
/// of the bundle and then not redirected at run time — an `ERR_MODULE_NOT_FOUND`
/// with no diagnosis.
fn external_matcher(packages: &[String]) -> Result<Option<IsExternal>> {
    if packages.is_empty() {
        return Ok(None);
    }
    validate_external_packages(packages)?;
    let packages = packages.to_vec();
    // The return type is spelled out because `IsExternal::Fn` holds a `dyn Fn`
    // whose return is itself a boxed trait object: without it, inference gives
    // the closure a concrete future type and the unsizing coercion fails.
    type Answer = Pin<Box<dyn Future<Output = Result<bool>> + Send + 'static>>;
    Ok(Some(IsExternal::Fn(Some(Arc::new(
        move |specifier: &str, _importer: Option<&str>, _is_resolved: bool| -> Answer {
            let matched = packages
                .iter()
                .any(|pkg| is_package_specifier(specifier, pkg));
            Box::pin(async move { Ok(matched) })
        },
    )))))
}

fn validate_external_packages(packages: &[String]) -> Result<()> {
    // Only a BARE specifier can be honored. A path-shaped value bakes the build
    // host's own path into the chunk as an import specifier (Rolldown's
    // resolved-id external call matches it) or normalizes to something the
    // runtime hook cannot re-base — either way a broken artifact with nothing
    // failing at build time.
    for pkg in packages {
        let bad = pkg.trim().is_empty()
            || pkg.starts_with(['.', '/', '\\'])
            || pkg.contains(':')
            || pkg != pkg.trim();
        if bad {
            bail!(
                "--external expects a bare package name, got {pkg:?}\n\
                 \x20\x20A path, a URL, or a `node:` specifier cannot be resolved on the\n\
                 \x20\x20machine the binary runs on. Name the package: --external prettier"
            );
        }
    }
    Ok(())
}

/// Does `specifier` name `pkg` or one of its subpaths?
fn is_package_specifier(specifier: &str, pkg: &str) -> bool {
    specifier == pkg
        || (specifier.starts_with(pkg) && specifier.as_bytes().get(pkg.len()) == Some(&b'/'))
}

/// Compile-only externalization. Rolldown's `external` callback runs before
/// plugins, so it cannot carry an importer into emitted code; this plugin
/// substitutes a unique external module id which the generated hook decodes.
#[derive(Debug)]
struct ExternalImports {
    packages: Vec<String>,
    launch_root: PathBuf,
    private_importers: Arc<Mutex<BTreeSet<String>>>,
    records: Mutex<BTreeMap<String, ExternalImport>>,
}

impl ExternalImports {
    fn new(
        packages: &[String],
        launch_root: &Path,
        private_importers: Arc<Mutex<BTreeSet<String>>>,
    ) -> Self {
        Self {
            packages: packages.to_vec(),
            // Rolldown canonicalizes module ids. Match that spelling before
            // deriving portable importer paths (`/tmp` is `/private/tmp` on
            // macOS), or provenance can accidentally encode the build root.
            launch_root: canonicalize_for_bundler(launch_root),
            private_importers,
            records: Mutex::new(BTreeMap::new()),
        }
    }

    fn take(&self) -> Vec<ExternalImport> {
        self.records
            .lock()
            .map(|mut records| std::mem::take(&mut *records).into_values().collect())
            .unwrap_or_default()
    }

    fn record(&self, specifier: &str, importer: Option<&str>) -> Option<ExternalImport> {
        // `ExternalImports` runs before `CompilePreamble`, so a matching
        // `--external` must not steal an import made by the private runtime
        // prelude. Those dependencies are bundled from nub's runtime directory,
        // not resolved from the user's launch directory.
        if importer.is_some_and(|importer| {
            is_compile_private_importer(importer)
                || self
                    .private_importers
                    .lock()
                    .map_or(true, |ids| ids.contains(importer))
        }) {
            return None;
        }
        let package = self
            .packages
            .iter()
            .find(|pkg| is_package_specifier(specifier, pkg))?
            .clone();
        let importer = importer.and_then(|id| portable_importer(id, &self.launch_root));
        let mut hash = Sha256::new();
        for part in [
            specifier,
            package.as_str(),
            importer.as_deref().unwrap_or(""),
        ] {
            hash.update((part.len() as u64).to_le_bytes());
            hash.update(part.as_bytes());
        }
        let id = format!("\0nub:compile-external:{:x}", hash.finalize());
        Some(ExternalImport {
            id,
            specifier: specifier.to_string(),
            package,
            importer,
        })
    }
}

/// Private compiler modules have their own runtime resolution contract and are
/// never subject to a user's `--external` package list.
fn is_compile_private_importer(id: &str) -> bool {
    id.starts_with("\0nub:compile-")
}

impl Plugin for ExternalImports {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:compile-externals")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId
    }

    fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let record = self.record(args.specifier, args.importer);
        if let Some(record) = &record {
            if let Ok(mut records) = self.records.lock() {
                records
                    .entry(record.id.clone())
                    .or_insert_with(|| record.clone());
            }
        }
        async move {
            Ok(record.map(|record| HookResolveIdOutput {
                id: record.id.into(),
                external: Some(ResolvedExternal::Bool(true)),
                ..Default::default()
            }))
        }
    }
}

/// Convert only a genuine file importer to a path portable from the launch cwd.
/// Opaque virtual/data ids intentionally keep the old cwd resolution behavior.
fn portable_importer(id: &str, launch_root: &Path) -> Option<String> {
    let mut path = if let Some(url) = id.strip_prefix("file:") {
        url::Url::parse(&format!("file:{url}"))
            .ok()?
            .to_file_path()
            .ok()?
    } else if id.starts_with('\0') || id.contains("://") {
        return None;
    } else {
        PathBuf::from(id)
    };
    if path.is_relative() {
        path = launch_root.join(path);
    }
    pathdiff::diff_paths(path, launch_root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

/// Split at the FIRST `=`: a define value (`K='a=b'`) and an alias target may
/// both legitimately contain one.
fn split_once_eq(s: &str) -> Option<(String, String)> {
    let (k, v) = s.split_once('=')?;
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

// ---- unresolvable-import rejection -------------------------------------------

/// Why the bundler cannot follow a site. Each variant is a DIFFERENT authoring
/// mistake, so each gets its own fix line in the diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SiteKind {
    /// `import(expr)` — no statically analyzable specifier.
    Dynamic,
    /// `import(name)` where `name` is a module-scope `const` whose value this
    /// pass can read. Static to its author and to nobody else: the bundler
    /// follows the ARGUMENT, never the binding, so the module is still left out
    /// of the artifact — but "make the specifier a static string" is advice this
    /// author has already taken, so the site needs its own fix line.
    Variable,
    /// `require("./x")` whose `require` is a LOCAL binding (a UMD factory
    /// parameter, or `createRequire`). The specifier is static, but the call is
    /// an ordinary function call to the bundler, so it is left verbatim and
    /// resolves — against the extracted app dir — only at runtime.
    Indirect,
}

/// One call site the bundler cannot follow.
#[derive(Debug, Clone)]
struct DynamicSite {
    kind: SiteKind,
    module: String,
    line: usize,
    column: usize,
    snippet: String,
    /// Every string the specifier can evaluate to, when that set is knowable.
    /// Empty for a genuinely computed one — which is the difference between
    /// telling the author what to inline and having nothing to say.
    resolves_to: Vec<String>,
}

/// Collects unresolvable `import()` / `require()` sites while the graph loads.
///
/// The `transform` hook is the right seam: it sees every module in the bundle
/// (dependencies included) with its ORIGINAL path and source, so a diagnostic
/// can name the real file and line. Scanning the emitted bundle instead would
/// only ever be able to point at minified output — and for the indirect-require
/// case it could not point at anything at all, since minification renames the
/// shadowing `require` parameter and erases the pattern.
#[derive(Debug, Default)]
struct DynamicImportScan {
    sites: Mutex<Vec<DynamicSite>>,
}

impl DynamicImportScan {
    fn take(&self) -> Vec<DynamicSite> {
        self.sites
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }
}

impl Plugin for DynamicImportScan {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:reject-unresolved")
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let scan = scan_unresolvable_with_rewrites(args.id, args.code);
        let rewritten = (!scan.dynamic_import_ignores.is_empty()).then(|| {
            let magic = dynamic_import_magic_string(args.code, &scan.dynamic_import_ignores);
            let code = magic.to_string();
            let map = magic.source_map(SourceMapOptions {
                hires: Hires::Boundary,
                include_content: true,
                source: args.id.to_string().into(),
            });
            (code, HookTransformOutputMap::from(map))
        });
        async move {
            if !scan.sites.is_empty() {
                if let Ok(mut sites) = self.sites.lock() {
                    sites.extend(scan.sites);
                }
            }
            Ok(rewritten.map(|(code, map)| HookTransformOutput {
                code: Some(code),
                map,
                ..Default::default()
            }))
        }
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::Transform
    }
}

/// Parse `source` and report every `import()` / `require()` that will still need
/// resolving once the artifact runs. A parse failure yields nothing: the
/// bundler's own parse error is the better diagnostic, and this pass must never
/// be the thing that fails a build.
#[cfg(test)]
fn scan_unresolvable(id: &str, source: &str) -> Vec<DynamicSite> {
    scan_unresolvable_with_rewrites(id, source).sites
}

/// The unresolved sites plus the dynamic `import()` arguments Rolldown must not
/// reinterpret as static after this scan has classified them as runtime work.
#[derive(Debug, Default)]
struct UnresolvableScan {
    sites: Vec<DynamicSite>,
    dynamic_import_ignores: Vec<usize>,
}

fn scan_unresolvable_with_rewrites(id: &str, source: &str) -> UnresolvableScan {
    use oxc_allocator::Allocator;
    use oxc_ast::AstKind;
    use oxc_ast::ast::{BindingPattern, CallExpression, Expression, FormalParameters, Statement};
    use oxc_ast_visit::{Visit, walk};
    use oxc_parser::Parser;
    use oxc_span::{GetSpan, SourceType, Span};

    struct Visitor<'source> {
        source: &'source str,
        /// One entry per enclosing function; `true` when that function's own
        /// parameters bind `require`.
        fn_stack: Vec<bool>,
        /// A module-level `var/let/const require = …` (the `createRequire` shape)
        /// shadows for the whole file.
        module_binds_require: bool,
        /// Module-scope `const` specifiers this pass could read. See
        /// [`literal_consts`].
        consts: BTreeMap<String, Vec<String>>,
        found: Vec<(SiteKind, Span, Vec<String>)>,
        /// Byte offsets at the start of every non-literal import argument.
        /// Rolldown's `/* @vite-ignore */` marker deliberately keeps the
        /// operation in the emitted JavaScript instead of making it a module
        /// graph edge. This is needed even for a constant binding: later
        /// optimization may fold the binding, but the opted-in runtime hook is
        /// the contract for every expression-shaped `import()`.
        dynamic_import_ignores: Vec<usize>,
    }

    impl Visitor<'_> {
        fn require_is_local(&self) -> bool {
            self.module_binds_require || self.fn_stack.iter().any(|shadows| *shadows)
        }

        /// What can be said about an `import()` whose specifier is not a literal.
        ///
        /// Naming a readable binding is the whole point; when the specifier is
        /// anything else this deliberately says nothing rather than guessing,
        /// because a wrong "it resolves to X" sends the author to inline a value
        /// their program never uses.
        fn classify_import(&self, source: &Expression<'_>) -> (SiteKind, Vec<String>) {
            let Expression::Identifier(name) = source.get_inner_expression() else {
                return (SiteKind::Dynamic, Vec::new());
            };
            match self.consts.get(name.name.as_str()) {
                Some(values) => (SiteKind::Variable, values.clone()),
                None => (SiteKind::Dynamic, Vec::new()),
            }
        }

        /// `Some` when this `require(...)` is a live, unconditional dependency
        /// the artifact cannot satisfy. An ordinary top-level `require("pkg")`
        /// returns `None` — the bundler rewrites those, and genuinely missing
        /// ones already arrive as Rolldown UNRESOLVED_IMPORT warnings.
        ///
        /// A NON-static `require(expr)` is deliberately NOT flagged, unlike its
        /// `import(expr)` counterpart. Measured against opencode's tree, every
        /// one of the five instances was a guarded optional loader —
        /// `@protobufjs/inquire` ("requires a module only if available"),
        /// node-pty's platform-binding probe, TypeScript's plugin loader,
        /// yargs-parser's `--config` reader — each wrapped in try/catch or a
        /// `typeof require` guard, and each harmless in a compiled artifact.
        /// Failing on them would break compiles that work.
        fn classify_require(&self, call: &CallExpression<'_>) -> Option<SiteKind> {
            let Expression::Identifier(callee) = &call.callee else {
                return None;
            };
            if callee.name != "require" {
                return None;
            }
            let spec = static_specifier(call.arguments.first()?.as_expression()?)?;
            // A builtin resolves from the artifact exactly as it does in
            // development — it never needed node_modules.
            if nub_phantom_core::builtins::is_builtin(&spec) {
                return None;
            }
            self.require_is_local().then_some(SiteKind::Indirect)
        }
    }

    impl<'a> Visit<'a> for Visitor<'_> {
        fn enter_node(&mut self, kind: AstKind<'a>) {
            match kind {
                AstKind::Function(f) => self.fn_stack.push(binds_require(&f.params)),
                AstKind::ArrowFunctionExpression(a) => self.fn_stack.push(binds_require(&a.params)),
                _ => {}
            }
        }

        fn leave_node(&mut self, kind: AstKind<'a>) {
            if matches!(
                kind,
                AstKind::Function(_) | AstKind::ArrowFunctionExpression(_)
            ) {
                self.fn_stack.pop();
            }
        }

        fn visit_expression(&mut self, expr: &Expression<'a>) {
            if let Expression::ImportExpression(imp) = expr
                && static_specifier(&imp.source).is_none()
            {
                let (kind, resolves_to) = self.classify_import(&imp.source);
                self.found.push((kind, imp.span, resolves_to));
                let argument_start = imp.source.span().start as usize;
                let import_start = imp.span.start as usize;
                // Do not duplicate an author-supplied marker. Rolldown has
                // already been told to preserve this import at runtime, while
                // the site must still reach Nub's opt-in diagnostic/policy.
                if !self.source[import_start..argument_start].contains("@vite-ignore") {
                    self.dynamic_import_ignores.push(argument_start);
                }
            }
            walk::walk_expression(self, expr);
        }

        fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
            if let Some(kind) = self.classify_require(call) {
                self.found.push((kind, call.span, Vec::new()));
            }
            walk::walk_call_expression(self, call);
        }
    }

    fn binds_require(params: &FormalParameters<'_>) -> bool {
        params.items.iter().any(
            |p| matches!(&p.pattern, BindingPattern::BindingIdentifier(id) if id.name == "require"),
        )
    }

    // A Rolldown module id can carry a `?query`, which is why every other path
    // read in this file cleans it first. Taking the source type off the raw id
    // falls back to `mjs`, and a TypeScript module parsed as JavaScript loses
    // every site in it — so the unresolved import this gate exists to refuse
    // would compile clean, which is the failure mode the gate is for.
    let module = clean_url(id);
    let source_type = SourceType::from_path(module).unwrap_or_else(|_| SourceType::mjs());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return UnresolvableScan::default();
    }
    let mut visitor = Visitor {
        source,
        fn_stack: Vec::new(),
        module_binds_require: parsed.program.body.iter().any(|stmt| {
            matches!(stmt, Statement::VariableDeclaration(d)
            if d.declarations.iter().any(|decl| {
                matches!(&decl.id, BindingPattern::BindingIdentifier(id) if id.name == "require")
            }))
        }),
        // A second full walk, so it is skipped for the modules that cannot need
        // it — the same cheap reject `cjs_path_globals_edit` opens with, and most
        // of a dependency graph names `import(` nowhere.
        consts: if source.contains("import(") {
            literal_consts(&parsed.program)
        } else {
            BTreeMap::new()
        },
        found: Vec::new(),
        dynamic_import_ignores: Vec::new(),
    };
    visitor.visit_program(&parsed.program);

    let sites = visitor
        .found
        .into_iter()
        .map(|(kind, span, resolves_to)| {
            let (line, column) = line_col(source, span.start as usize);
            DynamicSite {
                kind,
                module: module.to_string(),
                line,
                column,
                snippet: snippet(source, span.start as usize, span.end as usize),
                resolves_to,
            }
        })
        .collect();
    UnresolvableScan {
        sites,
        dynamic_import_ignores: visitor.dynamic_import_ignores,
    }
}

/// Keep every expression-shaped dynamic import out of Rolldown's static graph.
///
/// `/* @vite-ignore */` is Rolldown's documented scanner escape hatch. Its
/// scanner checks this marker before it accepts a static request, so it also
/// protects the constant variable form against later constant folding. The
/// inserted text does not change JavaScript's runtime value or control flow.
#[cfg(test)]
fn ignore_dynamic_imports(source: &str, offsets: &[usize]) -> String {
    dynamic_import_magic_string(source, offsets).to_string()
}

/// Make the scanner escape rewrite together with a source map that accounts
/// for every inserted marker. `prepend_right` fixes each insertion at the
/// original source offset, so no reverse-order string mutation can shift a
/// later insertion's location.
fn dynamic_import_magic_string(source: &str, offsets: &[usize]) -> MagicString<'static> {
    const MARKER: &str = "/* @vite-ignore */ ";
    let mut offsets = offsets.to_vec();
    offsets.sort_unstable();
    offsets.dedup();

    let mut magic = MagicString::new(source.to_owned());
    for offset in offsets {
        // Every offset originates from an oxc span into `source`, so it is a
        // UTF-8 boundary. Re-check to keep this helper total if a future caller
        // supplies a malformed offset.
        if source.is_char_boundary(offset) {
            magic.prepend_right(offset as u32, MARKER);
        }
    }
    magic
}

/// Module-scope `const` bindings whose specifier value this pass can read.
///
/// The narrowest analysis that recognizes the shape which keeps reaching the
/// unresolved-import diagnostic: a specifier named a line or two above the
/// `import()` that uses it, most often a platform package picked out of a short
/// list. `const` is the whole trick — the language guarantees no reassignment, so
/// there is no data flow to get wrong — and a ternary between literals is the one
/// composite worth reading through, because that is how a platform pick is
/// written. A concatenation, a call, a `let`: unresolved, deliberately. This pass
/// only ever adds a sentence to a diagnostic, and it never converts a refusal
/// into a pass, so the cost of saying nothing is small and the cost of saying
/// something wrong is an author inlining a value their program never takes.
///
/// A name bound more than once ANYWHERE is dropped: the `import(name)` may be
/// reaching an inner shadow, and telling which needs the scope analysis this pass
/// exists to avoid.
fn literal_consts(program: &Program<'_>) -> BTreeMap<String, Vec<String>> {
    use oxc_ast::ast::{BindingIdentifier, BindingPattern, Statement, VariableDeclarationKind};
    use oxc_ast_visit::Visit;

    #[derive(Default)]
    struct Bindings(BTreeMap<String, usize>);

    impl<'a> Visit<'a> for Bindings {
        fn visit_binding_identifier(&mut self, it: &BindingIdentifier<'a>) {
            *self.0.entry(it.name.to_string()).or_default() += 1;
        }
    }

    let mut bindings = Bindings::default();
    bindings.visit_program(program);

    let mut out = BTreeMap::new();
    for stmt in &program.body {
        let Statement::VariableDeclaration(decl) = stmt else {
            continue;
        };
        if decl.kind != VariableDeclarationKind::Const {
            continue;
        }
        for declarator in &decl.declarations {
            let BindingPattern::BindingIdentifier(id) = &declarator.id else {
                continue;
            };
            if bindings.0.get(id.name.as_str()) != Some(&1) {
                continue;
            }
            if let Some(init) = &declarator.init
                && let Some(values) = specifier_candidates(init, 8)
            {
                out.insert(id.name.to_string(), values);
            }
        }
    }
    out
}

/// Every string an initializer can evaluate to, or `None` when that set is not
/// knowable.
///
/// `depth` bounds the ternary recursion: a minified chain right-nests
/// arbitrarily far, and this pass must never be the thing that overflows the
/// stack on someone's dependency.
fn specifier_candidates(expr: &Expression<'_>, depth: u32) -> Option<Vec<String>> {
    if let Some(literal) = static_specifier(expr) {
        return Some(vec![literal]);
    }
    if depth == 0 {
        return None;
    }
    let Expression::ConditionalExpression(cond) = expr.get_inner_expression() else {
        return None;
    };
    let mut values = specifier_candidates(&cond.consequent, depth - 1)?;
    values.extend(specifier_candidates(&cond.alternate, depth - 1)?);
    Some(values)
}

/// Specifier values as a reader would write them, for a diagnostic.
fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|v| format!("{v:?}"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let prefix = source.get(..offset).unwrap_or(source);
    let line = prefix.matches('\n').count() + 1;
    let column = prefix
        .rsplit('\n')
        .next()
        .map_or(1, |l| l.chars().count() + 1);
    (line, column)
}

/// The offending expression, on one line and bounded — a bundled dependency can
/// carry a very long `import()` argument.
fn snippet(source: &str, start: usize, end: usize) -> String {
    const MAX: usize = 72;
    let text = source
        .get(start..end.min(source.len()))
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() > MAX {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    } else {
        text
    }
}

/// Render bundler errors as `file:line:column` + message, in the same shape the
/// unresolved-import and invalid-chunk gates below use.
///
/// `BuildDiagnostic`'s `Debug` prints only severity/kind/message, so `{e:?}` gave
/// a `PARSE_ERROR` with no indication of WHICH file failed — undebuggable on a
/// real migration without reproducing each failure standalone. Rolldown carries
/// the location already; `to_diagnostic_with` is what materializes it.
fn render_diagnostics(err: &rolldown_error::BatchedBuildDiagnostic) -> String {
    let opts = DiagnosticOptions::default();
    err.iter()
        .map(|d| {
            let diagnostic = d.to_diagnostic_with(&opts);
            let message = d.to_message_with(&opts);
            // A plugin-wrapped event's own `message()` is deliberately empty — its
            // real text is injected by `on_diagnostic` — so fall back to the full
            // rendered report, which carries its own location.
            if message.is_empty() {
                return diagnostic.to_string();
            }
            // Columns are 0-based here and 1-based in every other diagnostic nub
            // prints, so they are converted rather than passed through.
            match diagnostic.get_primary_location() {
                Some((file, line, column, _)) => {
                    format!(
                        "\x20\x20{file}:{line}:{}\n\x20\x20\x20\x20{message}",
                        column + 1
                    )
                }
                // No label — a config or resolve error, not a source position.
                // The module id is still the most locating thing available.
                None => match d.id() {
                    Some(id) => format!(
                        "\x20\x20{}\n\x20\x20\x20\x20{message}",
                        opts.stabilize_path(id)
                    ),
                    None => format!("\x20\x20{message}"),
                },
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fail the build on any import the bundler could not resolve — the
/// `import(expr)` / `require(…)` sites the scan found, plus Rolldown's own
/// UNRESOLVED_IMPORT warnings for named specifiers that resolved to nothing.
///
/// `allow_dynamic` excuses the `import()` sites ONLY — both the computed and the
/// variable-held shape, since the runtime hook serves them identically. An
/// indirect `require` is a different defect with a different fix (the resolver
/// picked a UMD build), and an UNRESOLVED_IMPORT is a static specifier that
/// resolved to nothing — neither is served by a runtime resolve hook, so neither
/// is opted out of.
/// Whether nothing is installed at all: no `node_modules` above the entry.
///
/// Separate from the Plug'n'Play check, which also finds no `node_modules` but
/// has a different remedy — so that one is asked first and this stays quiet
/// whenever a `.pnp.cjs` explains the absence.
fn nothing_installed(entry: &Path) -> bool {
    !uses_plug_n_play(entry)
        && !entry
            .ancestors()
            .any(|dir| dir.join("node_modules").is_dir())
}

/// Whether the entry sits in a Yarn Plug'n'Play project.
///
/// A `.pnp.cjs` with no `node_modules` beside it: dependencies live in zip
/// archives and are resolved through that loader, so a directory walk finds
/// nothing. Both halves are required — a project can carry a stale `.pnp.cjs`
/// after switching linkers, and that one resolves normally.
fn uses_plug_n_play(entry: &Path) -> bool {
    entry
        .ancestors()
        .any(|dir| dir.join(".pnp.cjs").is_file() && !dir.join("node_modules").is_dir())
}

fn reject_unresolved(
    sites: &[DynamicSite],
    warnings: &[BuildDiagnostic],
    allow_dynamic: bool,
    pnp: bool,
    uninstalled: bool,
) -> Result<()> {
    let mut lines = Vec::new();
    let mut any_indirect = false;
    let mut any_dynamic = false;
    let mut any_variable = false;
    let mut any_native = false;
    // A site inside a dependency is not something the author can rewrite, so the
    // source-editing advice below is unusable for it — see `dependency_site_hint`.
    let mut any_dependency_site = false;
    for site in sites {
        any_native |= specifier_names_native_addon(&site.snippet);
        any_dependency_site |= unresolved_importer_is_dependency(&site.module);
        match site.kind {
            SiteKind::Dynamic | SiteKind::Variable if allow_dynamic => continue,
            SiteKind::Dynamic => any_dynamic = true,
            SiteKind::Variable => any_variable = true,
            SiteKind::Indirect => any_indirect = true,
        }
        let resolved = if site.resolves_to.is_empty() {
            String::new()
        } else {
            format!(
                "\n\x20\x20\x20\x20resolves to {}",
                quoted_list(&site.resolves_to)
            )
        };
        lines.push(format!(
            "\x20\x20{}:{}:{}\n\x20\x20\x20\x20{}{resolved}",
            site.module, site.line, site.column, site.snippet
        ));
    }
    let diag_opts = DiagnosticOptions::default();
    // An unresolved import whose importer is itself under node_modules is a
    // different failure from one in the author's own source, and every hint below
    // talks past it — see `optional_backend_hint`.
    let mut any_in_dependency = false;
    for w in warnings {
        if !matches!(w.kind(), EventKind::UnresolvedImport) {
            continue;
        }
        let message = w.to_message_with(&diag_opts);
        any_in_dependency |= unresolved_importer_is_dependency(&message);
        lines.push(format!("\x20\x20{message}"));
    }
    if lines.is_empty() {
        return Ok(());
    }

    // The indirect-require fix is a different action from the dynamic-specifier
    // one — the specifier is already static; what is wrong is the module format
    // the resolver picked — so the hint is only shown when it applies.
    let indirect_hint = if any_indirect && !any_native {
        "\n\x20\x20A require() whose `require` is a local binding (a UMD factory parameter or a\n\
         \x20\x20createRequire result) is an ordinary call the bundler cannot rewrite. Replace a\n\
         \x20\x20relative createRequire call with a static import. For a UMD dependency, depend on\n\
         \x20\x20the package's ESM build or alias the specifier to it with --alias."
    } else {
        ""
    };
    // Checked BEFORE the indirect hint is chosen, because a native addon reaches
    // this code as an indirect require and would otherwise collect that hint —
    // which tells the author to "depend on the package's ESM build". For a
    // per-platform `.node` selected at run time there is no ESM build, and the
    // code is in somebody else's package. Wrong advice is worse than none: it
    // sends people to edit a dependency they do not own.
    let native_hint = if any_native {
        "\n\x20\x20A .node addon is a platform binary the bundler cannot inline, and packages\n\
         \x20\x20pick one at run time from a list of per-platform variants — so the specifier\n\
         \x20\x20above is not something you can make static in the package's own source.\n\
         \x20\x20If the machine you ship to will have this package installed, --external\n\
         \x20\x20<package> leaves it to be resolved there. A self-contained binary carrying\n\
         \x20\x20its own copy of a native package is not supported yet."
    } else {
        ""
    };
    // The flag is named only where it would actually help. Offering it against
    // an indirect require or an UNRESOLVED_IMPORT would send the user to a
    // switch that changes nothing about their failure.
    let dynamic_hint = if any_dynamic {
        "\n\x20\x20Make the specifier a static string so the bundler can follow it. A specifier\n\
         \x20\x20your program computes at run time — a plugin path, a config module — cannot\n\
         \x20\x20be made static. Pass --allow-dynamic-import to keep it, and the binary will\n\
         \x20\x20resolve it from the directory it is started in. What it loads then depends\n\
         \x20\x20on the machine you ship to."
    } else {
        ""
    };
    // Separate from the computed-specifier hint because the fix is different and
    // the generic one is actively wrong here: this author DID write a static
    // string. The bundler follows the argument, not the binding, so what is left
    // to do is move the literal into the call.
    let variable_hint = if any_variable {
        "\n\x20\x20A specifier held in a variable is static to you and not to the bundler, which\n\
         \x20\x20follows the argument and never the binding. Write the literal inside the\n\
         \x20\x20import() — one call per branch whose value differs — and each one is bundled.\n\
         \x20\x20Pass --allow-dynamic-import to keep it as written instead, and the binary will\n\
         \x20\x20resolve it from the directory it is started in."
    } else {
        ""
    };
    // Named first, because under Plug'n'Play EVERY dependency is unresolved and
    // the other hints describe the wrong problem. The project runs perfectly under
    // `yarn node`, so the author has no reason to suspect their install layout.
    let pnp_hint = if pnp {
        "\n\n\x20\x20This project uses Yarn's Plug'n'Play, which keeps its dependencies in zip\n\
         \x20\x20archives and resolves them through .pnp.cjs rather than node_modules. Nub reads\n\
         \x20\x20a real directory tree, so it finds nothing to bundle. Install with a\n\
         \x20\x20node_modules linker — `yarn config set nodeLinker node-modules && yarn install`\n\
         \x20\x20— and compile again."
    } else {
        ""
    };
    // The likeliest reason of all, and the one the other hints talk past: the
    // dependencies were simply never installed. Worth saying plainly rather than
    // explaining why an unresolved import matters.
    let uninstalled_hint = if uninstalled {
        "\n\n\x20\x20There is no node_modules directory here, so nothing is installed. Run your\n\
         \x20\x20package manager's install first — `nub install` — and compile again."
    } else {
        ""
    };
    // Shown only when nothing installed / PnP already explains it, because those
    // two produce unresolved dependency imports for a reason that has nothing to
    // do with optional backends and their fix comes first.
    //
    // The case: a package supporting several interchangeable backends imports all
    // of them and guards each at run time, so every backend the author did NOT
    // install surfaces here. knex is the type specimen — ten database dialects,
    // of which a project uses one. The other hints are wrong for it in the same
    // way `native_hint` documents: they tell the author to edit a specifier that
    // is in somebody else's package. --external is what actually works, and it is
    // not obvious, because its own help says the package must be installed where
    // the binary runs — true for its usual use, and beside the point here, where
    // the package is meant to stay absent so the guard keeps taking the branch it
    // already takes today.
    let optional_backend_hint = if any_in_dependency && !uninstalled && !pnp {
        "\n\n\x20\x20The imports above are inside a dependency, not your own source, so their\n\
         \x20\x20specifiers are not yours to make static. A package that supports several\n\
         \x20\x20interchangeable backends — a database driver, a queue, a transport — imports\n\
         \x20\x20every one and guards each at run time, so the ones you did not install arrive\n\
         \x20\x20here. Pass --external <package> for each to leave it out of the bundle; the\n\
         \x20\x20dependency's own guard then finds it missing, exactly as it does today."
    } else {
        ""
    };
    // The site hints above all end in "edit the specifier", which is not available
    // when the site is in a dependency. Naming --unbundled is what actually works
    // there: the package ships in its installed layout, so its own require() and
    // __dirname resolve against real files exactly as they did before compiling.
    //
    // Withheld for a native addon, which has its own hint naming --external, and
    // under the two whole-tree explanations whose fix comes first.
    let dependency_site_hint = if any_dependency_site && !any_native && !uninstalled && !pnp {
        "\n\n\x20\x20At least one site above is inside a dependency rather than your own source,\n\
         \x20\x20so its specifier is not yours to rewrite. --unbundled <package> ships that\n\
         \x20\x20package as installed instead of bundling it, which keeps its own require()\n\
         \x20\x20and __dirname working against real files. Name every package listed above\n\
         \x20\x20that you do not own: unbundling only some of them can compile cleanly and\n\
         \x20\x20still fail at run time, when a package left bundled reads a file beside its\n\
         \x20\x20own source."
    } else {
        ""
    };
    bail!(
        "{} import{} could not be resolved at build time:\n{}\n\n\
         \x20\x20A compiled binary carries no node_modules, so an unresolved import fails at\n\
         \x20\x20runtime on the machine you ship to.{}{}{}{}{}{}{}{}",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" },
        lines.join("\n"),
        uninstalled_hint,
        pnp_hint,
        optional_backend_hint,
        dependency_site_hint,
        native_hint,
        dynamic_hint,
        variable_hint,
        indirect_hint
    );
}

/// Whether an unresolved import's IMPORTER is itself a dependency, rather than
/// the author's own source.
///
/// Read off the rendered message because `BuildDiagnostic` exposes no structured
/// importer path — `to_message_with` is the only accessor it offers. Rolldown
/// renders the importer as a path ("Could not resolve 'pg' in
/// node_modules/knex/lib/dialects/postgres/index.js"), so the segment is what
/// distinguishes an import the author can fix from one in a package they do not
/// own. A false negative merely withholds a hint; the error itself is unchanged.
fn unresolved_importer_is_dependency(message: &str) -> bool {
    message.contains("node_modules")
}

/// Whether an unresolved import names a native addon.
///
/// Matched on the snippet rather than a parsed specifier because that is what the
/// diagnostic already carries, and the shape is unambiguous: a `.node` file is a
/// platform binary, so any specifier ending in one is the native case regardless
/// of how it was written.
fn specifier_names_native_addon(snippet: &str) -> bool {
    snippet
        .split(['\'', '"', '`'])
        .any(|part| part.ends_with(".node"))
}

/// Refuse a `--unbundled` name nothing in the graph resolves.
///
/// The flag ships a package that IS in the bundle's graph without flattening it.
/// A name nothing imports cannot be carried out, and doing nothing quietly is
/// the worst available answer: the build reports success and the binary fails on
/// the user's machine. Compiling opencode hit exactly this — the platform
/// package holding OpenTUI's native library lives in the package manager's
/// isolated store, unreachable from the entry, and `--unbundled` shipped nothing
/// while reporting nothing.
fn refuse_unreached_unbundled(names: &[String]) -> Result<()> {
    let Some(first) = names.first() else {
        return Ok(());
    };
    let list = names
        .iter()
        .map(|n| format!("  {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "--unbundled named {} package{} nothing in the build resolves:\n{list}\n\
         \x20\x20The flag ships a package the bundle already reaches, in its own installed\n\
         \x20\x20layout. Check the spelling, and check it is installed where the entry can\n\
         \x20\x20resolve it — a package reached only through a specifier built at run time\n\
         \x20\x20is invisible here too, and needs --include to travel.\n\
         \x20\x20For example: --include node_modules/{first}",
        names.len(),
        if names.len() == 1 { "" } else { "s" }
    );
}

/// Note which packages ship unbundled, and why.
///
/// Not a warning. Loading an addon through a computed path is ordinary, correct
/// behaviour for a native package, and shipping such a package in its own
/// installed layout is how compile is designed to handle it — the bundle carries
/// everything else, and these sit beside it exactly as they were on disk, where
/// Node's own resolution finds them.
///
/// Printed because the artifact's contents are worth knowing: these packages are
/// the reason it is larger than the bundle alone, and naming the rule that
/// selected each one is what makes a surprising selection debuggable.
fn report_unbundled_packages(packages: &[String]) {
    // Deduped and sorted here rather than at the source. The set behind this is
    // keyed by package ROOT, and one package can be reached through several —
    // a workspace link and the store path it resolves to are different paths to
    // the same directory. Only one copy is ever materialised, so the repetition
    // was the report's alone: compiling opencode listed `@opentui/core` five
    // times and called it nine packages.
    let mut unique: Vec<&str> = packages.iter().map(String::as_str).collect();
    unique.sort_unstable();
    unique.dedup();
    if unique.is_empty() {
        return;
    }
    eprintln!(
        "Shipping {} package{} unbundled, in {} own installed layout:",
        unique.len(),
        if unique.len() == 1 { "" } else { "s" },
        if unique.len() == 1 { "its" } else { "their" }
    );
    for package in unique {
        eprintln!("  {package}");
    }
}

/// Name every `import()` that `--allow-dynamic-import` let through.
///
/// The flag is the sanctioned way to ship a genuinely computed specifier, and it
/// is also the one place a RESOLVABLE import can leave the graph with nothing
/// failing: the site survives into the artifact and is resolved from the launch
/// directory, which for a self-contained binary usually has no `node_modules` at
/// all. That lands as `Cannot find package …` on the deploy machine with nothing
/// at build time to attribute it to. Refusing is not available — the flag exists
/// precisely to allow this, and the maintainer confirmed it is needed in the wild
/// — so the build says which sites it deferred, and what each resolves to when
/// that is knowable, which is the difference between an author seeing their
/// platform package go unbundled and finding out on the deploy machine.
fn warn_deferred_imports(sites: &[&DynamicSite]) {
    // Enough to see the shape without burying the rest of the compile output; a
    // large graph can carry a lot of these and the count line follows anyway.
    const MAX: usize = 10;
    for site in sites.iter().take(MAX) {
        let resolved = if site.resolves_to.is_empty() {
            String::new()
        } else {
            format!(" — resolves to {}", quoted_list(&site.resolves_to))
        };
        eprintln!(
            "note: {}:{}:{} {} is resolved where the binary runs, not at build time{resolved}",
            site.module, site.line, site.column, site.snippet
        );
    }
    if let Some(rest) = sites.len().checked_sub(MAX).filter(|n| *n > 0) {
        eprintln!("note: … and {rest} more deferred import site(s)");
    }
}

// ---- emitted-chunk validity ---------------------------------------------------

/// Re-parse every emitted chunk and fail the compile if any is not valid
/// JavaScript.
///
/// The bundler can emit unparseable output and still report success — a tsconfig
/// `jsx: "preserve"` writes raw JSX straight through, and any future
/// transform/plugin gap does the same. The artifact then throws `SyntaxError` on
/// first import, frequently from a SHARED chunk only some commands reach, so the
/// failure looks intermittent and points nowhere near the cause. One parse per
/// chunk is negligible next to bundling, and it is what makes "Compiled …" a
/// claim about output that can actually load.
fn reject_invalid_chunks(chunks: &[BundledFile]) -> Result<()> {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let mut lines = Vec::new();
    for chunk in chunks {
        let Ok(code) = std::str::from_utf8(&chunk.bytes) else {
            lines.push(format!("\x20\x20{}: not valid UTF-8", chunk.name));
            continue;
        };
        let allocator = Allocator::default();
        // ESM with JSX OFF, which is exactly what Node accepts for the `.mjs`
        // files this emits: `format` is always `esm` above, and no Node parses
        // JSX. Anything this rejects, the shipped runtime rejects too.
        let parsed = Parser::new(&allocator, code, SourceType::mjs()).parse();
        let Some(first) = parsed.diagnostics.first() else {
            continue;
        };
        let offset = first
            .labels
            .as_slice()
            .first()
            .map_or(0, |l| l.offset() as usize);
        let (line, column) = line_col(code, offset);
        lines.push(format!(
            "\x20\x20{}:{}:{}\n\x20\x20\x20\x20{}",
            chunk.name, line, column, first.message
        ));
    }
    if lines.is_empty() {
        return Ok(());
    }

    bail!(
        "the bundler emitted {} chunk{} that {} not valid JavaScript:\n{}\n\n\
         \x20\x20The compiled binary would throw a SyntaxError on startup. This usually means\n\
         \x20\x20a source language survived the bundle untransformed — most often JSX, from a\n\
         \x20\x20tsconfig whose compilerOptions.jsx is \"preserve\".",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" },
        if lines.len() == 1 { "is" } else { "are" },
        lines.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> BundleOptions {
        BundleOptions {
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
            target_node: None,
        }
    }

    fn emitted_entry_code(result: &BundleResult) -> String {
        let entry = result
            .files
            .iter()
            .find(|file| file.name == result.entry)
            .expect("the reported entry chunk must be emitted");
        String::from_utf8(entry.bytes.clone()).expect("the emitted entry must be UTF-8")
    }

    fn emitted_chunk_code(result: &BundleResult) -> String {
        result
            .files
            .iter()
            .filter(|file| !file.name.ends_with(".map"))
            .map(|file| String::from_utf8_lossy(&file.bytes).into_owned())
            .collect()
    }

    #[test]
    fn define_splits_on_the_first_equals_and_user_overrides_the_auto_define() {
        let mut o = opts();
        o.auto_define = vec![
            ("process.platform".into(), "\"linux\"".into()),
            ("process.arch".into(), "\"x64\"".into()),
        ];
        o.define = vec!["process.platform=\"darwin\"".into(), "Q=a===b".into()];
        let map = defines(&o).expect("valid defines");
        assert_eq!(
            map.get("process.platform").map(String::as_str),
            Some("\"darwin\""),
            "an explicit --define must win over the target auto-define"
        );
        assert_eq!(map.get("process.arch").map(String::as_str), Some("\"x64\""));
        assert_eq!(
            map.get("Q").map(String::as_str),
            Some("a===b"),
            "only the FIRST = separates key from value"
        );
    }

    #[test]
    fn define_without_an_equals_is_rejected() {
        let mut o = opts();
        o.define = vec!["JUST_A_KEY".into()];
        let err = defines(&o).expect_err("a define with no = must be rejected");
        assert!(
            err.to_string().contains("KEY=VALUE"),
            "the error must name the expected shape, got: {err}"
        );
    }

    #[test]
    fn alias_requires_both_sides() {
        assert!(
            alias_entries(&["a=b".to_string()])
                .expect("valid")
                .is_some()
        );
        assert!(alias_entries(&["a=".to_string()]).is_err());
        assert!(alias_entries(&["=b".to_string()]).is_err());
        assert!(alias_entries(&["ab".to_string()]).is_err());
    }

    // The whole point of taking a PACKAGE rather than a specifier: a package
    // whose subpaths are imported separately (prettier's plugins, a scoped
    // package's `/dist/x`) must externalize as a unit, and a package that merely
    // shares a name PREFIX must not be dragged along with it.
    #[test]
    fn external_matches_the_package_and_its_subpaths_only() {
        assert!(is_package_specifier("prettier", "prettier"));
        assert!(is_package_specifier("prettier/plugins/babel", "prettier"));
        assert!(is_package_specifier("@scope/pkg/sub", "@scope/pkg"));
        assert!(!is_package_specifier("prettier-plugin-x", "prettier"));
        assert!(!is_package_specifier("pretti", "prettier"));
        assert!(!is_package_specifier("./local/prettier", "prettier"));
    }

    // A path-shaped value would externalize to something no runtime resolve can
    // fix — the build host's own absolute path baked in as a specifier — and
    // nothing downstream would fail the build, so it has to be refused here.
    #[test]
    fn external_takes_a_bare_package_name_and_refuses_anything_else() {
        assert!(external_matcher(&[]).expect("valid").is_none());
        for ok in ["prettier", "@scope/pkg"] {
            assert!(
                external_matcher(&[ok.to_string()])
                    .expect("valid")
                    .is_some(),
                "{ok} is a bare package name"
            );
        }
        for bad in [
            "",
            "  ",
            "./local",
            "/abs/path",
            "node:fs",
            "C:\\pkg",
            " pad",
        ] {
            assert!(
                external_matcher(&[bad.to_string()]).is_err(),
                "{bad:?} is not a bare package name and must be refused"
            );
        }
    }

    // An externalized package's source is never loaded, so the sites the
    // unresolvable-import scan would otherwise reject never reach it. This is
    // the mechanism the flag exists for, and it is Rolldown's behavior rather
    // than ours — assert it against a real bundle, not by reading rolldown.
    #[test]
    fn externalizing_a_package_takes_its_unanalyzable_imports_out_of_the_build() {
        let dir = std::env::temp_dir().join(format!("nub-ext-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("node_modules").join("plugins-host");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            br#"{"name":"plugins-host","version":"1.0.0","main":"index.js","type":"module"}"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("index.js"),
            b"export const load = (n) => import(n);\nexport const NAME = 'host';\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("entry.ts"),
            b"import { NAME } from 'plugins-host';\nglobalThis.OUT = NAME;\n",
        )
        .unwrap();

        let mut o = opts();
        let Err(err) = bundle(&dir.join("entry.ts"), &o) else {
            panic!("control: the dependency's import(n) must fail the build when bundled");
        };
        assert!(
            format!("{err:#}").contains("could not be resolved"),
            "got: {err:#}"
        );

        o.external = vec!["plugins-host".to_string()];
        bundle(&dir.join("entry.ts"), &o)
            .expect("externalizing the package removes its unanalyzable site from the graph");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dependency in `node_modules` whose entry file is `body`, imported by a TS
    /// entry. Returns all emitted chunk code because CommonJS dependencies now
    /// occupy their own loader scope. `main` carries the EXTENSION, which is what
    /// decides the source type Rolldown parses the dependency with.
    fn bundle_with_dep(tag: &str, main: &str, pkg_json: &str, body: &str) -> String {
        let dir = std::env::temp_dir().join(format!("nub-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("node_modules").join("dep");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), pkg_json.as_bytes()).unwrap();
        std::fs::write(pkg.join(main), body.as_bytes()).unwrap();
        std::fs::write(
            dir.join("entry.ts"),
            b"import * as dep from 'dep';\nglobalThis.OUT = dep;\n",
        )
        .unwrap();
        let mut o = opts();
        // The shim declares ordinary function-scoped bindings, which the minifier
        // is free to rename — so read the unminified chunk and assert on the shape
        // rather than on names minify does not promise to keep.
        o.minify = false;
        let res = bundle(&dir.join("entry.ts"), &o).expect("the fixture bundles");
        let code = emitted_chunk_code(&res);
        let _ = std::fs::remove_dir_all(&dir);
        code
    }

    // The shim's contract, against a REAL bundle rather than by reading Rolldown:
    // a CommonJS dependency keeps the two path globals it had on Node, and both
    // derive from `import.meta.url` so the value follows the artifact into
    // whatever directory it is extracted to. Without this the chunk carries a bare
    // `__dirname`, which is a `ReferenceError` in ES module scope.
    #[test]
    fn a_bundled_commonjs_dependency_gets_its_path_globals_from_import_meta_url() {
        let code = bundle_with_dep(
            "cjs-dirname",
            "index.js",
            r#"{"name":"dep","version":"1.0.0","main":"index.js"}"#,
            "module.exports.dir = () => __dirname;\nmodule.exports.file = () => __filename;\n",
        );
        assert!(
            code.contains("import.meta.url") && code.contains("fileURLToPath"),
            "the CJS dependency's __dirname/__filename must be derived from \
             import.meta.url, got:\n{code}"
        );
    }

    // `.cjs` and `.cts` are parsed `with_commonjs(true)`, where `import.meta` is a
    // SYNTAX ERROR — so reading the URL inline would fail the BUILD on the two
    // extensions that most reliably mean CommonJS, turning a working compile into a
    // hard error. Bundling at all is therefore most of this assertion. The bodies
    // name neither `module` nor `exports`, so the EXTENSION is the only thing
    // marking them CommonJS — which is the half of the rule these two cover.
    #[test]
    fn the_commonjs_only_extensions_still_bundle_and_still_get_the_shim() {
        for (tag, main) in [("cjs-ext", "index.cjs"), ("cts-ext", "index.cts")] {
            let code = bundle_with_dep(
                tag,
                main,
                &format!(r#"{{"name":"dep","version":"1.0.0","main":"{main}"}}"#),
                "globalThis.dir = () => __dirname;\n",
            );
            assert!(
                code.contains("import.meta.url") && code.contains("fileURLToPath"),
                "{main} must bundle with the shim applied, got:\n{code}"
            );
        }
    }

    // Where the declarations land, for the two module shapes where byte 0 would
    // change what the module means: a directive prologue that stops being one, and
    // a hashbang that stops being at byte 0. Asserted on the spliced TEXT rather
    // than on an offset, because the offset alone would not show that the prologue
    // and the hashbang each still come first.
    #[test]
    fn the_declarations_splice_after_a_directive_prologue_and_after_a_hashbang() {
        let splice = |src: &str| {
            let (at, decls) = cjs_path_globals_edit("dep.js", src).expect("a CJS shim applies");
            format!("{}{decls}{}", &src[..at], &src[at..])
        };

        let strict = splice("\"use strict\";\nmodule.exports = () => __dirname;\n");
        assert!(
            strict.starts_with("\"use strict\";;const {"),
            "the directive must stay in the prologue, got:\n{strict}"
        );

        let hashbang = splice("#!/usr/bin/env node\nmodule.exports = () => __dirname;\n");
        assert!(
            hashbang.starts_with("#!/usr/bin/env node\n;const {"),
            "a hashbang is only a hashbang at byte 0, got:\n{hashbang}"
        );

        // Every line still starts on the line it started on — what makes the
        // transform's `Null` sourcemap honest.
        for (before, after) in [
            (
                "\"use strict\";\nmodule.exports = () => __dirname;\n",
                strict,
            ),
            (
                "#!/usr/bin/env node\nmodule.exports = () => __dirname;\n",
                hashbang,
            ),
        ] {
            assert_eq!(
                before.lines().count(),
                after.lines().count(),
                "the splice must not add a line"
            );
        }
    }

    // The three cases that must NOT be shimmed, and they fail differently. An ES
    // module has no `__dirname` on Node, so handing it one would make nub run code
    // plain Node rejects. A module that declares the name itself already owns it,
    // and a second `const` of the same name would not parse. A UMD factory takes
    // `require` as a PARAMETER, so the splice's own call would land on the caller's
    // function rather than the loader. In all three a successful bundle is itself
    // half the assertion, since `reject_invalid_chunks` re-parses what is emitted.
    #[test]
    fn the_path_global_shim_skips_esm_and_modules_that_bind_the_names_it_uses() {
        let cases = [
            (
                "esm-dirname",
                r#"{"name":"dep","version":"1.0.0","main":"index.js","type":"module"}"#,
                "export const dir = () => __dirname;\n",
                "an ES module's __dirname must stay unresolved, as it is on Node",
            ),
            (
                "own-dirname",
                r#"{"name":"dep","version":"1.0.0","main":"index.js"}"#,
                "var __dirname = '/baked';\nmodule.exports.dir = () => __dirname;\n",
                "a module that declares __dirname itself must be left alone",
            ),
            (
                "own-require",
                r#"{"name":"dep","version":"1.0.0","main":"index.js"}"#,
                "(function (require) { module.exports.dir = () => __dirname; })(null);\n",
                "a module that shadows require must not have a require call spliced in",
            ),
            // The case that proves the rule needs POSITIVE CommonJS evidence rather
            // than merely the absence of ESM keywords: a `"type": "module"` file
            // with no imports, no exports and no `module`/`exports` is a genuine ES
            // module, and the package `"type"` is invisible to a transform hook.
            // Detecting CommonJS by absence shimmed it, so the binary answered a
            // `__dirname` that plain Node answers with a ReferenceError.
            (
                "esm-no-keywords",
                r#"{"name":"dep","version":"1.0.0","main":"index.js","type":"module"}"#,
                "globalThis.dir = () => __dirname;\n",
                "an ES module with no ESM keywords is still an ES module",
            ),
        ];
        for (tag, pkg_json, body, why) in cases {
            assert!(
                cjs_path_globals_edit("node_modules/dep/index.js", body).is_none(),
                "{why}"
            );
            // The real bundle is the second half of the assertion: a duplicate
            // declaration or a shim call bound to the module's own `require`
            // would fail Rolldown or the emitted-chunk parse gate.
            let _ = bundle_with_dep(tag, "index.js", pkg_json, body);
        }
    }

    // The `file` loader's whole contract, asserted against a REAL bundle: the
    // asset leaves the graph as bytes in `assets` (never in `files`, which are
    // re-parsed as JS), and the module it leaves behind resolves its path
    // against `import.meta.url` rather than the cwd. A relative path — what
    // Rolldown's built-in asset loader yields — would read from wherever the
    // binary was launched, which works only in the directory it was tested from.
    #[test]
    fn the_file_loader_emits_bytes_and_a_cwd_independent_path() {
        let dir = std::env::temp_dir().join(format!("nub-file-loader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blob.bin"), b"\x00\x01BINARY").unwrap();
        std::fs::write(
            dir.join("entry.ts"),
            b"import p from './blob.bin';\nglobalThis.OUT = p;\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&dir.join("entry.ts"), &o).expect("a .bin import must compile");

        assert_eq!(res.assets.len(), 1, "the bytes must ship as an asset");
        assert_eq!(
            res.assets[0].bytes, b"\x00\x01BINARY",
            "the asset must be embedded verbatim"
        );
        assert!(
            res.assets.iter().all(|a| !a.name.contains('/')),
            "an asset must be a flat sibling of the chunks: {:?}",
            res.assets.iter().map(|a| &a.name).collect::<Vec<_>>()
        );
        let code = emitted_entry_code(&res);
        assert!(
            code.contains("import.meta.url"),
            "the path must be resolved against the module, got:\n{code}"
        );
        assert!(
            code.contains(&res.assets[0].name),
            "the chunk must name the asset it ships, got:\n{code}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Control for the test above: without the `file` mapping, the SAME source
    // ships no bytes. Without this the positive test proves nothing — "the .bin
    // import compiled" is equally true of an import that was quietly ignored.
    //
    // The pre-loader outcome is deliberately asserted as "no asset", not "build
    // error", because it is BOTH depending on the bytes: content that fails to
    // parse as JavaScript errors, while content that happens to parse (a binary
    // whose bytes read as identifiers) builds clean and leaves the import
    // `undefined` at runtime. That second case is the silent one this loader
    // exists to eliminate, and a control demanding an error would miss it.
    #[test]
    fn without_the_file_loader_no_bytes_are_shipped() {
        let dir = std::env::temp_dir().join(format!("nub-file-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blob.bin"), b"\x00\x01BINARY").unwrap();
        std::fs::write(
            dir.join("entry.ts"),
            b"import p from './blob.bin';\nglobalThis.OUT = p;\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        o.loaders = vec![".bin=js".to_string()];
        if let Ok(res) = bundle(&dir.join("entry.ts"), &o) {
            assert!(
                res.assets.is_empty(),
                "only the file loader may emit asset bytes, got {:?}",
                res.assets.iter().map(|a| &a.name).collect::<Vec<_>>()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // An asset the emitted code never names must not ship, and the payload must
    // NOT depend on whether the entry was written in TypeScript or JavaScript.
    //
    // This is the case that made the loader look correct when it was not: the
    // `load` hook collects bytes during graph construction, before tree-shaking,
    // so an unused import embeds its file — but only from a `.js` entry, because
    // the TypeScript transform elides an unused import as possibly-type-only and
    // the hook never runs. Testing only the `.ts` spelling reports success.
    #[test]
    fn an_asset_no_chunk_names_is_not_shipped_from_either_entry_language() {
        for ext in ["ts", "js"] {
            let dir = std::env::temp_dir().join(format!("nub-unused-{}-{ext}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("logo.png"), b"UNUSEDPNGBYTES").unwrap();
            std::fs::write(
                dir.join(format!("entry.{ext}")),
                b"import p from './logo.png';\nglobalThis.OUT = 'hi';\n",
            )
            .unwrap();

            let mut o = opts();
            o.minify = false;
            let res = bundle(&dir.join(format!("entry.{ext}")), &o).expect("compiles");
            assert!(
                res.assets.is_empty(),
                "a .{ext} entry shipped an asset nothing references: {:?}",
                res.assets.iter().map(|a| &a.name).collect::<Vec<_>>()
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // Reachability must not depend on the sourcemap mode. Maps travel in the
    // same `files` vec as chunks and a map can carry `sourcesContent`, so the
    // scan's input is easy to widen by accident; this pins the answer across all
    // four modes rather than only the default the other tests happen to use.
    #[test]
    fn an_unused_asset_is_dropped_under_every_sourcemap_mode() {
        for mode in [
            SourcemapMode::None,
            SourcemapMode::Inline,
            SourcemapMode::Linked,
            SourcemapMode::External,
        ] {
            let dir = std::env::temp_dir().join(format!("nub-map-{}-{mode:?}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("logo.png"), b"UNUSEDBYTES").unwrap();
            std::fs::write(
                dir.join("entry.js"),
                b"import p from './logo.png';\nglobalThis.OUT = 'hi';\n",
            )
            .unwrap();

            let mut o = opts();
            o.minify = false;
            o.sourcemap = mode;
            let res = bundle(&dir.join("entry.js"), &o).expect("compiles");
            assert!(
                res.assets.is_empty(),
                "{mode:?} retained an asset nothing references: {:?}",
                res.assets.iter().map(|a| &a.name).collect::<Vec<_>>()
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // The control for the test above: the SAME asset, actually used, still ships
    // from both entry languages. Without it, "no assets" would also be satisfied
    // by a loader that silently stopped working.
    #[test]
    fn a_used_asset_ships_from_either_entry_language() {
        for ext in ["ts", "js"] {
            let dir = std::env::temp_dir().join(format!("nub-used-{}-{ext}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("logo.png"), b"USEDPNGBYTES").unwrap();
            std::fs::write(
                dir.join(format!("entry.{ext}")),
                b"import p from './logo.png';\nglobalThis.OUT = p;\n",
            )
            .unwrap();

            let mut o = opts();
            o.minify = false;
            let res = bundle(&dir.join(format!("entry.{ext}")), &o).expect("compiles");
            assert_eq!(
                res.assets.len(),
                1,
                "a used asset must ship from a .{ext} entry"
            );
            assert_eq!(res.assets[0].bytes, b"USEDPNGBYTES");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // `.md` is the text half. Both spellings must land on the same string: the
    // bare import, and the standards-track attribute — which Rolldown 1.2.0
    // ignores entirely, so it must not contradict the extension map either.
    #[test]
    fn markdown_loads_as_text_with_or_without_the_import_attribute() {
        for spec in [
            "import t from './doc.md';",
            "import t from './doc.md' with { type: 'text' };",
        ] {
            let dir = std::env::temp_dir().join(format!(
                "nub-text-{}-{}",
                std::process::id(),
                spec.len()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("doc.md"), "# Title\n").unwrap();
            std::fs::write(
                dir.join("entry.ts"),
                format!("{spec}\nglobalThis.OUT = t;\n"),
            )
            .unwrap();

            let mut o = opts();
            o.minify = false;
            let res = bundle(&dir.join("entry.ts"), &o)
                .unwrap_or_else(|e| panic!("{spec} must compile, got: {e:#}"));
            let code = emitted_entry_code(&res);
            assert!(
                code.contains("# Title"),
                "{spec} must inline the file's text, got:\n{code}"
            );
            assert!(
                res.assets.is_empty(),
                "text is inlined, so nothing should be emitted beside the chunk"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A fixture dir unique to this call, so parallel test threads never share a
    /// path (the bundler keys diagnostics on it, and asset names on content).
    fn fixture_dir(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nub-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bundle_module_graph(tag: &str, entry: &str, files: &[(&str, &str)]) -> Result<BundleResult> {
        let dir = fixture_dir(tag);
        for (name, source) in files {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, source).unwrap();
        }
        let mut o = opts();
        o.minify = false;
        let result = bundle(&dir.join(entry), &o);
        let _ = std::fs::remove_dir_all(dir);
        result
    }

    // Rolldown lowers mixed CommonJS/ESM cycles through lazy init wrappers.
    // Compilation preserves supported lazy cycles rather than applying a
    // whole-graph refusal. The fixture covers a direct edge, a longer CJS
    // bridge, and a syntax-detected ESM `.js` target.
    #[test]
    fn mixed_commonjs_esm_cycles_bundle() {
        for (tag, files) in [
            (
                "require-esm-direct-cycle",
                vec![
                    (
                        "main.mjs",
                        "import './nested/a.cjs';\nexport const token = 'esm';\n",
                    ),
                    ("nested/a.cjs", "module.exports = require('../main.mjs');\n"),
                ],
            ),
            (
                "require-esm-long-cycle",
                vec![
                    ("main.mjs", "import './bridge.cjs';\n"),
                    ("bridge.cjs", "require('./a.cjs');\n"),
                    ("a.cjs", "require('./main.mjs');\n"),
                ],
            ),
            (
                "require-syntax-esm-cycle",
                vec![
                    ("main.mjs", "import './a.cjs';\n"),
                    ("a.cjs", "require('./target.js');\n"),
                    ("target.js", "import './a.cjs';\nexport const value = 1;\n"),
                ],
            ),
        ] {
            bundle_module_graph(tag, "main.mjs", &files)
                .unwrap_or_else(|err| panic!("{tag} must compile, got: {err:#}"));
        }
    }

    // The CommonJS module itself is retained for its top-level assignment, but
    // its back-edge is not called.  This is the control against a future static
    // graph refusal reappearing merely because an unused lazy closure is present.
    #[test]
    fn retained_unused_lazy_mixed_cycle_bundles() {
        bundle_module_graph(
            "require-esm-retained-lazy-cycle",
            "main.mjs",
            &[
                (
                    "main.mjs",
                    "import './a.cjs';\nexport const token = 'esm-token';\n",
                ),
                ("a.cjs", "module.exports = () => require('./main.mjs');\n"),
            ],
        )
        .expect("a retained but unused lazy CJS back-edge must compile");
    }

    // Invoking the CJS back-edge after the ESM has evaluated keeps the call site
    // inside the CJS wrapper. Rolldown lowers its ESM target through cached
    // init/namespace helpers.
    #[test]
    fn invoked_lazy_mixed_cycle_bundles() {
        bundle_module_graph(
            "require-esm-invoked-lazy-cycle",
            "main.mjs",
            &[
                (
                    "main.mjs",
                    "import getMain from './a.cjs';\nexport const token = 'esm-token';\nsetImmediate(() => getMain().token);\n",
                ),
                ("a.cjs", "module.exports = () => require('./main.mjs');\n"),
            ],
        )
        .expect("a delayed CJS back-edge must compile");
    }

    /// Rolldown resolves a module specifier through the real filesystem and cannot
    /// resolve a Windows verbatim path, so every Windows compile failed with
    /// `Could not resolve '\\?\C:\...' in \0nub:compile-root`. `fs::canonicalize`
    /// hands back exactly that spelling, and no macOS or Linux test can reach it —
    /// hence the pure `windows` parameter.
    #[test]
    fn a_verbatim_canonical_path_is_reduced_to_the_spelling_rolldown_resolves() {
        let drive = PathBuf::from(r"\\?\C:\Users\r\app.cjs");
        assert_eq!(
            strip_verbatim_prefix(drive.clone(), true),
            PathBuf::from(r"C:\Users\r\app.cjs")
        );
        let unc = PathBuf::from(r"\\?\UNC\server\share\app.cjs");
        assert_eq!(
            strip_verbatim_prefix(unc, true),
            PathBuf::from(r"\\server\share\app.cjs")
        );
        // An ordinary Windows path is already resolvable and must survive untouched.
        let plain = PathBuf::from(r"C:\Users\r\app.cjs");
        assert_eq!(strip_verbatim_prefix(plain.clone(), true), plain);
        // Off Windows the prefix is not a prefix at all, so nothing is stripped.
        assert_eq!(strip_verbatim_prefix(drive.clone(), false), drive);
    }

    #[test]
    fn compile_root_statically_imports_the_prelude_before_the_program() {
        let source = Path::new("/tmp/compile prelude/main.cjs");
        let wrapper = compile_root_source(source);
        let prelude = serde_json::to_string(COMPILE_PREAMBLE_ID).unwrap();
        let program = serde_json::to_string(&source.to_string_lossy()).unwrap();
        assert_eq!(wrapper, format!("import {prelude};\nimport {program};\n"));
        assert!(
            wrapper.find(&prelude).unwrap() < wrapper.find(&program).unwrap(),
            "the side-effect prelude must be evaluated before authored code: {wrapper}"
        );
    }

    #[test]
    fn generated_loader_sources_get_builtins_from_the_compile_bootstrap() {
        assert!(
            !PATH_GLOBALS_SOURCE.contains("import "),
            "the path globals module must not rely on static builtin imports: {PATH_GLOBALS_SOURCE}"
        );
        for builtin in ["node:url", "node:path"] {
            assert!(
                PATH_GLOBALS_SOURCE.contains(&format!("getBuiltin(\"{builtin}\")")),
                "the path globals module must fetch {builtin} from the bootstrap registry: {PATH_GLOBALS_SOURCE}"
            );
        }
        let intro = compile_commonjs_require_intro();
        assert!(
            intro.starts_with(COMPILE_COMMONJS_REQUIRE_MARKER),
            "the CommonJS chunk must open with the bootstrap-backed loader: {intro}"
        );
        assert!(
            !intro.contains("node:module") && !intro.contains("__nubCompileCommonjs"),
            "the CommonJS loader must be a fixed bootstrap-backed binding: {intro}"
        );
        // `require` has to survive a cycle re-entering this chunk before its body
        // runs, which a `const` cannot — see `compile_commonjs_require_intro`.
        assert!(
            intro.contains("function require(id)") && !intro.contains("const require"),
            "the loader must be a hoisted function declaration: {intro}"
        );
        for property in ["resolve", "cache", "main", "extensions"] {
            assert!(
                intro.contains(property),
                "the loader must carry require.{property}: {intro}"
            );
        }
    }

    #[test]
    fn executable_root_main_checks_are_true_without_touching_offsets() {
        let source = "if (require.main === module) main();\n\
                      if (module == require.main) reverse();\n\
                      if (require.main !== module) notMain();\n";
        let rewritten = rewrite_entry_main_checks("/tmp/entry.cjs", source)
            .expect("the CommonJS entry guards must be rewritten");
        assert_eq!(rewritten.len(), source.len());
        assert_eq!(
            rewritten.matches('\n').count(),
            source.matches('\n').count()
        );
        assert!(rewritten.contains("if (true"));
        assert_eq!(
            rewritten.matches("true").count(),
            2,
            "both equality spellings describe the executable root: {rewritten}"
        );
        assert!(
            rewritten.contains("if (false"),
            "inequality must take the non-main branch out: {rewritten}"
        );
        assert!(
            !rewritten.contains("require.main"),
            "the whole recognized predicates must be replaced: {rewritten}"
        );
    }

    #[test]
    fn computed_commonjs_main_checks_keep_global_and_fixed_width_semantics() {
        let source = "if (require[\"main\"] === module) main();\n\
                      if (module !== require['main']) notMain();\n";
        let rewritten = rewrite_entry_main_checks("/tmp/entry.cjs", source)
            .expect("computed CommonJS root guards must be rewritten");
        assert_eq!(rewritten.len(), source.len());
        assert_eq!(
            rewritten.matches('\n').count(),
            source.matches('\n').count()
        );
        assert!(rewritten.contains("if (true"), "{rewritten}");
        assert!(rewritten.contains("if (false"), "{rewritten}");
        assert!(
            !rewritten.contains("require["),
            "the whole computed predicate must be replaced: {rewritten}"
        );
        for shadowed in [
            "function check(require) { return require[\"main\"] === module; }",
            "function check(module) { return require['main'] === module; }",
        ] {
            assert!(
                rewrite_entry_main_checks("/tmp/entry.cjs", shadowed).is_none(),
                "a shadowed computed guard must retain its authored semantics: {shadowed}"
            );
        }
    }

    #[test]
    fn shadowed_commonjs_names_are_never_treated_as_node_entry_globals() {
        for source in [
            "function check(require) { return require.main === module; }\n",
            "function check(module) { return require.main === module; }\n",
            "const require = makeRequire(); require.main === module;\n",
        ] {
            assert!(
                rewrite_entry_main_checks("/tmp/entry.cjs", source).is_none(),
                "a user binding owns the comparison and must stay untouched: {source}"
            );
        }
    }

    #[test]
    fn esm_main_guards_keep_their_authored_reference_error_semantics() {
        for (path, source) in [
            ("/tmp/entry.mjs", "require.main === module;"),
            ("/tmp/entry.js", "export {}; require.main === module;"),
        ] {
            assert!(
                rewrite_entry_main_checks(path, source).is_none(),
                "{path} must remain ESM: {source}"
            );
        }
    }

    #[test]
    fn esm_roots_without_module_syntax_get_only_an_empty_export_marker() {
        let source = "require.main === module;\n";
        let marked = preserve_entry_esm_classification("/tmp/entry.mjs", source).expect(
            "an .mjs root with only CommonJS-looking globals still needs ESM classification",
        );
        assert!(marked.starts_with(source));
        assert!(marked.ends_with("export {};\n"));
        assert!(
            preserve_entry_esm_classification("/tmp/entry.cjs", source).is_none(),
            "a CJS root must retain its CommonJS module format"
        );
        assert!(
            preserve_entry_esm_classification("/tmp/entry.mjs", "export {};\n").is_none(),
            "existing ESM syntax already classifies the module"
        );
    }

    #[test]
    fn esm_root_guard_stays_an_esm_reference_in_the_emitted_chunk() {
        let dir = fixture_dir("esm-main-guard-output");
        let entry = dir.join("entry.mjs");
        std::fs::write(
            &entry,
            "try { console.log(require.main === module); } catch (e) { console.log('ESM_REFERENCE:' + e.name); }\n",
        )
        .unwrap();
        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("an ESM root must compile");
        let entry_code = res
            .files
            .iter()
            .find(|file| file.name == res.entry)
            .map(|file| String::from_utf8_lossy(&file.bytes))
            .expect("the named entry chunk must be emitted");
        assert!(entry_code.contains("ESM_REFERENCE:"));
        assert!(
            !entry_code.contains(COMPILE_COMMONJS_REQUIRE_MARKER),
            "the CommonJS loader must never become a lexical binding in authored ESM:\n{entry_code}"
        );
        let code = res
            .files
            .iter()
            .filter(|file| !file.name.ends_with(".map"))
            .map(|file| String::from_utf8_lossy(&file.bytes).into_owned())
            .collect::<String>();
        assert!(code.contains("ESM_REFERENCE:"));
        let manufactured_require = code
            .find("__require.main")
            .map(|offset| {
                let start = offset.saturating_sub(96);
                let end = (offset + 160).min(code.len());
                &code[start..end]
            })
            .unwrap_or("no manufactured require");
        assert!(
            !code.contains("__require.main"),
            "compiled ESM must not manufacture a require binding or polyfill near:\n{manufactured_require}"
        );
        let commonjs_chunks = res
            .files
            .iter()
            .filter(|file| {
                String::from_utf8_lossy(&file.bytes).contains(COMPILE_COMMONJS_REQUIRE_MARKER)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            commonjs_chunks.len(),
            1,
            "the bundled runtime's CommonJS modules need one isolated loader scope"
        );
        let runtime_commonjs = String::from_utf8_lossy(&commonjs_chunks[0].bytes);
        assert!(
            runtime_commonjs.contains("installSyncPolyfills"),
            "the private prelude's CommonJS runtime must use the isolated loader chunk"
        );
        assert!(
            runtime_commonjs.contains("require(\"node:fs\")"),
            "the private polyfill runtime must retain the builtin require this loader fixes"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn commonjs_builtin_require_is_isolated_from_the_esm_entry_chunk() {
        let dir = fixture_dir("commonjs-require-output");
        let entry = dir.join("entry.cjs");
        std::fs::write(
            &entry,
            "console.log('CJS_BUILTIN_REQUIRE:' + require('node:path').sep);\n",
        )
        .unwrap();
        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("a CommonJS root with a builtin require must compile");
        let commonjs = res
            .files
            .iter()
            .find(|file| String::from_utf8_lossy(&file.bytes).contains("CJS_BUILTIN_REQUIRE:"))
            .expect("the authored CommonJS module must be emitted");
        let commonjs_code = String::from_utf8_lossy(&commonjs.bytes);
        assert!(
            commonjs_code.contains(COMPILE_COMMONJS_REQUIRE_MARKER),
            "a raw Node builtin require needs createRequire in its CommonJS chunk:\n{commonjs_code}"
        );
        assert!(
            commonjs_code.contains("require(\"node:path\")"),
            "the fixture must retain the raw builtin call that needs the lexical loader:\n{commonjs_code}"
        );
        assert!(
            commonjs.name.starts_with(COMPILE_COMMONJS_CHUNK),
            "Rolldown must keep CommonJS behind the named manual boundary: {}",
            commonjs.name
        );
        assert_ne!(
            commonjs.name, res.entry,
            "the CommonJS module must not share the ESM facade's lexical scope"
        );
        let entry_code = res
            .files
            .iter()
            .find(|file| file.name == res.entry)
            .map(|file| String::from_utf8_lossy(&file.bytes))
            .expect("the named entry chunk must be emitted");
        assert!(
            !entry_code.contains(COMPILE_COMMONJS_REQUIRE_MARKER),
            "the ESM facade must keep require unbound:\n{entry_code}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Every module wrapper reaches the output as a hoisted function declaration,
    /// so a chunk re-entered through an import cycle finds it callable rather than
    /// an unassigned `var`. `require` gets the same treatment for the same reason.
    /// `--unbundled` ships a package the graph already reaches. A name nothing
    /// resolves cannot be carried out, and doing nothing quietly produced a
    /// successful build whose binary failed on the user's machine.
    #[test]
    fn an_unbundled_name_nothing_resolves_fails_the_build() {
        let err = refuse_unreached_unbundled(&["@opentui/core-darwin-arm64".to_string()])
            .expect_err("an unreached --unbundled name must not pass silently");
        let text = err.to_string();
        assert!(
            text.contains("@opentui/core-darwin-arm64"),
            "the error must name the package: {text}"
        );
        assert!(
            text.contains("--include"),
            "and point at the flag that does ship it: {text}"
        );
        refuse_unreached_unbundled(&[]).expect("nothing unreached is not an error");
    }

    /// One package reached through several roots — a workspace link and the store
    /// path behind it — is still one package in the artifact, and must read as one.
    #[test]
    fn the_unbundled_report_counts_each_package_once() {
        let summaries = [
            "@opentui/core — you asked for it with --unbundled",
            "pino — it loads its transport worker from a path built at run time",
            "@opentui/core — you asked for it with --unbundled",
            "@opentui/core — you asked for it with --unbundled",
        ];
        let mut unique: Vec<&str> = summaries.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            2,
            "three roots for @opentui/core are one package: {unique:?}"
        );
    }

    #[test]
    fn module_wrappers_and_require_are_hoisted_declarations() {
        let dir = fixture_dir("hoisted-wrappers");
        let pkg = dir.join("node_modules/cjsdep");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"cjsdep","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("index.js"),
            "module.exports = { sep: require('node:path').sep };\n",
        )
        .unwrap();
        let entry = dir.join("entry.mjs");
        std::fs::write(&entry, "import d from 'cjsdep'; console.log(d.sep);\n").unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("a CommonJS dependency must compile");
        let all = res
            .files
            .iter()
            .map(|file| String::from_utf8_lossy(&file.bytes).into_owned())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            all.contains("function require_index()") || all.contains("function require_cjsdep()"),
            "the CommonJS wrapper must be a hoisted function declaration:\n{all}"
        );
        assert!(
            !all.contains("var require_index = ") && !all.contains("var require_cjsdep = "),
            "no wrapper may survive as a `var`, which is unassigned during a cycle:\n{all}"
        );
        assert!(
            all.contains("function require(id)"),
            "the CommonJS chunk's own loader must hoist too:\n{all}"
        );
        assert!(
            !all.contains("const require = "),
            "a `const require` is in its temporal dead zone when a cycle re-enters:\n{all}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A dynamically imported CommonJS package emits an eager
    /// `export default require_pkg()` at the top of its own chunk. If the JSON it
    /// requires stays behind in that chunk while the package itself moves to
    /// `_nub_commonjs`, the two chunks import each other and that eager call runs
    /// against an unassigned wrapper — `require_pkg is not a function` at startup.
    /// Both halves must land in the same chunk.
    #[test]
    fn a_dynamically_imported_commonjs_package_keeps_its_json_in_one_chunk() {
        let dir = fixture_dir("commonjs-dynamic-json-chunk");
        let pkg = dir.join("node_modules/cjsdyn");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"cjsdyn","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("index.js"),
            "module.exports = { data: require('./data.json') };\n",
        )
        .unwrap();
        std::fs::write(pkg.join("data.json"), r#"{"v":1}"#).unwrap();
        let entry = dir.join("entry.mjs");
        std::fs::write(
            &entry,
            "const m = await import('cjsdyn'); console.log(m.default.data.v);\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("a dynamic CommonJS import must compile");
        let holder = res
            .files
            .iter()
            .find(|file| String::from_utf8_lossy(&file.bytes).contains("require('./data.json')"))
            .or_else(|| {
                res.files
                    .iter()
                    .find(|file| String::from_utf8_lossy(&file.bytes).contains(r#""v": 1"#))
            })
            .expect("the JSON must be emitted somewhere");
        let holder_code = String::from_utf8_lossy(&holder.bytes);
        assert!(
            !holder_code.contains(&format!("from \"./{COMPILE_COMMONJS_CHUNK}")),
            "the chunk holding the package's JSON must not import back out of the CommonJS \
             chunk — that edge is what closes the cycle:\n{}",
            holder.name
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn commonjs_dependency_keeps_global_and_shadowed_require_scopes_distinct() {
        let dir = fixture_dir("commonjs-dependency-require-output");
        let entry = dir.join("entry.mjs");
        std::fs::write(
            &entry,
            "import value from './dependency.cjs'; console.log(value);\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("dependency.cjs"),
            "const local = (require) => require();\nconst leaf = require('./leaf.mjs').default;\nmodule.exports = 'CJS_DEPENDENCY_REQUIRE:' + require('node:path').sep + local(() => 'local') + leaf;\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("leaf.mjs"),
            "globalThis.CJS_TO_ESM_LEAF = 'leaf';\nexport default globalThis.CJS_TO_ESM_LEAF;\n",
        )
        .unwrap();
        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("a CommonJS dependency must compile");
        let commonjs = res
            .files
            .iter()
            .find(|file| String::from_utf8_lossy(&file.bytes).contains("CJS_DEPENDENCY_REQUIRE:"))
            .expect("the CommonJS dependency must be emitted");
        let commonjs_code = String::from_utf8_lossy(&commonjs.bytes);
        assert!(
            commonjs_code.contains(COMPILE_COMMONJS_REQUIRE_MARKER),
            "the dependency's global require needs the CommonJS loader scope:\n{commonjs_code}"
        );
        assert!(
            commonjs_code.contains("require(\"node:path\")"),
            "the builtin require must remain bound by the chunk intro:\n{commonjs_code}"
        );
        assert!(
            commonjs_code.contains("local"),
            "the fixture's shadowed require path must survive bundling:\n{commonjs_code}"
        );
        assert!(
            !commonjs_code.contains("CJS_TO_ESM_LEAF"),
            "non-recursive grouping must keep a required ESM leaf outside the loader scope:\n{commonjs_code}"
        );
        assert!(
            res.files
                .iter()
                .any(|file| { String::from_utf8_lossy(&file.bytes).contains("CJS_TO_ESM_LEAF") }),
            "the required ESM leaf must still be emitted"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mjs_dependency_never_enters_the_commonjs_loader_scope() {
        let dir = fixture_dir("mjs-require-output");
        let entry = dir.join("entry.mjs");
        std::fs::write(&entry, "import './dependency.mjs';\n").unwrap();
        std::fs::write(
            dir.join("dependency.mjs"),
            "try { module.exports = require('node:path'); } catch (e) { console.log('MJS_REFERENCE:' + e.name); }\n",
        )
        .unwrap();
        let mut o = opts();
        o.minify = false;
        let res =
            bundle(&entry, &o).expect("an .mjs dependency with an unbound require must compile");
        let dependency = res
            .files
            .iter()
            .find(|file| String::from_utf8_lossy(&file.bytes).contains("MJS_REFERENCE:"))
            .expect("the authored ESM dependency must be emitted");
        let dependency_code = String::from_utf8_lossy(&dependency.bytes);
        assert!(
            !dependency_code.contains(COMPILE_COMMONJS_REQUIRE_MARKER),
            "an .mjs dependency must retain Node's unbound-require semantics:\n{dependency_code}"
        );
        assert!(
            dependency_code.contains("require(\"node:path\")"),
            "the authored .mjs require must remain an unbound reference:\n{dependency_code}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn package_type_classifies_ambiguous_root_extensions() {
        let dir = fixture_dir("main-guard-package-type");
        let entry = dir.join("entry.js");
        std::fs::write(&entry, "require.main === module;").unwrap();
        std::fs::write(dir.join("package.json"), r#"{"type":"module"}"#).unwrap();
        assert!(
            rewrite_entry_main_checks(&entry.to_string_lossy(), "require.main === module;")
                .is_none()
        );

        std::fs::write(dir.join("package.json"), r#"{"type":"commonjs"}"#).unwrap();
        assert!(
            rewrite_entry_main_checks(&entry.to_string_lossy(), "require.main === module;")
                .is_some(),
            "a nearest type:commonjs boundary must restore the executable-root guard"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn main_guard_rewrite_resolves_each_identifier_reference_by_scope() {
        let source = "if (require.main === module) top();\n\
                      function customRequire(require) { return require.main === module; }\n\
                      function customModule(module) { return require.main === module; }\n";
        let rewritten = rewrite_entry_main_checks("/tmp/entry.cjs", source).unwrap();
        assert!(
            rewritten.contains("if (true"),
            "the real top-level guard must run: {rewritten}"
        );
        assert!(
            rewritten.contains("return require.main === module"),
            "shadowed guards must retain their authored references: {rewritten}"
        );
    }

    #[test]
    fn worker_names_are_stable_across_checkout_relocation() {
        let first = worker_output_name(
            Path::new("/tmp/one/project"),
            Path::new("/tmp/one/project/src/worker.ts"),
            "worker",
        );
        let second = worker_output_name(
            Path::new("/var/folders/project"),
            Path::new("/var/folders/project/src/worker.ts"),
            "worker",
        );
        let sibling = worker_output_name(
            Path::new("/var/folders/project"),
            Path::new("/var/folders/project/other/worker.ts"),
            "worker",
        );
        assert_eq!(first, second);
        assert_ne!(
            first, sibling,
            "logical paths must remain collision-resistant"
        );
    }

    #[test]
    fn worker_names_retain_digest_suffixes_past_a_shared_prefix() {
        let a = worker_output_name_with_hash(
            "worker",
            "deadbeef00000000000000000000000000000000000000000000000000000001",
        );
        let b = worker_output_name_with_hash(
            "worker",
            "deadbeef00000000000000000000000000000000000000000000000000000002",
        );
        assert_ne!(a, b, "a shared digest prefix must not alias worker entries");
    }

    #[test]
    fn worker_entries_dedupe_the_same_source_and_reject_an_alias() {
        let mut workers = BTreeMap::new();
        let entry = "worker-deadbeef.mjs".to_string();
        let chunk = "worker-deadbeef-code.mjs".to_string();
        assert!(
            record_worker(
                &mut workers,
                entry.clone(),
                chunk.clone(),
                Path::new("/p/one/worker.ts"),
            )
            .unwrap()
        );
        assert!(
            !record_worker(
                &mut workers,
                entry.clone(),
                chunk,
                Path::new("/p/one/worker.ts"),
            )
            .unwrap()
        );
        assert!(
            record_worker(
                &mut workers,
                entry,
                "worker-deadbeef-code.mjs".to_string(),
                Path::new("/p/two/worker.ts"),
            )
            .is_err()
        );
    }

    #[test]
    fn only_registered_program_and_worker_sources_are_executable_roots() {
        let program = Path::new("/tmp/nub-prelude/program.cjs");
        let worker = Path::new("/tmp/nub-prelude/worker.cjs");
        let dependency = "/tmp/nub-prelude/node_modules/tool/cli.cjs";
        let roots = CompilePreamble::from_source(program, PathBuf::new(), String::new());
        roots.worker_root(worker).unwrap();
        assert!(roots.is_root_source(program.to_str().unwrap()));
        assert!(roots.is_root_source(worker.to_str().unwrap()));
        assert!(
            !roots.is_root_source(dependency),
            "a dependency's identical require.main guard must remain false"
        );
    }

    #[test]
    fn compile_bootstrap_stays_separate_from_layout_relative_support_files() {
        let mut prelude = CompilePreamble::from_source(
            Path::new("/tmp/program.mjs"),
            PathBuf::new(),
            String::new(),
        );
        prelude
            .support_files
            .push(("worker-blob-url.cjs".into(), Vec::new()));
        prelude
            .root_support_files
            .push((nub_core::compile::COMPILE_BOOTSTRAP_NAME.into(), Vec::new()));

        assert_eq!(
            prelude
                .support_files()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec!["worker-blob-url.cjs".to_string()]
        );
        assert_eq!(
            prelude
                .root_support_files()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![nub_core::compile::COMPILE_BOOTSTRAP_NAME.to_string()]
        );
    }

    #[test]
    fn each_static_worker_gets_its_own_prelude_root_without_touching_the_program_root() {
        let program = Path::new("/tmp/nub-prelude/program.ts");
        let worker = Path::new("/tmp/nub-prelude/worker.ts");
        let roots = CompilePreamble::from_source(program, PathBuf::new(), String::new());
        let worker_id = roots.worker_root(worker).unwrap();
        assert_ne!(worker_id, COMPILE_ROOT_ID);
        let program_wrapper = compile_root_source(program);
        assert_eq!(
            roots.root_source(COMPILE_ROOT_ID).as_deref(),
            Some(program_wrapper.as_str())
        );
        let worker_wrapper = compile_root_source(worker);
        assert_eq!(
            roots.root_source(&worker_id).as_deref(),
            Some(worker_wrapper.as_str())
        );
    }

    #[test]
    fn import_meta_snapshot_preserves_mutable_property_operations() {
        let source = r#"const read = import.meta.main;
import.meta.main = false;
import.meta.main ??= true;
import.meta.main++;
const removed = delete import.meta.main;
const computed = import.meta["main"];
const url = import.meta.url;
const metadata = import.meta;
"#;
        let rewritten = rewrite_import_meta_main("/tmp/entry.mjs", source, "true")
            .expect("an executable root needs module-local import metadata")
            .to_string();

        assert!(
            rewritten.starts_with(
                "const __nub_import_meta = { __proto__: null, ...import.meta, main: true };\n"
            ),
            "the snapshot must be null-prototype with an explicit writable data property: {rewritten}"
        );
        for operation in [
            "const read = __nub_import_meta.main",
            "__nub_import_meta.main = false",
            "__nub_import_meta.main ??= true",
            "__nub_import_meta.main++",
            "delete __nub_import_meta.main",
            "__nub_import_meta[\"main\"]",
            "__nub_import_meta.url",
            "const metadata = __nub_import_meta",
        ] {
            assert!(
                rewritten.contains(operation),
                "the authored reference operation must survive: {operation}\n{rewritten}"
            );
        }
        assert_eq!(
            rewritten.matches("import.meta").count(),
            1,
            "only the snapshot's read of the chunk metadata may remain: {rewritten}"
        );
    }

    #[test]
    fn computed_import_meta_main_has_distinct_root_and_dependency_values() {
        let source = "if (import.meta[\"main\"]) sideEffect();\n";
        let root = rewrite_import_meta_main("/tmp/entry.mjs", source, "true")
            .expect("the executable root's computed main marker must be restored")
            .to_string();
        let dependency = rewrite_non_root_import_meta_main("/tmp/dependency.mjs", source)
            .expect("a dependency's computed main marker must be false")
            .to_string();
        assert!(root.contains("main: true"), "{root}");
        assert!(dependency.contains("main: false"), "{dependency}");
        assert!(
            root.contains("if (__nub_import_meta[\"main\"])")
                && dependency.contains("if (__nub_import_meta[\"main\"])")
                && !root.contains("import.meta[")
                && !dependency.contains("import.meta["),
            "computed access must remain access against each module's snapshot"
        );
    }

    #[test]
    fn spaced_and_commented_import_meta_is_snapshotted_for_roots_and_dependencies() {
        let root_source = "const root = import . meta . main;\n";
        let dependency_source =
            "const dependency = import /* module metadata */ . meta[\"main\"];\n";
        let root = rewrite_import_meta_main("/tmp/entry.mjs", root_source, "true")
            .expect("a spaced root import.meta must be recognized")
            .to_string();
        let dependency =
            rewrite_non_root_import_meta_main("/tmp/dependency.mjs", dependency_source)
                .expect("a commented dependency import.meta must be recognized")
                .to_string();

        assert!(
            root.starts_with(
                "const __nub_import_meta = { __proto__: null, ...import.meta, main: true };\n"
            ),
            "{root}"
        );
        assert!(
            dependency.starts_with(
                "const __nub_import_meta = { __proto__: null, ...import.meta, main: false };\n"
            ),
            "{dependency}"
        );
        assert!(
            root.contains("const root = __nub_import_meta . main;"),
            "{root}"
        );
        assert!(
            dependency.contains("const dependency = __nub_import_meta[\"main\"];"),
            "{dependency}"
        );
        assert!(!root.contains("import . meta"), "{root}");
        assert!(
            !dependency.contains("import /* module metadata */ . meta"),
            "{dependency}"
        );
        assert_eq!(
            root.matches("import.meta").count(),
            1,
            "only the root snapshot may retain a contiguous import.meta: {root}"
        );
        assert_eq!(
            dependency.matches("import.meta").count(),
            1,
            "only the dependency snapshot may retain a contiguous import.meta: {dependency}"
        );
    }

    #[test]
    fn import_meta_alias_is_absent_from_the_authored_source() {
        let source = r#"const __nub_import_meta = "occupied";
const __nub_import_meta_1 = "also occupied";
function read(\u005f_nub_import_meta_2) { return import.meta.main; }
globalThis.value = read();
"#;
        let rewritten = rewrite_import_meta_main("/tmp/entry.mjs", source, "true")
            .expect("import.meta must be rewritten")
            .to_string();
        assert!(
            rewritten.starts_with(
                "const __nub_import_meta_3 = { __proto__: null, ...import.meta, main: true };\n"
            ),
            "the injected binding must not collide with raw or escaped authored bindings: {rewritten}"
        );
        assert!(rewritten.contains("return __nub_import_meta_3.main"));
    }

    #[test]
    fn import_meta_snapshot_sourcemap_tracks_insertions_and_updates() {
        let source = "const before = 1;\nconst value = import.meta.main; const after = 2;\n";
        let magic = rewrite_import_meta_main("/tmp/entry.mjs", source, "true")
            .expect("import.meta must be rewritten");
        let rewritten = magic.to_string();
        let map = magic.source_map(SourceMapOptions {
            hires: Hires::Boundary,
            include_content: true,
            source: "/tmp/entry.mjs".into(),
        });
        let authored_after = source.lines().nth(1).unwrap().find("const after").unwrap();
        let generated_line = rewritten.lines().nth(2).unwrap();
        let generated_after = generated_line.find("const after").unwrap();

        assert_eq!(
            map.get_sources().collect::<Vec<_>>(),
            vec!["/tmp/entry.mjs"]
        );
        assert_eq!(
            map.get_source_contents().collect::<Vec<_>>(),
            vec![Some(source)],
            "the map must retain the exact authored source"
        );
        assert!(
            map.get_tokens().any(|token| {
                token.get_dst_line() == 1
                    && token.get_dst_col() == 0
                    && token.get_src_line() == 0
                    && token.get_src_col() == 0
            }),
            "the prepended snapshot must shift the first authored line: {}",
            map.to_json_string()
        );
        assert!(
            map.get_tokens().any(|token| {
                token.get_dst_line() == 2
                    && token.get_dst_col() == generated_after as u32
                    && token.get_src_line() == 1
                    && token.get_src_col() == authored_after as u32
            }),
            "authored code after the longer alias update must retain its original column: {}",
            map.to_json_string()
        );
    }

    #[test]
    fn bundled_dependencies_do_not_inherit_the_entry_import_meta_main_value() {
        let dir = fixture_dir("import-meta-main-dependency");
        let entry = dir.join("entry.mjs");
        std::fs::write(
            &entry,
            "import './dependency.mjs';\nglobalThis.entryMain = import.meta.main;\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("dependency.mjs"),
            "globalThis.dependencyMain = import.meta.main;\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("an ESM root and dependency must compile");
        let code = res
            .files
            .iter()
            .filter(|file| !file.name.ends_with(".map"))
            .map(|file| String::from_utf8_lossy(&file.bytes).into_owned())
            .collect::<String>();
        assert!(
            !code.contains("import.meta.main"),
            "every authored main predicate must use module-local state before chunking: {code}"
        );
        assert!(
            code.contains("main: true"),
            "the executable root snapshot must start as main: {code}"
        );
        assert!(
            code.contains("main: false"),
            "a static dependency placed in the entry chunk must get a non-main snapshot: {code}"
        );
        assert_eq!(
            code.matches("...import.meta").count(),
            2,
            "flattening must retain one independent metadata snapshot per authored module: {code}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A wrapper is a separate, two-line virtual source rather than a prefix
    // spliced into the authored module.  That keeps all author-source lines and
    // columns owned by Rolldown's existing maps: linked/external maps still
    // describe the original module, and inline maps merely gain this virtual
    // root.  It also keeps the absolute source path out of generated chunks.
    #[test]
    fn compile_root_wrapper_keeps_source_maps_and_output_paths_separate() {
        let dir = fixture_dir("compile-prelude-maps");
        let entry = dir.join("entry.cjs");
        std::fs::write(&entry, "module.exports = globalThis['compileResult'];\n").unwrap();

        for mode in [
            SourcemapMode::Linked,
            SourcemapMode::Inline,
            SourcemapMode::External,
        ] {
            let mut o = opts();
            o.minify = false;
            o.sourcemap = mode;
            let res = bundle(&entry, &o).unwrap_or_else(|err| {
                panic!("a CJS root with {mode:?} source maps must compile: {err:#}")
            });
            let chunks = res
                .files
                .iter()
                .filter(|file| !file.name.ends_with(".map"))
                .map(|file| String::from_utf8_lossy(&file.bytes).into_owned())
                .collect::<Vec<_>>();
            assert!(
                chunks
                    .iter()
                    .all(|chunk| !chunk.contains(dir.to_string_lossy().as_ref())),
                "the build-machine fixture path must stay in the module graph, not emitted code"
            );
            match mode {
                SourcemapMode::Linked => {
                    assert!(res.files.iter().any(|file| file.name.ends_with(".map")));
                    assert!(
                        chunks
                            .iter()
                            .any(|chunk| chunk.contains("sourceMappingURL="))
                    );
                }
                SourcemapMode::Inline => {
                    assert!(res.files.iter().all(|file| !file.name.ends_with(".map")));
                    assert!(
                        chunks
                            .iter()
                            .any(|chunk| chunk.contains("sourceMappingURL=data:"))
                    );
                }
                SourcemapMode::External => {
                    assert!(!res.detached_maps.is_empty());
                    assert!(
                        chunks
                            .iter()
                            .all(|chunk| !chunk.contains("sourceMappingURL="))
                    );
                }
                SourcemapMode::None => unreachable!(),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_static_worker_is_a_second_esm_root_with_no_source_path_in_its_chunk() {
        let dir = fixture_dir("compile-prelude-worker");
        let entry = dir.join("entry.ts");
        let worker = dir.join("worker.cts");
        std::fs::write(
            &entry,
            "new Worker(new URL('./worker.cts', import.meta.url));\n",
        )
        .unwrap();
        std::fs::write(
            &worker,
            "console.log('CJS_WORKER_REQUIRE:' + require('node:path').sep);\nexport = globalThis['workerResult'];\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("a static CJS-authored worker must compile");
        let chunks = res
            .files
            .iter()
            .filter(|file| !file.name.ends_with(".map"))
            .map(|file| String::from_utf8_lossy(&file.bytes).into_owned())
            .collect::<Vec<_>>();
        assert!(
            chunks.len() >= 2,
            "the primary program and worker must each emit a root chunk: {chunks:#?}"
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.contains(dir.to_string_lossy().as_ref())),
            "neither root may emit the build-machine source path"
        );
        let commonjs = chunks
            .iter()
            .find(|chunk| chunk.contains("CJS_WORKER_REQUIRE:"))
            .expect("the CommonJS worker implementation must be emitted");
        assert!(
            commonjs.contains(COMPILE_COMMONJS_REQUIRE_MARKER),
            "the worker's raw builtin require needs the shared CommonJS loader scope:\n{commonjs}"
        );
        assert!(
            commonjs.contains("require(\"node:path\")"),
            "the worker fixture must retain the raw builtin call this loader fixes:\n{commonjs}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.js` entry Node runs as CommonJS must keep a usable `require`.
    ///
    /// Rolldown decides CommonJS from `module`/`exports` markers, so an entry that
    /// only CALLS require is scanned as ESM and its require survives into ESM
    /// output unbound — `require is not defined in ES module scope`, after a build
    /// that exited 0. Only visible when the require targets a builtin or an
    /// ejected package; a bundlable one is inlined and leaves nothing to fail.
    ///
    /// Asserted per-CHUNK, not over the whole bundle: the marker rides in the
    /// shared CommonJS chunk, which is emitted whenever anything in the graph is
    /// CommonJS — the prelude alone guarantees that — so a bundle-wide search
    /// passes either way and proves nothing. What matters is that the chunk
    /// carrying the entry's own code is the one holding the binding.
    ///
    /// The ESM half is the control: Node hands authored ESM no `require`, so
    /// neither may nub.
    #[test]
    fn a_commonjs_entry_without_markers_still_gets_its_require() {
        let dir = fixture_dir("compile-cjs-entry-require");
        // No "type" field: Node's default, and the shape most projects have.
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"f","version":"1.0.0"}"#,
        )
        .unwrap();

        let mut o = opts();
        // The marker is matched verbatim, so it must survive un-mangled.
        o.minify = false;

        let chunk_with = |res: &BundleResult, needle: &str| -> String {
            res.files
                .iter()
                .filter(|f| !f.name.ends_with(".map"))
                .map(|f| String::from_utf8_lossy(&f.bytes).into_owned())
                .find(|code| code.contains(needle))
                .unwrap_or_else(|| panic!("no chunk carried {needle}"))
        };

        let entry = dir.join("entry.js");
        std::fs::write(
            &entry,
            "const path = require('path');
console.log('CJS_ENTRY_MARK', path.sep);
",
        )
        .unwrap();
        let res = bundle(&entry, &o).expect("a CommonJS entry must compile");
        assert!(
            chunk_with(&res, "CJS_ENTRY_MARK").contains(COMPILE_COMMONJS_REQUIRE_MARKER),
            "the chunk holding the entry must also bind its require"
        );

        let esm = dir.join("esm.mjs");
        std::fs::write(
            &esm,
            "import path from 'node:path';
console.log('ESM_ENTRY_MARK', path.sep);
",
        )
        .unwrap();
        let res = bundle(&esm, &o).expect("an ESM entry must compile");
        assert!(
            !chunk_with(&res, "ESM_ENTRY_MARK").contains(COMPILE_COMMONJS_REQUIRE_MARKER),
            "authored ESM must not be handed a require binding Node would not give it"
        );
    }

    #[test]
    fn emitted_esm_chunks_use_mjs_with_coherent_worker_dynamic_import_and_maps() {
        let dir = fixture_dir("compile-mjs-chunks");
        let entry = dir.join("entry.ts");
        std::fs::write(
            &entry,
            "new Worker(new URL('./worker.ts', import.meta.url));\n\
             import('./dynamic.ts').then(({ value }) => (globalThis.OUT = value));\n",
        )
        .unwrap();
        std::fs::write(dir.join("worker.ts"), "postMessage('ready');\n").unwrap();
        std::fs::write(dir.join("dynamic.ts"), "export const value = 'dynamic';\n").unwrap();

        let mut o = opts();
        o.minify = false;
        o.sourcemap = SourcemapMode::Linked;
        let res = bundle(&entry, &o).expect("static worker and dynamic import must compile");
        let chunks = res
            .files
            .iter()
            .filter(|file| !file.name.ends_with(".map"))
            .collect::<Vec<_>>();
        assert!(res.entry.ends_with(".mjs"), "entry was {}", res.entry);
        assert!(
            chunks.len() >= 3,
            "main, dynamic, and static-worker chunks must all emit: {chunks:#?}"
        );
        assert!(
            chunks.iter().all(|chunk| chunk.name.ends_with(".mjs")),
            "every compiler-produced ESM chunk must have an unambiguous extension: {chunks:#?}"
        );

        let worker = chunks
            .iter()
            .find(|chunk| chunk.name.starts_with("worker-") && chunk.name.ends_with("-code.mjs"))
            .expect("the explicit static-worker filename must be emitted");
        let dynamic = chunks
            .iter()
            .find(|chunk| chunk.name.starts_with("dynamic-"))
            .expect("the dynamic import must be split into its own chunk");
        let main = chunks
            .iter()
            .find(|chunk| chunk.name == res.entry)
            .expect("the reported entry must be one emitted chunk");
        let main_code = String::from_utf8_lossy(&main.bytes);
        let public_worker = worker
            .name
            .strip_suffix("-code.mjs")
            .map(|stem| format!("{stem}.mjs"))
            .expect("worker chunks use the private -code.mjs suffix");
        assert!(
            main_code.contains(&public_worker),
            "the rewritten worker URL must name the public worker wrapper: {main_code}"
        );
        assert!(
            main_code.contains(&dynamic.name),
            "the dynamic import must name the emitted .mjs chunk: {main_code}"
        );
        for chunk in chunks {
            let map_name = format!("{}.map", chunk.name);
            if chunk.name.starts_with("rolldown-runtime-") {
                // Strict execution order adds a synthetic helper-only runtime
                // chunk. It has no authored source to map, and Rolldown
                // deliberately emits neither an empty map nor a reference.
                assert!(
                    !res.files.iter().any(|file| file.name == map_name)
                        && !String::from_utf8_lossy(&chunk.bytes).contains("sourceMappingURL="),
                    "the synthetic runtime must not pretend to map authored source"
                );
                continue;
            }
            assert!(
                res.files.iter().any(|file| file.name == map_name),
                "the authored .mjs chunk {} must keep its linked source map",
                chunk.name
            );
            assert!(
                String::from_utf8_lossy(&chunk.bytes)
                    .contains(&format!("sourceMappingURL={map_name}")),
                "the .mjs chunk {} must reference its own map",
                chunk.name
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_runtime_imports_keep_worker_provenance_after_its_source_is_removed() {
        let dir = fixture_dir("worker-runtime-imports");
        let entry = dir.join("entry.ts");
        let worker = dir.join("worker.ts");
        std::fs::write(
            &entry,
            "new Worker(new URL('./worker.ts', import.meta.url));\n",
        )
        .unwrap();
        std::fs::write(
            &worker,
            "import 'peer';\nconst selected = 'peer'; await import(selected);\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        o.external = vec!["peer".into()];
        o.allow_dynamic_import = true;
        let res = bundle_for_compile(&entry, &o, &dir)
            .expect("worker external and computed imports must stay runtime-resolvable");

        let root = res
            .worker_roots
            .iter()
            .find(|root| root.entry.starts_with("worker-") && root.chunk.ends_with("-code.mjs"))
            .expect("the worker remains a separately wrapped executable root");
        assert!(
            res.files.iter().any(|file| file.name == root.chunk),
            "the private worker chunk must be in the payload before its source disappears"
        );
        assert!(
            res.external_imports.iter().any(|external| {
                external.specifier == "peer" && external.importer.as_deref() == Some("worker.ts")
            }),
            "the resolver must retain the worker's launch-cwd-relative importer: {:#?}",
            res.external_imports
        );
        assert_eq!(res.dynamic_import_sites, 1);
        assert!(
            res.files.iter().all(|file| {
                !String::from_utf8_lossy(&file.bytes).contains(dir.to_string_lossy().as_ref())
            }),
            "the payload cannot depend on the build-tree path after extraction"
        );

        std::fs::remove_file(&worker).unwrap();
        assert!(
            !worker.exists(),
            "the artifact metadata above must be sufficient after the authored worker has gone away"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dynamic_global_access_still_has_an_unconditional_prelude_import() {
        let source = Path::new("/tmp/nub-prelude/dynamic-global.ts");
        let wrapper = compile_root_source(source);
        assert!(
            wrapper.starts_with("import "),
            "the wrapper must not depend on a statically named global that the prelude creates: {wrapper}"
        );
        assert!(wrapper.contains("\"\\u0000nub:compile-preamble\""));
    }

    #[test]
    fn inline_worker_forms_are_refused_but_dynamic_file_workers_remain_unresolved() {
        let forbidden = [
            ("data", "new Worker('data:text/javascript,postMessage(1)')"),
            (
                "blob",
                "new Worker(new URL('blob:opaque', import.meta.url))",
            ),
            ("eval", "new Worker(source, { eval: true })"),
        ];
        let mut refusals = Vec::new();
        for (tag, source) in forbidden {
            let scan = scan_new_urls(&format!("/tmp/{tag}.ts"), source);
            assert_eq!(scan.worker_refusals.len(), 1, "{tag}: {scan:?}");
            refusals.extend(scan.worker_refusals);
        }
        let err = reject_unsupported_workers(&mut refusals)
            .expect_err("inline worker forms must fail compilation with an actionable diagnostic");
        let message = format!("{err:#}");
        assert!(message.contains("data:") && message.contains("blob:") && message.contains("eval"));
        assert!(message.contains("file-backed module"), "{message}");
        let dynamic = scan_new_urls("/tmp/dynamic.ts", "new Worker(workerPath)");
        assert!(dynamic.worker_refusals.is_empty());
        assert!(dynamic.edits.is_empty());

        let cjs = scan_new_urls(
            "/tmp/cjs-worker.cjs",
            "const { Worker: NodeWorker } = require('node:worker_threads'); new NodeWorker(new URL('./worker.ts', import.meta.url));",
        );
        assert_eq!(cjs.worker_refusals.len(), 1, "{cjs:?}");
        assert!(cjs.worker_refusals[0].reason.contains("CommonJS"));

        for source in [
            concat!(
                "// require('node:worker_threads')\n",
                "new Custom(new URL('./worker.ts', import.meta.url));",
            ),
            concat!(
                "const example = \"require('node:worker_threads')\"; ",
                "new Custom(new URL('./worker.ts', import.meta.url));",
            ),
            concat!(
                "const { Worker: NodeWorker } = require('node:worker_threads'); ",
                "new Custom(new URL('./worker.ts', import.meta.url));",
            ),
        ] {
            let scan = scan_new_urls("/tmp/cjs-worker-false-positive.cjs", source);
            assert!(
                scan.worker_refusals.is_empty(),
                "only a constructor resolved to the destructured Node binding may be rejected: {source}"
            );
        }
    }

    // The whole contract of the `new URL` rewrite, against a REAL bundle: the
    // referenced file's bytes ship, the chunk names the embedded copy instead of
    // the source-tree path, and the base stays `import.meta.url` — which is what
    // makes the compiled binary read the right file from any cwd. Until this
    // existed the same source compiled clean and threw ENOENT on first run.
    #[test]
    fn a_new_url_beside_a_module_ships_its_bytes_and_keeps_the_module_relative_base() {
        let dir = fixture_dir("newurl");
        std::fs::write(dir.join("data.json"), br#"{"k":"NEWURLBYTES"}"#).unwrap();
        std::fs::write(
            dir.join("entry.ts"),
            b"import { readFileSync } from 'node:fs';\n\
              globalThis.OUT = readFileSync(new URL('./data.json', import.meta.url), 'utf8');\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&dir.join("entry.ts"), &o).expect("a new URL asset must compile");

        assert_eq!(res.assets.len(), 1, "the referenced file must ship");
        assert_eq!(res.assets[0].bytes, br#"{"k":"NEWURLBYTES"}"#);
        assert!(
            !res.assets[0].name.contains('/'),
            "the asset must be a flat sibling of the chunks: {}",
            res.assets[0].name
        );
        let code = emitted_entry_code(&res);
        assert!(
            code.contains(&res.assets[0].name),
            "the chunk must name the embedded copy, got:\n{code}"
        );
        assert!(
            !code.contains("\"./data.json\"") && !code.contains("'./data.json'"),
            "the source-tree path must not survive — it does not exist where the \
             binary runs, got:\n{code}"
        );
        assert!(
            code.contains("import.meta.url"),
            "the base must stay module-relative, not cwd-relative, got:\n{code}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The honest scope of the fix, and the control for the test above. A computed
    // URL cannot be embedded, and must NOT become a new build failure: the same
    // expression is how a program names a file it will WRITE, and a specifier
    // pointing at nothing on the build machine is that same case spelled
    // statically. Both still compile, and neither ships bytes.
    #[test]
    fn a_url_this_build_cannot_follow_still_compiles_and_ships_nothing() {
        for (tag, src) in [
            (
                "computed",
                "const name = globalThis.PICK;\n\
                 globalThis.OUT = new URL(name, import.meta.url);\n",
            ),
            (
                "absent",
                "globalThis.OUT = new URL('./written-at-runtime.log', import.meta.url);\n",
            ),
            (
                "scheme",
                "globalThis.OUT = [\n\
                 \x20\x20new URL('https://example.com/x.json', import.meta.url),\n\
                 \x20\x20new URL('data:text/plain,hi', import.meta.url),\n\
                 \x20\x20new URL('/data.json', import.meta.url),\n\
                 ];\n",
            ),
            // A module's own URL is not the global one. The `export` spelling is
            // listed because missing it made the guard depend on whether the
            // shadow happened to be exported.
            (
                "shadowed",
                "class URL { constructor(a, b) { this.a = a; this.b = b; } }\n\
                 globalThis.OUT = new URL('./data.json', import.meta.url);\n",
            ),
            (
                "shadowed-export",
                "export class URL { constructor(a, b) { this.a = a; } }\n\
                 globalThis.OUT = new URL('./data.json', import.meta.url);\n",
            ),
            (
                "shadowed-export-const",
                "export const URL = class { constructor(a, b) { this.a = a; } };\n\
                 globalThis.OUT = new URL('./data.json', import.meta.url);\n",
            ),
        ] {
            let dir = fixture_dir(&format!("newurl-{tag}"));
            // Present on disk for every case, so "nothing shipped" can only be the
            // rule under test and never a missing fixture.
            std::fs::write(dir.join("data.json"), b"{}").unwrap();
            std::fs::write(dir.join("entry.ts"), src).unwrap();

            let mut o = opts();
            o.minify = false;
            let res = bundle(&dir.join("entry.ts"), &o)
                .unwrap_or_else(|e| panic!("{tag} must still compile, got: {e:#}"));
            assert!(
                res.assets.is_empty(),
                "{tag} must ship no asset, got {:?}",
                res.assets.iter().map(|a| &a.name).collect::<Vec<_>>()
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // Resolution is PER MODULE, not per entry: a dependency in a subdirectory
    // names its own sibling, and after bundling both modules share one chunk at
    // the app root. Getting this wrong resolves against the entry's directory and
    // silently embeds the wrong file — or none.
    #[test]
    fn a_new_url_in_a_nested_module_resolves_against_that_module() {
        let dir = fixture_dir("newurl-nested");
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/table.bin"), b"NESTEDBYTES").unwrap();
        std::fs::write(
            dir.join("lib/load.ts"),
            b"export const at = new URL('./table.bin', import.meta.url);\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("entry.ts"),
            b"import { at } from './lib/load.ts';\nglobalThis.OUT = at;\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&dir.join("entry.ts"), &o).expect("compiles");
        assert_eq!(res.assets.len(), 1, "the nested sibling must ship");
        assert_eq!(res.assets[0].bytes, b"NESTEDBYTES");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A specifier is URL text, so its escapes have to be resolved before it names
    // a path. Skipping that does not merely miss the file — with both spellings on
    // disk it embeds the OTHER one, and the compiled binary then reads bytes the
    // same source reads differently under plain Node.
    #[test]
    fn a_percent_escaped_specifier_names_the_file_the_runtime_would_open() {
        let dir = fixture_dir("newurl-pct");
        std::fs::write(dir.join("my file.bin"), b"DECODED").unwrap();
        std::fs::write(dir.join("my%20file.bin"), b"LITERAL").unwrap();
        std::fs::write(
            dir.join("entry.ts"),
            b"globalThis.OUT = new URL('./my%20file.bin', import.meta.url);\n",
        )
        .unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&dir.join("entry.ts"), &o).expect("compiles");
        assert_eq!(res.assets.len(), 1, "the referenced file must ship");
        assert_eq!(
            res.assets[0].bytes, b"DECODED",
            "%20 is a space, so the space-named file is the one the URL resolves to"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn percent_decoding_follows_the_url_parser() {
        assert_eq!(percent_decode("./a.bin").as_deref(), Some("./a.bin"));
        assert_eq!(
            percent_decode("./my%20f.bin").as_deref(),
            Some("./my f.bin")
        );
        assert_eq!(percent_decode("./%C3%A9.bin").as_deref(), Some("./é.bin"));
        // An invalid or truncated escape is left verbatim, as the URL parser does.
        assert_eq!(percent_decode("./100%.bin").as_deref(), Some("./100%.bin"));
        assert_eq!(percent_decode("./a%zz.bin").as_deref(), Some("./a%zz.bin"));
        // Bytes that are not UTF-8 have no path spelling to resolve to.
        assert_eq!(percent_decode("./%FF.bin"), None);
    }

    // What separates a file that is unusable when embedded verbatim from one that
    // is exactly right that way. `.json` belongs on the second list: reading it as
    // data is the canonical correct use of a `new URL`, not a mistake to report.
    #[test]
    fn module_extensions_are_the_ones_node_reads_as_source() {
        for ext in ["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "TS"] {
            assert!(is_module_extension(Path::new(&format!("w.{ext}"))), "{ext}");
        }
        for ext in ["json", "wasm", "png", "md", "txt", "node"] {
            assert!(
                !is_module_extension(Path::new(&format!("w.{ext}"))),
                "{ext}"
            );
        }
    }

    // The invariant behind `map: HookTransformOutputMap::Null` — the transform
    // does not move positions, so every line must still begin where it did. A
    // no-substitution template literal is the one specifier form that can span
    // lines, so its newlines have to come back.
    #[test]
    fn the_rewrite_preserves_the_sources_line_structure() {
        let source = "const a = new URL(`./one\n.json`, import.meta.url);\nconst b = 2;\n";
        let edits = vec![(
            NewUrlEdit {
                start: source.find('`').unwrap(),
                end: source.rfind('`').unwrap() + 1,
                source: PathBuf::from("/p/one\n.json"),
                // A data asset, not a worker entry — the splice preserves line
                // structure identically either way, which is what this asserts.
                worker: false,
            },
            "one-a1b2c3d4.json".to_string(),
        )];
        let out = rewrite_new_urls(source, &edits);
        assert_eq!(
            out.matches('\n').count(),
            source.matches('\n').count(),
            "line count must survive the splice, got:\n{out}"
        );
        assert!(out.contains("\"./one-a1b2c3d4.json\""), "{out}");
        assert!(out.ends_with("const b = 2;\n"), "{out}");
    }

    #[test]
    fn tree_shake_false_beats_ignore_annotations() {
        let mut o = opts();
        o.tree_shake = false;
        o.ignore_annotations = true;
        assert!(
            matches!(treeshake_options(&o), TreeshakeOptions::Boolean(false)),
            "--tree-shake false must disable tree-shaking outright"
        );
        o.tree_shake = true;
        assert!(matches!(
            treeshake_options(&o),
            TreeshakeOptions::Option(InnerOptions {
                annotations: Some(false),
                ..
            })
        ));
    }

    #[test]
    fn scan_flags_only_non_literal_dynamic_imports() {
        let src = r#"
await import("./static.js");
await import(`./also-static.js`);
const mod = await import(name);
await import(`./${name}.js`);
"#;
        let sites = scan_unresolvable("/p/app.ts", src);
        assert_eq!(
            sites.len(),
            2,
            "only the two expression specifiers are unresolvable, got {sites:?}"
        );
        assert_eq!(sites[0].line, 4, "line numbers are 1-based into the source");
        assert!(
            sites[0].snippet.contains("import(name)"),
            "the snippet must show the offending call, got {:?}",
            sites[0].snippet
        );
    }

    // Rolldown has a scanner escape hatch for a dynamic import that must remain
    // runtime work. This is deliberately an AST-driven rewrite, not a special
    // case for a constant: an optimizer may learn that ANY expression is
    // constant after this plugin has scanned it. Literal strings and templates
    // without substitutions stay graph edges and are still bundled.
    #[test]
    fn nonliteral_dynamic_imports_are_marked_runtime_before_rolldown_scans_them() {
        let src = r#"
await import("./literal.mjs");
await import(`./template.mjs`);
const name = "foreign-dynamic";
await import(name);
await import(process.env.NUB_DYNAMIC);
await import(`./${process.env.NUB_SUFFIX}.mjs`);
await import("./" + process.env.NUB_SUFFIX + ".mjs");
await import(optionName, { with: { type: "json" } });
await import(/* @vite-ignore */ alreadyMarked);
"#;
        let scan = scan_unresolvable_with_rewrites("/p/app.mjs", src);
        assert_eq!(
            scan.sites.len(),
            6,
            "only expression imports defer: {scan:#?}"
        );
        assert_eq!(scan.sites[0].kind, SiteKind::Variable, "{scan:#?}");
        assert!(
            scan.sites[1..]
                .iter()
                .all(|site| site.kind == SiteKind::Dynamic),
            "only the constant variable has a readable diagnostic value: {scan:#?}"
        );

        let rewritten = ignore_dynamic_imports(src, &scan.dynamic_import_ignores);
        assert_eq!(
            rewritten.matches("/* @vite-ignore */").count(),
            6,
            "every expression-shaped import needs the Rolldown scanner escape: {rewritten}"
        );
        assert!(
            rewritten.contains("import(\"./literal.mjs\")")
                && rewritten.contains("import(`./template.mjs`)"),
            "literal and interpolation-free template imports must remain static: {rewritten}"
        );
        for expression in [
            "name)",
            "process.env.NUB_DYNAMIC)",
            "`./${process.env.NUB_SUFFIX}.mjs`)",
            "\"./\" + process.env.NUB_SUFFIX + \".mjs\")",
            "optionName, { with: { type: \"json\" } })",
            "alreadyMarked)",
        ] {
            assert!(
                rewritten.contains(&format!("/* @vite-ignore */ {expression}")),
                "runtime expression was not protected: {rewritten}"
            );
        }
    }

    #[test]
    fn dynamic_import_marker_sourcemap_tracks_inserted_columns() {
        const MARKER: &str = "/* @vite-ignore */ ";
        let source = "const loaded = import(name);\nconst after = 1;\n";
        let argument_start = source.find("name").expect("the import argument exists");
        let magic = dynamic_import_magic_string(source, &[argument_start]);
        let rewritten = magic.to_string();
        let map = magic.source_map(SourceMapOptions {
            hires: Hires::Boundary,
            include_content: true,
            source: "entry.mjs".into(),
        });

        assert_eq!(
            rewritten,
            "const loaded = import(/* @vite-ignore */ name);\nconst after = 1;\n"
        );
        assert_eq!(map.get_sources().collect::<Vec<_>>(), vec!["entry.mjs"]);
        assert_eq!(
            map.get_source_contents().collect::<Vec<_>>(),
            vec![Some(source)],
            "the transform map must preserve the authored source for linked and inline maps"
        );
        assert!(
            map.get_tokens().any(|token| {
                token.get_dst_line() == 0
                    && token.get_dst_col() == (argument_start + MARKER.len()) as u32
                    && token.get_src_line() == 0
                    && token.get_src_col() == argument_start as u32
            }),
            "the argument after an insertion must map from its shifted generated column: {}",
            map.to_json_string()
        );
        assert!(
            map.get_tokens().any(|token| {
                token.get_dst_line() == 1
                    && token.get_dst_col() == 0
                    && token.get_src_line() == 1
                    && token.get_src_col() == 0
            }),
            "the next line must retain its original source position: {}",
            map.to_json_string()
        );
    }

    #[test]
    fn dynamic_import_expression_forms_compile_only_with_the_opt_in() {
        let dir = fixture_dir("dynamic-import-expression-forms");
        let entry = dir.join("entry.mjs");
        std::fs::write(
            dir.join("literal.mjs"),
            "console.log('literal-static-import');\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("template.mjs"),
            "console.log('template-static-import');\n",
        )
        .unwrap();
        std::fs::write(
            &entry,
            r#"
await import("./literal.mjs");
await import(`./template.mjs`);
const name = "foreign-dynamic";
await import(name);
await import(process.env.NUB_DYNAMIC);
await import(`./${process.env.NUB_SUFFIX}.mjs`);
await import("./" + process.env.NUB_SUFFIX + ".mjs");
"#,
        )
        .unwrap();

        let mut allowed = opts();
        allowed.minify = false;
        allowed.sourcemap = SourcemapMode::None;
        allowed.allow_dynamic_import = true;
        let result = bundle(&entry, &allowed)
            .expect("the opt-in must preserve every nonliteral dynamic import for runtime");
        assert_eq!(
            result.dynamic_import_sites, 4,
            "constant, runtime, interpolated, and concatenated imports need the runtime hook"
        );
        assert!(
            emitted_chunk_code(&result).contains("literal-static-import")
                && emitted_chunk_code(&result).contains("template-static-import"),
            "literal and no-substitution template imports must still be bundled"
        );

        let err = match bundle(&entry, &opts()) {
            Ok(_) => panic!("the default must fail closed"),
            Err(err) => err,
        };
        let message = err.to_string();
        assert!(
            message.contains("--allow-dynamic-import") && message.contains("import(name)"),
            "the default refusal must retain the public dynamic-import diagnostic: {message}"
        );
        assert!(
            !message.contains("Could not resolve 'am'"),
            "a nonliteral import must never leak Rolldown's malformed resolution: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A cast is erased before the bundler resolves, so reading the specifier as
    // WRITTEN reported a literal as computed — and then told the author to make it
    // a static string they had already written. The last two lines are the control
    // that erasure does not make everything look static: unwrapping `spec!` and a
    // template with a substitution still leaves something no bundler can follow.
    #[test]
    fn a_type_wrapped_specifier_is_read_through_the_wrapper() {
        let src = r#"
await import("./as.js" as string);
await import(("./paren.js"));
await import(("./satisfies.js" satisfies string));
await import(spec!);
await import(`./${spec}.js`);
"#;
        let sites = scan_unresolvable("/p/app.ts", src);
        let lines: Vec<_> = sites.iter().map(|s| s.line).collect();
        assert_eq!(
            lines,
            vec![5, 6],
            "only the two genuinely non-static specifiers are unresolvable, got {sites:?}"
        );
    }

    // The shape that kept reaching the generic diagnostic: a specifier named a
    // line above the import that uses it, including a platform pick written as a
    // ternary. `let` is the control — this pass reads `const` only, because that
    // is what makes "no reassignment" a language guarantee rather than an
    // analysis, and a wrong value is worse than none.
    #[test]
    fn a_const_specifier_is_read_and_a_let_is_not() {
        let src = r#"
const plugin = "./plugin.js";
const pkg = process.platform === "darwin" ? "@x/core-darwin" : "@x/core-linux";
let later = "./later.js";
await import(plugin);
await import(pkg);
await import(later);
"#;
        let sites = scan_unresolvable("/p/app.ts", src);
        let read: Vec<_> = sites
            .iter()
            .map(|s| (s.kind, s.resolves_to.clone()))
            .collect();
        assert_eq!(
            read,
            vec![
                (SiteKind::Variable, vec!["./plugin.js".to_string()]),
                (
                    SiteKind::Variable,
                    vec!["@x/core-darwin".to_string(), "@x/core-linux".to_string()]
                ),
                (SiteKind::Dynamic, Vec::new()),
            ],
            "got {sites:?}"
        );
    }

    // A name bound twice may be reaching an inner shadow, and saying which needs
    // the scope analysis this pass exists to avoid — so it claims nothing and the
    // site stays an ordinary computed one.
    #[test]
    fn a_shadowed_name_is_not_read() {
        let src = r#"
const pkg = "./outer.js";
export function load(pkg) { return import(pkg); }
"#;
        let sites = scan_unresolvable("/p/app.ts", src);
        assert_eq!(sites.len(), 1, "got {sites:?}");
        assert_eq!(sites[0].kind, SiteKind::Dynamic);
        assert!(sites[0].resolves_to.is_empty(), "got {sites:?}");
    }

    // The jsonc-parser@2.3.1 shape: a UMD factory takes `require` as a
    // PARAMETER, so its static `require("./impl/format")` is an ordinary call no
    // bundler can rewrite, and it reaches the artifact intact. Every other line
    // is a control that must NOT be flagged — a rewritable top-level require, a
    // builtin, and a guarded `require(expr)` probe (the shape five real packages
    // in opencode's tree use, all try/catch'd).
    #[test]
    fn scan_flags_require_only_when_the_bundler_cannot_rewrite_it() {
        let src = r#"
const fine = require("some-pkg");
(function (factory) { factory(require, exports); })(function (require, exports) {
  const fmt = require("./impl/format");
  const os = require("node:os");
  try { const opt = require(maybeInstalled); } catch {}
});
"#;
        let sites = scan_unresolvable("/p/umd.js", src);
        let kinds: Vec<_> = sites.iter().map(|s| (s.kind, s.line)).collect();
        assert_eq!(
            kinds,
            vec![(SiteKind::Indirect, 4)],
            "only the shadowed relative require is unconditionally broken, got {sites:?}"
        );
    }

    // `const require = createRequire(...)` shadows for the whole module, so its
    // calls are live at runtime too — but a builtin still resolves from inside
    // the artifact, and failing that would be a false positive.
    #[test]
    fn module_level_create_require_shadows_but_builtins_still_pass() {
        let src = r#"
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const fs = require("node:fs");
const pkg = require("./package.json");
"#;
        let sites = scan_unresolvable("/p/app.mjs", src);
        assert_eq!(
            sites.len(),
            1,
            "only the non-builtin survives, got {sites:?}"
        );
        assert_eq!(sites[0].kind, SiteKind::Indirect);
        assert!(sites[0].snippet.contains("./package.json"), "{sites:?}");
    }

    fn dynamic_site() -> DynamicSite {
        DynamicSite {
            kind: SiteKind::Dynamic,
            module: "/p/src/plugins.ts".into(),
            line: 12,
            column: 20,
            snippet: "import(pluginPath)".into(),
            resolves_to: Vec::new(),
        }
    }

    /// A site the author cannot rewrite must be told about --unbundled, and one
    /// they CAN rewrite must not be — the flag would ship a package unbundled to
    /// work around a line they could simply edit.
    ///
    /// The partial-unbundling warning is load-bearing rather than decorative:
    /// naming only some of the listed packages compiles cleanly and then fails at
    /// run time, which is how jsdom presented (unbundling css-tree alone built
    /// fine and died on jsdom's own readFileSync of a file beside its source).
    #[test]
    fn a_site_inside_a_dependency_is_pointed_at_unbundled() {
        let in_dependency = DynamicSite {
            module: "/p/node_modules/css-tree/lib/data.js".into(),
            kind: SiteKind::Indirect,
            snippet: "require('mdn-data/css/properties.json')".into(),
            ..dynamic_site()
        };
        let err =
            reject_unresolved(&[in_dependency], &[], false, false, false).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("--unbundled"), "got: {msg}");
        assert!(
            msg.contains("still fail at run time"),
            "the partial-unbundling trap must be named: {msg}"
        );

        let own_source =
            reject_unresolved(&[dynamic_site()], &[], false, false, false).expect_err("must fail");
        assert!(
            !own_source.to_string().contains("--unbundled"),
            "the author's own site must not be sent to --unbundled: {own_source}"
        );
    }

    // The default is refusal, so the error has to carry everything needed to fix
    // the source AND name the flag that accepts it as written — a build that
    // fails with no way forward is what drove users to hide the site from the
    // scanner instead.
    #[test]
    fn rejection_names_the_site_the_fix_and_the_flag() {
        let err =
            reject_unresolved(&[dynamic_site()], &[], false, false, false).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("/p/src/plugins.ts:12:20"), "got: {msg}");
        assert!(msg.contains("import(pluginPath)"), "got: {msg}");
        assert!(msg.contains("static string"), "got: {msg}");
        assert!(msg.contains("--allow-dynamic-import"), "got: {msg}");
        assert!(
            !msg.contains("UMD"),
            "the indirect-require hint must not show for a dynamic-specifier site: {msg}"
        );
    }

    /// Pins the message shape the optional-backend hint keys on. Rolldown renders
    /// the importer as a path, so a dependency's own unresolved import is
    /// distinguishable from the author's — the hint tells them to reach for
    /// --external, which is wrong advice for code they can actually edit.
    #[test]
    fn tells_a_dependencys_unresolved_import_from_the_authors_own() {
        assert!(unresolved_importer_is_dependency(
            "Could not resolve 'pg' in node_modules/knex/lib/dialects/postgres/index.js"
        ));
        assert!(
            !unresolved_importer_is_dependency("Could not resolve 'left-pad' in src/app.ts"),
            "the author's own source must not collect the --external hint"
        );
    }

    // The flag excuses a computed `import()` and nothing else. An indirect
    // require is a different defect that no runtime resolve hook can serve, so it
    // must still fail — and must not advertise a flag that would not help it.
    #[test]
    fn the_flag_excuses_only_the_computed_import() {
        assert!(
            reject_unresolved(&[dynamic_site()], &[], true, false, false).is_ok(),
            "a permitted dynamic site must not fail the build"
        );

        let indirect = DynamicSite {
            kind: SiteKind::Indirect,
            module: "/p/node_modules/jsonc-parser/umd.js".into(),
            line: 4,
            column: 15,
            snippet: r#"require("./impl/format")"#.into(),
            resolves_to: Vec::new(),
        };
        let err = reject_unresolved(&[dynamic_site(), indirect], &[], true, false, false)
            .expect_err("an indirect require must still fail");
        let msg = err.to_string();
        assert!(msg.contains("umd.js:4:15"), "got: {msg}");
        assert!(
            !msg.contains("plugins.ts"),
            "the permitted dynamic site must be gone from the list: {msg}"
        );
        assert!(msg.contains("1 import could not"), "got: {msg}");
        assert!(msg.contains("UMD factory parameter"), "got: {msg}");
        assert!(msg.contains("createRequire result"), "got: {msg}");
        assert!(msg.contains("static import"), "got: {msg}");
        assert!(
            !msg.contains("--allow-dynamic-import"),
            "a flag that cannot help this site must not be offered: {msg}"
        );
    }

    // A variable specifier refuses by default like any other unfollowable import,
    // but it earns its own fix line: the author already wrote a static string, so
    // "make it a static string" is advice that cannot be acted on. The flag is
    // still the sanctioned way past it, and the second half asserts it is not
    // weakened.
    #[test]
    fn rejection_of_a_variable_specifier_names_its_value_and_the_flag() {
        let site = DynamicSite {
            kind: SiteKind::Variable,
            module: "/p/src/platform.ts".into(),
            line: 7,
            column: 22,
            snippet: "import(pkg)".into(),
            resolves_to: vec!["@x/core-darwin".into(), "@x/core-linux".into()],
        };
        let err = reject_unresolved(std::slice::from_ref(&site), &[], false, false, false)
            .expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("/p/src/platform.ts:7:22"), "got: {msg}");
        assert!(msg.contains("import(pkg)"), "got: {msg}");
        assert!(
            msg.contains(r#""@x/core-darwin" or "@x/core-linux""#),
            "the value the author has to inline must be named, got: {msg}"
        );
        assert!(msg.contains("--allow-dynamic-import"), "got: {msg}");

        reject_unresolved(&[site], &[], true, false, false)
            .expect("the flag must excuse a variable specifier exactly as it does a computed one");
    }

    // The gate that makes "Compiled …" mean something: a chunk carrying raw JSX
    // is what `jsx: "preserve"` produced, and it must not reach an artifact.
    #[test]
    fn emitted_chunk_gate_rejects_unparseable_output() {
        let bad = BundledFile {
            name: "config-a1b2.js".into(),
            bytes: b"export const view = () => <box width=\"100%\">hi</box>;\n".to_vec(),
        };
        let err = reject_invalid_chunks(&[bad]).expect_err("raw JSX must fail the compile");
        let msg = err.to_string();
        assert!(
            msg.contains("config-a1b2.js:1:"),
            "the failing chunk and position must be named, got: {msg}"
        );
        assert!(
            msg.contains("compilerOptions.jsx"),
            "the message must point at the usual cause, got: {msg}"
        );

        // Control: modern syntax the artifact really does run must pass, or the
        // gate would just be a build-breaker.
        let good = BundledFile {
            name: "entry.js".into(),
            bytes:
                b"#!/usr/bin/env node\nimport fs from 'node:fs';\n\
                     const x = globalThis.a?.b ?? 1;\nawait fs.promises.stat('.');\nexport { x };\n"
                    .to_vec(),
        };
        reject_invalid_chunks(&[good]).expect("valid ESM must pass the gate");
    }

    // End-to-end proof for the two halves of the JSX fix: the override defeats
    // `preserve`, and the emit gate would have caught it had the override
    // failed. Bundling a .tsx under a preserve tsconfig must yield JS.
    #[test]
    fn jsx_preserve_is_transformed_not_emitted_raw() {
        let dir = std::env::temp_dir().join(format!("nub-jsx-preserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("jsx-runtime")).unwrap();
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{"compilerOptions":{"jsx":"preserve","jsxImportSource":"./jsx-runtime"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("jsx-runtime/package.json"),
            r#"{"name":"jsx-runtime","type":"module","exports":{"./jsx-runtime":"./jsx-runtime.js"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("jsx-runtime/jsx-runtime.js"),
            "export function jsx(t, p) { return { t, p }; }\n\
             export function jsxs(t, p) { return { t, p }; }\n\
             export const Fragment = 'Fragment';\n",
        )
        .unwrap();
        let entry = dir.join("entry.tsx");
        std::fs::write(&entry, "globalThis.OUT = <div id=\"a\">hi</div>;\n").unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("a preserve tsconfig must still compile");
        let code = emitted_entry_code(&res);
        assert!(
            !code.contains("<div"),
            "JSX must not survive into the bundle, got:\n{code}"
        );
        assert!(
            code.contains("jsx"),
            "the automatic runtime call must be there, got:\n{code}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // `preserve` is the ONLY setting overridden: naming a runtime freezes
    // Rolldown's per-file choice, so a classic-runtime project must be left to it.
    #[test]
    fn jsx_override_applies_only_to_preserve() {
        let dir = std::env::temp_dir().join(format!("nub-jsx-override-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("entry.tsx");
        std::fs::write(&entry, "export const x = 1;\n").unwrap();

        for (setting, want_override) in [("preserve", true), ("react", false), ("react-jsx", false)]
        {
            std::fs::write(
                dir.join("tsconfig.json"),
                format!(r#"{{"compilerOptions":{{"jsx":"{setting}"}}}}"#),
            )
            .unwrap();
            assert_eq!(
                jsx_override(&entry, None).is_some(),
                want_override,
                "jsx: {setting:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn bundle_fixture(source: &str, o: &BundleOptions) -> String {
        // A unique dir per call keeps parallel test threads from colliding on
        // the entry path (the bundler keys diagnostics on it).
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nub-bundle-test-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("entry.ts"), source).unwrap();
        let res = bundle(&dir.join("entry.ts"), o).expect("bundle succeeds");
        let code = emitted_entry_code(&res);
        let _ = std::fs::remove_dir_all(&dir);
        code
    }

    // The load-bearing guarantee: minify is ON by default and would rename
    // `Registry` to a single letter, silently breaking every framework that
    // keys on `Class.name` at runtime inside a frozen binary. Setting the
    // rolldown flag is NOT proof it works (Bun's own --keep-names is a verified
    // no-op), so this asserts against the EMITTED, minified bundle.
    #[test]
    fn keep_names_survives_minification() {
        const SRC: &str = "class Registry {}\nfunction handler() {}\n\
                           globalThis.OUT = Registry.name + handler.name;\n";

        let kept = opts();
        let code = bundle_fixture(SRC, &kept);
        assert!(
            code.contains("Registry") && code.contains("handler"),
            "keep_names=true must preserve the class/fn names through minify, got:\n{code}"
        );

        let mut mangled = opts();
        mangled.keep_names = false;
        let code = bundle_fixture(SRC, &mangled);
        assert!(
            !code.contains("Registry"),
            "control: keep_names=false must let minify rename the class (else the \
             positive case proves nothing), got:\n{code}"
        );
    }

    #[test]
    fn compile_external_ids_keep_competing_importer_provenance_distinct() {
        let root = Path::new("/workspace/app");
        let externals = ExternalImports::new(
            &["peer".into()],
            root,
            Arc::new(Mutex::new(BTreeSet::new())),
        );
        let root_peer = externals
            .record("peer", Some("/workspace/app/entry.mjs"))
            .expect("configured package");
        let nested_peer = externals
            .record("peer", Some("/workspace/app/node_modules/host/index.mjs"))
            .expect("configured package");
        let nested_again = externals
            .record("peer", Some("/workspace/app/node_modules/host/index.mjs"))
            .expect("configured package");

        assert_ne!(root_peer.id, nested_peer.id);
        assert_eq!(nested_peer.id, nested_again.id);
        assert_eq!(root_peer.importer.as_deref(), Some("entry.mjs"));
        assert_eq!(
            nested_peer.importer.as_deref(),
            Some("node_modules/host/index.mjs")
        );

        let sibling_peer = externals
            .record("peer", Some("/workspace/sibling/index.mjs"))
            .expect("configured package");
        assert_eq!(
            sibling_peer.importer.as_deref(),
            Some("../sibling/index.mjs"),
            "a source outside the launch cwd must retain relocatable provenance"
        );
    }

    #[test]
    fn the_transitive_compile_prelude_graph_cannot_be_externalized() {
        let dir = fixture_dir("private-prelude-external");
        let entry = dir.join("entry.mjs");
        std::fs::write(&entry, "console.log('ok');\n").unwrap();
        let mut o = opts();
        o.external = vec!["urlpattern-polyfill".into()];
        let res = bundle_for_compile(&entry, &o, &dir)
            .expect("a user external must not intercept the private runtime graph");
        assert!(
            res.external_imports.is_empty(),
            "no authored module imported the configured external: {:#?}",
            res.external_imports
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compile_prelude_imports_are_not_user_externals() {
        let root = Path::new("/workspace/app");
        let externals = ExternalImports::new(
            &["urlpattern-polyfill".into()],
            root,
            Arc::new(Mutex::new(BTreeSet::new())),
        );
        assert!(
            externals
                .record("urlpattern-polyfill", Some(COMPILE_PREAMBLE_ID))
                .is_none(),
            "the private prelude resolves from nub's runtime, never the launch cwd"
        );
        assert!(
            externals
                .record("urlpattern-polyfill", Some("/workspace/app/entry.mjs"))
                .is_some(),
            "the user's direct import remains controlled by --external"
        );
    }

    #[test]
    fn query_suffixed_typescript_worker_source_is_scanned_as_typescript() {
        let dir = fixture_dir("query-worker");
        std::fs::write(dir.join("worker.js"), "postMessage('ok')").unwrap();
        let id = format!("{}?worker", dir.join("entry.ts").display());
        let scan = scan_new_urls(
            &id,
            "const typed: string = './worker.js'; new Worker(new URL('./worker.js', import.meta.url));",
        );
        assert!(
            !scan.edits.is_empty(),
            "the TypeScript syntax must parse rather than silently skipping this worker scan: {scan:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn local_or_custom_worker_and_url_bindings_are_not_compiler_syntax() {
        let dir = fixture_dir("shadowed-worker-url");
        std::fs::write(dir.join("worker.ts"), "postMessage('ok')").unwrap();
        for source in [
            "const Worker = makeWorker; new Worker(new URL('./worker.ts', import.meta.url));",
            "const URL = makeUrl; new Worker(new URL('./worker.ts', import.meta.url));",
            "import { Worker } from 'custom-workers'; new Worker(new URL('./worker.ts', import.meta.url));",
            "import { URL } from 'custom-url'; new Worker(new URL('./worker.ts', import.meta.url));",
        ] {
            let scan = scan_new_urls(&dir.join("entry.ts").to_string_lossy(), source);
            assert!(
                scan.edits.is_empty(),
                "authored binding was rewritten: {source}"
            );
            assert!(
                scan.worker_refusals.is_empty(),
                "authored Worker was treated as the Node builtin: {source}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn named_node_url_and_worker_import_aliases_remain_compiler_syntax() {
        let dir = fixture_dir("node-worker-url-imports");
        std::fs::write(dir.join("worker.ts"), "postMessage('ok')").unwrap();
        for source in [
            "import { URL } from 'node:url'; new Worker(new URL('./worker.ts', import.meta.url));",
            "import { Worker } from 'node:worker_threads'; new Worker(new URL('./worker.ts', import.meta.url));",
            "import { URL as NodeUrl } from 'node:url'; new Worker(new NodeUrl('./worker.ts', import.meta.url));",
            "import { Worker as NodeWorker } from 'node:worker_threads'; new NodeWorker(new URL('./worker.ts', import.meta.url));",
            concat!(
                "import { URL as NodeUrl } from 'node:url'; ",
                "import { Worker as NodeWorker } from 'node:worker_threads'; ",
                "new NodeWorker(new NodeUrl('./worker.ts', import.meta.url));",
            ),
        ] {
            let scan = scan_new_urls(&dir.join("entry.ts").to_string_lossy(), source);
            assert_eq!(
                scan.edits.len(),
                1,
                "Node builtin was not recognized: {source}"
            );
            assert!(
                scan.edits[0].worker,
                "worker entry was treated as data: {source}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Every spelling that statically reaches Node's `Worker` — through a global
    /// receiver, a binding, a destructure, or the isomorphic `??` guard — must
    /// bundle a worker chunk exactly as the bare global does. A miss here is not
    /// a missed optimization: the entry falls through to the data-asset path and
    /// the compiled binary runs it verbatim on bare Node, with nothing failing
    /// at build time.
    #[test]
    fn indirectly_spelled_workers_bundle_a_chunk_like_the_bare_global() {
        let dir = fixture_dir("indirect-worker-spellings");
        std::fs::write(dir.join("worker.ts"), "postMessage('ok')").unwrap();
        const NODE_WORKER: &str = "import { Worker as NodeWorker } from 'node:worker_threads'; ";
        const CONSTRUCT: &str = "new URL('./worker.ts', import.meta.url));";
        for source in [
            format!("new globalThis.Worker({CONSTRUCT}"),
            format!("new self.Worker({CONSTRUCT}"),
            format!("new window.Worker({CONSTRUCT}"),
            format!("new global.Worker({CONSTRUCT}"),
            format!("new globalThis['Worker']({CONSTRUCT}"),
            format!("const A = Worker; new A({CONSTRUCT}"),
            format!("const A = Worker; const B = A; new B({CONSTRUCT}"),
            format!("const W = globalThis.Worker; new W({CONSTRUCT}"),
            format!("const {{ Worker: W }} = globalThis; new W({CONSTRUCT}"),
            format!("{NODE_WORKER}const N = NodeWorker; new N({CONSTRUCT}"),
            format!("{NODE_WORKER}const W = globalThis.Worker ?? NodeWorker; new W({CONSTRUCT}"),
        ] {
            let scan = scan_new_urls(&dir.join("entry.ts").to_string_lossy(), &source);
            assert_eq!(
                scan.edits.len(),
                1,
                "no worker chunk for: {source} ({scan:?})"
            );
            assert!(
                scan.edits[0].worker,
                "worker entry shipped as data: {source}"
            );
            assert!(
                scan.worker_refusals.is_empty(),
                "{source}: {:?}",
                scan.worker_refusals
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A `Worker`-shaped constructor this pass cannot resolve fails the build.
    /// The alternative is not "leave it alone": an unclaimed URL is embedded as
    /// a data asset, so the real choice is between a build error on a spelling
    /// nobody has to write and a worker that breaks on the user's machine.
    #[test]
    fn unprovable_worker_spellings_are_refused_rather_than_embedded_as_data() {
        let dir = fixture_dir("unprovable-worker-spellings");
        std::fs::write(dir.join("worker.ts"), "postMessage('ok')").unwrap();
        const CONSTRUCT: &str = "new URL('./worker.ts', import.meta.url));";
        for (spelling, source) in [
            (
                "registry.Worker",
                format!("new registry.Worker({CONSTRUCT}"),
            ),
            ("W", format!("const W = registry.Worker; new W({CONSTRUCT}")),
            (
                "W",
                format!("const W = globalThis.Worker ?? makePolyfill(); new W({CONSTRUCT}"),
            ),
            ("W", format!("let W = Worker; W = Other; new W({CONSTRUCT}")),
            (
                "W",
                format!(
                    "const {{ Worker: W }} = await import('node:worker_threads'); new W({CONSTRUCT}"
                ),
            ),
        ] {
            let scan = scan_new_urls(&dir.join("entry.ts").to_string_lossy(), &source);
            assert!(
                scan.edits.is_empty(),
                "an unprovable worker entry was embedded as data: {source} ({scan:?})"
            );
            assert_eq!(
                scan.worker_refusals.len(),
                1,
                "not refused: {source} ({scan:?})"
            );
            assert!(
                scan.worker_refusals[0]
                    .reason
                    .contains(&format!("new {spelling}(")),
                "the refusal must name the spelling as authored: {source} -> {}",
                scan.worker_refusals[0].reason
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The isomorphic guard through a real bundle. A scan-level edit is not
    /// sufficient evidence for this one: what broke was that the spelling never
    /// became a worker ROOT, so the thread started with no preamble — reporting
    /// bare Node's `process.execPath` and no global `Worker` — while the build
    /// stayed green.
    #[test]
    fn an_isomorphic_worker_guard_emits_a_prelude_bearing_worker_root() {
        let dir = fixture_dir("isomorphic-worker-guard");
        let entry = dir.join("entry.ts");
        std::fs::write(
            &entry,
            "import { Worker as NodeWorker } from 'node:worker_threads';\n\
             const W = globalThis.Worker ?? NodeWorker;\n\
             new W(new URL('./worker.ts', import.meta.url));\n",
        )
        .unwrap();
        std::fs::write(dir.join("worker.ts"), "postMessage('ISOMORPHIC_WORKER');\n").unwrap();

        let mut o = opts();
        o.minify = false;
        let res = bundle(&entry, &o).expect("an isomorphic Worker guard must compile");
        assert_eq!(
            res.worker_roots.len(),
            1,
            "the guarded constructor must register a worker root: {:?}",
            res.worker_roots
        );
        let root = &res.worker_roots[0];
        let worker = res
            .files
            .iter()
            .find(|file| file.name == root.chunk)
            .unwrap_or_else(|| {
                panic!("the chunk named by the worker root must be emitted: {root:?}")
            });
        let code = String::from_utf8_lossy(&worker.bytes);
        assert!(
            code.contains("ISOMORPHIC_WORKER"),
            "the worker body must be bundled as code, not copied as data:\n{code}"
        );
        let program = res
            .files
            .iter()
            .find(|file| file.name == res.entry)
            .expect("the program chunk must be emitted");
        let program = String::from_utf8_lossy(&program.bytes);
        assert!(
            program.contains(&root.entry),
            "the program must construct the worker from the emitted entry {}:\n{program}",
            root.entry
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The polyfill gate keeps what the target lacks and drops what it has.
    ///
    /// Both halves matter and neither is sufficient alone: dropping too much ships a
    /// binary referencing a global its runtime does not have, and dropping nothing
    /// is the 195 KB / 18 ms regression this exists to prevent. The unknown-version
    /// case (`--smol`) must keep everything.
    #[test]
    fn native_polyfills_are_stripped_only_for_targets_that_have_them() {
        let src = "\
before
// #region nub:polyfill:temporal
import { Temporal } from \"@js-temporal/polyfill\";
// #endregion
// #region nub:polyfill:urlpattern
import { URLPattern } from \"urlpattern-polyfill/urlpattern\";
// #endregion
after
";
        // Node 26 has both.
        let n26 = strip_native_polyfills(src, Some((26, 0, 0)));
        assert!(
            !n26.contains("js-temporal"),
            "Temporal is native on 26 and must be stripped"
        );
        assert!(
            !n26.contains("urlpattern"),
            "URLPattern is native on 26 and must be stripped"
        );

        // Node 24.1 has URLPattern but NOT Temporal — the case a major-only gate
        // would get wrong in the dangerous direction.
        let n24 = strip_native_polyfills(src, Some((24, 1, 0)));
        assert!(
            n24.contains("js-temporal"),
            "Temporal is NOT native until 26 and must be kept"
        );
        assert!(
            !n24.contains("urlpattern"),
            "URLPattern is native on 24.1 and must be stripped"
        );

        // The support floor has neither.
        let n18 = strip_native_polyfills(src, Some((18, 19, 0)));
        assert!(
            n18.contains("js-temporal") && n18.contains("urlpattern"),
            "a floor-version target must keep every polyfill"
        );

        // `--smol`: the runtime is unknown until launch, so nothing may be dropped.
        let smol = strip_native_polyfills(src, None);
        assert_eq!(
            smol, src,
            "an unknown target must be left exactly as written"
        );

        // Surrounding code always survives — otherwise the assertions above could
        // pass by the stripper eating the whole file.
        for out in [&n26, &n24, &n18] {
            assert!(
                out.contains("before") && out.contains("after"),
                "the stripper must only remove its own regions"
            );
        }
    }

    /// A `.node` specifier is recognised however it was written, and an ordinary
    /// one is not.
    ///
    /// The negative half is what matters. This predicate SUPPRESSES the
    /// indirect-require hint, so a false positive would silently withhold correct
    /// advice from someone with a genuine UMD problem — a worse failure than the
    /// wrong-advice bug it exists to fix.
    #[test]
    fn only_a_dot_node_specifier_is_read_as_a_native_addon() {
        for native in [
            r#"require("@img/sharp-darwin-arm64/sharp.node")"#,
            r#"require('./build/Release/better_sqlite3.node')"#,
            "require(`./prebuilds/darwin-arm64/x.node`)",
        ] {
            assert!(
                specifier_names_native_addon(native),
                "must be read as a native addon: {native}"
            );
        }
        for ordinary in [
            r#"require("lodash")"#,
            r#"require('./config.json')"#,
            // The word appears, but not as the specifier's extension.
            r#"require("node-fetch")"#,
            r#"require("./node/index.js")"#,
        ] {
            assert!(
                !specifier_names_native_addon(ordinary),
                "must NOT be read as a native addon, or the indirect-require hint is \
                 wrongly suppressed: {ordinary}"
            );
        }
    }

    /// A `.node` specifier is recognised, and an ordinary one is not.
    ///
    /// This gate decides which REMEDY an unresolved import gets, and the wrong
    /// remedy is worse than none: without it a native addon collects the indirect
    /// hint, which tells the author to "depend on the package's ESM build or alias
    /// the specifier". For a per-platform `.node` picked at run time there is no
    /// ESM build, and the code lives in somebody else's package — so that advice
    /// sends people to edit a dependency they do not own.
    ///
    /// The negative cases are the real content. Firing on an ordinary specifier
    /// would suppress the indirect hint for the UMD authors it was written for.
    #[test]
    fn native_addon_specifiers_are_told_apart_from_ordinary_ones() {
        for native in [
            r#"require("@img/sharp-darwin-arm64/sharp.node")"#,
            r#"require('./build/Release/better_sqlite3.node')"#,
            "require(`./prebuilds/darwin-arm64/node.napi.node`)",
        ] {
            assert!(
                specifier_names_native_addon(native),
                "must recognise a native addon: {native}"
            );
        }

        for ordinary in [
            r#"require("node:fs")"#,
            r#"require('./worker.js')"#,
            // The word appears, but not as the specifier's extension — this is the
            // case a naive `contains(".node")` would get wrong.
            r#"require("./my.node.helper.js")"#,
            r#"require("nodemailer")"#,
        ] {
            assert!(
                !specifier_names_native_addon(ordinary),
                "must NOT claim an ordinary specifier is a native addon: {ordinary}"
            );
        }
    }

    /// Plug'n'Play is only claimed when it is actually in effect.
    ///
    /// Under PnP every dependency comes back unresolved, so the ordinary hints
    /// describe the wrong problem entirely — and the author has no reason to
    /// suspect their install layout, because the same project runs under
    /// `yarn node`. Both conditions are required: switching a project back to a
    /// node_modules linker can leave `.pnp.cjs` behind, and that project resolves
    /// normally, so keying on the file alone would misdirect on a working tree.
    #[test]
    fn plug_n_play_is_claimed_only_when_it_is_in_effect() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let entry = dir.path().join("src").join("app.mjs");
        std::fs::create_dir_all(entry.parent().expect("a parent")).expect("creating src");

        assert!(!uses_plug_n_play(&entry), "an ordinary project is not PnP");

        std::fs::write(dir.path().join(".pnp.cjs"), "// loader").expect("writing .pnp.cjs");
        assert!(
            uses_plug_n_play(&entry),
            "a .pnp.cjs above the entry with no node_modules is PnP"
        );

        std::fs::create_dir_all(dir.path().join("node_modules")).expect("creating node_modules");
        assert!(
            !uses_plug_n_play(&entry),
            "a leftover .pnp.cjs beside a real node_modules resolves normally"
        );
    }

    /// "Nothing is installed" and "this is Plug'n'Play" both see no node_modules
    /// and have different remedies, so they must never both fire.
    #[test]
    fn nothing_installed_stays_quiet_when_plug_n_play_explains_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let entry = dir.path().join("app.mjs");

        assert!(
            nothing_installed(&entry),
            "no node_modules and no .pnp.cjs is simply not installed"
        );

        std::fs::write(dir.path().join(".pnp.cjs"), "// loader").expect("writing .pnp.cjs");
        assert!(
            !nothing_installed(&entry),
            "Plug'n'Play explains the absence and owns the remedy"
        );

        std::fs::create_dir_all(dir.path().join("node_modules")).expect("creating node_modules");
        assert!(
            !nothing_installed(&entry),
            "a real node_modules means something is installed"
        );
    }

    #[test]
    fn nested_url_and_worker_parameters_do_not_disable_unshadowed_call_sites() {
        let dir = fixture_dir("nested-shadowed-worker-url");
        std::fs::write(dir.join("worker.ts"), "postMessage('ok')").unwrap();
        let id = dir.join("entry.ts").to_string_lossy().into_owned();
        for source in [
            "new Worker(new URL('./worker.ts', import.meta.url)); function custom(URL) { new Worker(new URL('./worker.ts', import.meta.url)); }",
            "new Worker(new URL('./worker.ts', import.meta.url)); function custom(Worker) { new Worker(new URL('./worker.ts', import.meta.url)); }",
        ] {
            let scan = scan_new_urls(&id, source);
            assert_eq!(
                scan.edits.len(),
                1,
                "only the unshadowed constructor pair may be compiler syntax: {source}"
            );
            assert!(scan.edits[0].worker);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
