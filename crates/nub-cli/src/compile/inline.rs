//! The no-extract payload shape: a compiled artifact that writes nothing.
//!
//! An ordinary compiled binary extracts its app files to `~/.cache/nub/compile-app/…`
//! on first run, because Node has to be handed a `--require` preload and an entry
//! by PATH. A payload that reduces to generated JavaScript needs neither: the
//! launcher passes the bootstrap as `-e` and the bootstrap serves each chunk to
//! `import()` as a `data:` URL read straight out of the executable.
//!
//! That removes the APP's write, which is the only one this module can remove. The
//! DEFAULT shape still extracts its embedded Node to the cache to exec it, so an
//! inline payload alone does not survive a read-only `HOME` — measured, both shapes
//! failing identically on the same fixture. It is `--smol` plus an inline payload
//! that writes nothing at all: no Node to extract, no app to unpack, and it runs
//! under a read-only `HOME` and `TMPDIR` where every other shape refuses to start.
//!
//! Everything here runs AFTER bundling, on the emitted chunks. That is the design
//! constraint, not an implementation detail: whether a payload qualifies is not
//! knowable until the bundler has reported its worker roots, its native islands and
//! its chunk graph, and a build that turns out not to qualify must not pay for a
//! second bundle. So the bundler is configured identically either way and this
//! module rewrites what came out — or declines, and the payload extracts exactly as
//! it always has.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};

use super::AppFiles;
use super::bundle;

/// The virtual directory every inline chunk reports as its own location, matching
/// the string the runtime loader publishes. A `file:` URL rather than a private
/// scheme because the bundled preamble calls `fileURLToPath(import.meta.url)`,
/// which throws on a scheme it does not know. Bun publishes `/$bunfs/root` for the
/// same reason.
///
/// The DRIVE LETTER is what makes it portable, and it is not decoration: Node's
/// `fileURLToPath` has a separate Windows implementation that rejects any path
/// whose second character is not a drive letter followed by `:`, so the obvious
/// `file:///$nub/` threw `ERR_INVALID_FILE_URL_PATH` on Windows — and every
/// payload hits it, because the emitted entry and the CommonJS interop chunk both
/// open with `createRequire(import.meta.url)`, which converts before it does
/// anything else. Measured: every inline fixture died at startup on both Windows
/// legs while the two that decline inline passed. `/N:/$nub/` is an ordinary
/// absolute path on POSIX — a colon is a legal filename character there — so one
/// string satisfies both conversions and the two implementations of this shape
/// cannot drift apart on a platform only one of them was tested on. Bun instead
/// carries a second root (`B:\~BUN\root`) for Windows; the single string is chosen
/// here because a divergence is exactly what shipped this bug.
///
/// Nothing is read through it, so a machine that really has an `N:` drive is
/// unaffected: the chunks are served as `data:` URLs and this string is only an
/// identity.
const VIRTUAL_ROOT: &str = "file:///N:/$nub/";

/// The specifier an inline chunk carries in place of a relative import of another
/// chunk. The loader replaces it with that chunk's `data:` URL before importing.
/// It stays a syntactically valid specifier so a payload the loader somehow failed
/// to rewrite fails as a plain module-not-found rather than a parse error.
const CHUNK_SPECIFIER_PREFIX: &str = "nub-inline:";

/// The placeholder `compile-inline-loader.cjs` carries for the entry chunk's name.
const ENTRY_PLACEHOLDER: &str = "__NUB_INLINE_ENTRY__";

/// Node's `-e` publishes the CommonJS wrapper's five names as GLOBALS, so a
/// compiled program's authored ESM would see `typeof require === "function"` where
/// plain Node — and the extracted shape — give `undefined`. That is not cosmetic:
/// `typeof require !== "undefined"` and `typeof module !== "undefined"` are the two
/// most common CommonJS/ESM feature detections in published packages, and both
/// would take the wrong branch.
///
/// It has to run from inside a MODULE rather than from the `-e` script, because
/// Node re-publishes a lazy global `module` while it brings the ESM loader up — a
/// delete before the first `import()` is undone. Every chunk carries it and the
/// first one evaluated does the work; the rest are five failed lookups.
///
/// `-e` also publishes every BUILTIN MODULE NAME as a lazy global — `fs`, `http`,
/// `node:sqlite`, 46 of them on Node 26 — which a plain script does not have. That
/// is the larger half of the divergence and the one `a-global-parity` catches:
/// measured, an inline artifact carried 44 globals the same file run through
/// `nub <file>` did not.
///
/// The DESCRIPTOR is what identifies an injected name, and it is chosen over
/// comparing the global against the module it would load. `addBuiltinLibsToObject`
/// installs a lazy ACCESSOR and skips any name globalThis already owns, so a
/// preload that assigned its own `globalThis.fs` leaves a DATA property, which is
/// kept. Reading the value instead would be correct too, and was measured to cost
/// far more than it is worth: it instantiates every builtin at startup, and
/// touching the deprecated ones emits four `DeprecationWarning`s — DEP0192 twice,
/// DEP0040 and DEP0025 — on stderr of every artifact that starts.
///
/// `crypto` is the one name a descriptor cannot settle, because it is an accessor
/// either way: Node 18 has no WebCrypto global, so `-e` injects the MODULE and it
/// must go, while from Node 19 the global is a `Crypto` instance that must stay.
/// Its constructor name separates them, and reading that one value loads nothing
/// that warns. `process` and `console` are accessors too and are simply named —
/// both are real globals on every supported version.
///
/// Measured exact on three majors, nothing over-deleted and nothing left behind:
/// 113 = 113 on Node 18, 135 = 135 on Node 24, 141 = 141 on Node 26.
///
/// The name list comes from the bootstrap's own builtin accessor rather than the
/// `module` global, because that global is itself one of the injected names — it
/// is the `node:module` NAMESPACE, not a CJS module instance, so the
/// `module.constructor.builtinModules` that works in a real `-e` script reads
/// `undefined` here. The accessor also survives the deletions below, which take
/// `module` with them.
///
/// One line, so the source-map shift stays exactly one generated line.
const EVAL_GLOBAL_CLEANUP: &str = "{const L=process[Symbol.for(\"nub.compile.bootstrap\")]?.getBuiltin(\"node:module\")?.builtinModules;for(const k of[\"require\",\"module\",\"exports\",\"__filename\",\"__dirname\"])delete globalThis[k];if(L)for(const k of L){if(k===\"process\"||k===\"console\")continue;const d=Object.getOwnPropertyDescriptor(globalThis,k);if(!d||!d.get)continue;if(k===\"crypto\"&&globalThis.crypto?.constructor?.name===\"Crypto\")continue;delete globalThis[k];}}";

