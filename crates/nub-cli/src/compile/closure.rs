//! Bundling the bundlable half of an eject closure.
//!
//! Ejecting a package used to eject everything it depends on. The predicate was
//! evaluated once, at the root, and the rest of the closure inherited the verdict
//! by association — an association nothing justified. `jsdom` must ship verbatim
//! because it reads a stylesheet from a path built at run time; `parse5` never had
//! to, and its 64 files now ship as a chunk and a stub manifest.
//!
//! That matters more than the file count suggests. The launcher writes the WHOLE
//! payload and checks every file against it (`app_cache_tree_matches`) before it
//! publishes an extraction, so an unloaded payload file is not free: 37 of
//! pdfkit's 1194 files are ever loaded, and all 1194 are extracted and checked.
//!
//! So [`super::native`] classifies every package in the closure on its own and
//! hands the clean ones here. Each specifier an ejected package names across the
//! boundary becomes an entry, and the chunk answering it is emitted inside the
//! directory Node will look in — `node_modules/parse5/__nub/0.js` — beside a stub
//! manifest whose `exports` points at it.
//! Writing the chunk in the stub's own directory rather than at the app root is
//! what keeps a `require` of a still-verbatim neighbour resolving: Node walks up
//! from the file that ran, which is exactly where the materialiser placed that
//! neighbour.
//!
//! Two refusals are what make this safe rather than optimistic:
//!
//! - Every closure member is classified by the same rules that ejected the root,
//!   so a dependency that reads its own data off `__dirname` stays verbatim.
//! - An ejected package that COMPUTES a specifier gives up its whole closure. A
//!   stub answers the specifiers the scan saw; it cannot answer one it never saw,
//!   and that failure is a clean build that dies on a user's machine.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use rolldown::plugin::__inner::SharedPluginable;
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookUsage, Plugin, PluginContext, SharedLoadPluginContext,
};
use rolldown::{BundlerBuilder, BundlerOptions, InputItem};
use rolldown_common::{
    AttachDebugInfo, ExperimentalOptions, IsExternal, ModuleType, Output, OutputFormat, Platform,
    RawMinifyOptions, ResolveOptions,
};
use rolldown_error::{BuildDiagnostic, DiagnosticOptions, EventKind};

use super::native_layout::IslandFile;

/// How an ejected package reaches a specifier — and therefore which module format
/// the chunk answering it has to be in.
///
/// Not cosmetic. A CommonJS chunk handed to a named ESM import fails at load with
/// `SyntaxError: does not provide an export named 'Array'`, because Node's
/// cjs-module-lexer cannot see through a bundled, minified chunk to the names it
/// re-exports. `fontkit` reaches `@swc/helpers` one way and `pdfkit` reaches
/// `fontkit` the other, so both formats are real and a package can want both.
#[derive(Default, Clone, Copy)]
pub struct Uses {
    pub required: bool,
    pub imported: bool,
}

/// One specifier an ejected package names across the eject boundary, in one
/// format. A specifier used both ways gets two entries and two chunks.
pub struct Entry {
    /// A path inside the package that wrote the specifier. No such file exists —
    /// only its DIRECTORY is used, so the bundler resolves the specifier exactly
    /// as Node would from there, `exports` map and all.
    pub importer: PathBuf,
    pub specifier: String,
    /// Where the chunk lands: `node_modules/parse5/__nub/0.js`.
    pub chunk: String,
    pub esm: bool,
}

#[derive(Default)]
pub struct Plan {
    pub entries: Vec<Entry>,
    /// Installs left verbatim, by real directory. The bundle leaves every one of
    /// them to Node: the real files are already in the payload, and a copy inside a
    /// chunk would be a second module object with a different `__dirname`. Not by
    /// name: a clean install of a package another closure ships verbatim still
    /// bundles, and left to Node it would load the other one.
    pub verbatim: BTreeSet<PathBuf>,
    /// The next chunk number. It only ever goes up: entries are filtered and removed
    /// after they are numbered, so counting them hands out a number a live chunk holds.
    pub next_chunk: usize,
}

/// A verbatim install a bundled module imports, and the chunk that module landed in.
pub struct VerbatimImport {
    pub chunk: String,
    pub module: PathBuf,
    pub package: String,
    pub instance: PathBuf,
}

/// What [`bundle`] emitted, with every import it left to Node.
#[derive(Default)]
pub struct Bundled {
    pub files: Vec<IslandFile>,
    pub imports: Vec<VerbatimImport>,
}

/// The bare specifiers `root` names in its own source.
///
/// `None` is the refusal: either the walk was truncated, or the package computes
/// a specifier somewhere. Both mean the set cannot be trusted to be complete, and
/// an incomplete set is what turns a clean build into a runtime `Cannot find
/// module` — so the caller ships that whole closure verbatim instead.
pub fn boundary_specifiers(root: &Path) -> Option<BTreeMap<String, Uses>> {
    let mut found = BTreeMap::new();
    for file in super::unbundlable::loadable_code(root)? {
        let text = std::fs::read_to_string(root.join(&file)).ok()?;
        scan(&text, &mut found)?;
    }
    Some(found)
}

/// The package a bare specifier names: `@noble/hashes/sha2` -> `@noble/hashes`.
pub fn package_of(specifier: &str) -> &str {
    let segments = if specifier.starts_with('@') { 2 } else { 1 };
    let mut end = 0;
    for _ in 0..segments {
        match specifier[end..].find('/') {
            Some(offset) => end += offset + 1,
            None => return specifier,
        }
    }
    &specifier[..end - 1]
}

/// Whether a chunk can stand in for `specifier` at all.
///
/// The chunk is JavaScript, and the `exports` map is free to point any subpath at
/// any file — so the spelling of the subpath does not constrain the chunk's NAME.
/// What it constrains is the module TYPE, and that answer depends on how the
/// specifier is reached:
///
/// - `require("pkg/data.json")` is answerable. The chunk exports the parsed
///   object, which is precisely what requiring the file produces. This is not a
///   corner: `mdn-data/css/at-rules.json` is the single specifier that gave up
///   jsdom's entire closure, all 1843 payload files of it.
/// - `import ... from "pkg/data.json"` is not. Node validates a JSON module by the
///   resolved file's extension, so no `.js` chunk can satisfy it.
/// - Anything else that is not JavaScript — a stylesheet, a font — has no
///   JavaScript form at all and gives up the closure either way.
pub fn answerable(specifier: &str, uses: Uses) -> bool {
    let subpath = specifier[package_of(specifier).len()..].trim_start_matches('/');
    let file = subpath.rsplit_once('/').map_or(subpath, |(_, file)| file);
    match file.rsplit_once('.') {
        None | Some((_, "js" | "cjs" | "mjs")) => true,
        Some((_, "json")) => !uses.imported,
        Some(_) => false,
    }
}

/// Where the chunk for one entry lands inside a package placed at `at`.
///
/// A reserved directory and a running number, not the subpath: the `exports` map
/// is the indirection, so the name is nub's to choose, and choosing it keeps a
/// chunk from ever being written over a file the stub itself needs — the stub's
/// own `package.json` first among them.
pub fn chunk_path(at: &str, index: usize, esm: bool) -> String {
    format!("{at}/__nub/{index}.{}", if esm { "mjs" } else { "js" })
}

