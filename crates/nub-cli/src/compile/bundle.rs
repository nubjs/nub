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
use rolldown::plugin::__inner::SharedPluginable;
use rolldown::plugin::{
    HookTransformArgs, HookTransformReturn, HookUsage, Plugin, SharedTransformPluginContext,
};
use rolldown::{BundlerBuilder, BundlerOptions, InputItem};
use rolldown_common::bundler_options::{BundlerTransformOptions, Either, JsxOptions};
use rolldown_common::{
    InnerOptions, IsExternal, Output, OutputFormat, Platform, RawMinifyOptions, ResolveOptions,
    SourceMapType, StrOrBytes, TreeshakeOptions, TsConfig,
};
use rolldown_error::{BuildDiagnostic, DiagnosticOptions, EventKind};
use rolldown_utils::indexmap::FxIndexMap;

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

    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some(stem),
            import,
        }]),
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

    let scan = std::sync::Arc::new(DynamicImportScan::default());
    let plugins: Vec<SharedPluginable> = vec![std::sync::Arc::clone(&scan) as SharedPluginable];

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
    })?;

    reject_unresolved(&scan.take(), &output.warnings)?;

    let mut entry = None;
    let mut files = Vec::new();
    let mut maps = Vec::new();
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
            // Only source maps are collected. Nothing configures a loader that
            // emits other assets, and `--include` never routes through the
            // bundler at all — it embeds bytes straight from disk, which is what
            // makes it verbatim.
            Output::Asset(a) if a.filename.ends_with(".map") => maps.push(BundledFile {
                name: a.filename.to_string(),
                bytes: match &a.source {
                    StrOrBytes::Str(s) => s.as_bytes().to_vec(),
                    StrOrBytes::Bytes(b) => b.clone(),
                },
            }),
            Output::Asset(_) => {}
        }
    }
    let entry = entry.context("the bundler emitted no entry chunk")?;
    if files.is_empty() {
        bail!("the bundler produced no chunks");
    }
    // `files` is still chunks-only here — maps are merged in below.
    reject_invalid_chunks(&files)?;

    // A referenced map has to travel with the chunk that names it; an external
    // one must NOT, or `--sourcemap=external` would ship the sources it exists
    // to keep out of the artifact.
    let detached_maps = if opts.sourcemap == SourcemapMode::External {
        std::mem::take(&mut maps)
    } else {
        Vec::new()
    };
    files.extend(maps);

    Ok(BundleResult {
        entry,
        files,
        detached_maps,
    })
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
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
                && !is_static_specifier(&imp.source)
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

    /// A template literal with no interpolation is as static as a string
    /// literal — Rolldown resolves both.
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

    fn is_static_specifier(expr: &Expression<'_>) -> bool {
        static_specifier(expr).is_some()
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
