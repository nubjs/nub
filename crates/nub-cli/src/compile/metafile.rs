//! `--metafile`: a build report in esbuild's metafile schema.
//!
//! The question this answers is "why is my binary this size, and what did each
//! dependency contribute?" — which the compile summary line cannot, since it
//! reports one number for the whole artifact.
//!
//! THE SCHEMA IS ESBUILD'S, DELIBERATELY, not one of nub's own. esbuild's
//! metafile is what the existing analysis tools read (esbuild's own
//! `analyzeMetafile`, esbuild-visualizer, bundle-buddy), so matching it means a
//! nub report drops into a treemap without a converter. The cost is that nub
//! must not populate a field it cannot measure: every field below is either
//! filled from something Rolldown reports or omitted, and the ones nub cannot
//! answer are listed in the docs rather than guessed at.
//!
//! Two measurement facts the docs repeat, because both are visible in the
//! numbers and neither is a defect:
//!
//! - `bytesInOutput` is Rolldown's PRE-minification rendered length for that
//!   module ([`RenderedModule::rendered_length`]), while an output's `bytes` is
//!   the final emitted chunk. Under the default `minify` those two do not
//!   reconcile — the per-input numbers sum high. They line up under
//!   `--no-minify`. Rolldown attributes no post-minify byte count to a module,
//!   and scaling one from the ratio would be a number nothing measured.
//! - An input's `bytes` is the module source as the bundler parsed it (after
//!   any plugin `transform`, before TypeScript is erased), not the file's size
//!   on disk.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{MAIN_SEPARATOR, Path};
use std::sync::{Arc, Mutex};

use rolldown::plugin::{
    HookNoopReturn, HookResolveIdArgs, HookResolveIdReturn, HookUsage, Plugin, PluginContext,
    PluginContextResolveOptions,
};
use rolldown_common::{ExportsKind, ImportKind, ModuleInfo, NormalModule, OutputChunk};

use super::bundle::ExternalImport;
use serde::Serialize;

/// esbuild's import-kind vocabulary, narrowed to the edges Rolldown reports.
///
/// The static kind is decided by the IMPORTER's module format, because
/// `ModuleInfo` carries no per-edge kind: `imported_ids` is a `FxIndexSet`, so it
/// is neither ordered against nor the same length as the import records that do
/// carry one, and the two cannot be zipped.
///
/// This is the FALLBACK. The real kind comes from the resolver hook, which is the
/// only place Rolldown offers it — see [`EdgeWatcher::resolve_id`] — and reaches
/// here as the [`EdgeKinds`] set. The format is consulted only for an edge that
/// hook never observed: a synthetic module, or a resolve that failed.
///
/// It has to be a fallback rather than the answer, because the format is wrong for
/// a module that is not wholly one thing. An ESM module containing an unbound
/// `require()` has an ESM format and a require edge at the same time; that shape is
/// bundler-only — a bare `require` in an ESM file throws under plain Node and
/// survives only because the bundler rewrites it — but it compiles, so the report
/// has to get it right. Pinned by
/// `an_unbound_require_in_an_esm_module_reports_its_real_edge_kind`.
///
/// The obvious source is not available, which is worth recording so nobody
/// re-derives it: `NormalModule::ecma_view::import_records` carry a per-edge kind,
/// but at `module_parsed` time that vector is EMPTY for every module — including
/// one with two static imports — because it is filled during linking.
const REQUIRE_CALL: &str = "require-call";
const IMPORT_STATEMENT: &str = "import-statement";
const DYNAMIC_IMPORT: &str = "dynamic-import";

/// The static-edge kind for a module of this format.
fn static_kind(format: ExportsKind) -> &'static str {
    match format {
        ExportsKind::CommonJs => REQUIRE_CALL,
        _ => IMPORT_STATEMENT,
    }
}

/// A Rollup-convention virtual id (`\0name`) rendered as an esbuild-style
/// namespaced path. The NUL itself must not reach the JSON: a JSON string may
/// carry one, but every consumer that treats a key as a filename breaks on it.
const VIRTUAL_PREFIX: &str = "virtual:";