/// The manifest that makes a stub resolvable: the real version so the payload does
/// not misreport what it carries, and an `exports` map naming every specifier the
/// closure answers, the bare one as `.`.
///
/// The `exports` map is not decoration. Node's CJS lookup would find
/// `./_/_define_property.js` from the extensionless specifier on its own, but ESM
/// resolution does no extension guessing — and an ejected package is as often ESM
/// as CommonJS. Without the map, `fontkit`'s
/// `import { _ } from "@swc/helpers/_/_define_property"` fails with
/// `ERR_MODULE_NOT_FOUND`, on a build that reported nothing.
///
/// It doubles as the honest failure. `exports` is exhaustive by definition, so a
/// specifier the scan never saw cannot silently resolve to some other file — it
/// raises `ERR_PACKAGE_PATH_NOT_EXPORTED` and names itself.
///
/// No `type` field, so the CommonJS chunks beside it are read as CommonJS however
/// the real package declared itself.
pub fn stub_manifest(
    name: &str,
    version: Option<&str>,
    exports: &BTreeMap<String, BTreeMap<&'static str, String>>,
) -> Vec<u8> {
    let manifest = serde_json::json!({
        "name": name,
        "version": version.unwrap_or("0.0.0"),
        "exports": exports,
    });
    format!("{manifest}\n").into_bytes()
}

/// The `exports` key for one answered specifier, and the condition its chunk
/// answers under.
pub fn export_entry(at: &str, entry: &Entry) -> (String, &'static str, String) {
    let subpath = entry.specifier[package_of(&entry.specifier).len()..].trim_start_matches('/');
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    };
    let condition = if entry.esm { "import" } else { "require" };
    (key, condition, format!(".{}", &entry.chunk[at.len()..]))
}

/// Bundle every entry into the payload file that answers it.
///
/// Entry chunks land at their stub paths and everything shared between them lands
/// under `__nub_closure/` at the app root — so `@noble/hashes/sha2` and
/// `@noble/hashes/utils` share their internals instead of carrying two copies.
///
/// A package left verbatim is external here, so a chunk finds it by walking up from
/// where the chunk landed: an entry chunk from its stub, a shared one from the app
/// root. Neither is where the importing module sat, so the imports are returned for
/// the caller to check against the payload.
///
/// `external` is the plugin that gives the main bundle's `--external` imports their
/// ids. A chunk can inline modules from several packages, and sits apart from all of
/// them when it is shared, so only the import itself can say which module made it.
pub fn bundle(
    plan: &Plan,
    external: Option<&SharedPluginable>,
    minify: bool,
    keep_names: bool,
) -> Result<Bundled> {
    let mut bundled = Bundled::default();
    for esm in [false, true] {
        let entries: Vec<&Entry> = plan.entries.iter().filter(|e| e.esm == esm).collect();
        if !entries.is_empty() {
            let built = build(&entries, &plan.verbatim, external, esm, minify, keep_names)?;
            bundled.files.extend(built.files);
            bundled.imports.extend(built.imports);
        }
    }
    Ok(bundled)
}

