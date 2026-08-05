//! Asset loaders — what a non-JavaScript import evaluates to.
//!
//! Two families, split by who implements them:
//!
//! - **Source-shaped** (`text`, `json`, `base64`, `dataurl`, `binary`, `empty`)
//!   are Rolldown's own `module_types`, handed straight through. Rolldown does
//!   the read, the BOM strip, the JS-string escaping, and the lazy
//!   `export default`, and gets tree-shaking right — reimplementing any of that
//!   here would be strictly worse.
//! - **`file`** is nub's, because Rolldown's built-in equivalent
//!   (`ModuleType::Asset`) yields a path RELATIVE TO THE CHUNK. Relative is
//!   correct for esbuild, whose output sits in a directory the app already
//!   knows; it is wrong here. A compiled artifact runs from a content-hashed
//!   cache directory that the user never cd's into, so `readFileSync("./x.wasm")`
//!   — resolved against `process.cwd()` — reads from wherever the binary
//!   happened to be launched. That fails everywhere except the one directory the
//!   developer tested from, which is the worst available failure mode. nub
//!   therefore emits an ABSOLUTE path computed at runtime from `import.meta.url`
//!   (Bun's `type: "file"` semantics), so the value works from any cwd.
//!
//! IMPORT ATTRIBUTES ARE INERT, AND THAT IS WHY THIS IS AN EXTENSION MAP.
//! Rolldown 1.2.0 parses `with { type: "…" }` into an `import_attribute_map`
//! that is only ever used to re-print attributes on external imports — no hook
//! sees it (`HookResolveIdArgs` has no attributes field, and `load`'s
//! `asserted_module_type` is set by `new URL()` and nothing else). `with { type:
//! "json" }` "works" today purely because `.json` is in Rolldown's default
//! extension map. So the extension decides the loader, and an attribute is
//! accepted-and-ignored — which means the standards-track spelling starts
//! working the moment its extension is mapped, and never contradicts it.
//!
//! The default map is deliberately short: an extension earns a `file` default
//! only if it CANNOT be JavaScript, so mapping it converts a guaranteed build
//! failure into working behavior and can regress nothing. Ambiguous extensions
//! (`.css`, `.html`) are left to Rolldown and to `--loader`.

use std::borrow::Cow;
use std::collections::{BTreeMap, btree_map::Entry};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookUsage, Plugin, SharedLoadPluginContext,
};
use rolldown_common::ModuleType;
use rolldown_common::side_effects::HookSideEffects;
use rolldown_utils::url::clean_url;
use sha2::{Digest, Sha256};

/// Extensions nub maps beyond Rolldown's defaults (which already cover `.js`,
/// `.ts`, `.json`, `.txt`, `.css`, …).
///
/// `.md` is the whole text half: prompt/description files are the common shape
/// in the tooling nub compiles, and `.txt` already worked only because Rolldown
/// happens to default it.
const DEFAULT_TEXT: &[&str] = &["md"];

/// The data formats nub's RUNTIME loads as imports, which the bundler must load
/// the same way or a program that runs under `nub` cannot be compiled.
///
/// `.json` and `.txt` are deliberately absent: Rolldown already handles both, and
/// verified against `nub <file>` they agree. These five do not — each fails the
/// build today with `"default" is not exported by …`, which is the same
/// "guaranteed failure, so mapping can regress nothing" test the `file` defaults
/// meet.
///
/// The parse happens at build time through `nub-data-formats`, the same crate the
/// N-API addon calls at run time, so a document cannot mean one thing run and
/// another compiled. The value is inlined into the module as a JSON literal —
/// there is no parser in the artifact.
const DEFAULT_DATA: &[&str] = &["yaml", "yml", "toml", "jsonc", "json5"];

/// Binary extensions that get `file`. Each is a format no JavaScript parser can
/// read, so the import fails the build today.
const DEFAULT_FILE: &[&str] = &[
    "wasm", // instantiated from the path
    "mp3", "wav", "ogg", "flac", "mp4", "webm", // media
    "png", "jpg", "jpeg", "gif", "webp", "avif", "ico", // images
    "woff", "woff2", "ttf", "otf", // fonts
    "pdf", "zip", "bin", "dat", // opaque payloads
];

