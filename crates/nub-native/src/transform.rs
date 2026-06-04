//! In-process TS/JSX transpiler, mirroring `oxc-transform@0.132.0`'s `transformSync`.
//!
//! This module is a faithful, trimmed mirror of the oxc napi crate's
//! `napi/transform/src/transformer.rs` at tag `crates_v0.132.0` — the exact
//! source the `oxc-transform` npm package is built from. Mirroring it (rather
//! than hand-wiring `Parser → Semantic → Transformer → Codegen`) is what
//! guarantees the emit is **byte-identical** to the npm package nub shipped
//! before this transpiler moved in-process, so existing transpile-cache entries
//! stay valid (no `NUB_VERSION` bump needed). Verified by the transpile-parity
//! corpus gate.
//!
//! Trimmed relative to upstream — nub uses none of these, so they are omitted to
//! shrink the surface and the addon:
//!   * the async `transform` / `TransformTask` (nub only calls the sync path),
//!   * `moduleRunnerTransform*` (Vite-only),
//!   * `isolatedDeclaration*` and the `typescript.declaration` field (nub never
//!     emits `.d.ts`).
//! Everything on the path nub *does* exercise — `get_source_type`, the
//! `TransformOptions` → `oxc::transformer::TransformOptions` mapping, the
//! `Compiler`/`CompilerInterface` driver, codegen + sourcemap, and diagnostic
//! shaping — is reproduced verbatim.

use std::{ops::ControlFlow, path::Path, path::PathBuf};

use napi::Either;
use napi_derive::napi;
use rustc_hash::FxHashMap;

use oxc::{
    CompilerInterface,
    codegen::CodegenReturn,
    diagnostics::OxcDiagnostic,
    transformer::{
        EnvOptions, HelperLoaderMode, HelperLoaderOptions, JsxRuntime, ProposalOptions,
        RewriteExtensionsMode,
    },
    transformer_plugins::{InjectGlobalVariablesConfig, InjectImport, ReplaceGlobalDefinesConfig},
};
use oxc_napi::{OxcError, get_source_type};
use oxc_sourcemap::napi::SourceMap;

/// The result of a transform. Mirror of oxc's `TransformResult`, minus the
/// isolated-declarations fields nub never requests.
#[derive(Default)]
#[napi(object)]
pub struct TransformResult {
    /// The transformed code. Empty string if parsing failed.
    pub code: String,

    /// The source map, populated when `sourcemap: true`.
    pub map: Option<SourceMap>,

    /// Helpers used, e.g. `{ "_objectSpread": "@oxc-project/runtime/helpers/objectSpread2" }`.
    /// @internal
    #[napi(ts_type = "Record<string, string>")]
    pub helpers_used: FxHashMap<String, String>,

    /// Parse and transformation errors. Oxc recovers from common syntax errors,
    /// so `code` may still be populated even when this is non-empty.
    pub errors: Vec<OxcError>,
}

/// Options for transforming a JS/TS file. Mirror of oxc's `TransformOptions`,
/// minus `cwd` is kept (nub may set it), but the isolated-declaration path is
/// dropped from `TypeScriptOptions`.
#[napi(object)]
#[derive(Default)]
pub struct TransformOptions {
    /// Treat the source text as `js`, `jsx`, `ts`, `tsx`, or `dts`.
    #[napi(ts_type = "'js' | 'jsx' | 'ts' | 'tsx' | 'dts'")]
    pub lang: Option<String>,

    /// Treat the source text as `script` or `module` code. Nub passes
    /// `'commonjs'` / `'module'` here (format detected in JS), so a CJS-syntax
    /// `.ts` is honored as commonjs.
    #[napi(ts_type = "'script' | 'module' | 'commonjs' | 'unambiguous' | undefined")]
    pub source_type: Option<String>,

    /// The current working directory, used to resolve relative paths in other options.
    pub cwd: Option<String>,

    /// Enable source map generation. Nub always sets this `true`.
    pub sourcemap: Option<bool>,

    /// Set assumptions in order to produce smaller output.
    pub assumptions: Option<CompilerAssumptions>,

    /// Configure how TypeScript is transformed.
    pub typescript: Option<TypeScriptOptions>,

    /// Configure how TSX and JSX are transformed.
    #[napi(ts_type = "'preserve' | JsxOptions")]
    pub jsx: Option<Either<String, JsxOptions>>,