fn build(
    plan: &[&Entry],
    verbatim: &BTreeSet<PathBuf>,
    external: Option<&SharedPluginable>,
    esm: bool,
    minify: bool,
    keep_names: bool,
) -> Result<Bundled> {
    let verbatim = verbatim.clone();
    let synthetic: BTreeSet<String> = plan
        .iter()
        .map(|entry| entry.importer.to_string_lossy().into_owned())
        .collect();
    let imports: Arc<Mutex<BTreeSet<(String, String, PathBuf)>>> = Arc::default();
    let recorded = Arc::clone(&imports);
    let entries = Arc::new(Entries {
        sources: plan
            .iter()
            .map(|entry| {
                (
                    entry.importer.to_string_lossy().into_owned(),
                    (entry.specifier.clone(), esm),
                )
            })
            .collect(),
    });
    let options = BundlerOptions {
        input: Some(
            plan.iter()
                .map(|entry| InputItem {
                    // Rolldown renders `[name]` into the filename verbatim, and the
                    // pattern below adds nothing — so the entry name IS the payload
                    // path, extension included.
                    name: Some(entry.chunk.clone()),
                    import: entry.importer.to_string_lossy().into_owned(),
                })
                .collect(),
        ),
        format: Some(if esm {
            OutputFormat::Esm
        } else {
            OutputFormat::Cjs
        }),
        platform: Some(Platform::Node),
        entry_filenames: Some("[name]".to_string().into()),
        // Shared chunks sit at the app root, and their EXTENSION is what tells Node
        // how to read them — nothing else there declares a type.
        //
        // Which is why the CommonJS case must be `.cjs` and not `.js`. `.js` is the
        // one extension that does NOT decide: Node resolves it against the nearest
        // `package.json`, and the payload has none, so the answer comes from
        // whatever happens to sit above the cache directory. Point `XDG_CACHE_HOME`
        // inside a `"type": "module"` project — which the Dockerfile in our own docs
        // does — and every one of these chunks is read as ESM, so the first
        // `require` in them throws and the binary dies after a clean build.
        chunk_filenames: Some(
            format!(
                "__nub_closure/[name]-[hash].{}",
                if esm { "mjs" } else { "cjs" }
            )
            .into(),
        ),
        minify: Some(RawMinifyOptions::Bool(minify)),
        keep_names: Some(keep_names),
        // Unminified, each module would be headed by a `//#region` comment naming it
        // relative to this process, through the store on this machine. Nothing reads
        // them in a chunk that ships third-party code.
        experimental: Some(ExperimentalOptions {
            attach_debug_info: Some(AttachDebugInfo::None),
            ..Default::default()
        }),
        external: Some(IsExternal::Fn(Some(Arc::new(
            move |specifier: &str, importer: Option<&str>, _resolved: bool| {
                // Never an entry's own specifier: its chunk is written into a stub
                // of that package, so leaving it to Node is a `require` of itself.
                let install = importer
                    .filter(|importer| !synthetic.contains(*importer))
                    .and_then(|importer| {
                        verbatim_install(&verbatim, specifier, importer)
                            .map(|install| (importer.to_string(), install))
                    });
                let external = install.is_some();
                if let (Some((importer, install)), Ok(mut seen)) = (install, recorded.lock()) {
                    seen.insert((importer, package_of(specifier).to_string(), install));
                }
                Box::pin(async move { Ok(external) })
            },
        )))),
        resolve: Some(ResolveOptions {
            // `module` before `main`, matching the main bundle: a legacy dual
            // package's `main` is a UMD build whose factory takes `require` as a
            // parameter, and those calls survive into the output.
            main_fields: Some(vec!["module".to_string(), "main".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let plugins: Vec<SharedPluginable> = external
        .cloned()
        .into_iter()
        .chain([entries as SharedPluginable])
        .collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow!("building the closure bundler runtime: {e}"))?;
    let output = rt.block_on(async move {
        let mut bundler = BundlerBuilder::default()
            .with_options(options)
            .with_plugins(plugins)
            .build()
            .map_err(|e| anyhow!("closure bundler init: {e:?}"))?;
        bundler
            .generate()
            .await
            .map_err(|e| anyhow!("bundling the eject closure failed: {e:?}"))
    })?;
    reject_unresolved_entries(plan, &output.warnings)?;
    let recorded = imports
        .lock()
        .map_err(|_| anyhow!("the closure import collector was poisoned"))?;
    let mut bundled = Bundled::default();
    for asset in &output.assets {
        let Output::Chunk(chunk) = asset else {
            continue;
        };
        bundled.files.push(IslandFile {
            name: chunk.filename.to_string(),
            bytes: chunk.code.as_bytes().to_vec(),
            executable: false,
        });
        for (module, package, instance) in recorded.iter() {
            if chunk.modules.keys.iter().any(|key| key.as_str() == module) {
                bundled.imports.push(VerbatimImport {
                    chunk: chunk.filename.to_string(),
                    module: PathBuf::from(module),
                    package: package.clone(),
                    instance: instance.clone(),
                });
            }
        }
    }
    Ok(bundled)
}

/// The verbatim install a bare specifier resolves to from `importer`, if it is one.
fn verbatim_install(
    verbatim: &BTreeSet<PathBuf>,
    specifier: &str,
    importer: &str,
) -> Option<PathBuf> {
    if specifier.starts_with('.') || Path::new(specifier).is_absolute() {
        return None;
    }
    let root = super::native::resolve_package_root(Path::new(importer), package_of(specifier))?;
    let real = std::fs::canonicalize(root).ok()?;
    verbatim.contains(&real).then_some(real)
}

/// Fail the build when an entry's own specifier resolved to nothing.
///
/// Every entry names a package the closure walk found installed, so this is never
/// the package's fault. And the outcome is never a working binary: Rolldown leaves
/// an unresolved bare specifier external, so the chunk becomes a `require` of the
/// very stub it is written into, and Node's cycle handling gives every caller an
/// empty object. That is how every jsdom chunk shipped under an isolated install,
/// failing at run time with `SymbolTree is not a constructor`.
///
/// Matched on the rendered message because [`BuildDiagnostic`] offers no
/// structured importer, and the synthetic importer's file name is nub's own.
fn reject_unresolved_entries(plan: &[&Entry], warnings: &[BuildDiagnostic]) -> Result<()> {
    let importers: Vec<String> = plan
        .iter()
        .filter_map(|entry| entry.importer.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    let opts = DiagnosticOptions::default();
    let unresolved: Vec<String> = warnings
        .iter()
        .filter(|w| matches!(w.kind(), EventKind::UnresolvedImport))
        .map(|w| w.to_message_with(&opts))
        .filter(|message| importers.iter().any(|name| message.contains(name.as_str())))
        .collect();
    if unresolved.is_empty() {
        return Ok(());
    }
    bail!(
        "Nub could not bundle the dependencies of an unbundled package:\n  {}\n\
         \x20 This is a bug in Nub. Passing --unbundled <package> for each package named\n\
         \x20 above ships it as installed instead, which avoids it.",
        unresolved.join("\n  ")
    )
}

/// Supplies each entry: a one-line CommonJS module whose whole body is the
/// specifier being replaced, loaded from inside the package that wrote it.
#[derive(Debug)]
struct Entries {
    sources: BTreeMap<String, (String, bool)>,
}

impl Plugin for Entries {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:closure-entries")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load
    }

    fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let claimed = self
            .sources
            .contains_key(args.specifier)
            .then(|| args.specifier.to_string());
        async move {
            Ok(claimed.map(|id| HookResolveIdOutput {
                id: id.into(),
                ..Default::default()
            }))
        }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        // `export *` carries the named exports; the default comes off the
        // NAMESPACE rather than through `export { default } from`, which is a hard
        // build error against a module that has none — `restructure`, `@noble/*`
        // and `@swc/helpers` are all named-only and all failed that way.
        // `module.exports =` is the CommonJS equivalent. Each entry is written in
        // the format its own chunk is emitted in, so nothing is interoped back.
        let source = self.sources.get(args.id).map(|(specifier, esm)| {
            let specifier = serde_json::Value::from(specifier.as_str());
            if *esm {
                format!(
                    "import * as ns from {specifier};\nexport * from {specifier};\nexport default ns.default;\n"
                )
            } else {
                format!("module.exports = require({specifier});\n")
            }
        });
        async move {
            Ok(source.map(|code| HookLoadOutput {
                code: code.into(),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }))
        }
    }
}

/// What a `/` means after the token just read.
#[derive(Clone, Copy, PartialEq)]
enum Slash {
    /// It opens a regular expression: only an expression can come next.
    Regex,
    /// It divides: the token ended a value.
    Divides,
    /// Either, and nothing in the tokens says which.
    Unknown,
}

/// What an open `(` belongs to, which decides what follows its `)`.
#[derive(Clone, Copy, PartialEq)]
enum Paren {
    /// A call, a grouping or a parameter list: its `)` ends a value.
    Value,
    /// The head of an `if`, `while`, `with`, `switch` or `catch`: a statement
    /// follows its `)`.
    Head,
    /// The head of a `for`, which is also the one place `of` is a keyword.
    ForHead,
}

/// What the token just read tells the one after it.
#[derive(Default)]
struct Carry {
    /// It is a statement keyword whose `(` opens a head.
    head: Option<Paren>,
    /// A `{` after it can only open a block: the `)` of a head, `else`, `do`,
    /// `try`, `finally`, or the `=>` of an arrow function.
    blocks: bool,
    /// It makes a word a name whatever the word spells: the `.` of a property
    /// read (`x.in`, `x?.return`, `x. typeof`) or the `#` of a private one.
    names: bool,
    /// A name after it can be whole where it stands: `var`, `let`, `const` and
    /// the `,` between the names they declare, and the `break` or `continue`
    /// before a label.
    declares: bool,
    /// It is `var`, `let` or `const`, and the word after it is the name it
    /// declares whatever that word spells: `var let`, `for (var of of x)`.
    declarator: bool,
    /// It is a name that can be whole where it stands.
    whole: bool,
    /// It is `require` or `import`, and a `(` after it opens a call: whether
    /// that call requires.
    calls: Option<bool>,
    /// It is `import` or `from`, and a string after it is the specifier of a
    /// declaration.
    sources: bool,
    /// It is a `}`.
    closes: bool,
    /// A line end follows it.
    new_line: bool,
}

/// Collect the literal specifiers in `text`, refusing the moment one is computed.
///
/// Textual, like the rest of nub's package scanning, and for the same reason: a
/// published package ships bundles measured in megabytes, and the question — is
/// the argument one quoted string? — needs no scope information. It over-reports,
/// since a method that happens to be named `require` counts, and that is the safe
/// direction: a specifier nobody loads costs an unused chunk, where a missed one
/// costs a binary that fails on the machine it was shipped to.
///
/// What it steps over to stay in step with the code is described on
/// [`skip_noncode`]; template text and regular expressions are handled here,
/// because both depend on state carried from one token to the next.
fn scan(text: &str, out: &mut BTreeMap<String, Uses>) -> Option<()> {
    let bytes = text.as_bytes();
    // A `#!` line is a comment, and the path on it is not a regular expression.
    let mut i = if bytes.starts_with("\u{feff}".as_bytes()) {
        3
    } else {
        0
    };
    if bytes[i..].starts_with(b"#!") {
        i = line_end(bytes, i);
    }
    // One entry per open `${`, counting the `{` opened inside it, so the `}` that
    // ends a substitution is told apart from one that closes a block within it.
    let mut substitutions: Vec<usize> = Vec::new();
    // What a `/` here means. Both mistakes hide code: division read as a regular
    // expression steps over everything up to the next `/`, and a regular
    // expression read as division has its body scanned as code, where a quote or
    // a backtick opens a string that is not there. So where the tokens leave both
    // readings open, the scan gives up instead of choosing.
    let mut slash = Slash::Regex;
    let mut parens: Vec<Paren> = Vec::new();
    // One entry per open `{`: whether it is known to open a statement block, after
    // whose `}` a new statement starts. Any other `}` may close an object literal
    // or a function expression, which are values.
    let mut braces: Vec<bool> = Vec::new();
    // The start of the file is the start of a line.
    let mut carry = Carry {
        new_line: true,
        ..Carry::default()
    };
    while i < bytes.len() {
        // Prose is not code, and this is not pedantry: jsdom's `interfaces.js`
        // carries the phrase "require()s" in a comment, which read as a call with
        // a computed argument and gave up jsdom's ENTIRE closure — 1843 payload
        // files that stayed exactly as they were.
        if let Some(after) = skip_noncode(bytes, i) {
            if bytes[i] == b'/' {
                // A comment leaves the token before it in charge of what follows.
                carry.new_line |= line_end(&bytes[i..after], 0) < after - i;
            } else if std::mem::take(&mut carry).sources
                && after <= bytes.len()
                && after - i >= 2
                && bytes[after - 1] == bytes[i]
            {
                // `import "x"`, and the tail of `import … from "x"` and
                // `export … from "x"`: whatever clause sits between the two
                // words is passed over by matching `from` on its own. The
                // declaration ends here, so a `/` on the next line opens a
                // regular expression. Where `from` is a variable instead, the
                // string is a value and a `/` divides it.
                take(&text[i + 1..after - 1], false, out)?;
                slash = Slash::Unknown;
            } else {
                // A string is a value.
                slash = Slash::Divides;
            }
            i = after;
            continue;
        }
        let Some(c) = text.get(i..).and_then(|rest| rest.chars().next()) else {
            i += 1;
            continue;
        };
        if is_space(c) {
            carry.new_line |= matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
            i += c.len_utf8();
            continue;
        }
        let was = std::mem::take(&mut carry);
        // `...` spreads an expression. It is one token, so a `.` after it can
        // start a number: `....5`.
        if bytes[i..].starts_with(b"...") {
            i += 3;
            slash = Slash::Regex;
            continue;
        }
        // A number is read whole, so its own `.` and the sign of its exponent
        // stay inside it.
        if c.is_ascii_digit() || c == '.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i = number_end(bytes, i);
            slash = Slash::Divides;
            continue;
        }
        if !is_word_char(c) {
            let byte = bytes[i];
            match byte {
                b'`' => {
                    (i, slash) = skip_template_text(bytes, i + 1, &mut substitutions);
                    continue;
                }
                b'/' => match slash {
                    Slash::Regex => {
                        i = skip_regex(bytes, i);
                        slash = Slash::Divides;
                        continue;
                    }
                    // `var a` and `break a` are whole, so a `/` on the line after
                    // one opens a regular expression, where after any other name
                    // it divides. The name does not say which it is: a `,` sits
                    // between declared names and between arguments alike.
                    Slash::Divides if was.whole && was.new_line => return None,
                    Slash::Divides => slash = Slash::Regex,
                    Slash::Unknown => return None,
                },
                b'(' => {
                    // A call: its argument is the whole verdict, so anything that
                    // is not one quoted string gives up. `'a' + b` is the case
                    // worth naming — the string is real, and reading it as the
                    // specifier would report a module nobody asked for while
                    // hiding the one they did.
                    if let Some(required) = was.calls {
                        let (specifier, end) = literal(text, skip_spaces(bytes, i + 1))?;
                        let close = skip_spaces(bytes, end);
                        if bytes.get(close) != Some(&b')') {
                            return None;
                        }
                        take(specifier, required, out)?;
                        // The call is read whole, so the stack of open parens
                        // sees neither its `(` nor its `)`.
                        i = close + 1;
                        slash = Slash::Divides;
                        continue;
                    }
                    parens.push(was.head.unwrap_or(Paren::Value));
                    slash = Slash::Regex;
                }
                // `if (x) /re/.test(y)` starts a statement; `(a + b) / 2` divides.
                b')' => {
                    carry.blocks = matches!(parens.pop(), Some(Paren::Head | Paren::ForHead));
                    slash = if carry.blocks {
                        Slash::Regex
                    } else {
                        Slash::Divides
                    };
                }
                b'>' if i > 0 && bytes[i - 1] == b'=' => {
                    carry.blocks = true;
                    slash = Slash::Regex;
                }
                b'{' => {
                    if let Some(depth) = substitutions.last_mut() {
                        *depth += 1;
                    }
                    braces.push(was.blocks);
                    slash = Slash::Regex;
                }
                b'}' => {
                    match substitutions.pop() {
                        Some(0) => {
                            (i, slash) = skip_template_text(bytes, i + 1, &mut substitutions);
                            continue;
                        }
                        Some(depth) => substitutions.push(depth - 1),
                        None => {}
                    }
                    carry.closes = true;
                    slash = match braces.pop() {
                        Some(true) => Slash::Regex,
                        _ => Slash::Unknown,
                    };
                }
                b']' => slash = Slash::Divides,
                // `<!--` anywhere and `-->` at the start of a line are comments in
                // a script and operators in a module.
                b'<' if bytes[i..].starts_with(b"<!--") => return None,
                b'-' if was.new_line && bytes[i..].starts_with(b"-->") => return None,
                // `++` and `--` sit on either side of a value, so what a `/` means
                // after one is what it meant before it. After a line end only
                // the prefix one is left, and its operand follows.
                b'+' | b'-' if bytes.get(i + 1) == Some(&byte) => {
                    if was.new_line {
                        slash = Slash::Regex;
                    }
                    i += 2;
                    continue;
                }
                // A `.` here reads a property: a spread and a number have each
                // taken their own.
                b'.' => {
                    carry.names = true;
                    slash = Slash::Divides;
                }
                b'#' => carry.names = true,
                b',' => {
                    carry.declares = true;
                    slash = Slash::Regex;
                }
                // TypeScript's `x!` asserts the value before it, on the same line,
                // and is a value still. Any other `!` negates what follows.
                b'!' if slash != Slash::Regex
                    && !was.new_line
                    && bytes.get(i + 1) != Some(&b'=') => {}
                // A JSX closing tag. Nothing here reads JSX, and its text is not
                // code. Before a comment the `<` is an operator, or opens a list
                // of TypeScript type arguments.
                b'<' if bytes.get(i + 1) == Some(&b'/')
                    && !matches!(bytes.get(i + 2), Some(b'/' | b'*')) =>
                {
                    return None;
                }
                _ => slash = Slash::Regex,
            }
            i += 1;
            continue;
        }
        let start = i;
        i += text[start..]
            .chars()
            .take_while(|c| is_word_char(*c))
            .map(char::len_utf8)
            .sum::<usize>();
        let word = &text[start..i];
        slash = match word {
            _ if was.names => Slash::Divides,
            "if" | "while" | "with" | "switch" | "catch" => {
                carry.head = Some(Paren::Head);
                // `catch {` takes no binding.
                carry.blocks = word == "catch";
                Slash::Regex
            }
            "try" | "finally" => {
                carry.blocks = true;
                Slash::Regex
            }
            "for" => {
                carry.head = Some(Paren::ForHead);
                Slash::Regex
            }
            // `for await (`.
            "await" if was.head == Some(Paren::ForHead) => {
                carry.head = was.head;
                Slash::Regex
            }
            // Not reserved. It is the keyword only in the head of a `for`, after
            // the binding (`for (x of /re/)`); anywhere else it is an identifier,
            // and `of / 2` divides.
            "of" if parens.last() == Some(&Paren::ForHead)
                && slash != Slash::Regex
                && !was.declarator =>
            {
                // The binding before the keyword ends in a value or in the `}`
                // of a pattern. After anything else that leaves a `/` open, as
                // `yield` and `await` do, `of` is as open as they are.
                if slash == Slash::Divides || was.closes {
                    Slash::Regex
                } else {
                    Slash::Unknown
                }
            }
            // Keywords in a generator, an async function or a module, and
            // variable names in a sloppy script.
            "yield" | "await" => Slash::Unknown,
            // After another of them, `let` is the name declared: `var let`.
            "var" | "let" | "const" if !was.declarator => {
                carry.declarator = true;
                carry.declares = true;
                // After a `,` it may be a name too.
                carry.whole = was.declares;
                Slash::Divides
            }
            _ if REGEX_KEYWORDS.contains(&word) => {
                carry.blocks = matches!(word, "else" | "do");
                carry.declares = matches!(word, "break" | "continue");
                Slash::Regex
            }
            _ => {
                carry.whole = was.declares;
                Slash::Divides
            }
        };
        // What follows may make it a call or a declaration, across any space
        // or comment: `require /* c */ ('x')`.
        carry.calls = match word {
            "require" => Some(true),
            "import" => Some(false),
            _ => None,
        };
        carry.sources = matches!(word, "import" | "from");
    }
    Some(())
}

/// Reserved words after which a `/` opens a regular expression: none is a value,
/// and none can name a variable, so there is nothing for a `/` to divide.
const REGEX_KEYWORDS: &[&str] = &[
    "return",
    "typeof",
    "case",
    "do",
    "else",
    "in",
    "instanceof",
    "new",
    "delete",
    "void",
    "throw",
    "default",
    "break",
    "continue",
    "debugger",
];

/// Skip template text from `i` to its closing backtick or its next `${`, and say
/// what a `/` means there: a closed template is a value, and a substitution is
/// code, where an expression starts. Opening a substitution pushes its brace depth.
fn skip_template_text(
    bytes: &[u8],
    mut i: usize,
    substitutions: &mut Vec<usize>,
) -> (usize, Slash) {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'`' => return (i + 1, Slash::Divides),
            b'$' if bytes.get(i + 1) == Some(&b'{') => {
                substitutions.push(0);
                return (i + 2, Slash::Regex);
            }
            _ => i += 1,
        }
    }
    (i, Slash::Divides)
}