/// Why a payload cannot run inline. Reported in the build summary rather than as an
/// error: each of these is a payload that works, just not without extracting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decline {
    /// `--external`, a surviving computed `import()`, or a verbatim payload file —
    /// exactly `Manifest::sealed_module_graph` being false. Each hands the real
    /// module loader a path at run time, and an inline payload has no paths.
    UnsealedGraph,
    /// A statically traced `new Worker(...)`. Its chunk is loaded by URL from inside
    /// the program rather than imported by another chunk, and `new Worker` does not
    /// take a `data:` URL.
    StaticWorker,
    /// A payload file that is not a generated `.mjs` chunk — a `--sourcemap=linked`
    /// map is the one that occurs in practice, and a linked map cannot be fetched
    /// from a `data:` URL anyway.
    NonChunkFile,
    /// The chunks import each other in a cycle. Nesting one `data:` URL inside
    /// another needs a topological order and a cycle has none. Rolldown's manual
    /// CommonJS boundary makes this reachable rather than theoretical.
    CyclicChunks,
    /// The payload reaches the `cluster` builtin. `cluster.fork()` defaults the
    /// child's module to `process.argv[1]`, and an inline artifact publishes the
    /// EXECUTABLE there — so the fork hands a Mach-O/ELF/PE to the real Node, which
    /// parses it as JavaScript and dies. Declining is the answer rather than a
    /// re-entry fixup because an inline payload writes nothing to disk, so no
    /// JavaScript file exists to point the child at; the extracted tree has one.
    ClusterReentry,
    /// The artifact embeds a Node, so it extracts that to the cache to exec it
    /// whatever this module decides — and then serving the app from memory buys
    /// nothing, while costing a measurable amount. Measured on an identical
    /// hello-world payload, macOS arm64, the two shapes byte-for-byte the same
    /// size: 47.2 ms inline against 39.8 ms extracted, +19%. The `-e` script is
    /// parsed with no code cache, the chunks are brotli-decoded in JavaScript and
    /// re-encoded as base64 `data:` URLs, and none of that is cached between runs
    /// the way the extracted tree's compile cache is.
    ///
    /// So the no-write shape is `--smol` plus an inline payload, which is the pair
    /// that runs under a read-only `HOME` — measured, an embedding artifact fails
    /// there identically whether its app inlined or not. Paying 19% for a property
    /// the artifact does not end up having is the wrong default, and the build
    /// summary would have claimed "nothing is written to disk" beside a 28 MB Node
    /// being unpacked.
    EmbeddedNode,
    /// The build asked for a source map this container cannot honour. Which maps
    /// those are is per container and per mode, and the answer is measured rather
    /// than argued — the same throwing `app.ts`, compiled six ways on Node 26.8:
    ///
    /// | `--sourcemap=` | `Mode::Sea` | `Mode::Inline` |
    /// | --- | --- | --- |
    /// | `inline` | `app.ts:2:13` | `app.mjs:3:10743` |
    /// | `linked` | needs a file | needs a file |
    /// | `external` | `app.mjs:2:10743` | `app.mjs:2:10743` |
    ///
    /// `linked` ships a `.map` beside the chunks and names it in a
    /// `sourceMappingURL`, which neither container can resolve; it is caught by
    /// [`Decline::NonChunkFile`] too, and kept here only because this reason reads
    /// better. `external` emits its map beside the EXECUTABLE and puts no reference
    /// in the bundle, so its frames are unmapped in every shape including today's
    /// extracted one — declining it bought a first-run write and nothing else.
    ///
    /// `inline` is the one that splits, and it is why this was worth measuring: the
    /// single-executable container serves each module through a `registerHooks`
    /// `load` hook, and V8 honours the `sourceMappingURL` it finds there; the
    /// no-extract launcher hands its chunks over as `-e` plus `data:` URLs, and it
    /// does not.
    SourceMap,
    /// The payload names `child_process`, so the bootstrap installs its `fork()`
    /// identity fix-up — which sets the fork's executable to the Node the artifact
    /// runs on. A single-executable artifact IS that Node, and Node ignores a
    /// single-executable's `argv[1]`, so such a fork re-runs the application
    /// rather than the requested module, and an application that forks forks
    /// itself without end. Measured, not reasoned: a two-line `fork()` fixture
    /// printed its first line until it was killed.
    ///
    /// A launcher artifact has a real Node path to hand the child, so this is a
    /// [`Mode::Sea`] decline only. [`Self::ClusterReentry`] is the same hazard
    /// reached through `node:cluster` and is refused for both shapes, because
    /// `cluster` re-executes the entry rather than forking a named module.
    ChildProcessReentry,
}

impl Decline {
    /// The clause the build summary prints after "extracts on first run because …".
    pub fn reason(self) -> &'static str {
        match self {
            Self::UnsealedGraph => "it resolves modules at run time",
            Self::StaticWorker => "it carries a worker chunk",
            Self::NonChunkFile => "it carries a file that is not a compiled chunk",
            Self::CyclicChunks => "its chunks import each other in a cycle",
            Self::EmbeddedNode => "it extracts its embedded Node anyway",
            Self::ClusterReentry => "it uses node:cluster, which re-runs the executable",
            Self::SourceMap => "it was built with source maps",
            Self::ChildProcessReentry => "it forks child processes, which re-run the executable",
        }
    }
}

/// What the caller knows about the payload that this module cannot see for itself.
pub struct Inputs<'a> {
    /// `Manifest::sealed_module_graph`, computed by the caller.
    pub sealed_module_graph: bool,
    /// Statically traced worker roots, plus the public wrappers written for them.
    pub worker_roots: usize,
    pub worker_wrappers: usize,
    /// Which source map the build asked for. The MODE rather than a yes/no,
    /// because the three differ in what has to be reachable at run time and only
    /// one of them is out of reach here — see [`Decline::SourceMap`].
    pub sourcemap: bundle::SourcemapMode,
    /// Whether the artifact carries a Node of its own — everything but `--smol`.
    pub embeds_node: bool,
    /// `BundleResult::app_computes_module_specifier`.
    pub computes_module_specifier: bool,
    /// The entry chunk's payload name.
    pub entry: &'a str,
}