    /// Sets the target environment for the generated JavaScript. Nub passes
    /// `"es2022"`, which lowers `using`/`await using` to the
    /// `@oxc-project/runtime/helpers/usingCtx` shape while leaving TLA / class
    /// fields / private methods (all ≤ es2022) untouched.
    #[napi(ts_type = "string | Array<string>")]
    pub target: Option<Either<String, Vec<String>>>,

    /// Behaviour for runtime helpers.
    pub helpers: Option<Helpers>,

    /// Define Plugin.
    #[napi(ts_type = "Record<string, string>")]
    pub define: Option<FxHashMap<String, String>>,

    /// Inject Plugin.
    #[napi(ts_type = "Record<string, string | [string, string]>")]
    pub inject: Option<FxHashMap<String, Either<String, Vec<String>>>>,

    /// Decorator plugin.
    pub decorator: Option<DecoratorOptions>,

    /// Third-party plugins to use.
    pub plugins: Option<PluginsOptions>,
}

impl TryFrom<TransformOptions> for oxc::transformer::TransformOptions {
    type Error = String;

    fn try_from(options: TransformOptions) -> Result<Self, Self::Error> {
        let env = match options.target {
            Some(Either::A(s)) => EnvOptions::from_target(&s)?,
            Some(Either::B(list)) => EnvOptions::from_target_list(&list)?,
            _ => EnvOptions::default(),
        };
        Ok(Self {
            cwd: options.cwd.map(PathBuf::from).unwrap_or_default(),
            assumptions: options.assumptions.map(Into::into).unwrap_or_default(),
            typescript: options
                .typescript
                .map(oxc::transformer::TypeScriptOptions::from)
                .unwrap_or_default(),
            decorator: options
                .decorator
                .map(oxc::transformer::DecoratorOptions::from)
                .unwrap_or_default(),
            jsx: match options.jsx {
                Some(Either::A(s)) => {
                    if s == "preserve" {
                        oxc::transformer::JsxOptions::disable()
                    } else {
                        return Err(format!("Invalid jsx option: `{s}`."));
                    }
                }
                Some(Either::B(options)) => oxc::transformer::JsxOptions::from(options),
                None => oxc::transformer::JsxOptions::enable(),
            },
            env,
            proposals: ProposalOptions::default(),
            helper_loader: options
                .helpers
                .map_or_else(HelperLoaderOptions::default, HelperLoaderOptions::from),
            plugins: options
                .plugins
                .map(oxc::transformer::PluginsOptions::from)
                .unwrap_or_default(),
        })
    }
}

#[napi(object)]
#[derive(Default, Debug)]
pub struct CompilerAssumptions {
    pub ignore_function_length: Option<bool>,
    pub no_document_all: Option<bool>,
    pub object_rest_no_symbols: Option<bool>,
    pub pure_getters: Option<bool>,
    /// When using public class fields, assume that they don't shadow any getter
    /// in the current class, its subclasses or superclass — so it's safe to
    /// assign them rather than using `Object.defineProperty`.
    pub set_public_class_fields: Option<bool>,
}

impl From<CompilerAssumptions> for oxc::transformer::CompilerAssumptions {
    fn from(value: CompilerAssumptions) -> Self {
        let ops = oxc::transformer::CompilerAssumptions::default();
        Self {
            ignore_function_length: value
                .ignore_function_length
                .unwrap_or(ops.ignore_function_length),
            no_document_all: value.no_document_all.unwrap_or(ops.no_document_all),
            object_rest_no_symbols: value
                .object_rest_no_symbols
                .unwrap_or(ops.object_rest_no_symbols),
            pure_getters: value.pure_getters.unwrap_or(ops.pure_getters),
            set_public_class_fields: value
                .set_public_class_fields
                .unwrap_or(ops.set_public_class_fields),
            ..ops
        }
    }
}

#[napi(object)]
#[derive(Default)]
pub struct TypeScriptOptions {
    pub jsx_pragma: Option<String>,
    pub jsx_pragma_frag: Option<String>,
    pub only_remove_type_imports: Option<bool>,
    pub allow_namespaces: Option<bool>,
    /// @deprecated — built-in support in oxc; use
    /// `remove_class_fields_without_initializer` instead.
    pub allow_declare_fields: Option<bool>,
    /// When enabled, class fields without initializers are removed (aligns with
    /// TypeScript's `useDefineForClassFields: false`).
    pub remove_class_fields_without_initializer: Option<bool>,
    /// When true, optimize const enums by inlining their values at usage sites.
    pub optimize_const_enums: Option<bool>,
    /// When true, optimize regular (non-const) enums by inlining member accesses.
    pub optimize_enums: Option<bool>,
    /// Rewrite or remove TypeScript import/export declaration extensions.
    #[napi(ts_type = "'rewrite' | 'remove' | boolean")]
    pub rewrite_import_extensions: Option<Either<bool, String>>,
}