/// The resolved loader configuration for one build.
#[derive(Debug, Default)]
pub struct Loaders {
    /// Handed to Rolldown as `module_types`.
    pub module_types: BTreeMap<String, ModuleType>,
    /// Extensions nub's own `file` loader claims. Deliberately NOT in
    /// `module_types`: routing them to `ModuleType::Asset` would hand them to
    /// Rolldown's built-in plugin and produce the chunk-relative path this
    /// module exists to avoid.
    pub file: Vec<String>,
    /// Extensions nub parses at build time and inlines as a JSON literal. Also
    /// not in `module_types`: Rolldown's `json` loader would have to be handed
    /// JSON, and these are not JSON until nub has parsed them.
    pub data: Vec<String>,
}

impl Loaders {
    /// Whether either family claims `ext`.
    ///
    /// Used for extensions nub handles OUTSIDE both families — today only
    /// `.node`, whose addon plugin must yield to a user who mapped it. Neither
    /// default set contains such an extension, so a hit here can only have come
    /// from `--loader`, which is exactly the "the user asked for something else"
    /// signal those plugins need.
    pub fn claims_extension(&self, ext: &str) -> bool {
        self.module_types.contains_key(ext)
            || self.file.iter().any(|e| e == ext)
            || self.data.iter().any(|e| e == ext)
    }
}

/// Build the loader map: nub's defaults, then `--loader EXT=TYPE` over the top.
///
/// A user entry wins over a default in BOTH directions — `--loader .wasm=binary`
/// moves `.wasm` out of `file` and into Rolldown's inlining loader — so the flag
/// is a real override rather than an append.
pub fn plan(raw: &[String]) -> Result<Loaders> {
    let mut module_types = BTreeMap::new();
    let mut file: Vec<String> = Vec::new();
    let mut data: Vec<String> = Vec::new();

    for ext in DEFAULT_TEXT {
        module_types.insert((*ext).to_string(), ModuleType::Text);
    }
    for ext in DEFAULT_FILE {
        file.push((*ext).to_string());
    }
    for ext in DEFAULT_DATA {
        data.push((*ext).to_string());
    }

    for token in raw {
        let (ext, name) = token.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "--loader expects EXT=TYPE, got {token:?}\n\
                 \x20\x20For example: --loader .html=file --loader .md=text"
            )
        })?;
        // Kept EXACTLY as written, case included — see `FilePlugin::claims` for
        // why matching is case-sensitive on both sides.
        let ext = ext.trim().trim_start_matches('.').to_string();
        // The VALUE is trimmed too, or `--loader ".png=file "` reports `unknown
        // loader "file "` while listing `file` as available.
        let name = name.trim();
        if ext.is_empty() {
            bail!("--loader expects an extension before the `=`, got {token:?}");
        }
        if name == "file" {
            module_types.remove(&ext);
            data.retain(|e| *e != ext);
            if !file.contains(&ext) {
                file.push(ext);
            }
            continue;
        }
        // `from_known_str` ALSO accepts `asset`, `copy`, and `css`, none of which
        // may reach Rolldown from this flag. Two of them build clean and produce
        // an artifact that dies on first run: `copy` leaves a literal
        // `import p from "./x.png"` in the chunk (valid JavaScript, so the
        // emitted-chunk gate passes it) which throws
        // ERR_UNKNOWN_FILE_EXTENSION, and `asset` yields the chunk-relative path
        // that this module's `file` loader exists to avoid. `css` merely errors,
        // but from inside Rolldown rather than from the flag the user typed.
        if matches!(name, "asset" | "copy" | "css") {
            bail!(
                "--loader {token:?}: {name:?} is not a loader nub supports\n\
                 \x20\x20For a file that should ship beside the executable and be opened by \
                 path, use: --loader .{ext}=file"
            );
        }
        let module_type = ModuleType::from_known_str(name).map_err(|_| {
            anyhow::anyhow!(
                "--loader {token:?}: unknown loader {name:?}\n\
                 \x20\x20Available: file, text, json, base64, dataurl, binary, empty, js, jsx, ts, tsx"
            )
        })?;
        file.retain(|e| *e != ext);
        data.retain(|e| *e != ext);
        module_types.insert(ext, module_type);
    }

    file.sort();
    file.dedup();
    data.sort();
    data.dedup();
    Ok(Loaders {
        module_types,
        file,
        data,
    })
}

// ---- the `file` loader --------------------------------------------------------