/// The offset just past the number at `i`, which is at its first digit or at the
/// `.` before it.
///
/// A decimal integer takes the `.` after it as its decimal point, so `1. in x`
/// tests a property. Every other `.` after a number reads one: after a
/// fraction or an exponent (`1.5.in`, `1e+2.in`), and after a number that takes
/// neither, which a `0x`, `0o` or `0b` one, a BigInt and an octal `017` are.
fn number_end(bytes: &[u8], i: usize) -> usize {
    let digits = |mut end: usize| {
        while bytes
            .get(end)
            .is_some_and(|b| b.is_ascii_digit() || *b == b'_')
        {
            end += 1;
        }
        end
    };
    let mut end = digits(i);
    let integer = &bytes[i..end];
    let radix = integer == b"0"
        && matches!(
            bytes.get(end),
            Some(b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
        );
    let octal = integer.len() > 1
        && integer[0] == b'0'
        && integer.iter().all(|b| (b'0'..=b'7').contains(b));
    if radix || octal || bytes.get(end) == Some(&b'n') {
        while bytes.get(end).is_some_and(|b| is_word_byte(*b)) {
            end += 1;
        }
        return end;
    }
    if bytes.get(end) == Some(&b'.') {
        end = digits(end + 1);
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let exponent = end + 1 + usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        if bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
            end = digits(exponent);
        }
    }
    end
}

