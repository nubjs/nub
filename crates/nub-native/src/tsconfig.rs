//! N-API wrapper over [`nub_tsconfig`] — the get-tsconfig@4.14.0 port, which lives in
//! its own crate so the CLI can read the same tsconfig before it spawns Node (it turns
//! `compilerOptions.customConditions` into `--conditions=`, a decision this addon runs
//! too late to make). Everything of substance is there; this file exists only because
//! `#[napi(object)]` has to be applied in the cdylib.

use napi_derive::napi;

/// The transform-relevant `compilerOptions` slice surfaced to the JS transpiler.
/// Mirror of [`nub_tsconfig::TsCompilerOptions`].
#[napi(object)]
pub struct TsCompilerOptions {
    pub jsx: Option<String>,
    pub jsx_import_source: Option<String>,
    pub jsx_factory: Option<String>,
    pub jsx_fragment_factory: Option<String>,
    pub experimental_decorators: Option<bool>,
    pub emit_decorator_metadata: Option<bool>,
}

impl From<nub_tsconfig::TsCompilerOptions> for TsCompilerOptions {
    fn from(o: nub_tsconfig::TsCompilerOptions) -> Self {
        Self {
            jsx: o.jsx,
            jsx_import_source: o.jsx_import_source,
            jsx_factory: o.jsx_factory,
            jsx_fragment_factory: o.jsx_fragment_factory,
            experimental_decorators: o.experimental_decorators,
            emit_decorator_metadata: o.emit_decorator_metadata,
        }
    }
}

/// Mirror of [`nub_tsconfig::TsconfigResult`]. `path`/`compilerOptions` are null when
/// no tsconfig was found walking up from `dir` (identical to get-tsconfig → null).
#[napi(object)]
pub struct TsconfigResult {
    pub path: Option<String>,
    pub compiler_options: Option<TsCompilerOptions>,
    pub tsconfig_hash: String,
}

/// Parse + resolve the project's tsconfig: `explicit` when the project names one, else
/// the nearest found walking up from `dir`. Memoized per pair in `nub_tsconfig`.
#[napi]
pub fn load_tsconfig(dir: String, explicit: Option<String>) -> TsconfigResult {
    let r = nub_tsconfig::load_tsconfig(&dir, explicit.as_deref());
    TsconfigResult {
        path: r.path,
        compiler_options: r.compiler_options.map(Into::into),
        tsconfig_hash: r.tsconfig_hash,
    }
}