/// One emitted asset: the payload name it takes, and its bytes.
pub struct FileAsset {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The assets one build collected, keyed by the payload name each takes.
///
/// SHARED by every path that embeds a file — this module's `file` loader and the
/// `new URL(…, import.meta.url)` rewrite in [`crate::compile::bundle`] — so one
/// file reached both ways dedupes to a single payload entry instead of shipping
/// twice, and both paths inherit the same naming and the same flat layout.
#[derive(Debug, Default)]
pub struct Assets(Mutex<BTreeMap<String, Vec<u8>>>);

impl Assets {
    /// Record `bytes` under the payload name `source` earns, and return that name
    /// for the emitting code to reference.
    pub fn add(&self, source: &Path, bytes: Vec<u8>) -> Result<String> {
        let name = asset_name(source, &bytes);
        let mut assets = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("the asset collector was poisoned by an earlier panic"))?;
        record_asset(&mut assets, name.clone(), bytes)?;
        Ok(name)
    }

    /// Every asset this build emitted, in a deterministic order — `app_sha256`
    /// hashes the payload in order, so a nondeterministic one would re-key the
    /// extraction dir on every compile of identical inputs.
    pub fn take(&self) -> Vec<FileAsset> {
        self.0
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
            .into_iter()
            .map(|(name, bytes)| FileAsset { name, bytes })
            .collect()
    }
}

fn record_asset(
    assets: &mut BTreeMap<String, Vec<u8>>,
    name: String,
    bytes: Vec<u8>,
) -> Result<()> {
    match assets.entry(name.clone()) {
        Entry::Vacant(entry) => {
            entry.insert(bytes);
            Ok(())
        }
        Entry::Occupied(entry) if entry.get() == &bytes => Ok(()),
        Entry::Occupied(_) => {
            bail!("generated asset name {name:?} identifies different payload bytes")
        }
    }
}

/// Rolldown plugin implementing `file`: emit the bytes as a sibling of the
/// chunks, and evaluate the import to that sibling's ABSOLUTE path at runtime.
///
/// The path is computed rather than baked because the build machine's directory
/// layout has nothing to do with the deploy machine's. `import.meta.url` is the
/// only base that is right on both, and it works from ANY chunk because every
/// chunk and every emitted asset land in one flat directory — that flatness is
/// the invariant this loader rests on, and [`crate::compile::bundle`] asserts it.
#[derive(Debug, Default)]
pub struct FilePlugin {
    /// Every mapped extension → the loader that owns it. Both families are
    /// present so [`FilePlugin::claims`] can resolve specificity across them; see
    /// its doc comment. The loader's NAME is kept, not just which family it is
    /// in, so [`FilePlugin::case_mismatch`] can suggest the flag verbatim.
    by_ext: BTreeMap<String, &'static str>,
    collected: Arc<Assets>,
    /// Hints for imports whose extension is mapped only in another case. Kept
    /// here rather than raised from the hook — see the `load` implementation.
    case_hints: Mutex<std::collections::BTreeSet<String>>,
}

impl FilePlugin {
    pub fn new(loaders: &Loaders, collected: Arc<Assets>) -> Self {
        let mut by_ext: BTreeMap<String, &'static str> = loaders
            .module_types
            .iter()
            .map(|(ext, ty)| (ext.clone(), loader_name(ty)))
            .collect();
        by_ext.extend(loaders.file.iter().map(|ext| (ext.clone(), "file")));
        Self {
            by_ext,
            collected,
            case_hints: Mutex::new(std::collections::BTreeSet::new()),
        }
    }

