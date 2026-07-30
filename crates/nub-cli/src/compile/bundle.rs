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
//! the build rather than warning, and there is no flag to opt out. A compiled
//! artifact carries no `node_modules`, so a specifier that cannot be resolved at
//! build time is a guaranteed `ERR_MODULE_NOT_FOUND` on the deploy machine —
//! with no stack-trace clue and nothing the operator can install to fix it. The
//! only fix is to make the specifier statically analyzable, so an escape hatch
//! would just defer the same crash to the user's machine. `--external` is the
//! one sanctioned way past it, and it works by REMOVING the package from the
//! graph — an external is matched on its raw specifier before resolution, so
//! its source is never loaded and its unanalyzable sites are never scanned.
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
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use oxc_ast::ast::{Expression, NewExpression, Program};
use rolldown::plugin::__inner::SharedPluginable;
use rolldown::plugin::{
    HookTransformArgs, HookTransformOutput, HookTransformOutputMap, HookTransformReturn, HookUsage,
    Plugin, SharedTransformPluginContext,
};
use rolldown::{BundlerBuilder, BundlerOptions, InputItem};
use rolldown_common::bundler_options::{BundlerTransformOptions, Either, JsxOptions};
use rolldown_common::{
    InnerOptions, IsExternal, ModuleType, Output, OutputFormat, Platform, RawMinifyOptions,
    ResolveOptions, SourceMapType, StrOrBytes, TreeshakeOptions, TsConfig,
};
use rolldown_error::{BuildDiagnostic, DiagnosticOptions, EventKind};
use rolldown_utils::indexmap::FxIndexMap;
use rolldown_utils::url::clean_url;

use super::loaders;

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
    /// Explicit tsconfig; `None` keeps Rolldown's auto-discovery.
    pub tsconfig: Option<PathBuf>,
    /// `EXT=TYPE` from `--loader`, applied over nub's defaults. See
    /// [`crate::compile::loaders`].
    pub loaders: Vec<String>,
}

/// One emitted file: a chunk, or a source map that travels with it.
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
}