/// Which no-extract container is asking. The eligibility rules are ALMOST the
/// same, and the two differences are both about what a chunk's identity is.
///
/// The inline shape serves each chunk as a `data:` URL, which has no base: a
/// relative specifier cannot resolve against one, so the compiler substitutes
/// every cross-chunk specifier ahead of time and needs a topological order to do
/// it — hence [`Decline::CyclicChunks`]. A SEA serves the same chunks from
/// `module.registerHooks` at ordinary `file:` URLs, where relative specifiers
/// resolve the way they do in the extracted tree and a cycle is just an ESM
/// cycle. And [`Decline::EmbeddedNode`] is the reverse: it exists because an
/// artifact that unpacks a Node to the cache gains nothing from serving its app
/// from memory. A SEA unpacks nothing, so the whole premise is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `-e` plus `data:` URLs, read back out of the executable's own tail.
    Inline,
    /// A Node single-executable blob, with the chunks as assets.
    Sea,
}

/// What [`rewrite`] decided. The files come back either way: a declined payload
/// extracts exactly as it always has, so a decline is a report, not a failure.
pub enum Rewritten {
    Inline(AppFiles),
    Extract(AppFiles, Decline),
}

/// The root manifest `assemble_app` synthesizes for the extracted tree. Spelled
/// here rather than shared, because the two uses are independent: that one exists
/// so a walk-up finds a package boundary on disk, and this one drops it again
/// because an inline payload has no disk to walk.
const ROOT_MANIFEST_NAME: &str = "package.json";