    /// What to add to a failed bundle's error, if a case-mismatched extension
    /// could explain it.
    pub fn case_hints(&self) -> Vec<String> {
        self.case_hints
            .lock()
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether the `file` loader owns `id`.
    ///
    /// MIRRORS ROLLDOWN'S OWN EXTENSION LOOKUP, and must keep doing so. Rolldown
    /// matches the suffix after EVERY dot, left to right, taking the first hit —
    /// so `x.tar.gz` tries `tar.gz` before `gz`, and the most specific mapping
    /// wins. Matching only `Path::extension()` here (the last component) made the
    /// two families disagree: `--loader .tar.gz=text` worked while
    /// `--loader .tar.gz=file` silently did not claim the file, which then
    /// reached the JS parser and failed the build.
    ///
    /// The lookup spans BOTH families rather than just this plugin's own, because
    /// this hook runs BEFORE Rolldown consults `module_types`. Checking only the
    /// `file` set would let a broad `.gz=file` steal a file that a more specific
    /// `.tar.gz=text` had claimed — the loser being decided by hook order rather
    /// than by specificity.
    ///
    /// CASE-SENSITIVE, DELIBERATELY, and it must stay that way. Lowercasing here
    /// is tempting — `.PNG` and `.JPG` are ordinary on macOS and Windows — but it
    /// can only ever fix nub's half. Rolldown looks its own `module_types` up
    /// with an exact-case `get`, and the text family goes through THAT, so
    /// lowercasing only here bought `PHOTO.PNG` at the cost of making `PROMPT.MD`
    /// fail the build: one convenience, two rules, and the difference invisible
    /// until it bites. Routing the text family through this plugin instead would
    /// not close it either, since `HookLoadOutput.code` is a string and the
    /// byte-oriented loaders (`base64`, `binary`, `dataurl`) need bytes. One rule
    /// for both families is worth more than a convenience that holds in one;
    /// an uppercase extension is spelled `--loader .PNG=file`.
    pub fn claims(&self, id: &str) -> bool {
        id.match_indices('.')
            .find_map(|(i, _)| self.by_ext.get(&id[i + 1..]))
            .is_some_and(|loader| *loader == "file")
    }

    /// The mapped spelling of an extension `id` matches in every respect BUT
    /// case, if there is one.
    ///
    /// Case-sensitivity is the deliberate rule (see [`Self::claims`]), and this
    /// is what keeps it teachable. Without it a `PHOTO.PNG` import fails inside
    /// Rolldown — `MISSING_EXPORT` for the file family, `PARSE_ERROR` for the
    /// text family — neither of which names the extension or hints that a
    /// mapping exists under another case.
    fn case_mismatch(&self, id: &str) -> Option<(&str, &'static str)> {
        id.match_indices('.').find_map(|(i, _)| {
            let suffix = &id[i + 1..];
            self.by_ext
                .iter()
                .find(|(mapped, _)| mapped.eq_ignore_ascii_case(suffix) && *mapped != suffix)
                .map(|(mapped, loader)| (mapped.as_str(), *loader))
        })
    }
}

/// The `--loader` spelling of a Rolldown module type, for echoing back in a
/// diagnostic. Only the types [`plan`] admits appear here.
fn loader_name(ty: &ModuleType) -> &'static str {
    match ty {
        ModuleType::Text => "text",
        ModuleType::Json => "json",
        ModuleType::Base64 => "base64",
        ModuleType::Dataurl => "dataurl",
        ModuleType::Binary => "binary",
        ModuleType::Empty => "empty",
        ModuleType::Jsx => "jsx",
        ModuleType::Ts => "ts",
        ModuleType::Tsx => "tsx",
        _ => "js",
    }
}

/// The payload name for an asset: its stem, a full content hash, its extension.
///
/// Content-hashed, not path-hashed, so two imports of the same bytes dedupe to
/// one payload entry while two different files that merely share a basename
/// (`icons/logo.png` and `brand/logo.png`) cannot collide.
///
/// The hash suffix is UNCONDITIONAL, and that is what keeps a generated name off
/// Win32's reserved-device list: a source named `aux.txt` ships as
/// `aux-<sha256>.txt`, whose stem is no longer `AUX`. `assemble_app`'s payload-name
/// gate refuses an `--include`d `aux.txt` for a Windows target while the same file
/// reached through `new URL(…)` or a `file` import compiles and runs — the
/// asymmetry is safe only because of this construction, so a refactor that makes
/// the suffix conditional (an unhashed passthrough, a "keep the original name"
/// mode) reopens the hole with nothing failing at build time.
///
/// LOWERCASED, so a generated name can never collide with another one on a
/// filesystem that folds case. `a/Logo.bin` and `b/logo.bin` otherwise yield two
/// payload names differing only in case: identical bytes, one file on Windows or
/// on default APFS, and the build fails a collision check the user cannot act on
/// because neither name is one they wrote. Folding here makes the pair dedupe to
/// a single entry — correct, since a shared content hash means shared bytes —
/// while different content still separates on the hash.
fn asset_name(source: &Path, bytes: &[u8]) -> String {
    let stem: String = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let stem = if stem.is_empty() {
        "asset".to_string()
    } else {
        stem
    };
    let hash = format!("{:x}", Sha256::digest(bytes));
    asset_name_with_hash(source, &stem, &hash)
}

fn asset_name_with_hash(source: &Path, stem: &str, hash: &str) -> String {
    match source.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}-{hash}.{}", ext.to_lowercase()),
        None => format!("{stem}-{hash}"),
    }
}

/// The module a `file` import evaluates to.
///
/// `fileURLToPath`, not `new URL(...).pathname`: the latter leaves an import
/// percent-encoded (a space becomes `%20`) and on Windows yields `/C:/…`, so
/// every `fs` call against it fails on exactly the paths that are hardest to
/// debug.
fn file_module(name: &str) -> String {
    // `./` is explicit rather than incidental: a bare relative reference whose
    // first component contained a colon would parse as a URL SCHEME, and the
    // prefix removes that class of surprise entirely.
    format!(
        "const record = process[Symbol.for(\"nub.compile.bootstrap\")];\n\
         export default record.getBuiltin(\"node:url\").fileURLToPath(new URL({}, import.meta.url));\n",
        serde_json::to_string(&format!("./{name}")).expect("a file name serializes")
    )
}