impl From<TypeScriptOptions> for oxc::transformer::TypeScriptOptions {
    fn from(options: TypeScriptOptions) -> Self {
        let ops = oxc::transformer::TypeScriptOptions::default();
        oxc::transformer::TypeScriptOptions {
            jsx_pragma: options.jsx_pragma.map(Into::into).unwrap_or(ops.jsx_pragma),
            jsx_pragma_frag: options
                .jsx_pragma_frag
                .map(Into::into)
                .unwrap_or(ops.jsx_pragma_frag),
            only_remove_type_imports: options
                .only_remove_type_imports
                .unwrap_or(ops.only_remove_type_imports),
            allow_namespaces: options.allow_namespaces.unwrap_or(ops.allow_namespaces),
            allow_declare_fields: options
                .allow_declare_fields
                .unwrap_or(ops.allow_declare_fields),
            optimize_const_enums: options
                .optimize_const_enums
                .unwrap_or(ops.optimize_const_enums),
            optimize_enums: options.optimize_enums.unwrap_or(ops.optimize_enums),
            remove_class_fields_without_initializer: options
                .remove_class_fields_without_initializer
                .unwrap_or(ops.remove_class_fields_without_initializer),
            rewrite_import_extensions: options.rewrite_import_extensions.and_then(|value| {
                match value {
                    Either::A(v) => {
                        if v {
                            Some(RewriteExtensionsMode::Rewrite)
                        } else {
                            None
                        }
                    }
                    Either::B(v) => match v.as_str() {
                        "rewrite" => Some(RewriteExtensionsMode::Rewrite),
                        "remove" => Some(RewriteExtensionsMode::Remove),
                        _ => None,
                    },
                }
            }),
        }
    }
}

#[napi(object)]
#[derive(Default)]
pub struct DecoratorOptions {
    /// Enables experimental (legacy, pre-TC39) decorators.
    pub legacy: Option<bool>,
    /// Enables emitting decorator metadata. Only effective when `legacy` is true.
    pub emit_decorator_metadata: Option<bool>,
}

impl From<DecoratorOptions> for oxc::transformer::DecoratorOptions {
    fn from(options: DecoratorOptions) -> Self {
        oxc::transformer::DecoratorOptions {
            legacy: options.legacy.unwrap_or_default(),
            emit_decorator_metadata: options.emit_decorator_metadata.unwrap_or_default(),
        }
    }
}

#[napi(object)]
#[derive(Default)]
pub struct StyledComponentsOptions {
    pub display_name: Option<bool>,
    pub file_name: Option<bool>,
    pub ssr: Option<bool>,
    pub transpile_template_literals: Option<bool>,
    pub minify: Option<bool>,
    pub css_prop: Option<bool>,
    pub pure: Option<bool>,
    pub namespace: Option<String>,
    pub meaningless_file_names: Option<Vec<String>>,
    pub top_level_import_paths: Option<Vec<String>>,
}

#[napi(object)]
#[derive(Default)]
pub struct PluginsOptions {
    pub styled_components: Option<StyledComponentsOptions>,
    pub tagged_template_escape: Option<bool>,
}

impl From<PluginsOptions> for oxc::transformer::PluginsOptions {
    fn from(options: PluginsOptions) -> Self {
        oxc::transformer::PluginsOptions {
            styled_components: options
                .styled_components
                .map(oxc::transformer::StyledComponentsOptions::from),
            tagged_template_transform: options.tagged_template_escape.unwrap_or(false),
        }
    }
}

impl From<StyledComponentsOptions> for oxc::transformer::StyledComponentsOptions {
    fn from(options: StyledComponentsOptions) -> Self {
        let ops = oxc::transformer::StyledComponentsOptions::default();
        oxc::transformer::StyledComponentsOptions {
            display_name: options.display_name.unwrap_or(ops.display_name),
            file_name: options.file_name.unwrap_or(ops.file_name),
            ssr: options.ssr.unwrap_or(ops.ssr),
            transpile_template_literals: options
                .transpile_template_literals
                .unwrap_or(ops.transpile_template_literals),
            minify: options.minify.unwrap_or(ops.minify),
            css_prop: options.css_prop.unwrap_or(ops.css_prop),
            pure: options.pure.unwrap_or(ops.pure),
            namespace: options.namespace,
            meaningless_file_names: options
                .meaningless_file_names
                .unwrap_or(ops.meaningless_file_names),
            top_level_import_paths: options
                .top_level_import_paths
                .unwrap_or(ops.top_level_import_paths),
        }
    }
}