/// Rewrite `files` for inline launch, or say why the payload has to extract.
///
/// On success every `.mjs` chunk has been given its virtual identity and had its
/// cross-chunk specifiers replaced, and the bootstrap entry carries the loader that
/// reads them back out of the executable. The caller compresses the result with
/// brotli and sets `Manifest::inline_app`.
pub fn rewrite(files: AppFiles, inputs: &Inputs<'_>) -> Result<Rewritten> {
    match classify(&files, inputs, Mode::Inline)? {
        Err(decline) => Ok(Rewritten::Extract(files, decline)),
        Ok(chunk_names) => {
            let loader = loader_source(inputs.entry)?;
            let bootstrap_name = nub_core::compile::COMPILE_BOOTSTRAP_NAME;
            let rewritten = files
                .into_iter()
                // Dropped, not rewritten: nothing resolves through it once the
                // payload never lands on disk, and the arm below would otherwise
                // hand its JSON to the chunk rewriter as if it were a module.
                .filter(|file| file.name != ROOT_MANIFEST_NAME)
                .map(|mut file| {
                    if file.name == bootstrap_name {
                        // Wrapped, because this pair is handed to Node as `-e`, where
                        // a top-level function declaration becomes a GLOBAL. The
                        // bootstrap has one — `installCompiledForkIdentity` — and it
                        // showed up in `a-global-parity`'s diff beside the builtins
                        // `EVAL_GLOBAL_CLEANUP` removes. Under `--require`, which is
                        // how the extracted shape loads the same file, CJS module
                        // scope already contained it, so only this path needs it.
                        // Nothing here reads top-level `this` or exports anything;
                        // the bootstrap publishes through `process[Symbol.for(…)]`,
                        // which an arrow body reaches unchanged.
                        let mut wrapped = b"(()=>{\n".to_vec();
                        wrapped.append(&mut file.bytes);
                        wrapped.push(b'\n');
                        wrapped.extend_from_slice(loader.as_bytes());
                        wrapped.extend_from_slice(b"\n})();\n");
                        file.bytes = wrapped;
                        return Ok(file);
                    }
                    let source = String::from_utf8(std::mem::take(&mut file.bytes))
                        .with_context(|| format!("reading the emitted chunk {}", file.name))?;
                    file.bytes = rewrite_chunk(&source, &file.name, &chunk_names).into_bytes();
                    Ok(file)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Rewritten::Inline(rewritten))
        }
    }
}

/// The eligibility half, kept separate so the borrow of `files` ends before the
/// rewrite consumes it. Returns the chunk names on success.
pub fn classify(
    files: &AppFiles,
    inputs: &Inputs<'_>,
    mode: Mode,
) -> Result<Result<BTreeSet<String>, Decline>> {
    if !inputs.sealed_module_graph {
        return Ok(Err(Decline::UnsealedGraph));
    }
    if inputs.worker_roots != 0 || inputs.worker_wrappers != 0 {
        return Ok(Err(Decline::StaticWorker));
    }
    // `Linked` in either container, and `Inline` in the one that cannot apply it.
    // The other four cells are reachable; the table behind that is on
    // [`Decline::SourceMap`].
    if matches!(inputs.sourcemap, bundle::SourcemapMode::Linked)
        || (mode == Mode::Inline && matches!(inputs.sourcemap, bundle::SourcemapMode::Inline))
    {
        return Ok(Err(Decline::SourceMap));
    }
    // Cheap and last of the caller-supplied checks, so a payload that could never
    // inline still reports the reason it could never inline rather than this one.
    if mode == Mode::Inline && inputs.embeds_node {
        return Ok(Err(Decline::EmbeddedNode));
    }
    let bootstrap_name = nub_core::compile::COMPILE_BOOTSTRAP_NAME;
    // Every file is the bootstrap, a chunk, or the root manifest `assemble_app`
    // synthesizes. `--include`s, emitted assets and native islands are already
    // excluded by the sealed-graph check above; what this catches is the compiler's
    // OWN non-chunk output, which today means a `--sourcemap=linked` map travelling
    // beside the bundle.
    //
    // The manifest is exempt because it exists only for a walk-up through the
    // EXTRACTED tree, and an inline payload is never on disk for anything to walk.
    // A user-supplied `package.json` cannot reach here: it arrives via `--include`,
    // which unseals the graph and is declined above. So dropping it below loses
    // nothing, and keeping it would decline every build — the manifest is
    // unconditional, so this check rejected 100% of payloads once it landed.
    if files.iter().any(|file| {
        file.name != bootstrap_name
            && file.name != ROOT_MANIFEST_NAME
            && !file.name.ends_with(".mjs")
    }) {
        return Ok(Err(Decline::NonChunkFile));
    }
    let chunk_names: BTreeSet<String> = files
        .iter()
        .filter(|file| file.name.ends_with(".mjs"))
        .map(|file| file.name.clone())
        .collect();
    if !chunk_names.contains(inputs.entry) {
        bail!(
            "the compiled entry {:?} is not among the emitted chunks",
            inputs.entry
        );
    }

    let any_chunk_reaches = |is_target: fn(&str) -> bool| {
        files
            .iter()
            .filter(|file| file.name.ends_with(".mjs"))
            .any(|file| {
                std::str::from_utf8(&file.bytes)
                    .is_ok_and(|source| reaches_builtin(source, is_target))
            })
    };

    if any_chunk_reaches(is_cluster) {
        return Ok(Err(Decline::ClusterReentry));
    }

    // A computed specifier is checked FIRST because it is the case the scan cannot
    // answer: a module that builds its specifier resolves something this pass
    // never sees, so the only safe reading is that it might be `child_process`.
    if mode == Mode::Sea
        && (inputs.computes_module_specifier || any_chunk_reaches(is_child_process))
    {
        return Ok(Err(Decline::ChildProcessReentry));
    }

    // The chunk graph, read the way the loader will read it: a chunk depends on
    // another when it names it in a relative specifier. Textual on both sides on
    // purpose — the loader has no bundler metadata, so a graph derived from anything
    // else would not be the graph it walks.
    let mut edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for file in files.iter().filter(|f| f.name.ends_with(".mjs")) {
        let source = std::str::from_utf8(&file.bytes)
            .with_context(|| format!("reading the emitted chunk {}", file.name))?;
        let deps = chunk_names
            .iter()
            .map(String::as_str)
            .filter(|dep| *dep != file.name && imports_chunk(source, dep))
            .collect();
        edges.insert(file.name.as_str(), deps);
    }
    if mode == Mode::Inline && has_cycle(&edges) {
        return Ok(Err(Decline::CyclicChunks));
    }
    Ok(Ok(chunk_names))
}

/// `node:cluster` under both spellings the emitted bundle can carry.
///
/// The authored ESM `node:cluster` keeps its prefix, and the interop shim Rolldown
/// writes around it is a bare `require("cluster")`.
fn is_cluster(specifier: &str) -> bool {
    matches!(specifier, "cluster" | "node:cluster")
}

/// `node:child_process`, likewise.
fn is_child_process(specifier: &str) -> bool {
    matches!(specifier, "child_process" | "node:child_process")
}

/// Whether a chunk names a builtin `is_target` accepts as a module specifier.
///
/// Import syntax is not the only route — a chunk can also take the module from
/// `createRequire(import.meta.url)(…)`, from `process.getBuiltinModule(…)`, or from
/// the renamed `__require` a bundler emits — and every one of them ends in the same
/// re-entry crash, so the callee shapes in `resolves_builtin` count too. AST rather
/// than a substring search because both builtins this is asked about are spelled
/// with ordinary English words: a payload logging "cluster failed", or one whose
/// dependency ships a `clusterApiUrl` export, resolves nothing and must still take
/// the no-extract container. A chunk that fails to parse yields false: the bundler
/// already emitted it, so a parse failure here is this pass being wrong about the
/// syntax, and it must never be what fails or degrades a build.
///
/// What it cannot see is a COMPUTED specifier, and nothing reading the emitted
/// chunks can. `Inputs::computes_module_specifier` carries that case, from a scan
/// of the application's own modules before they were bundled.
fn reaches_builtin(source: &str, is_target: fn(&str) -> bool) -> bool {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{
        AssignmentExpression, AssignmentTarget, CallExpression, ExportAllDeclaration,
        ExportNamedDeclaration, Expression, ImportDeclaration, MemberExpression,
        VariableDeclarator,
    };
    use oxc_ast_visit::{Visit, walk};
    use oxc_parser::Parser;
    use oxc_span::SourceType;
    use std::collections::BTreeSet;

    /// A no-substitution template literal is as static as a string literal, and
    /// the minifier emits both.
    fn literal_specifier<'a>(expr: &'a Expression<'_>) -> Option<&'a str> {
        match expr.get_inner_expression() {
            Expression::StringLiteral(s) => Some(s.value.as_str()),
            Expression::TemplateLiteral(t) if t.expressions.is_empty() => t
                .quasis
                .first()
                .map(|q| q.value.cooked.as_ref().unwrap_or(&q.value.raw).as_str()),
            _ => None,
        }
    }

    /// Whether an identifier NAMES a require.
    ///
    /// Case-insensitive and a substring, which is wider than it looks and has to
    /// be. The suffix test this replaced missed nub's OWN emitted helper: a
    /// CommonJS wrapper opens `const require = __nubCjsRequire`, and
    /// `__nubCjsRequire` ends in a capital R, so `ends_with("require")` was false
    /// for every CommonJS payload the compiler produces. Minification then renames
    /// the wrapper's own `require` binding, leaving `let e = __nubCjsRequire` and a
    /// call site reading `e("node:cluster")` with nothing left to match on. A
    /// suffix test also misses `require$1`, which the comment it carried claimed
    /// it caught.
    ///
    /// The cost of the width is a payload whose `requireAuth("cluster")` resolves
    /// nothing and extracts anyway. That is the direction this pass is documented
    /// to err in, and the call must additionally pass one of four exact builtin
    /// specifiers as its first argument.
    fn names_require(name: &str) -> bool {
        name.to_ascii_lowercase().contains("require")
    }

    /// Whether a callee is a plausible way to obtain a builtin module by name.
    ///
    /// Targeted on purpose: an unnecessary decline costs a real optimization, so
    /// this matches the shapes that hand back the module — a name containing
    /// `require` under any casing, a name bound to one of those, the three property
    /// names that stand in for one, and the require a `createRequire` call returns —
    /// and leaves `logger.info("cluster")` alone.
    fn resolves_builtin(callee: &Expression<'_>, aliases: &BTreeSet<String>) -> bool {
        match callee.get_inner_expression() {
            // `aliases` is what closes the one shape the name test cannot reach:
            // `const load = require` rebinds it under a name that says nothing, and
            // the emitted chunk then reads `load("child_process")`. See
            // [`require_aliases`].
            Expression::Identifier(id) => {
                names_require(&id.name) || aliases.contains(id.name.as_str())
            }
            // `createRequire(import.meta.url)("cluster")`: the require is the value a
            // call produced, so the callee is itself a call. ANY call, deliberately.
            // Requiring the inner callee to name `createRequire` was tried and
            // reverted: minification is on by default and renames the import, so the
            // emitted chunk reads `i(import.meta.url)(`cluster`)` and the narrower
            // rule let a real cluster payload inline — measured, and it crashes at
            // `fork()`. A local rule cannot do better: the emitted binding is
            // imported from the runtime chunk, so the name `createRequire` is not in
            // this file to match on, minified or not.
            //
            // Both directions can break a build, so the choice is which failure to
            // take. Over-declining `makeLogger()("cluster")` extracts a payload that
            // did not need to — which is not merely a lost optimization, since a
            // `--smol` artifact built FOR a read-only HOME then has nowhere to
            // extract to and will not start there. Under-declining ships a binary
            // whose `cluster.fork()` hands the real Node a Mach-O/ELF/PE and dies,
            // for every user, in every environment. The second is unconditional and
            // the first is confined to one deployment shape, so this over-declines.
            Expression::CallExpression(_) => true,
            other => other
                .as_member_expression()
                .and_then(MemberExpression::static_property_name)
                .is_some_and(|property| {
                    matches!(property, "require" | "getBuiltinModule" | "createRequire")
                }),
        }
    }

    struct Visitor<'s> {
        is_target: fn(&str) -> bool,
        aliases: &'s BTreeSet<String>,
        found: bool,
    }

    impl<'a> Visit<'a> for Visitor<'_> {
        fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
            self.found |= (self.is_target)(it.source.value.as_str());
            walk::walk_import_declaration(self, it);
        }

        fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
            if let Some(source) = &it.source {
                self.found |= (self.is_target)(source.value.as_str());
            }
            walk::walk_export_named_declaration(self, it);
        }

        fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
            self.found |= (self.is_target)(it.source.value.as_str());
            walk::walk_export_all_declaration(self, it);
        }

        fn visit_expression(&mut self, expr: &Expression<'a>) {
            if let Expression::ImportExpression(import) = expr
                && let Some(specifier) = literal_specifier(&import.source)
            {
                self.found |= (self.is_target)(specifier);
            }
            walk::walk_expression(self, expr);
        }

        fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
            if resolves_builtin(&call.callee, self.aliases)
                && let Some(argument) = call.arguments.first().and_then(|a| a.as_expression())
                && let Some(specifier) = literal_specifier(argument)
            {
                self.found |= (self.is_target)(specifier);
            }
            walk::walk_call_expression(self, call);
        }
    }

    /// Every local name this chunk binds to a require, directly or through another
    /// such name.
    ///
    /// `resolves_builtin` recognizes a require by its NAME, which covers every
    /// renaming that keeps the word — `__require`, `require$1`, `__nubCjsRequire`.
    /// An alias keeps nothing: `const load = require` produces a call
    /// site reading `load("child_process")`, where neither the callee nor anything
    /// else in the expression says what `load` is. That shape reached the emitted
    /// chunk from ordinary authored CommonJS, and before the scan replaced a
    /// substring search it was caught by accident, because the specifier's own
    /// letters were in the file.
    ///
    /// One pass in source order is enough for the chained case. A binding can only
    /// alias a name declared before it — the reverse is a temporal-dead-zone error
    /// at run time — so `const a = require; const b = a;` adds `a` before `b` is
    /// reached, and nothing needs a second pass.
    fn require_aliases(program: &oxc_ast::ast::Program<'_>) -> BTreeSet<String> {
        struct Collect {
            names: BTreeSet<String>,
        }

        impl Collect {
            /// Whether an initializer hands back a require: the builtin under any
            /// name the bundler gave it, one of the property spellings, or a name
            /// already known to be one.
            fn is_require(&self, expr: &Expression<'_>) -> bool {
                match expr.get_inner_expression() {
                    Expression::Identifier(id) => {
                        names_require(&id.name) || self.names.contains(id.name.as_str())
                    }
                    other => other
                        .as_member_expression()
                        .and_then(MemberExpression::static_property_name)
                        .is_some_and(|property| matches!(property, "require" | "createRequire")),
                }
            }
        }

        impl<'a> Visit<'a> for Collect {
            fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
                if let Some(init) = &it.init
                    && self.is_require(init)
                    && let Some(name) = it.id.get_identifier_name()
                {
                    self.names.insert(name.to_string());
                }
                walk::walk_variable_declarator(self, it);
            }

            fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
                if self.is_require(&it.right)
                    && let AssignmentTarget::AssignmentTargetIdentifier(id) = &it.left
                {
                    self.names.insert(id.name.to_string());
                }
                walk::walk_assignment_expression(self, it);
            }
        }

        let mut collect = Collect {
            names: BTreeSet::new(),
        };
        collect.visit_program(program);
        collect.names
    }

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();
    if parsed.panicked {
        return false;
    }
    let aliases = require_aliases(&parsed.program);
    let mut visitor = Visitor {
        is_target,
        aliases: &aliases,
        found: false,
    };
    visitor.visit_program(&parsed.program);
    visitor.found
}