/// Parses nub's data-format imports at build time and inlines the result.
///
/// The runtime reaches these through `nub-native`'s parsers; this calls the same
/// functions in the same crate, so the two surfaces cannot disagree about what a
/// document means. The emitted module is a JSON literal and nothing else — no
/// parser reaches the artifact, which is the point: the work moved to build time.
#[derive(Debug, Default)]
pub struct DataPlugin {
    /// Extensions this plugin owns, after `--loader` has had its say.
    exts: Vec<String>,
}

impl DataPlugin {
    pub fn new(loaders: &Loaders) -> Self {
        Self {
            exts: loaders.data.clone(),
        }
    }

    /// Longest matching extension wins, so `.tar.yaml` is claimed by a `yaml`
    /// mapping rather than missed. Case-sensitive on both sides, matching
    /// [`FilePlugin::claims`] — Node's own extension matching is exact.
    fn claims(&self, id: &str) -> Option<&str> {
        id.match_indices('.')
            .find_map(|(i, _)| self.exts.iter().find(|e| *e == &id[i + 1..]))
            .map(String::as_str)
    }
}

impl Plugin for DataPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:data-loader")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::Load
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let id = clean_url(args.id).to_string();
        let claimed = self.claims(&id).map(str::to_string);
        async move {
            let Some(ext) = claimed else {
                return Ok(None);
            };
            let source = std::fs::read_to_string(&id)
                .map_err(|e| anyhow::anyhow!("reading the imported data file {id}: {e}"))?;
            let value = match ext.as_str() {
                "yaml" | "yml" => nub_data_formats::parse_yaml(&source),
                "toml" => nub_data_formats::parse_toml(&source),
                "jsonc" => nub_data_formats::parse_jsonc(&source),
                "json5" => nub_data_formats::parse_json5(&source),
                // `exts` is built from DEFAULT_DATA, so this is unreachable
                // unless that list grows without a match arm to answer it.
                other => anyhow::bail!("no data parser for .{other}"),
            }
            // The parser's message already names its format, so the file is all
            // this has to add.
            .map_err(|e| anyhow::anyhow!("{id}: {e}"))?;
            // `to_string` emits valid JSON, which is a subset of the object
            // literal syntax this position accepts — no escaping pass needed.
            let json = serde_json::to_string(&value)
                .map_err(|e| anyhow::anyhow!("serializing the parsed {id}: {e}"))?;
            Ok(Some(HookLoadOutput {
                code: format!("export default {json};\n").into(),
                module_type: Some(ModuleType::Js),
                // A data literal observes nothing, so it must not anchor
                // anything that would otherwise be shaken out.
                side_effects: Some(HookSideEffects::False),
                ..Default::default()
            }))
        }
    }
}

