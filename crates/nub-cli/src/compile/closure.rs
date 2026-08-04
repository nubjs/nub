//! Bundling the bundlable half of an eject closure.
//!
//! Ejecting a package used to eject everything it depends on. The predicate was
//! evaluated once, at the root, and the rest of the closure inherited the verdict
//! by association — an association nothing justified. `pdfkit` must ship verbatim
//! because it reads its fonts off `__dirname`; `fontkit` never had to, and
//! `@swc/helpers` shipped 438 files so that two of them could be loaded.
//!
//! That matters more than the file count suggests. The launcher byte-verifies the
//! WHOLE extracted tree on every warm start (`app_cache_tree_matches`), so an
//! unloaded payload file is not free: 37 of pdfkit's 1194 files are ever loaded
//! and all 1194 are checked, at roughly 20 µs each.
//!
//! So [`super::native`] classifies every package in the closure on its own and
//! hands the clean ones here. Each specifier an ejected package names across the
//! boundary becomes an entry, and the chunk answering it is emitted AT the path
//! Node will look for — `node_modules/fontkit/index.js` — beside a stub manifest.
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
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rolldown::plugin::__inner::SharedPluginable;
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookUsage, Plugin, PluginContext, SharedLoadPluginContext,
};
use rolldown::{BundlerBuilder, BundlerOptions, InputItem};
use rolldown_common::{
    IsExternal, ModuleType, Output, OutputFormat, Platform, RawMinifyOptions, ResolveOptions,
};

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
    /// Where the chunk lands: `node_modules/fontkit/__nub/0.js`.
    pub chunk: String,
    pub esm: bool,
}