/// Every spelling Rolldown can emit for a sibling chunk's specifier.
///
/// Three, not one, and the third is the one that bites: a STATIC import's specifier
/// must be a string literal, so it keeps its double quotes, but a DYNAMIC
/// `import()` takes an expression and the minifier rewrites its argument to a
/// template literal. A rewrite that only knew the double-quoted form left
/// `import(`./lazy-BPasMaRY.mjs`)` untouched, and the artifact died at run time on
/// `ERR_UNSUPPORTED_RESOLVE_REQUEST` — a relative specifier has nothing to resolve
/// against inside a `data:` URL.
fn relative_specifiers(name: &str) -> [String; 3] {
    [
        format!("\"./{name}\""),
        format!("'./{name}'"),
        format!("`./{name}`"),
    ]
}

/// Whether `source` imports the chunk `name`, in any of those spellings.
fn imports_chunk(source: &str, name: &str) -> bool {
    relative_specifiers(name)
        .iter()
        .any(|spelling| source.contains(spelling))
}

/// Iterative three-colour depth-first search. Iterative rather than recursive
/// because the graph is derived from user code, and a payload has a handful of
/// chunks so nothing here is worth optimizing.
fn has_cycle(edges: &BTreeMap<&str, Vec<&str>>) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Open,
        Done,
    }
    let mut marks: BTreeMap<&str, Mark> = BTreeMap::new();
    for root in edges.keys().copied() {
        if marks.contains_key(root) {
            continue;
        }
        marks.insert(root, Mark::Open);
        let mut stack = vec![(root, 0usize)];
        while let Some((node, index)) = stack.pop() {
            let deps = edges.get(node).map(Vec::as_slice).unwrap_or_default();
            match deps.get(index).copied() {
                Some(dep) => {
                    stack.push((node, index + 1));
                    match marks.get(dep) {
                        Some(Mark::Open) => return true,
                        Some(Mark::Done) => {}
                        None => {
                            marks.insert(dep, Mark::Open);
                            stack.push((dep, 0));
                        }
                    }
                }
                None => {
                    marks.insert(node, Mark::Done);
                }
            }
        }
    }
    false
}