/// Skip the regular expression opening at `i`, returning the offset just past its
/// closing `/`. A `/` inside a `[...]` class does not close it, and a newline ends
/// the skip, since a literal cannot span lines.
fn skip_regex(bytes: &[u8], i: usize) -> usize {
    let mut end = i + 1;
    let mut class = false;
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end += 2,
            b'\n' => return end,
            b']' if class => {
                class = false;
                end += 1;
            }
            b'[' => {
                class = true;
                end += 1;
            }
            b'/' if !class => return end + 1,
            _ => end += 1,
        }
    }
    end
}

/// Record a specifier, refusing one whose name cannot be read off its text.
fn take(specifier: &str, required: bool, out: &mut BTreeMap<String, Uses>) -> Option<()> {
    // Bare specifiers only. A relative or absolute path is the package's own file
    // and travels with it, and a `node:`/`data:`/`file:` URL — plus, on the same
    // test, a Windows drive letter — names nothing in `node_modules`.
    if !specifier.is_empty()
        && !specifier.starts_with(['.', '/', '\\', '#'])
        && !specifier.contains(':')
    {
        // Escapes are not decoded here, so the text of one is not the name
        // Node resolves.
        if specifier.contains('\\') {
            return None;
        }
        let uses = out.entry(specifier.to_string()).or_default();
        uses.required |= required;
        uses.imported |= !required;
    }
    Some(())
}