#[derive(Debug, Serialize)]
pub struct Metafile {
    pub inputs: BTreeMap<String, Input>,
    pub outputs: BTreeMap<String, OutputFile>,
}

#[derive(Debug, Serialize)]
pub struct Input {
    pub bytes: usize,
    pub imports: Vec<Import>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct Import {
    pub path: String,
    pub kind: &'static str,
    /// esbuild omits this when false, and a consumer reads a missing value as
    /// false, so the flag is only ever written to mark a real external.
    #[serde(skip_serializing_if = "is_false")]
    pub external: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputFile {
    pub bytes: usize,
    pub inputs: BTreeMap<String, InputInOutput>,
    pub imports: Vec<Import>,
    pub exports: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_point: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputInOutput {
    pub bytes_in_output: usize,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// What one chunk contributed, captured while Rolldown's output is still typed.
/// Held apart from the emitted bytes because nub rewrites and prunes its output
/// list after the bundler returns, so the final size of a file is only known
/// later — see [`Report::finish`].
#[derive(Debug)]
pub struct ChunkMeta {
    pub inputs: BTreeMap<String, InputInOutput>,
    pub imports: Vec<Import>,
    pub exports: Vec<String>,
    pub entry_point: Option<String>,
}

/// One module as the bundler parsed it.
#[derive(Debug)]
struct Module {
    id: String,
    bytes: usize,
    format: ExportsKind,
    imports: Vec<String>,
    dynamic_imports: Vec<String>,
}

/// Every static source edge the resolver hook saw, as
/// `(importer id, resolved target id, esbuild kind)`.
///
/// A SET of triples rather than a map, because one importer can reach one target
/// by two different edges — `import './dep.js'` beside `require('./dep.js')` — and
/// esbuild's schema reports both. Rolldown's `imported_ids` deduplicates by
/// target, so it can never express that; the observations can.
pub type EdgeKinds = Arc<Mutex<BTreeSet<(String, String, &'static str)>>>;

/// Watches resolution to learn each edge's kind, and nothing else.
///
/// Split from [`Collector`] because the two need OPPOSITE plugin positions. The
/// collector must run LAST so the source length it records is what the bundler
/// parsed; this must run FIRST, because a resolver plugin ahead of it can claim
/// an edge and return before it is ever asked — which is exactly how an
/// `--external` require came to be reported as an import statement.
///
/// It never influences resolution: `resolve_id` always returns `Ok(None)`.
#[derive(Debug)]
pub struct EdgeWatcher {
    edges: EdgeKinds,
}

impl EdgeWatcher {
    pub fn new(edges: EdgeKinds) -> Self {
        Self { edges }
    }
}

impl Plugin for EdgeWatcher {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:metafile-edges")
    }

    /// Observe the import kind, which no later hook reports.
    ///
    /// To learn where the edge lands it re-runs the same resolver chain through
    /// `ctx.resolve` with that edge's own kind, so the answer is the build's
    /// rather than a second opinion. `skip_self` defaults to true, so the call
    /// cannot re-enter this hook.
    fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let importer = args.importer.map(str::to_string);
        let specifier = args.specifier.to_string();
        let kind = args.kind;
        async move {
            if let Some(importer) = importer
                && let Some(rendered) = esbuild_kind(kind)
            {
                let options = PluginContextResolveOptions {
                    import_kind: kind,
                    ..Default::default()
                };
                if let Ok(Ok(resolved)) = ctx
                    .resolve(&specifier, Some(&importer), Some(options))
                    .await
                    && let Ok(mut edges) = self.edges.lock()
                {
                    edges.insert((importer, resolved.id.to_string(), rendered));
                }
            }
            Ok(None)
        }
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId
    }
}

/// Rolldown's kind in esbuild's vocabulary. `None` for an edge esbuild's schema
/// has no static name for — a dynamic import is reported from its own list, and
/// the CSS and `new URL` kinds never reach a nub compile graph.
fn esbuild_kind(kind: ImportKind) -> Option<&'static str> {
    match kind {
        ImportKind::Import => Some(IMPORT_STATEMENT),
        ImportKind::Require => Some(REQUIRE_CALL),
        _ => None,
    }
}

/// Records every module the bundler parses.
///
/// `module_parsed` is the only hook that reports a module's import edges and its
/// module format; the chunk output carries neither. It fires once per module,
/// concurrently, so the collector is a plain append-only list and the ordering
/// is imposed at serialization time by the `BTreeMap`.
#[derive(Debug, Default)]
pub struct Collector {
    modules: Mutex<Vec<Module>>,
}

impl Plugin for Collector {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:metafile")
    }

    fn module_parsed(
        &self,
        _ctx: &PluginContext,
        module_info: Arc<ModuleInfo>,
        _module: &NormalModule,
    ) -> impl std::future::Future<Output = HookNoopReturn> + Send {
        let module = Module {
            id: module_info.id.as_str().to_string(),
            bytes: module_info.code.as_ref().map_or(0, |code| code.len()),
            format: module_info.input_format,
            imports: module_info
                .imported_ids
                .iter()
                .map(|id| id.as_str().to_string())
                .collect(),
            dynamic_imports: module_info
                .dynamically_imported_ids
                .iter()
                .map(|id| id.as_str().to_string())
                .collect(),
        };
        async move {
            if let Ok(mut modules) = self.modules.lock() {
                modules.push(module);
            }
            Ok(())
        }
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ModuleParsed
    }
}

/// The report under construction: modules from the build phase, chunk metadata
/// from the generate phase, and emitted sizes added last.
#[derive(Debug)]
pub struct Report {
    base: Option<std::path::PathBuf>,
    chunks: BTreeMap<String, ChunkMeta>,
}

impl Report {
    /// `base` is what output paths are made relative to — the directory the user
    /// ran the compile from, so the report reads in the same terms as the
    /// command that produced it. A module outside it keeps its absolute path
    /// rather than growing a `../../..` chain.
    pub fn new(base: Option<std::path::PathBuf>) -> Self {
        Self {
            base,
            chunks: BTreeMap::new(),
        }
    }