impl Plugin for FilePlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:file-loader")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::Load
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        // Rolldown does not strip a `?query` before handing an id to `load`, and
        // a query is legal in a specifier, so the extension test and the read
        // must both run against the cleaned path.
        let id = clean_url(args.id).to_string();
        let claimed = self.claims(&id);
        let mismatch = (!claimed)
            .then(|| {
                self.case_mismatch(&id)
                    .map(|(mapped, loader)| (mapped.to_string(), loader))
            })
            .flatten();
        async move {
            if !claimed {
                // Recorded, not raised. A plugin error is swallowed by Rolldown
                // into a generic `UNLOADABLE_DEPENDENCY: Could not load <file>`
                // that drops the message entirely, so failing here would replace
                // one unhelpful diagnostic with another. Letting the module fall
                // through keeps Rolldown's own failure and lets `compile::bundle`
                // append this hint to it.
                if let Some((mapped, loader)) = mismatch
                    && let Ok(mut seen) = self.case_hints.lock()
                {
                    let as_written = Path::new(&id)
                        .extension()
                        .map_or_else(|| mapped.clone(), |e| e.to_string_lossy().into_owned());
                    seen.insert(format!(
                        "\x20\x20Extensions match exactly, case included, so .{as_written} is not \
                         covered by the mapping for .{mapped}.\n\
                         \x20\x20Map this spelling too: --loader .{as_written}={loader}"
                    ));
                }
                return Ok(None);
            }
            let bytes = std::fs::read(&id)
                .map_err(|e| anyhow::anyhow!("reading the imported asset {id}: {e}"))?;
            let code = file_module(&self.collected.add(Path::new(&id), bytes)?);
            Ok(Some(HookLoadOutput {
                code: code.into(),
                module_type: Some(ModuleType::Js),
                // The emitted module observes nothing, so it must not anchor
                // anything that would otherwise be shaken out.
                //
                // This does NOT keep unused assets out of the payload — this
                // hook runs during graph construction, before anything is
                // shaken, so the bytes are already collected by then. Dropping
                // the ones no chunk ends up naming is a separate pass over the
                // emitted output; see `retain_referenced` in `compile::bundle`.
                side_effects: Some(HookSideEffects::False),
                ..Default::default()
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_text_and_binary_without_claiming_ambiguous_extensions() {
        let l = plan(&[]).expect("defaults are valid");
        assert_eq!(l.module_types.get("md"), Some(&ModuleType::Text));
        assert!(l.file.iter().any(|e| e == "wasm"));
        assert!(l.file.iter().any(|e| e == "png"));
        // Rolldown has real handling for these; claiming them would regress it.
        assert!(!l.file.iter().any(|e| e == "css"));
        assert!(!l.file.iter().any(|e| e == "html"));
        assert!(!l.file.iter().any(|e| e == "js"));
        assert!(!l.module_types.contains_key("css"));
    }

    // The flag has to move an extension BETWEEN families, not just add to one —
    // otherwise `--loader .wasm=binary` would leave `.wasm` claimed by `file`
    // and the user's override would silently do nothing.
    #[test]
    fn a_user_loader_overrides_a_default_in_both_directions() {
        let to_binary = plan(&[".wasm=binary".into()]).expect("valid");
        assert!(
            !to_binary.file.iter().any(|e| e == "wasm"),
            "the file default must yield"
        );
        assert_eq!(
            to_binary.module_types.get("wasm"),
            Some(&ModuleType::Binary)
        );

        let to_file = plan(&[".md=file".into()]).expect("valid");
        assert!(to_file.file.iter().any(|e| e == "md"));
        assert!(
            !to_file.module_types.contains_key("md"),
            "the text default must yield, or Rolldown would load it as text \
             and nub's file loader would never see it"
        );
    }

    // Rolldown matches the suffix after EVERY dot, so a multi-dot mapping has to
    // as well. Matching only the last component made `--loader .tar.gz=file`
    // silently not claim the file (which then failed in the JS parser) while
    // `--loader .tar.gz=text` worked — the same flag behaving differently
    // depending on which loader family the value named.
    #[test]
    fn a_multi_dot_extension_is_claimed_the_way_rolldown_matches_it() {
        let p = FilePlugin::new(
            &plan(&[".tar.gz=file".into()]).expect("valid"),
            Arc::default(),
        );
        assert!(p.claims("/proj/bundle.tar.gz"));
        assert!(!p.claims("/proj/notes.gz"), "only .tar.gz was mapped");
    }

    // The `file` hook runs BEFORE Rolldown consults its extension map, so
    // without a cross-family lookup a broad `.gz=file` would steal a file that a
    // more specific `.tar.gz=text` had claimed — specificity decided by hook
    // order instead of by the mapping.
    #[test]
    fn the_more_specific_mapping_wins_across_loader_families() {
        let loaders = plan(&[".gz=file".into(), ".tar.gz=text".into()]).expect("valid");
        let p = FilePlugin::new(&loaders, Arc::default());
        assert!(
            !p.claims("/proj/bundle.tar.gz"),
            "the text mapping is more specific and must win"
        );
        assert!(p.claims("/proj/blob.gz"), "the broad mapping still applies");
    }

    // A path component containing a dot must not be read as an extension.
    #[test]
    fn a_dot_in_a_directory_name_does_not_decide_the_loader() {
        let p = FilePlugin::new(&plan(&[]).expect("valid"), Arc::default());
        assert!(p.claims("/home/user.v2/app/icon.png"));
        assert!(!p.claims("/home/user.v2/app/main.ts"));
    }

    // `from_known_str` accepts these, and two of them BUILD CLEAN and produce an
    // artifact that dies on first run — `copy` leaves a literal `import p from
    // "./x.png"` (valid JavaScript, so the emitted-chunk gate passes it), and
    // `asset` yields the chunk-relative path the file loader exists to avoid.
    #[test]
    fn the_rolldown_loaders_that_would_ship_a_broken_binary_are_refused() {
        for bad in ["asset", "copy", "css"] {
            let err = plan(&[format!(".png={bad}")])
                .err()
                .unwrap_or_else(|| panic!("--loader .png={bad} must be refused"));
            let msg = format!("{err:#}");
            assert!(msg.contains(bad), "the error must name the value: {msg}");
            assert!(
                msg.contains("--loader .png=file"),
                "and point at what to use instead: {msg}"
            );
        }
    }

    // Case-sensitivity is the rule, so the error a user hits while learning it
    // has to name the extension and the exact flag — not Rolldown's
    // MISSING_EXPORT (file family) or PARSE_ERROR (text family), neither of
    // which mentions loaders at all. The suggested loader must match the family
    // the mapped extension is in.
    #[test]
    fn a_case_mismatched_extension_names_the_flag_that_fixes_it() {
        let p = FilePlugin::new(&plan(&[]).expect("valid"), Arc::default());
        assert_eq!(p.case_mismatch("/proj/PHOTO.PNG"), Some(("png", "file")));
        assert_eq!(p.case_mismatch("/proj/PROMPT.MD"), Some(("md", "text")));
        assert_eq!(
            p.case_mismatch("/proj/photo.png"),
            None,
            "an exact match is claimed, not reported as a mismatch"
        );
        assert_eq!(
            p.case_mismatch("/proj/notes.rst"),
            None,
            "an extension mapped in no case is not this diagnostic's business"
        );
    }

    #[test]
    fn a_loader_value_is_trimmed_like_its_extension() {
        let l = plan(&[" .png = file ".into()]).expect("padding must not change the mapping");
        assert!(l.file.contains(&"png".to_string()));
    }

    #[test]
    fn extension_matching_is_case_sensitive_in_both_families() {
        let d = FilePlugin::new(&plan(&[]).expect("valid"), Arc::default());
        assert!(d.claims("/proj/photo.png"));
        assert!(
            !d.claims("/proj/PHOTO.PNG"),
            "a lowercase default must not claim an uppercase extension — Rolldown's \
             own lookup is exact-case, so claiming it here would make the file \
             family case-insensitive while the text family stayed exact"
        );

        let upper = plan(&[".PNG=file".into()]).expect("valid");
        assert!(
            FilePlugin::new(&upper, Arc::default()).claims("/proj/PHOTO.PNG"),
            "naming the extension as written is how an uppercase one is mapped"
        );
    }

    #[test]
    fn an_extension_may_be_spelled_with_or_without_its_dot() {
        let dotted = plan(&[".html=file".into()]).expect("valid");
        let bare = plan(&["html=file".into()]).expect("valid");
        assert!(dotted.file.iter().any(|e| e == "html") && bare.file.iter().any(|e| e == "html"));
    }

    #[test]
    fn a_malformed_loader_names_the_flag_and_what_is_available() {
        let err = plan(&["md".into()]).expect_err("no = must be rejected");
        assert!(format!("{err:#}").contains("EXT=TYPE"), "{err:#}");

        let err = plan(&[".md=markdown".into()]).expect_err("unknown loader");
        let msg = format!("{err:#}");
        assert!(msg.contains("markdown"), "must name the bad value: {msg}");
        assert!(msg.contains("text"), "must list what IS available: {msg}");

        assert!(plan(&["=file".into()]).is_err(), "an empty extension");
    }

    // A shared basename is the collision this naming exists to prevent, and
    // identical bytes are the dedup it exists to get for free.
    #[test]
    fn asset_names_separate_by_content_and_collapse_on_it() {
        let a = asset_name(Path::new("/p/icons/logo.png"), b"AAA");
        let b = asset_name(Path::new("/p/brand/logo.png"), b"BBB");
        let c = asset_name(Path::new("/other/logo.png"), b"AAA");
        assert_ne!(a, b, "same basename, different bytes must not collide");
        assert_eq!(a, c, "identical bytes must dedupe to one payload entry");
        assert!(a.starts_with("logo-") && a.ends_with(".png"), "{a}");
    }

    // A generated name must be safe on a filesystem that folds case, or two
    // assets nub named itself can collide — and the collision error would tell
    // the user to rename a file they never wrote.
    #[test]
    fn asset_names_retain_digest_suffixes_past_a_shared_prefix() {
        let a = asset_name_with_hash(
            Path::new("/p/logo.png"),
            "logo",
            "deadbeef00000000000000000000000000000000000000000000000000000001",
        );
        let b = asset_name_with_hash(
            Path::new("/p/logo.png"),
            "logo",
            "deadbeef00000000000000000000000000000000000000000000000000000002",
        );
        assert_ne!(a, b, "a shared digest prefix must not alias payload names");
    }

    #[test]
    fn assets_dedupe_identical_bytes_without_overwriting_distinct_entries() {
        let assets = Assets::default();
        let first = assets
            .add(Path::new("/p/logo.png"), b"first".to_vec())
            .unwrap();
        let second = assets
            .add(Path::new("/p/logo.png"), b"second".to_vec())
            .unwrap();
        let duplicate = assets
            .add(Path::new("/p/logo.png"), b"first".to_vec())
            .unwrap();

        assert_ne!(
            first, second,
            "distinct bytes must retain distinct payloads"
        );
        assert_eq!(first, duplicate, "identical bytes must dedupe");
        let emitted = assets.take();
        assert_eq!(
            emitted.len(),
            2,
            "distinct payloads must not be overwritten"
        );
        assert!(
            emitted
                .iter()
                .any(|asset| asset.name == first && asset.bytes == b"first")
        );
        assert!(
            emitted
                .iter()
                .any(|asset| asset.name == second && asset.bytes == b"second")
        );

        let mut named = BTreeMap::new();
        record_asset(&mut named, "forced-name".to_string(), b"first".to_vec()).unwrap();
        record_asset(&mut named, "forced-name".to_string(), b"first".to_vec()).unwrap();
        assert!(record_asset(&mut named, "forced-name".to_string(), b"second".to_vec()).is_err());
    }

    #[test]
    fn generated_names_cannot_differ_only_in_case() {
        let upper = asset_name(Path::new("/p/a/Logo.BIN"), b"SAME");
        let lower = asset_name(Path::new("/p/b/logo.bin"), b"SAME");
        assert_eq!(
            upper, lower,
            "identical bytes must collapse to one entry rather than to a case-variant pair"
        );
        assert_eq!(upper, upper.to_lowercase(), "{upper} must be lowercase");
        assert_ne!(
            asset_name(Path::new("/p/a/Logo.bin"), b"ONE"),
            asset_name(Path::new("/p/b/logo.bin"), b"TWO"),
            "different bytes must still separate on the hash"
        );
    }

    #[test]
    fn an_asset_name_is_always_a_plain_relative_filename() {
        // The payload name is checked against `is_safe_relative_name` at build
        // AND extract time; a stem carrying a separator would fail there, far
        // from the cause. Sanitizing here keeps it a single component.
        let n = asset_name(Path::new("/p/we ird/../na/me?x.bin"), b"x");
        assert!(
            nub_core::compile::is_safe_relative_name(&n),
            "{n} must be a safe single-component name"
        );
        assert!(!n.contains('/') && !n.contains('?'), "{n}");
    }

    // The hash suffix is what makes a Windows-reserved stem shippable through
    // this path at all, while the same file named by --include is refused. Pin
    // it here so a change to the naming cannot open that hole silently.
    #[test]
    fn a_reserved_windows_stem_is_neutralized_by_the_hash_suffix() {
        for src in ["/p/aux.txt", "/p/CON.json", "/p/com1"] {
            let n = asset_name(Path::new(src), b"x");
            assert!(
                nub_core::compile::is_safe_relative_name_for(
                    nub_core::compile::NameRules::Windows,
                    &n
                ),
                "{src} named {n}, which a Windows target cannot create"
            );
        }
    }

    // The emitted module is what every `file` import evaluates to, so its two
    // load-bearing properties are worth pinning: an absolute path (not the
    // chunk-relative one Rolldown's built-in would give), and a base of
    // `import.meta.url` rather than cwd.
    #[test]
    fn the_file_module_resolves_against_the_module_not_the_cwd() {
        let code = file_module("logo-a1b2c3d4.png");
        assert!(code.contains("import.meta.url"), "{code}");
        assert!(
            code.contains(r#"process[Symbol.for("nub.compile.bootstrap")]"#)
                && code.contains(r#"record.getBuiltin("node:url").fileURLToPath"#),
            "the bootstrap record must provide node:url: {code}"
        );
        assert!(
            !code.contains(r#"from "node:url""#),
            "the generated module must not statically link a redirectable builtin: {code}"
        );
        assert!(code.contains("\"./logo-a1b2c3d4.png\"") || code.contains("\"logo-a1b2c3d4.png\""));
        assert!(!code.contains("process.cwd"), "{code}");
    }

    #[test]
    fn a_file_name_needing_escapes_stays_valid_javascript() {
        let code = file_module("a\"b.png");
        assert!(
            code.contains("\\\""),
            "the name must be JSON-escaped: {code}"
        );
    }
}