#[napi(object)]
pub struct JsxOptions {
    /// 'automatic' (auto-import factories) or 'classic' (no auto-import).
    #[napi(ts_type = "'classic' | 'automatic'")]
    pub runtime: Option<String>,
    /// Emit dev-specific info (`__source`, `__self`).
    pub development: Option<bool>,
    pub throw_if_namespace: Option<bool>,
    pub pure: Option<bool>,
    /// Replaces the import source. @default 'react'
    pub import_source: Option<String>,
    /// Classic-runtime JSX factory (e.g. `React.createElement`).
    pub pragma: Option<String>,
    /// Classic-runtime fragment factory (e.g. `React.Fragment`).
    pub pragma_frag: Option<String>,
    /// React Fast Refresh.
    pub refresh: Option<Either<bool, ReactRefreshOptions>>,
}

impl From<JsxOptions> for oxc::transformer::JsxOptions {
    fn from(options: JsxOptions) -> Self {
        let ops = oxc::transformer::JsxOptions::default();
        oxc::transformer::JsxOptions {
            runtime: match options.runtime.as_deref() {
                Some("classic") => JsxRuntime::Classic,
                /* "automatic" */ _ => JsxRuntime::Automatic,
            },
            development: options.development.unwrap_or(ops.development),
            throw_if_namespace: options.throw_if_namespace.unwrap_or(ops.throw_if_namespace),
            pure: options.pure.unwrap_or(ops.pure),
            import_source: options.import_source,
            pragma: options.pragma,
            pragma_frag: options.pragma_frag,
            use_built_ins: None,
            use_spread: None,
            refresh: options.refresh.and_then(|value| match value {
                Either::A(b) => b.then(oxc::transformer::ReactRefreshOptions::default),
                Either::B(options) => Some(oxc::transformer::ReactRefreshOptions::from(options)),
            }),
            ..Default::default()
        }
    }
}

#[napi(object)]
pub struct ReactRefreshOptions {
    pub refresh_reg: Option<String>,
    pub refresh_sig: Option<String>,
    pub emit_full_signatures: Option<bool>,
}

impl From<ReactRefreshOptions> for oxc::transformer::ReactRefreshOptions {
    fn from(options: ReactRefreshOptions) -> Self {
        let ops = oxc::transformer::ReactRefreshOptions::default();
        oxc::transformer::ReactRefreshOptions {
            refresh_reg: options.refresh_reg.unwrap_or(ops.refresh_reg),
            refresh_sig: options.refresh_sig.unwrap_or(ops.refresh_sig),
            emit_full_signatures: options
                .emit_full_signatures
                .unwrap_or(ops.emit_full_signatures),
        }
    }
}

#[napi(object)]
#[derive(Default)]
pub struct Helpers {
    pub mode: Option<HelperMode>,
}

#[derive(Default, Clone, Copy)]
#[napi(string_enum)]
pub enum HelperMode {
    /// Runtime mode (default): helpers imported from `@oxc-project/runtime`.
    #[default]
    Runtime,
    /// External mode: helpers accessed from a global `babelHelpers` object.
    External,
}

impl From<Helpers> for HelperLoaderOptions {
    fn from(value: Helpers) -> Self {
        Self {
            mode: value.mode.map(HelperLoaderMode::from).unwrap_or_default(),
            ..HelperLoaderOptions::default()
        }
    }
}

impl From<HelperMode> for HelperLoaderMode {
    fn from(value: HelperMode) -> Self {
        match value {
            HelperMode::Runtime => Self::Runtime,
            HelperMode::External => Self::External,
        }
    }
}

/// The compiler driver — implements `CompilerInterface` exactly as the oxc napi
/// crate's internal `Compiler` does, so the parse→transform→codegen→sourcemap
/// pipeline (and therefore the emit) is identical.
#[derive(Default)]
struct Compiler {
    transform_options: oxc::transformer::TransformOptions,
    sourcemap: bool,

    printed: String,
    printed_sourcemap: Option<SourceMap>,