/// Give one chunk its virtual identity and point its cross-chunk imports at the
/// loader's placeholders.
///
/// The `import.meta.url` assignment is what keeps the rest of a compiled artifact
/// working from a `data:` URL: the bundled CommonJS chunk builds its `require` with
/// `createRequire(import.meta.url)`, and the `__filename`/`__dirname` globals
/// spliced into CommonJS modules are derived from it. Left alone, all three would
/// see the base64 blob — `createRequire` throws on one outright.
///
/// Prefixed as a WHOLE LINE, so every column in the chunk stays where the bundler
/// put it. Prefixing without a newline would have been simpler and is wrong:
/// minified output is one long line, so it would displace every column in the
/// program — and the emitted line numbers are what a stack frame reports.
fn rewrite_chunk(source: &str, name: &str, chunks: &BTreeSet<String>) -> String {
    let mut out = String::with_capacity(source.len() + 256);
    out.push_str(EVAL_GLOBAL_CLEANUP);
    out.push_str("import.meta.url = \"");
    out.push_str(VIRTUAL_ROOT);
    out.push_str(name);
    out.push_str("\";\n");
    let mut code = source.to_string();
    for dep in chunks.iter().filter(|dep| dep.as_str() != name) {
        // Always DOUBLE-quoted on the way out, whatever it was on the way in, so the
        // loader has one spelling to look for.
        let placeholder = format!("\"{CHUNK_SPECIFIER_PREFIX}{dep}\"");
        for spelling in relative_specifiers(dep) {
            code = code.replace(&spelling, &placeholder);
        }
    }
    out.push_str(&code);
    out
}

/// The `-e` script's second half, with the entry chunk's name substituted in.
fn loader_source(entry: &str) -> Result<String> {
    let source = bundle::compile_runtime_file("compile-inline-loader.cjs")?;
    let source = String::from_utf8(source).context("the compile inline loader is not utf-8")?;
    // A payload name is validated against the target's path rules long before this,
    // so it cannot carry a quote or a backslash — but it is substituted into a
    // JavaScript string literal, so it is escaped rather than trusted.
    let escaped = serde_json::to_string(entry).expect("a payload name serializes");
    Ok(source.replace(&format!("\"{ENTRY_PLACEHOLDER}\""), &escaped))
}

#[cfg(test)]
mod tests {
    use super::*;
    // `AppFiles` is the alias this module works in; the element type is only named
    // here, to build payloads by hand.
    use nub_core::compile::AppFile;

    fn chunks(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn sealed(entry: &str) -> Inputs<'_> {
        Inputs {
            sealed_module_graph: true,
            worker_roots: 0,
            worker_wrappers: 0,
            sourcemap: bundle::SourcemapMode::None,
            embeds_node: false,
            computes_module_specifier: false,
            entry,
        }
    }

    /// Classify a one-chunk payload that is inlinable but for `source` itself.
    fn decline_of(source: &str) -> Option<Decline> {
        let files = vec![
            AppFile::plain(
                nub_core::compile::COMPILE_BOOTSTRAP_NAME.to_string(),
                b"// bootstrap\n".to_vec(),
            ),
            AppFile::plain("main.mjs".to_string(), source.as_bytes().to_vec()),
        ];
        match rewrite(files, &sealed("main.mjs")).expect("classification succeeds") {
            Rewritten::Extract(_, why) => Some(why),
            Rewritten::Inline(_) => None,
        }
    }

    /// Classify the same one-chunk payload for a single-executable container.
    fn sea_decline_of(source: &str) -> Option<Decline> {
        sea_decline_with(source, false)
    }

    /// As above, with the caller's "this payload can compute a specifier" signal.
    fn sea_decline_with(source: &str, computes_module_specifier: bool) -> Option<Decline> {
        let files = vec![
            AppFile::plain(
                nub_core::compile::COMPILE_BOOTSTRAP_NAME.to_string(),
                b"// bootstrap\n".to_vec(),
            ),
            AppFile::plain("main.mjs".to_string(), source.as_bytes().to_vec()),
        ];
        let mut inputs = sealed("main.mjs");
        inputs.computes_module_specifier = computes_module_specifier;
        classify(&files, &inputs, Mode::Sea)
            .expect("classification succeeds")
            .err()
    }

    /// Which source maps each container can honour — one cell per measured row of
    /// the table on [`Decline::SourceMap`].
    ///
    /// The two `Inline` cells are the whole point. The same mode is reachable from
    /// the single-executable container and not from the no-extract launcher, so a
    /// test that checked one of them would read as coverage whichever way the code
    /// went. `External` is here because declining it was the cost with no benefit:
    /// its map is never named by the bundle, so its frames are unmapped in every
    /// shape, extracted ones included.
    #[test]
    fn each_container_declines_only_the_source_maps_it_cannot_honour() {
        use bundle::SourcemapMode::{External, Inline as InlineMap, Linked, None as NoMap};

        let files = vec![
            AppFile::plain(
                nub_core::compile::COMPILE_BOOTSTRAP_NAME.to_string(),
                b"// bootstrap\n".to_vec(),
            ),
            AppFile::plain("main.mjs".to_string(), b"console.log(1);\n".to_vec()),
        ];
        let decline = |map, mode| {
            let mut inputs = sealed("main.mjs");
            inputs.sourcemap = map;
            classify(&files, &inputs, mode)
                .expect("classification succeeds")
                .err()
        };

        for (map, sea, inline, why) in [
            (NoMap, None, None, "no map was asked for"),
            (
                InlineMap,
                None,
                Some(Decline::SourceMap),
                "an inline map is honoured through the blob's load hook, and not from a `data:` URL",
            ),
            (
                Linked,
                Some(Decline::SourceMap),
                Some(Decline::SourceMap),
                "a linked map is a file neither container can reach",
            ),
            (
                External,
                None,
                None,
                "an external map is never named by the bundle, so nothing resolves it at run time",
            ),
        ] {
            assert_eq!(
                decline(map, Mode::Sea),
                sea,
                "{map:?} in a single-executable: {why}"
            );
            assert_eq!(
                decline(map, Mode::Inline),
                inline,
                "{map:?} in a no-extract launcher: {why}"
            );
        }
    }

    /// A single-executable artifact's `process.execPath` is the artifact, and Node
    /// ignores a single-executable's `argv[1]`, so the bootstrap's `fork()` fix-up
    /// would point every fork back at the application. Only that container is
    /// affected: a launcher artifact hands the child a real Node path, which is why
    /// the same payload stays eligible for the inline shape.
    #[test]
    fn a_payload_that_forks_declines_the_sea_shape_only() {
        let source = "import{fork}from\"node:child_process\";fork(\"./w.js\");";
        assert_eq!(sea_decline_of(source), Some(Decline::ChildProcessReentry));
        assert_eq!(
            decline_of(source),
            None,
            "the inline shape keeps a real Node to fork, so it must stay eligible"
        );
    }