    /// Record one emitted chunk. Called while iterating Rolldown's output, where
    /// the module list and the import graph are still attached to the chunk.
    ///
    /// `Modules` is two parallel vectors rather than a map, ordered by execution
    /// order; the zip is how a module id is paired with its rendered length.
    pub fn add_chunk(&mut self, chunk: &OutputChunk) {
        let inputs = chunk
            .modules
            .keys
            .iter()
            .zip(chunk.modules.values.iter())
            .map(|(id, module)| {
                (
                    render_id(id.as_str(), self.base.as_deref()),
                    InputInOutput {
                        bytes_in_output: module.rendered_length(),
                    },
                )
            })
            .collect();
        let edges = chunk
            .imports
            .iter()
            .map(|path| (path, IMPORT_STATEMENT))
            .chain(
                chunk
                    .dynamic_imports
                    .iter()
                    .map(|path| (path, DYNAMIC_IMPORT)),
            )
            .map(|(path, kind)| Import {
                path: path.to_string(),
                kind,
                external: false,
            })
            .collect();
        self.chunks.insert(
            chunk.filename.to_string(),
            ChunkMeta {
                inputs,
                imports: edges,
                exports: chunk.exports.iter().map(ToString::to_string).collect(),
                entry_point: chunk
                    .facade_module_id
                    .as_ref()
                    .map(|id| render_id(id.as_str(), self.base.as_deref())),
            },
        );
    }