    define: Option<ReplaceGlobalDefinesConfig>,
    inject: Option<InjectGlobalVariablesConfig>,

    helpers_used: FxHashMap<String, String>,
    errors: Vec<OxcDiagnostic>,
}

impl Compiler {
    fn new(options: Option<TransformOptions>) -> Result<Self, Vec<OxcDiagnostic>> {
        let mut options = options;

        let sourcemap = options
            .as_ref()
            .and_then(|o| o.sourcemap)
            .unwrap_or_default();

        let define = options
            .as_mut()
            .and_then(|options| options.define.take())
            .map(|map| {
                let define = map.into_iter().collect::<Vec<_>>();
                ReplaceGlobalDefinesConfig::new(&define)
            })
            .transpose()?;

        let inject = options
            .as_mut()
            .and_then(|options| options.inject.take())
            .map(|map| {
                map.into_iter()
                    .map(|(local, value)| match value {
                        Either::A(source) => Ok(InjectImport::default_specifier(&source, &local)),
                        Either::B(v) => {
                            if v.len() != 2 {
                                return Err(vec![OxcDiagnostic::error(
                                    "Inject plugin did not receive a tuple [string, string].",
                                )]);
                            }
                            let source = &v[0];
                            Ok(if v[1] == "*" {
                                InjectImport::namespace_specifier(source, &local)
                            } else {
                                InjectImport::named_specifier(source, Some(&v[1]), &local)
                            })
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .map(InjectGlobalVariablesConfig::new);

        let transform_options = match options {
            Some(options) => oxc::transformer::TransformOptions::try_from(options)
                .map_err(|err| vec![OxcDiagnostic::error(err)])?,
            None => oxc::transformer::TransformOptions::default(),
        };

        Ok(Self {
            transform_options,
            sourcemap,
            printed: String::default(),
            printed_sourcemap: None,
            define,
            inject,
            helpers_used: FxHashMap::default(),
            errors: vec![],
        })
    }
}

impl CompilerInterface for Compiler {
    fn handle_errors(&mut self, errors: Vec<OxcDiagnostic>) {
        self.errors.extend(errors);
    }

    fn enable_sourcemap(&self) -> bool {
        self.sourcemap
    }

    fn transform_options(&self) -> Option<&oxc::transformer::TransformOptions> {
        Some(&self.transform_options)
    }

    fn define_options(&self) -> Option<ReplaceGlobalDefinesConfig> {
        self.define.clone()
    }

    fn inject_options(&self) -> Option<InjectGlobalVariablesConfig> {
        self.inject.clone()
    }

    fn after_codegen(&mut self, ret: CodegenReturn) {
        self.printed = ret.code;
        self.printed_sourcemap = ret.map.map(SourceMap::from);
    }

    #[expect(deprecated)]
    fn after_transform(
        &mut self,
        _program: &mut oxc::ast::ast::Program<'_>,
        transformer_return: &mut oxc::transformer::TransformerReturn,
    ) -> ControlFlow<()> {
        self.helpers_used = transformer_return
            .helpers_used
            .drain()
            .map(|(helper, source)| (helper.name().to_string(), source))
            .collect();
        ControlFlow::Continue(())
    }
}

/// Transpile a JavaScript or TypeScript file into a target ECMAScript version.
///
/// Byte-compatible mirror of `oxc-transform@0.132.0`'s `transformSync`. The JS
/// preload consumes `code`, `map`, and `errors`; `helpers_used` is carried for
/// parity but unused.
#[allow(clippy::needless_pass_by_value, clippy::allow_attributes)]
#[napi]
pub fn transform(
    filename: String,
    source_text: String,
    options: Option<TransformOptions>,
) -> TransformResult {
    let source_path = Path::new(&filename);

    let source_type = get_source_type(
        &filename,
        options.as_ref().and_then(|options| options.lang.as_deref()),
        options
            .as_ref()
            .and_then(|options| options.source_type.as_deref()),
    );

    let mut compiler = match Compiler::new(options) {
        Ok(compiler) => compiler,
        Err(errors) => {
            return TransformResult {
                errors: OxcError::from_diagnostics(&filename, &source_text, errors),
                ..Default::default()
            };
        }
    };

    compiler.compile(&source_text, source_type, source_path);

    TransformResult {
        code: compiler.printed,
        map: compiler.printed_sourcemap,
        helpers_used: compiler.helpers_used,
        errors: OxcError::from_diagnostics(&filename, &source_text, compiler.errors),
    }
}
