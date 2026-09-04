//! The no-extract payload shape: a compiled artifact that writes nothing.
//!
//! An ordinary compiled binary extracts its app files to `~/.cache/nub/compile-app/…`
//! on first run, because Node has to be handed a `--require` preload and an entry
//! by PATH. A payload that reduces to generated JavaScript needs neither: the
//! launcher passes the bootstrap as `-e` and the bootstrap serves each chunk to
//! `import()` as a `data:` URL read straight out of the executable. Nothing on the
//! path to running such an artifact touches the filesystem, so it starts under a
//! read-only `HOME` and `TMPDIR`, where today it refuses to start at all.
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
const VIRTUAL_ROOT: &str = "file:///$nub/";

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
/// One line, so the source-map shift stays exactly one generated line.
const EVAL_GLOBAL_CLEANUP: &str = "for(const k of[\"require\",\"module\",\"exports\",\"__filename\",\"__dirname\"])delete globalThis[k];";

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
    /// The build asked for source maps. Measured on Node 26.7: `--enable-source-maps`
    /// does not apply an inline map to a `data:` URL module at all — with or without
    /// a `//# sourceURL` — so an inline artifact would report unmapped frames where
    /// the extracted one reports `app.ts:1`. Extracting is what keeps the maps the
    /// build was asked to produce, so a build that wants them gets exactly today's
    /// behavior.
    SourceMap,
}

impl Decline {
    /// The clause the build summary prints after "extracts on first run because …".
    pub fn reason(self) -> &'static str {
        match self {
            Self::UnsealedGraph => "it resolves modules at run time",
            Self::StaticWorker => "it carries a worker chunk",
            Self::NonChunkFile => "it carries a file that is not a compiled chunk",
            Self::CyclicChunks => "its chunks import each other in a cycle",
            Self::SourceMap => "it was built with source maps",
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
    /// Whether any source map was asked for.
    pub sourcemap: bool,
    /// The entry chunk's payload name.
    pub entry: &'a str,
}

/// What [`rewrite`] decided. The files come back either way: a declined payload
/// extracts exactly as it always has, so a decline is a report, not a failure.
pub enum Rewritten {
    Inline(AppFiles),
    Extract(AppFiles, Decline),
}

/// Rewrite `files` for inline launch, or say why the payload has to extract.
///
/// On success every `.mjs` chunk has been given its virtual identity and had its
/// cross-chunk specifiers replaced, and the bootstrap entry carries the loader that
/// reads them back out of the executable. The caller compresses the result with
/// brotli and sets `Manifest::inline_app`.
pub fn rewrite(files: AppFiles, inputs: &Inputs<'_>) -> Result<Rewritten> {
    match classify(&files, inputs)? {
        Err(decline) => Ok(Rewritten::Extract(files, decline)),
        Ok(chunk_names) => {
            let loader = loader_source(inputs.entry)?;
            let bootstrap_name = nub_core::compile::COMPILE_BOOTSTRAP_NAME;
            let rewritten = files
                .into_iter()
                .map(|mut file| {
                    if file.name == bootstrap_name {
                        file.bytes.push(b'\n');
                        file.bytes.extend_from_slice(loader.as_bytes());
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
fn classify(files: &AppFiles, inputs: &Inputs<'_>) -> Result<Result<BTreeSet<String>, Decline>> {
    if !inputs.sealed_module_graph {
        return Ok(Err(Decline::UnsealedGraph));
    }
    if inputs.worker_roots != 0 || inputs.worker_wrappers != 0 {
        return Ok(Err(Decline::StaticWorker));
    }
    if inputs.sourcemap {
        return Ok(Err(Decline::SourceMap));
    }
    let bootstrap_name = nub_core::compile::COMPILE_BOOTSTRAP_NAME;
    // Every file is the bootstrap or a chunk. `--include`s, emitted assets and
    // native islands are already excluded by the sealed-graph check above; what this
    // catches is the compiler's OWN non-chunk output, which today means a
    // `--sourcemap=linked` map travelling beside the bundle.
    if files
        .iter()
        .any(|file| file.name != bootstrap_name && !file.name.ends_with(".mjs"))
    {
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
    if has_cycle(&edges) {
        return Ok(Err(Decline::CyclicChunks));
    }
    Ok(Ok(chunk_names))
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

    fn chunks(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn a_chunk_keeps_every_line_below_the_prefixed_identity() {
        let source = "import{a}from\"./dep-A1.mjs\";\nconsole.log(a);\n";
        let out = rewrite_chunk(source, "main.mjs", &chunks(&["main.mjs", "dep-A1.mjs"]));
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].ends_with("import.meta.url = \"file:///$nub/main.mjs\";"),
            "the identity assignment closes the single prefix line: {}",
            lines[0]
        );
        assert!(
            lines[0].starts_with("for(const k of["),
            "and the `-e` global cleanup opens it"
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