    /// Fold in the files that actually ship and serialize.
    ///
    /// `emitted` is `(name, size)` for every bundler-produced file left after
    /// nub's own pruning and rewriting, so a chunk recorded by [`Self::add_chunk`]
    /// but dropped later never reaches the report. A file with no chunk metadata
    /// — an emitted asset — gets the empty relations esbuild uses for a file it
    /// copied rather than compiled.
    /// `externals` renames the edges that leave the bundle. An `--external`
    /// request is rewritten to a synthetic id so Rolldown keeps each importer's
    /// copy distinct, and that id is an internal compiler identity — esbuild
    /// reports the SPECIFIER the runtime will resolve, so the report translates
    /// back rather than publishing nub's hash.
    pub fn finish(
        self,
        emitted: &[(&str, usize)],
        modules: &Collector,
        edge_kinds: &EdgeKinds,
        externals: &[ExternalImport],
    ) -> Metafile {
        let observed = edge_kinds.lock().map(|e| e.clone()).unwrap_or_default();
        let external_specifiers: BTreeMap<&str, &str> = externals
            .iter()
            .map(|e| (e.id.as_str(), e.specifier.as_str()))
            .collect();
        let collected = modules
            .modules
            .lock()
            .map(|modules| {
                modules
                    .iter()
                    .map(|module| {
                        (
                            render_id(&module.id, self.base.as_deref()),
                            (
                                module.bytes,
                                module.format,
                                module.imports.clone(),
                                module.dynamic_imports.clone(),
                                // The RAW id as well as the rendered key: the
                                // require-edge set is keyed by raw ids on both
                                // sides, and rendering is lossy (a path under the
                                // base becomes relative).
                                module.id.clone(),
                            ),
                        )
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let inputs = collected
            .iter()
            .map(
                |(path, (bytes, format, imports, dynamic_imports, raw_id))| {
                    // One entry per SOURCE edge, which is esbuild's model and not
                    // Rolldown's: `imported_ids` is deduplicated by target, so a
                    // module that both imports and requires the same file has one
                    // entry there and two here. The observations supply the
                    // multiplicity and the kinds; `imported_ids` stays the floor, so
                    // a target the resolver hook never saw still appears, with the
                    // format as its kind.
                    let edges = imports
                        .iter()
                        .flat_map(|id| {
                            let seen: Vec<&'static str> = observed
                                .iter()
                                .filter(|(from, to, _)| from == raw_id && to == id)
                                .map(|(_, _, kind)| *kind)
                                .collect();
                            if seen.is_empty() {
                                vec![(id, static_kind(*format))]
                            } else {
                                seen.into_iter().map(|kind| (id, kind)).collect()
                            }
                        })
                        .chain(dynamic_imports.iter().map(|id| (id, DYNAMIC_IMPORT)))
                        .map(|(id, kind)| {
                            let rendered = render_id(id, self.base.as_deref());
                            // A target the bundler never parsed is one it never
                            // pulled in: an `--external` package, or a `node:`
                            // builtin. That is exactly esbuild's `external`.
                            let external = !collected.contains_key(&rendered);
                            Import {
                                external,
                                path: external_specifiers
                                    .get(id.as_str())
                                    .map_or(rendered, |specifier| (*specifier).to_string()),
                                kind,
                            }
                        })
                        .collect();
                    (
                        path.clone(),
                        Input {
                            bytes: *bytes,
                            imports: edges,
                            format: match format {
                                ExportsKind::Esm => Some("esm"),
                                ExportsKind::CommonJs => Some("cjs"),
                                ExportsKind::None => None,
                            },
                        },
                    )
                },
            )
            .collect();
        let mut chunks = self.chunks;
        let outputs = emitted
            .iter()
            .map(|(name, bytes)| {
                let meta = chunks.remove(*name);
                (
                    (*name).to_string(),
                    OutputFile {
                        bytes: *bytes,
                        inputs: meta
                            .as_ref()
                            .map(|m| clone_inputs(&m.inputs))
                            .unwrap_or_default(),
                        imports: meta.as_ref().map(clone_imports).unwrap_or_default(),
                        exports: meta.as_ref().map(|m| m.exports.clone()).unwrap_or_default(),
                        entry_point: meta.and_then(|m| m.entry_point),
                    },
                )
            })
            .collect();
        Metafile { inputs, outputs }
    }
}

fn clone_inputs(inputs: &BTreeMap<String, InputInOutput>) -> BTreeMap<String, InputInOutput> {
    inputs
        .iter()
        .map(|(path, entry)| {
            (
                path.clone(),
                InputInOutput {
                    bytes_in_output: entry.bytes_in_output,
                },
            )
        })
        .collect()
}

fn clone_imports(meta: &ChunkMeta) -> Vec<Import> {
    meta.imports
        .iter()
        .map(|edge| Import {
            path: edge.path.clone(),
            kind: edge.kind,
            external: edge.external,
        })
        .collect()
}

/// Render a module id as a report path: `/`-separated, relative to `base` where
/// it sits under it, and namespaced rather than NUL-prefixed when virtual.
fn render_id(id: &str, base: Option<&Path>) -> String {
    if let Some(rest) = id.strip_prefix('\0') {
        return format!("{VIRTUAL_PREFIX}{}", slashes(rest));
    }
    let path = Path::new(id);
    let relative = base
        .filter(|_| path.is_absolute())
        .and_then(|base| path.strip_prefix(base).ok())
        .map(|rest| rest.to_string_lossy().into_owned());
    slashes(&relative.unwrap_or_else(|| id.to_string()))
}

fn slashes(path: &str) -> String {
    if MAIN_SEPARATOR == '/' {
        path.to_string()
    } else {
        path.replace(MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_ids_are_namespaced_rather_than_nul_prefixed() {
        // A raw `\0` would reach the JSON as an actual NUL byte and break every
        // consumer that treats an input key as a path.
        let rendered = render_id("\0nub:compile-root", None);
        assert_eq!(rendered, "virtual:nub:compile-root");
        assert!(
            !rendered.contains('\0'),
            "rendered id kept the NUL: {rendered}"
        );
    }

    #[test]
    fn paths_under_the_base_are_relative_and_others_stay_absolute() {
        let base = Path::new("/proj");
        assert_eq!(render_id("/proj/src/app.ts", Some(base)), "src/app.ts");
        assert_eq!(
            render_id("/elsewhere/lib.ts", Some(base)),
            "/elsewhere/lib.ts",
            "a path outside the base must not be rewritten"
        );
    }

    #[test]
    fn an_unparsed_import_target_is_reported_external() {
        let collector = Collector::default();
        collector.modules.lock().unwrap().push(Module {
            id: "/proj/app.ts".into(),
            bytes: 12,
            format: ExportsKind::Esm,
            imports: vec!["/proj/dep.ts".into(), "node:fs".into()],
            dynamic_imports: Vec::new(),
        });
        collector.modules.lock().unwrap().push(Module {
            id: "/proj/dep.ts".into(),
            bytes: 4,
            format: ExportsKind::Esm,
            imports: Vec::new(),
            dynamic_imports: Vec::new(),
        });
        let report = Report::new(Some("/proj".into()));
        let metafile = report.finish(&[("app.mjs", 9)], &collector, &EdgeKinds::default(), &[]);

        let app = &metafile.inputs["app.ts"];
        assert_eq!(app.bytes, 12);
        assert_eq!(app.format, Some("esm"));
        let external: Vec<_> = app
            .imports
            .iter()
            .map(|edge| (edge.path.as_str(), edge.external))
            .collect();
        assert_eq!(
            external,
            vec![("dep.ts", false), ("node:fs", true)],
            "a target the bundler parsed is internal; one it never saw is external"
        );
        // An emitted file with no chunk metadata still carries the relation
        // fields, empty — esbuild's shape for a file it copied.
        let out = &metafile.outputs["app.mjs"];
        assert_eq!(out.bytes, 9);
        assert!(out.inputs.is_empty() && out.imports.is_empty());
    }

    #[test]
    fn a_pruned_chunk_never_reaches_the_report() {
        let mut report = Report::new(None);
        report.chunks.insert(
            "gone.mjs".into(),
            ChunkMeta {
                inputs: BTreeMap::new(),
                imports: Vec::new(),
                exports: Vec::new(),
                entry_point: None,
            },
        );
        let metafile = report.finish(
            &[("kept.mjs", 3)],
            &Collector::default(),
            &EdgeKinds::default(),
            &[],
        );
        assert!(
            !metafile.outputs.contains_key("gone.mjs"),
            "a chunk nub dropped after bundling must not be reported as shipped"
        );
        assert!(metafile.outputs.contains_key("kept.mjs"));
    }
}