/// Skip a comment or a quoted string, returning the offset just past it.
///
/// Both directions matter. Scanning INTO a comment invents computed calls that
/// give up a closure; scanning into a string does the same for any bundle that
/// happens to quote `require(`. Neither can hide a real call, since a call inside
/// a string is not a call.
///
/// Comments and quoted strings are half of what [`scan`] steps over to stay in
/// step with the code. Template text and regular expressions are the other half,
/// and [`scan`] handles them itself. A template's `${…}` substitutions are code,
/// so only the text between them is skipped, nested templates included. A `/`
/// opens a regular expression only where an expression can start — after an
/// operator, an opening bracket, the head of an `if`, or a keyword such as
/// `return` — and divides after a value; the expression is skipped with its `\`
/// escapes and `[...]` classes. Without both, one apostrophe in template text or
/// one backtick or quote in a character class opens a "string" that is not there,
/// every quote after it is read inside out, and in a minified bundle — one line —
/// that hides the rest of the file, real `require` calls included.
///
/// Four positions leave a `/` open to both readings without a parser: after a `}`
/// that is not known to close a statement block, since an object literal and a
/// function expression are values; after `yield` or `await`, which are keywords
/// in some functions and variable names in others; on the line after a name
/// that may be whole where it stands, as a variable declared without a value or
/// a label is; and after the specifier of an `import` or `export` declaration.
/// [`scan`] refuses a file with a `/` in any of them.
fn skip_noncode(bytes: &[u8], i: usize) -> Option<usize> {
    match (bytes[i], bytes.get(i + 1)) {
        (b'/', Some(b'/')) => Some(line_end(bytes, i)),
        (b'/', Some(b'*')) => Some(
            bytes[i + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len(), |end| i + end + 4),
        ),
        (quote @ (b'\'' | b'"'), _) => {
            let mut end = i + 1;
            while end < bytes.len() {
                match bytes[end] {
                    // An escape, which a line continuation is too, and that one
                    // ends in `\r\n` as readily as in `\n`.
                    b'\\' if bytes[end + 1..].starts_with(b"\r\n") => end += 3,
                    b'\\' => end += 2,
                    b'\n' => break,
                    b if b == quote => return Some(end + 1),
                    _ => end += 1,
                }
            }
            Some(end)
        }
        _ => None,
    }
}

/// The offset of the first line terminator at or after `i`: `\n`, `\r`, U+2028 or
/// U+2029. A `//` comment and a `#!` line both end there.
fn line_end(bytes: &[u8], i: usize) -> usize {
    let mut end = i;
    while end < bytes.len() {
        match bytes[end] {
            b'\n' | b'\r' => break,
            0xe2 if matches!(bytes.get(end + 1..end + 3), Some([0x80, 0xa8 | 0xa9])) => break,
            _ => end += 1,
        }
    }
    end
}

pub(super) fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

/// Whether `c` separates tokens without being one: a space or a line end.
fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// Whether `c` is part of an identifier or a number. Outside a string or a
/// comment, a non-ASCII character that is not a space can be nothing else.
fn is_word_char(c: char) -> bool {
    if c.is_ascii() {
        is_word_byte(c as u8)
    } else {
        !is_space(c)
    }
}