    /// The routes are the ones `Decline::ClusterReentry` already covers, because
    /// both declines read the emitted chunks through the same scan — and the last
    /// two cases are the reason that scan replaced a substring search. `cluster` and
    /// `child_process` are both spellings a payload can carry without resolving
    /// anything: `clusterApiUrl` is a real export of a published package, and a
    /// message naming either builtin is ordinary.
    #[test]
    fn the_fork_decline_reads_what_a_chunk_resolves_rather_than_what_it_spells() {
        assert_eq!(
            sea_decline_of("const cp = require(\"child_process\");cp.fork(m);"),
            Some(Decline::ChildProcessReentry),
            "the interop shim requires the bare builtin name"
        );
        assert_eq!(
            sea_decline_of("process.getBuiltinModule(\"node:child_process\").fork(m);"),
            Some(Decline::ChildProcessReentry),
            "getBuiltinModule hands back the builtin with no import at all"
        );
        assert_eq!(
            sea_decline_with("export default 1;", true),
            Some(Decline::ChildProcessReentry),
            "a payload that can build its own specifier resolves something this scan \
             never sees, so the only safe reading is that it might fork"
        );
        assert_eq!(
            sea_decline_of("import{clusterApiUrl}from\"./rpc.mjs\";clusterApiUrl(\"devnet\");"),
            None,
            "a published export whose NAME contains the word resolves no builtin"
        );
        assert_eq!(
            sea_decline_of("console.log(\"child_process spawn failed\");"),
            None,
            "and neither does a message naming one"
        );
    }

    /// The one shape a name test cannot reach, and the one the replaced substring
    /// search caught by accident: an alias keeps none of the letters that identify
    /// a require, so the call site says nothing about what it resolves. Both
    /// builtins go through the same scan, so both are covered here.
    #[test]
    fn a_require_reached_through_an_alias_still_declines() {
        assert_eq!(
            sea_decline_of("const load = require;load(\"node:child_process\").fork(m);"),
            Some(Decline::ChildProcessReentry),
            "a plain rebinding is the shape authored CommonJS produces"
        );
        assert_eq!(
            sea_decline_of("const r = __require;const load = r;load(\"child_process\").fork(m);"),
            Some(Decline::ChildProcessReentry),
            "and it chains, from the name the bundler emitted"
        );
        assert_eq!(
            sea_decline_of("let load;load = require;load(\"child_process\").fork(m);"),
            Some(Decline::ChildProcessReentry),
            "an assignment binds it as surely as a declaration"
        );
        assert_eq!(
            decline_of("const load = require;load(\"cluster\").fork();"),
            Some(Decline::ClusterReentry),
            "the cluster decline reads the same aliases, for both containers"
        );
        assert_eq!(
            sea_decline_of("const load = makeLoader;load(\"child_process\");"),
            None,
            "a name bound to something that is not a require resolves nothing"
        );
    }

    /// The shape the compiler emits for its OWN CommonJS wrapper, minified, which
    /// is what every CommonJS payload actually carries. Written out rather than
    /// paraphrased: `__nubCjsRequire` ends in a capital R, so a suffix test on the
    /// word was false here, and the minifier had already renamed the wrapper's
    /// `require` binding to a single letter — which left a real `node:cluster`
    /// payload with nothing for the scan to match and the wrong container.
    #[test]
    fn the_compilers_own_commonjs_require_is_recognized_after_minification() {
        assert_eq!(
            sea_decline_of(
                "var f=(function f(){let e=__nubCjsRequire;\
                 return(R??=__commonJSMin(((t,n)=>{let r=e(`node:child_process`);r.fork(n)}))).\
                 apply(this,arguments)});"
            ),
            Some(Decline::ChildProcessReentry),
            "the emitted CommonJS wrapper, minified, is the common case and not a corner"
        );
        assert_eq!(
            decline_of("let e=__nubCjsRequire;e(`cluster`).fork();"),
            Some(Decline::ClusterReentry),
            "and the cluster decline reads the same binding, for both containers"
        );
        assert_eq!(
            decline_of("const cluster = require$1(\"cluster\");cluster.fork();"),
            Some(Decline::ClusterReentry),
            "a suffixed rename resolves the builtin as surely as a prefixed one"
        );
    }

    /// `cluster.fork()` re-runs `process.argv[1]`, which an inline artifact
    /// publishes as the executable — so the child feeds the binary to Node and dies
    /// on the first byte. Every route to the module ends there, import syntax or
    /// not, and the word on its own decides nothing: a substring search would
    /// decline a payload that only mentions clusters in a message.
    #[test]
    fn a_payload_that_reaches_the_cluster_builtin_declines_but_a_mere_mention_does_not() {
        assert_eq!(
            decline_of("import cluster from\"node:cluster\";cluster.fork();"),
            Some(Decline::ClusterReentry),
            "the authored ESM spelling keeps its node: prefix"
        );
        assert_eq!(
            decline_of("const cluster = require(\"cluster\");cluster.fork();"),
            Some(Decline::ClusterReentry),
            "the interop shim requires the bare builtin name"
        );
        assert_eq!(
            decline_of("const cluster = __require(\"cluster\");cluster.fork();"),
            Some(Decline::ClusterReentry),
            "a bundler renames the require binding and the call still resolves"
        );
        assert_eq!(
            decline_of(
                "import{createRequire}from\"node:module\";\
                 createRequire(import.meta.url)(\"cluster\").fork();"
            ),
            Some(Decline::ClusterReentry),
            "the callee is the require a createRequire call returned"
        );
        assert_eq!(
            decline_of("process.getBuiltinModule(\"node:cluster\").fork();"),
            Some(Decline::ClusterReentry),
            "getBuiltinModule hands back the builtin with no import at all"
        );
        assert_eq!(
            decline_of("const make = () => (n) => n;make()(\"cluster\");"),
            Some(Decline::ClusterReentry),
            "an immediately-invoked call result is accepted without proving it is a \
             require, because minification renames the createRequire binding out of \
             reach — deliberately over-declining, which costs the no-write launch and \
             so a read-only deployment, against under-declining, which ships a binary \
             that crashes at fork() for everyone"
        );
        assert_eq!(
            decline_of("const msg = \"cluster failed\";console.log(msg);"),
            None,
            "the word in a string literal resolves nothing, so the payload still inlines"
        );
        assert_eq!(
            decline_of("logger.info(\"cluster\");"),
            None,
            "an unrelated call passing the word resolves nothing either"
        );
    }

