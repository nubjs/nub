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
//! would just defer the same crash to the user's machine.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use rolldown::plugin::__inner::SharedPluginable;
use rolldown::plugin::{
    HookTransformArgs, HookTransformReturn, HookUsage, Plugin, SharedTransformPluginContext,
};
use rolldown::{BundlerBuilder, BundlerOptions, InputItem};
use rolldown_common::{
    InnerOptions, Output, OutputFormat, Platform, RawMinifyOptions, ResolveOptions, SourceMapType,
    StrOrBytes, TreeshakeOptions, TsConfig,
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
        resolve: Some(ResolveOptions {
            alias: alias_entries(&opts.alias)?,
            condition_names: (!opts.conditions.is_empty()).then(|| opts.conditions.clone()),
            ..Default::default()
        }),
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
            .map_err(|e| anyhow!("rolldown init: {e:?}"))?;
        bundler
            .generate()
            .await
            .map_err(|e| anyhow!("rolldown bundle failed: {e:?}"))
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

/// One `import(<expression>)` the bundler cannot follow.
#[derive(Debug, Clone)]
struct DynamicSite {
    module: String,
    line: usize,
    column: usize,
    snippet: String,
}

/// Collects non-literal dynamic-import sites while the graph loads.
///
/// The `transform` hook is the right seam: it sees every module in the bundle
/// (dependencies included) with its ORIGINAL path and source, so a diagnostic
/// can name the real file and line. Scanning the emitted bundle instead would
/// only ever be able to point at minified output.
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
        let found = scan_dynamic_imports(args.id, args.code);
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

/// Parse `source` and report every `import(...)` whose specifier is not a static
/// string. A parse failure yields nothing: the bundler's own parse error is the
/// better diagnostic, and this pass must never be the thing that fails a build.
fn scan_dynamic_imports(id: &str, source: &str) -> Vec<DynamicSite> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::Expression;
    use oxc_ast_visit::{Visit, walk};
    use oxc_parser::Parser;
    use oxc_span::{SourceType, Span};

    struct Visitor {
        spans: Vec<Span>,
    }
    impl<'a> Visit<'a> for Visitor {
        fn visit_expression(&mut self, expr: &Expression<'a>) {
            if let Expression::ImportExpression(imp) = expr
                && !is_static_specifier(&imp.source)
            {
                self.spans.push(imp.span);
            }
            walk::walk_expression(self, expr);
        }
    }

    /// A template literal with no interpolation is as static as a string
    /// literal — Rolldown resolves both.
    fn is_static_specifier(expr: &Expression<'_>) -> bool {
        match expr {
            Expression::StringLiteral(_) => true,
            Expression::TemplateLiteral(t) => t.expressions.is_empty(),
            _ => false,
        }
    }

    let source_type = SourceType::from_path(id).unwrap_or_else(|_| SourceType::mjs());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    let mut visitor = Visitor { spans: Vec::new() };
    visitor.visit_program(&parsed.program);

    visitor
        .spans
        .into_iter()
        .map(|span| {
            let start = span.start as usize;
            let (line, column) = line_col(source, start);
            DynamicSite {
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

/// Fail the build on any import the bundler could not resolve — the dynamic
/// `import(expr)` sites the scan found, plus Rolldown's own UNRESOLVED_IMPORT
/// warnings for named specifiers that resolved to nothing.
fn reject_unresolved(sites: &[DynamicSite], warnings: &[BuildDiagnostic]) -> Result<()> {
    let mut lines = Vec::new();
    for site in sites {
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

    bail!(
        "{} import{} could not be resolved at build time:\n{}\n\n\
         \x20\x20A compiled binary carries no node_modules, so an unresolved import fails at\n\
         \x20\x20runtime on the machine you ship to. Make the specifier a static string so\n\
         \x20\x20the bundler can follow it.",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" },
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
        let sites = scan_dynamic_imports("/p/app.ts", src);
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

    // Rejection is unconditional — there is no opt-out — so the error has to
    // carry everything needed to fix the source, and point at the only fix.
    #[test]
    fn rejection_names_the_site_and_the_fix() {
        let sites = vec![DynamicSite {
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