pub fn bundle(entry_abs: &Path, opts: &BundleOptions) -> Result<BundleResult> {
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

    let loader_plan = loaders::plan(&opts.loaders)?;

    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some(stem),
            import,
        }]),
        module_types: (!loader_plan.module_types.is_empty()).then(|| {
            loader_plan
                .module_types
                .iter()
                .map(|(ext, ty)| (ext.clone(), ty.clone()))
                .collect()
        }),
        cwd: Some(cwd),
        format: Some(OutputFormat::Esm),
        platform: Some(Platform::Node),
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
        external: external_matcher(&opts.external)?,
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
    let new_urls = Arc::new(NewUrlAssets {
        collected: Arc::clone(&collected),
        files: Arc::clone(&files_plugin),
    });
    let plugins: Vec<SharedPluginable> = vec![
        Arc::clone(&scan) as SharedPluginable,
        Arc::clone(&files_plugin) as SharedPluginable,
        Arc::clone(&new_urls) as SharedPluginable,
    ];

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
    let output = output.map_err(|e| {
        let hints = files_plugin.case_hints();
        if hints.is_empty() {
            e
        } else {
            anyhow!("{e}\n\n{}", hints.join("\n"))
        }
    })?;

    reject_unresolved(&scan.take(), &output.warnings)?;

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
    for asset in &output.assets {
        match asset {
            Output::Chunk(c) => {
                if c.is_entry {
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

    Ok(BundleResult {
        entry,
        files,
        detached_maps,
        assets,
    })
}

/// Drop assets that no emitted chunk names.
///
/// Both asset paths record their bytes while the graph is being built — the
/// `file` loader in `load`, [`NewUrlAssets`] in `transform` — which is before
/// anything is tree-shaken. So a reference whose result goes unused still
/// collects its bytes, and without this pass a stray `import splash from
/// "./splash.mp4"` would embed the file in every artifact forever. (This is entry-language-dependent and therefore easy to
/// miss: a `.ts` entry never reaches the hook at all, because the TypeScript
/// transform elides an unused import as possibly-type-only, while the identical
/// `.js` entry does. The payload should not depend on which one you wrote.)
///
/// Reachability is decided by a substring scan for the asset's name, which is
/// exact here rather than approximate: the name carries an 8-hex content hash,
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

// ---- `new URL(…, import.meta.url)` asset embedding -----------------------------

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
#[derive(Debug)]
struct NewUrlAssets {
    collected: Arc<loaders::Assets>,
    /// The `file` loader, consulted only to skip the modules IT synthesized: the
    /// module it emits is itself a `new URL(…, import.meta.url)`, so rescanning
    /// one would re-resolve an already-hashed asset name against the original
    /// file's directory.
    files: Arc<loaders::FilePlugin>,
}

/// One `new URL(<literal>, import.meta.url)` whose file this build can embed.
#[derive(Debug, Clone)]
struct NewUrlEdit {
    /// Byte range of the FIRST argument within the module's own source.
    start: usize,
    end: usize,
    /// The file on the build machine the URL names.
    source: PathBuf,
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
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        // Real JavaScript sources only. A `text`/`json` module's "code" IS the
        // file's content, so parsing markdown as JavaScript and rewriting what
        // happened to look like a URL inside it would corrupt the user's data.
        let scannable = matches!(
            args.module_type,
            ModuleType::Js | ModuleType::Jsx | ModuleType::Ts | ModuleType::Tsx
        ) && !self.files.claims(clean_url(args.id));
        let edits = if scannable {
            scan_new_urls(args.id, args.code)
        } else {
            Vec::new()
        };
        // Cloned only when there is an edit to make — this hook runs on every
        // module in the graph, and almost none of them have one.
        let source = (!edits.is_empty()).then(|| args.code.to_string());
        async move {
            let Some(source) = source else {
                return Ok(None);
            };
            let mut named = Vec::with_capacity(edits.len());
            for edit in edits {
                let bytes = std::fs::read(&edit.source).map_err(|e| {
                    anyhow!("reading {} for new URL(): {e}", edit.source.display())
                })?;
                let name = self.collected.add(&edit.source, bytes)?;
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
fn scan_new_urls(id: &str, source: &str) -> Vec<NewUrlEdit> {
    use oxc_allocator::Allocator;
    use oxc_ast_visit::{Visit, walk};
    use oxc_parser::Parser;
    use oxc_span::{GetSpan, SourceType};

    let Some(dir) = Path::new(clean_url(id)).parent().map(Path::to_path_buf) else {
        return Vec::new();
    };
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(id).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || binds_url(&parsed.program) {
        return Vec::new();
    }

    struct Visitor {
        dir: PathBuf,
        found: Vec<NewUrlEdit>,
    }

    impl<'a> Visit<'a> for Visitor {
        fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
            if let Some(edit) = self.embeddable(it) {
                self.found.push(edit);
            }
            walk::walk_new_expression(self, it);
        }
    }

    impl Visitor {
        fn embeddable(&self, expr: &NewExpression<'_>) -> Option<NewUrlEdit> {
            let Expression::Identifier(callee) = &expr.callee else {
                return None;
            };
            if callee.name != "URL" {
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
            let path = self.dir.join(spec);
            path.is_file().then_some(path)
        }
    }

    let mut visitor = Visitor {
        dir,
        found: Vec::new(),
    };
    visitor.visit_program(&parsed.program);
    visitor.found.sort_by_key(|e| e.start);
    visitor.found
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

/// Does this module define its own `URL` at the top level?
///
/// A module that does is not talking about the global constructor, so rewriting
/// its argument could change what the program means. Value declarations only:
/// an `import { URL } from "node:url"` binds the SAME constructor, and skipping
/// those would silently exclude a perfectly ordinary way to write this.
fn binds_url(program: &Program<'_>) -> bool {
    use oxc_ast::ast::{BindingPattern, Statement};

    program.body.iter().any(|stmt| match stmt {
        Statement::VariableDeclaration(d) => d.declarations.iter().any(|decl| {
            matches!(&decl.id, BindingPattern::BindingIdentifier(id) if id.name == "URL")
        }),
        Statement::ClassDeclaration(c) => c.id.as_ref().is_some_and(|id| id.name == "URL"),
        Statement::FunctionDeclaration(f) => f.id.as_ref().is_some_and(|id| id.name == "URL"),
        _ => false,
    })
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
fn static_specifier(expr: &Expression<'_>) -> Option<String> {
    match expr {
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

/// Does `specifier` name `pkg` or one of its subpaths?
fn is_package_specifier(specifier: &str, pkg: &str) -> bool {
    specifier == pkg
        || (specifier.starts_with(pkg) && specifier.as_bytes().get(pkg.len()) == Some(&b'/'))
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
        let found = scan_unresolvable(args.id, args.code);
        async move {
            if !found.is_empty() {
                if let Ok(mut sites) = self.sites.lock() {
                    sites.extend(found);
                }
            }
            Ok(None)
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
fn scan_unresolvable(id: &str, source: &str) -> Vec<DynamicSite> {
    use oxc_allocator::Allocator;
    use oxc_ast::AstKind;
    use oxc_ast::ast::{BindingPattern, CallExpression, Expression, FormalParameters, Statement};
    use oxc_ast_visit::{Visit, walk};
    use oxc_parser::Parser;
    use oxc_span::{SourceType, Span};

    struct Visitor {
        /// One entry per enclosing function; `true` when that function's own
        /// parameters bind `require`.
        fn_stack: Vec<bool>,
        /// A module-level `var/let/const require = …` (the `createRequire` shape)
        /// shadows for the whole file.
        module_binds_require: bool,
        found: Vec<(SiteKind, Span)>,
    }

    impl Visitor {
        fn require_is_local(&self) -> bool {
            self.module_binds_require || self.fn_stack.iter().any(|shadows| *shadows)
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

    impl<'a> Visit<'a> for Visitor {
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
                self.found.push((SiteKind::Dynamic, imp.span));
            }
            walk::walk_expression(self, expr);
        }

        fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
            if let Some(kind) = self.classify_require(call) {
                self.found.push((kind, call.span));
            }
            walk::walk_call_expression(self, call);
        }
    }

    fn binds_require(params: &FormalParameters<'_>) -> bool {
        params.items.iter().any(
            |p| matches!(&p.pattern, BindingPattern::BindingIdentifier(id) if id.name == "require"),
        )
    }

    let source_type = SourceType::from_path(id).unwrap_or_else(|_| SourceType::mjs());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    let mut visitor = Visitor {
        fn_stack: Vec::new(),
        module_binds_require: parsed.program.body.iter().any(|stmt| {
            matches!(stmt, Statement::VariableDeclaration(d)
            if d.declarations.iter().any(|decl| {
                matches!(&decl.id, BindingPattern::BindingIdentifier(id) if id.name == "require")
            }))
        }),
        found: Vec::new(),
    };
    visitor.visit_program(&parsed.program);

    visitor
        .found
        .into_iter()
        .map(|(kind, span)| {
            let (line, column) = line_col(source, span.start as usize);
            DynamicSite {
                kind,
                module: id.to_string(),
                line,
                column,
                snippet: snippet(source, span.start as usize, span.end as usize),
            }
        })
        .collect()
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
fn reject_unresolved(sites: &[DynamicSite], warnings: &[BuildDiagnostic]) -> Result<()> {
    let mut lines = Vec::new();
    let mut any_indirect = false;
    for site in sites {
        any_indirect |= site.kind == SiteKind::Indirect;
        lines.push(format!(
            "\x20\x20{}:{}:{}\n\x20\x20\x20\x20{}",
            site.module, site.line, site.column, site.snippet
        ));
    }
    let diag_opts = DiagnosticOptions::default();
    for w in warnings {
        if !matches!(w.kind(), EventKind::UnresolvedImport) {
            continue;
        }
        lines.push(format!("\x20\x20{}", w.to_message_with(&diag_opts)));
    }
    if lines.is_empty() {
        return Ok(());
    }

    // The indirect-require fix is a different action from the dynamic-specifier
    // one — the specifier is already static; what is wrong is the module format
    // the resolver picked — so the hint is only shown when it applies.
    let indirect_hint = if any_indirect {
        "\n\x20\x20A require() whose `require` is a local binding (a UMD factory parameter) is\n\
         \x20\x20an ordinary call the bundler cannot rewrite. Depend on the package's ESM\n\
         \x20\x20build, or alias the specifier to it with --alias."
    } else {
        ""
    };
    bail!(
        "{} import{} could not be resolved at build time:\n{}\n\n\
         \x20\x20A compiled binary carries no node_modules, so an unresolved import fails at\n\
         \x20\x20runtime on the machine you ship to. Make the specifier a static string so\n\
         \x20\x20the bundler can follow it.{}",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" },
        lines.join("\n"),
        indirect_hint
    );
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
        // ESM with JSX OFF, which is exactly what Node accepts for the `.js`
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
            tsconfig: None,
            loaders: Vec::new(),
        }
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
        let code = String::from_utf8(res.files[0].bytes.clone()).unwrap();
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
            let code = String::from_utf8(res.files[0].bytes.clone()).unwrap();
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
        let dir =
            std::env::temp_dir().join(format!("nub-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        let code = String::from_utf8(res.files[0].bytes.clone()).unwrap();
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
            (
                "shadowed",
                "class URL { constructor(a, b) { this.a = a; this.b = b; } }\n\
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

    // Rejection is unconditional — there is no opt-out — so the error has to
    // carry everything needed to fix the source, and point at the only fix.
    #[test]
    fn rejection_names_the_site_and_the_fix() {
        let sites = vec![DynamicSite {
            kind: SiteKind::Dynamic,
            module: "/p/src/plugins.ts".into(),
            line: 12,
            column: 20,
            snippet: "import(pluginPath)".into(),
        }];
        let err = reject_unresolved(&sites, &[]).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("/p/src/plugins.ts:12:20"), "got: {msg}");
        assert!(msg.contains("import(pluginPath)"), "got: {msg}");
        assert!(msg.contains("static string"), "got: {msg}");
        assert!(
            !msg.contains("UMD"),
            "the indirect-require hint must not show for a dynamic-specifier site: {msg}"
        );
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
        let code = String::from_utf8(res.files[0].bytes.clone()).unwrap();
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
        let code = String::from_utf8(res.files[0].bytes.clone()).expect("utf8 bundle");
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
}