    /// `assemble_app` puts a root `package.json` in EVERY payload, so a decline on
    /// any non-chunk file declines every build — which is exactly what happened, and
    /// silently, because nothing exercised the inline path. The manifest is for a
    /// walk-up through the extracted tree; an inline payload has no tree, so it is
    /// dropped rather than shipped.
    #[test]
    fn the_synthesized_root_manifest_neither_declines_the_payload_nor_rides_in_it() {
        let files = vec![
            AppFile::plain(
                nub_core::compile::COMPILE_BOOTSTRAP_NAME.to_string(),
                b"// bootstrap\n".to_vec(),
            ),
            AppFile::plain("main.mjs".to_string(), b"console.log(1);\n".to_vec()),
            AppFile::plain(
                ROOT_MANIFEST_NAME.to_string(),
                b"{\"private\":true}\n".to_vec(),
            ),
        ];

        match rewrite(files, &sealed("main.mjs")).expect("classification succeeds") {
            Rewritten::Extract(_, why) => {
                panic!("the manifest declined an otherwise inlinable payload: {why:?}")
            }
            Rewritten::Inline(out) => {
                assert!(
                    !out.iter().any(|f| f.name == ROOT_MANIFEST_NAME),
                    "the manifest must be dropped: nothing can resolve through it off disk, \
                     and the chunk rewriter would otherwise treat its JSON as a module"
                );
                assert!(
                    out.iter().any(|f| f.name == "main.mjs"),
                    "the chunk still ships"
                );
            }
        }
    }

    /// The complement, so the exemption above cannot silently widen into "any
    /// non-chunk file is fine" — a `--sourcemap=linked` map must still decline.
    #[test]
    fn a_non_chunk_file_that_is_not_the_manifest_still_declines() {
        let files = vec![
            AppFile::plain(
                nub_core::compile::COMPILE_BOOTSTRAP_NAME.to_string(),
                b"// bootstrap\n".to_vec(),
            ),
            AppFile::plain("main.mjs".to_string(), b"console.log(1);\n".to_vec()),
            AppFile::plain("main.mjs.map".to_string(), b"{}\n".to_vec()),
        ];

        match rewrite(files, &sealed("main.mjs")).expect("classification succeeds") {
            Rewritten::Extract(_, why) => assert_eq!(
                why,
                Decline::NonChunkFile,
                "a linked source map declines for being a non-chunk file"
            ),
            Rewritten::Inline(_) => panic!("a linked source map cannot be served from a data: URL"),
        }
    }

    /// An embedding artifact unpacks its Node to the cache to exec it, so inlining
    /// its app removes one write out of two and the binary still needs a writable
    /// cache. Measured, it costs 19% of warm start to remove that one write, so the
    /// shape declines and the no-write claim belongs to `--smol` alone.
    #[test]
    fn a_payload_that_embeds_its_own_node_declines_however_inlinable_it_is() {
        let files = vec![AppFile::plain(
            nub_core::compile::COMPILE_BOOTSTRAP_NAME.to_string(),
            b"//boot\n".to_vec(),
        )];
        let inputs = Inputs {
            embeds_node: true,
            ..sealed("main.mjs")
        };
        match rewrite(files, &inputs).expect("classification succeeds") {
            Rewritten::Extract(_, why) => assert_eq!(why, Decline::EmbeddedNode),
            Rewritten::Inline(_) => {
                panic!(
                    "an embedding artifact writes its Node out regardless, so inlining is a cost"
                )
            }
        }
    }

    /// The root is a CONTRACT between this file and the runtime loader, and it has
    /// one non-obvious requirement: Node converts it with `fileURLToPath`, whose
    /// Windows implementation rejects anything that is not `<letter>:`. Without the
    /// drive letter every inline artifact died at startup on Windows and nowhere
    /// else, because `createRequire(import.meta.url)` runs before any user code.
    /// Both halves are asserted here — the shape, and that the loader agrees —
    /// since a second implementation drifting silently is what shipped the bug.
    #[test]
    fn the_virtual_root_is_a_windows_convertible_path_the_loader_agrees_on() {
        let path = VIRTUAL_ROOT
            .strip_prefix("file:///")
            .expect("the virtual root is an absolute file URL");
        let drive: Vec<char> = path.chars().take(2).collect();
        assert!(
            drive[0].is_ascii_alphabetic() && drive[1] == ':',
            "a file URL Node can convert on Windows opens with a drive letter, not {path:?}"
        );
        let loader = loader_source("main.mjs").expect("the loader source is readable");
        assert!(
            loader.contains(&format!("const ROOT = {VIRTUAL_ROOT:?};")),
            "the loader publishes the same root this bakes into every chunk"
        );
    }

    #[test]
    fn a_chunk_keeps_every_line_below_the_prefixed_identity() {
        let source = "import{a}from\"./dep-A1.mjs\";\nconsole.log(a);\n";
        let out = rewrite_chunk(source, "main.mjs", &chunks(&["main.mjs", "dep-A1.mjs"]));
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].ends_with("import.meta.url = \"file:///N:/$nub/main.mjs\";"),
            "the identity assignment closes the single prefix line: {}",
            lines[0]
        );
        assert!(
            lines[0].starts_with(EVAL_GLOBAL_CLEANUP),
            "and the `-e` global cleanup opens it: {}",
            lines[0]
        );
        assert_eq!(
            lines[1], "import{a}from\"nub-inline:dep-A1.mjs\";",
            "the cross-chunk specifier becomes the loader's placeholder"
        );
        assert_eq!(
            lines[2], "console.log(a);",
            "code below the imports is untouched, so a map shifted by one line still lands"
        );
    }

    #[test]
    fn a_chunk_cycle_declines_rather_than_producing_an_unbuildable_url() {
        let mut cyclic: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        cyclic.insert("a.mjs", vec!["b.mjs"]);
        cyclic.insert("b.mjs", vec!["a.mjs"]);
        assert!(has_cycle(&cyclic));

        // The shape every ordinary build produces: the entry imports both the
        // CommonJS chunk and the Rolldown runtime, and the CommonJS chunk imports the
        // runtime too. That is a diamond, and a diamond must not read as a cycle.
        let mut diamond: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        diamond.insert("entry.mjs", vec!["cjs.mjs", "rt.mjs"]);
        diamond.insert("cjs.mjs", vec!["rt.mjs"]);
        diamond.insert("rt.mjs", vec![]);
        assert!(!has_cycle(&diamond));
    }
}