pub(super) fn skip_spaces(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// The quoted string starting at `i`, and the offset just past its closing quote.
///
/// A template literal deliberately does not count. `` require(`fs`) `` is a
/// constant any parser would fold, but a scanner that accepted the backtick would
/// then have to decide whether a `${` follows — and reading a substitution as a
/// constant is the one mistake here that ships a broken binary.
pub(super) fn literal(text: &str, i: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    let quote = *bytes.get(i)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut end = i + 1;
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end += 2,
            b'\n' => return None,
            byte if byte == quote => return Some((&text[i + 1..end], end + 1)),
            _ => end += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each specifier tagged with the formats it was named in: `r` required,
    /// `i` imported, `ri` both.
    fn scanned(source: &str) -> Option<Vec<String>> {
        let mut out = BTreeMap::new();
        scan(source, &mut out)?;
        Some(
            out.into_iter()
                .map(|(specifier, uses)| {
                    let mut how = String::new();
                    if uses.required {
                        how.push('r');
                    }
                    if uses.imported {
                        how.push('i');
                    }
                    format!("{specifier} {how}")
                })
                .collect(),
        )
    }

    /// Rolldown would head each module with its path from this process, which runs
    /// through the store on the building machine.
    #[test]
    fn an_unminified_chunk_names_no_module_by_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("store/x@1.0.0-aaa/node_modules");
        std::fs::create_dir_all(tree.join("x")).unwrap();
        std::fs::write(
            tree.join("x/package.json"),
            r#"{ "name": "x", "version": "1.0.0", "main": "index.js" }"#,
        )
        .unwrap();
        std::fs::write(tree.join("x/index.js"), "module.exports = \"from x\";\n").unwrap();
        let plan = Plan {
            entries: vec![Entry {
                importer: tree.join("y/importer.js"),
                specifier: "x".to_string(),
                chunk: "node_modules/x/__nub/0.js".to_string(),
                esm: false,
            }],
            ..Default::default()
        };
        let bundled = bundle(&plan, None, false, false).expect("bundle the closure");
        let chunks: String = bundled
            .files
            .iter()
            .map(|file| String::from_utf8_lossy(&file.bytes).into_owned())
            .collect();
        assert!(chunks.contains("from x"), "the chunk is missing: {chunks}");
        assert!(!chunks.contains("#region"), "{chunks}");
    }

    /// The shapes pdfkit really uses, plus the ones that must not add to the set.
    /// Every entry is a stub the artifact would otherwise be missing, so
    /// under-reading fails at run time and over-reading costs an unused chunk.
    #[test]
    fn every_literal_module_reference_is_collected_and_nothing_else_is() {
        assert_eq!(
            scanned(
                r#"
                const fontkit = require('fontkit');
                const { sha256 } = require("@noble/hashes/sha2");
                import LineBreaker from "linebreak";
                const lb = require("linebreak");
                export { x } from './local.js';
                import "./side-effect";
                const fs = require('fs'), n = require('node:zlib');
                const paths = Array.from(new Set([1]));
                const cfg = { from: "not-a-module" };
                const later = await import('png-js');
                const spaced = require /* inline */ ('js-md5');
                import /* side effect */ "brotli";
                "#
            )
            .expect("every specifier here is a literal"),
            [
                "@noble/hashes/sha2 r",
                "brotli i",
                "fontkit r",
                "fs r",
                "js-md5 r",
                // Named both ways in one file, which is what makes a package need
                // a chunk in each format rather than one that is interoped.
                "linebreak ri",
                "png-js i"
            ]
        );
    }

    /// The refusal, and what makes it a refusal rather than an omission: a
    /// computed specifier abandons the whole scan instead of being skipped. A stub
    /// set missing one entry compiles clean and dies on the user's machine.
    #[test]
    fn a_computed_specifier_abandons_the_scan() {
        for computed in [
            "const m = require(name);",
            "const m = require(`@scope/${plat}`);",
            "const m = await import(spec);",
            "const m = require('a' + b);",
            // A comment between the word and its `(` leaves it a call.
            "const m = await import /* lazy */ (spec);",
            // An escape is not decoded, so the name is not the one in the text.
            "const m = require('js-\\x6dd5');",
        ] {
            assert_eq!(
                scanned(&format!("require('fontkit');\n{computed}")),
                None,
                "must give up rather than report a partial set: {computed}"
            );
        }
        // The control. Without it every assertion above would still pass if the
        // scanner simply never reached the second line.
        assert_eq!(
            scanned("require('fontkit');\nconst m = require('js-md5');"),
            Some(vec!["fontkit r".into(), "js-md5 r".into()])
        );
    }

    /// Prose that mentions a call is not a call.
    ///
    /// Each line here gave up a whole closure before comments and strings were
    /// skipped, and the jsdom one is not hypothetical — `lib/jsdom/living/
    /// interfaces.js` really does say "require()s", and it really did cost 1843
    /// payload files. The literal require on the last line is the positive
    /// control: it proves the scan still reaches code AFTER what it skipped,
    /// rather than passing because it gave up early and found nothing.
    #[test]
    fn a_call_named_in_a_comment_or_a_string_is_not_a_call() {
        assert_eq!(
            scanned(
                r#"
                // These are the generated interfaces, and their require()s.
                /* Also see require(name) and
                   import(spec) in the block below. */
                const message = "call require(whatever) to load it";
                const real = require('fontkit');
                "#
            ),
            Some(vec!["fontkit r".into()])
        );
    }

    /// Template text is text: an apostrophe in it opens no string, so the computed
    /// call after it is still seen and still refuses the scan.
    #[test]
    fn an_apostrophe_in_template_text_does_not_hide_a_computed_require() {
        assert_eq!(scanned("const a = `it's`; const m = require(name);"), None);
    }

    /// A substitution is code, at any nesting depth, so a call inside one counts.
    #[test]
    fn a_require_inside_a_template_substitution_is_found() {
        assert_eq!(
            scanned("const s = `it's ${require('fontkit')} done`;"),
            Some(vec!["fontkit r".into()])
        );
        assert_eq!(
            scanned("const s = `a ${`b's ${require('js-md5')}`} c`;"),
            Some(vec!["js-md5 r".into()]),
            "a template nested in a substitution"
        );
        assert_eq!(
            scanned("const s = `${ {a: 1}.a }'`; require('linebreak');"),
            Some(vec!["linebreak r".into()]),
            "a block inside a substitution does not end it early"
        );
    }

    #[test]
    fn a_literal_require_after_template_text_holding_a_quote_is_found() {
        assert_eq!(
            scanned("const a = `it's`; require('fontkit');"),
            Some(vec!["fontkit r".into()])
        );
    }

    /// The token-character class HTTP header parsers carry: a backtick in a
    /// regular expression opens no template.
    #[test]
    fn a_backtick_in_a_regex_class_does_not_hide_the_require_after_it() {
        assert_eq!(
            scanned("var t = /^[!#$%&'*+.0-9A-Z^_`a-z|~-]+$/; e.exports = require(\"buffer\");"),
            Some(vec!["buffer r".into()])
        );
    }

    #[test]
    fn a_quote_in_a_regex_does_not_hide_the_require_after_it() {
        assert_eq!(scanned("var q = /[\"']/g; const m = require(name);"), None);
    }

    /// Every position where an expression starts reads `/` as a regular
    /// expression, so the quote inside each one stays inside it.
    #[test]
    fn a_regex_where_an_expression_starts_is_skipped_whole() {
        for source in [
            "function f(s) { return /'/.test(s); }",
            "const t = typeof /'/;",
            "f(/'/);",
            "const r = /'/;",
            "f(a, /'/);",
            "const l = [.../'/.exec(s)];",
            "export default /'/;",
            // After a number that ends at its decimal point, `in` is the operator.
            "const t = 1. in /'/;",
            // The head of a statement, as against a call or a grouping.
            "if (a) /'/.test(s);",
            "while (a) /'/.test(s);",
            "for (;;) { break\n/'/.test(s); }",
            // A `}` that can only close a statement block.
            "if (a) { b(); }\n/'/.test(s);",
            "try { a(); } finally { b(); }\n/'/.test(s);",
            "const f = () => { a(); }\n/'/.test(s);",
            // `of` in the head of a `for`, after any shape of binding.
            "for (const m of /'/.exec(s)) {}",
            "for (const { a } of /'/.exec(s)) {}",
            "for await (const m of /'/.exec(s)) {}",
            // The word after `var` is the name declared, whatever it spells.
            "for (var of of /'/.exec(s)) {}",
            "for (var let of /'/.exec(s)) {}",
            // A Unicode space is still a space.
            "const r =\u{a0}/'/;",
            // A `!` or a `++` that starts a line applies to what follows it.
            "a()\n!/'/.test(s);",
            "a()\n++/'/.lastIndex;",
        ] {
            assert_eq!(
                scanned(&format!("{source}\nconst m = require(name);")),
                None,
                "{source}"
            );
            assert_eq!(
                scanned(&format!("{source} require('fontkit');")),
                Some(vec!["fontkit r".into()]),
                "{source}"
            );
        }
    }

    /// After a value a `/` divides. Reading it as a regular expression would skip
    /// to the next `/` and step over the call between them: a literal specifier
    /// would go missing, and a computed one would no longer refuse the scan.
    #[test]
    fn a_slash_after_a_value_is_division() {
        for value in [
            "total",
            "(a + b)",
            "list[0]",
            "f(a)",
            "`${n}`",
            "'n'",
            "n++",
            // A keyword spelled as a property or a private name, however spaced.
            "x.in",
            "x. in",
            "x\n  .typeof",
            "x?.delete",
            "x. /* c */ return",
            "this.#in",
            "x.if(a)",
            // A number that ends at its decimal point, and a property of one
            // that has its point or takes none.
            "1.",
            "1.5.in",
            "1e+2.in",
            ".5e-2.in",
            "0e+1.in",
            "0x1e.in",
            "10n.in",
            "017.in",
            // Not reserved, and a keyword only in the head of a `for`.
            "of",
            "café",
            "café\u{a0}",
            // One identifier, whatever its ASCII part spells.
            "caféin",
            "éin",
            // TypeScript's non-null assertion.
            "width!",
        ] {
            let literal = format!("const r = {value} / require('os').cpus().length / 2;");
            assert_eq!(scanned(&literal), Some(vec!["os r".into()]), "{literal}");
            let computed = format!("const r = {value} / require(name).cpus().length / 2;");
            assert_eq!(scanned(&computed), None, "{computed}");
        }
        // A spread is one token, and the number after it another.
        assert_eq!(
            scanned("const o = {....5.in / require('os').cpus().length / 2};"),
            Some(vec!["os r".into()])
        );
        assert_eq!(
            scanned("const o = {....5.in / require(name).cpus().length / 2};"),
            None
        );
        assert_eq!(
            scanned("var a, b = a / 2; while (b --> 0) f(a, b / require('os').cpus().length);"),
            Some(vec!["os r".into()]),
            "a `/` on the same line as a name after `var` or `,`"
        );
        assert_eq!(
            scanned("let of = 12; {}\nof / require('os').cpus().length / 2;"),
            Some(vec!["os r".into()]),
            "`of` opening a statement after a block"
        );
    }

    /// A literal call is read whole, its `)` included. Left behind, that `)` would
    /// close the head around it, and the division after it would read as the
    /// regular expression that follows a head.
    #[test]
    fn a_literal_call_inside_a_head_closes_its_own_paren() {
        for head in ["if", "while", "switch", "with"] {
            let literal = format!("{head} (require('fontkit') / require('os') / 2) {{}}");
            assert_eq!(
                scanned(&literal),
                Some(vec!["fontkit r".into(), "os r".into()]),
                "{literal}"
            );
            let computed = format!("{head} (require('fontkit') / require(name) / 2) {{}}");
            assert_eq!(scanned(&computed), None, "{computed}");
        }
        // The head's own `)` still starts a statement.
        assert_eq!(
            scanned("if (await import('fontkit')) /'/.test(s); require('os');"),
            Some(vec!["fontkit i".into(), "os r".into()])
        );
    }

    /// A `//` comment and a `#!` line end at any line terminator, so the code on
    /// the next line of a file with bare `\r` line endings is still code.
    #[test]
    fn a_comment_ends_at_every_line_terminator() {
        for end in ["\n", "\r", "\r\n", "\u{2028}", "\u{2029}"] {
            for comment in [
                "#!/usr/bin/env node",
                "\u{feff}#!/usr/bin/env node",
                "// it's",
            ] {
                assert_eq!(
                    scanned(&format!("{comment}{end}require('fontkit');")),
                    Some(vec!["fontkit r".into()]),
                    "{comment:?} ended by {end:?}"
                );
            }
        }
    }

    /// A string continued over a line with `\` is one string, whichever way the
    /// line ends.
    #[test]
    fn a_string_continued_over_a_line_is_one_string() {
        for end in ["\n", "\r\n"] {
            assert_eq!(
                scanned(&format!(
                    "const s = 'a\\{end}b'; const m = require('fontkit');"
                )),
                Some(vec!["fontkit r".into()]),
                "{end:?}"
            );
        }
    }

    /// After the `}` of a function or an object literal, after `yield` or
    /// `await`, on the line after a name that may be whole, and after an `import`
    /// declaration, a `/` may open a regular expression or divide, and either
    /// wrong guess hides code: the expression's backtick would open a template
    /// over the `require` after it, and the division's right-hand side would be
    /// skipped as a pattern. Every specifier here is a literal, so only the `/`
    /// or an HTML-like comment can be what abandons the scan.
    #[test]
    fn a_slash_that_could_be_either_abandons_the_scan() {
        for source in [
            "async function f() { await /`/; return require('fontkit'); }",
            "function* g() { yield /`/; return require('fontkit'); }",
            // `of` after one of them is the keyword only where they are names.
            "async function f() { for (const x of await of / require('fontkit') / 2) {} }",
            "for (await of ++of / require('fontkit') / 2) {}",
            "var await = 8; var r = await / require('fontkit') / 2;",
            "var yield = 8; var r = yield / require('fontkit') / 2;",
            "function f() { b(); }\n/`/.test(s); require('fontkit');",
            "const r = {} / require('fontkit') / 2;",
            "const a = <b>x</b>; const c = <d>{require('fontkit')}</d>;",
            // A declaration with no value and a label end where the name does.
            "var a\n/`/.test(s); require('fontkit');",
            "let a = 1, b\n/`/.test(s); require('fontkit');",
            "var let\n/`/.test(s); require('fontkit');",
            "out: for (;;) { break out\n/`/.test(s); } require('fontkit');",
            // The same tokens, dividing.
            "f(a, b\n/ require('fontkit') / 2);",
            "import a from 'os'\n/`/.test(s); require('fontkit');",
            "import 'os'\n/`/.test(s); require('fontkit');",
            // Comments in a script, operators in a module.
            "<!-- it's\nrequire('fontkit');",
            "a()\n--> it's\nrequire('fontkit');",
        ] {
            assert_eq!(scanned(source), None, "{source}");
        }
        // The control: the same tokens with no `/` after them scan to the end.
        assert_eq!(
            scanned("async function f() { await g(); function h() {} require('fontkit'); }"),
            Some(vec!["fontkit r".into()])
        );
        // A `<` before a comment closes no tag.
        assert_eq!(
            scanned("const m: Record</* key */ string, number> = require('fontkit');"),
            Some(vec!["fontkit r".into()])
        );
    }

    /// What a chunk can and cannot stand in for. The refusals are about the module
    /// TYPE the specifier promises, since the `exports` map frees the chunk's own
    /// name — and each one gives up a whole closure, so they have to be narrow.
    #[test]
    fn a_specifier_names_its_package_and_says_whether_a_chunk_can_answer_it() {
        let required = Uses {
            required: true,
            imported: false,
        };
        let imported = Uses {
            required: false,
            imported: true,
        };
        for (specifier, package, by_require, by_import) in [
            ("fontkit", "fontkit", true, true),
            ("@noble/hashes/sha2", "@noble/hashes", true, true),
            ("@scope/pkg", "@scope/pkg", true, true),
            ("pkg/lib/deep.js", "pkg", true, true),
            (
                "@swc/helpers/cjs/_define_property.cjs",
                "@swc/helpers",
                true,
                true,
            ),
            ("pkg/mod.mjs", "pkg", true, true),
            // JSON splits: a require gets the parsed object out of the chunk,
            // where an import is validated against the resolved file's extension.
            ("pkg/package.json", "pkg", true, false),
            // Not JavaScript in any form, so neither way works.
            ("pkg/style.css", "pkg", false, false),
        ] {
            assert_eq!(package_of(specifier), package, "package of {specifier}");
            assert_eq!(
                super::answerable(specifier, required),
                by_require,
                "answerable by require: {specifier}"
            );
            assert_eq!(
                super::answerable(specifier, imported),
                by_import,
                "answerable by import: {specifier}"
            );
        }
    }

    /// The stub is the whole resolution contract, so both halves are asserted: the
    /// condition each chunk answers under, and the fact that `exports` is
    /// exhaustive — a specifier nobody scanned has to fail loudly rather than fall
    /// through to some other file.
    #[test]
    fn the_stub_maps_each_specifier_to_the_chunk_in_the_format_it_was_named_in() {
        let at = "node_modules/@swc/helpers";
        let mut exports: BTreeMap<String, BTreeMap<&'static str, String>> = BTreeMap::new();
        for (index, (specifier, esm)) in [
            ("@swc/helpers/cjs/_define_property.cjs", false),
            ("@swc/helpers/_/_define_property", true),
        ]
        .into_iter()
        .enumerate()
        {
            let entry = Entry {
                importer: PathBuf::new(),
                specifier: specifier.to_string(),
                chunk: chunk_path(at, index, esm),
                esm,
            };
            let (key, condition, target) = export_entry(at, &entry);
            exports.entry(key).or_default().insert(condition, target);
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&stub_manifest("@swc/helpers", Some("0.5.19"), &exports))
                .expect("the stub manifest is JSON");
        assert_eq!(
            manifest["exports"],
            serde_json::json!({
                "./cjs/_define_property.cjs": { "require": "./__nub/0.js" },
                "./_/_define_property": { "import": "./__nub/1.mjs" },
            }),
            "each specifier answers under the condition it was named in"
        );
        assert_eq!(manifest["version"], "0.5.19");
    }
}