#[derive(Default)]
pub struct Plan {
    pub entries: Vec<Entry>,
    /// Packages left verbatim. The bundle leaves every one of them to Node: the
    /// real files are already in the payload, and a copy inside a chunk would be a
    /// second module object with a different `__dirname`.
    pub verbatim: BTreeSet<String>,
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
/// not misreport what it carries, an `exports` map naming every specifier the
/// closure answers, and `main` for the bare one.
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
/// One consequence is worth stating rather than hiding: a package left verbatim is
/// external here, and a shared chunk resolves it by walking up from the app root,
/// which finds a top-level `node_modules` and not a nested one. Every tree
/// measured so far puts the verbatim packages at the top level, and the shape that
/// would break — a bundled package requiring a verbatim one out of a NESTED
/// install — has not been produced. It stays a known limit rather than machinery
/// nothing has needed.
pub fn bundle(plan: &Plan, minify: bool, keep_names: bool) -> Result<Vec<IslandFile>> {
    let mut files = Vec::new();
    for esm in [false, true] {
        let entries: Vec<&Entry> = plan.entries.iter().filter(|e| e.esm == esm).collect();
        if !entries.is_empty() {
            files.extend(build(&entries, &plan.verbatim, esm, minify, keep_names)?);
        }
    }
    Ok(files)
}

fn build(
    plan: &[&Entry],
    verbatim: &BTreeSet<String>,
    esm: bool,
    minify: bool,
    keep_names: bool,
) -> Result<Vec<IslandFile>> {
    let verbatim = verbatim.clone();
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
        chunk_filenames: Some(
            format!(
                "__nub_closure/[name]-[hash].{}",
                if esm { "mjs" } else { "js" }
            )
            .into(),
        ),
        minify: Some(RawMinifyOptions::Bool(minify)),
        keep_names: Some(keep_names),
        external: Some(IsExternal::Fn(Some(Arc::new(
            move |specifier: &str, _importer: Option<&str>, _resolved: bool| {
                let external = verbatim.contains(package_of(specifier));
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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow!("building the closure bundler runtime: {e}"))?;
    let output = rt.block_on(async move {
        let mut bundler = BundlerBuilder::default()
            .with_options(options)
            .with_plugins(vec![entries as SharedPluginable])
            .build()
            .map_err(|e| anyhow!("closure bundler init: {e:?}"))?;
        bundler
            .generate()
            .await
            .map_err(|e| anyhow!("bundling the eject closure failed: {e:?}"))
    })?;
    Ok(output
        .assets
        .iter()
        .filter_map(|asset| match asset {
            Output::Chunk(chunk) => Some(IslandFile {
                name: chunk.filename.to_string(),
                bytes: chunk.code.as_bytes().to_vec(),
                executable: false,
            }),
            Output::Asset(_) => None,
        })
        .collect())
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

/// Collect the literal specifiers in `text`, refusing the moment one is computed.
///
/// Textual, like the rest of nub's package scanning, and for the same reason: a
/// published package ships bundles measured in megabytes, and the question — is
/// the argument one quoted string? — needs no scope information. It over-reports,
/// since a `require(` inside a comment counts, and that is the safe direction: a
/// specifier nobody loads costs an unused chunk, where a missed one costs a
/// binary that fails on the machine it was shipped to.
fn scan(text: &str, out: &mut BTreeMap<String, Uses>) -> Option<()> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Prose is not code, and this is not pedantry: jsdom's `interfaces.js`
        // carries the phrase "require()s" in a comment, which read as a call with
        // a computed argument and gave up jsdom's ENTIRE closure — 1843 payload
        // files that stayed exactly as they were.
        if let Some(after) = skip_noncode(bytes, i) {
            i = after;
            continue;
        }
        if !is_word_byte(bytes[i]) || (i > 0 && is_word_byte(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_word_byte(bytes[i]) {
            i += 1;
        }
        let word = &text[start..i];
        if !matches!(word, "require" | "import" | "from") {
            continue;
        }
        let after = skip_spaces(bytes, i);
        // A call: its argument is the whole verdict, so anything that is not one
        // quoted string gives up. `'a' + b` is the case worth naming — the string
        // is real, and reading it as the specifier would report a module nobody
        // asked for while hiding the one they did.
        if word != "from" && bytes.get(after) == Some(&b'(') {
            let (specifier, end) = literal(text, skip_spaces(bytes, after + 1))?;
            if bytes.get(skip_spaces(bytes, end)) != Some(&b')') {
                return None;
            }
            take(specifier, word == "require", out);
            i = end;
            continue;
        }
        // `import "x"`, and the tail of `import … from "x"` / `export … from "x"`.
        // Whatever clause sits between the two words is skipped by matching `from`
        // on its own rather than trying to parse the clause.
        if word != "require" {
            if let Some((specifier, end)) = literal(text, after) {
                take(specifier, false, out);
                i = end;
            }
        }
    }
    Some(())
}

fn take(specifier: &str, required: bool, out: &mut BTreeMap<String, Uses>) {
    // Bare specifiers only. A relative or absolute path is the package's own file
    // and travels with it, and a `node:`/`data:`/`file:` URL — plus, on the same
    // test, a Windows drive letter — names nothing in `node_modules`.
    if !specifier.is_empty()
        && !specifier.starts_with(['.', '/', '\\', '#'])
        && !specifier.contains(':')
    {
        let uses = out.entry(specifier.to_string()).or_default();
        uses.required |= required;
        uses.imported |= !required;
    }
}

/// Skip a comment or a quoted string, returning the offset just past it.
///
/// Both directions matter. Scanning INTO a comment invents computed calls that
/// give up a closure; scanning into a string does the same for any bundle that
/// happens to quote `require(`. Neither can hide a real call, since a call inside
/// a string is not a call.
///
/// The one thing it gets wrong is a regular expression whose character class
/// holds an unescaped `/` — `/[//]/` reads as a line comment, so the rest of that
/// line goes unscanned. That is the unsafe direction, and it is accepted on the
/// evidence: it needs a computed require LATER ON THE SAME LINE naming a package
/// the closure bundled, and no package in the corpus contains the construct at
/// all. Template literals are deliberately not skipped — a `${…}` substitution is
/// code, and skipping to the closing backtick would step over it.
fn skip_noncode(bytes: &[u8], i: usize) -> Option<usize> {
    match (bytes[i], bytes.get(i + 1)) {
        (b'/', Some(b'/')) => Some(
            bytes[i..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |end| i + end),
        ),
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

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn skip_spaces(bytes: &[u8], mut i: usize) -> usize {
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
fn literal(text: &str, i: usize) -> Option<(&str, usize)> {
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
                "#
            )
            .expect("every specifier here is a literal"),
            [
                "@noble/hashes/sha2 r",
                "fontkit r",
                "fs r",
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
