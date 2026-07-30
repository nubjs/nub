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
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

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
}

/// Build the loader map: nub's defaults, then `--loader EXT=TYPE` over the top.
///
/// A user entry wins over a default in BOTH directions — `--loader .wasm=binary`
/// moves `.wasm` out of `file` and into Rolldown's inlining loader — so the flag
/// is a real override rather than an append.
pub fn plan(raw: &[String]) -> Result<Loaders> {
    let mut module_types = BTreeMap::new();
    let mut file: Vec<String> = Vec::new();

    for ext in DEFAULT_TEXT {
        module_types.insert((*ext).to_string(), ModuleType::Text);
    }
    for ext in DEFAULT_FILE {
        file.push((*ext).to_string());
    }

    for token in raw {
        let (ext, name) = token.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "--loader expects EXT=TYPE, got {token:?}\n\
                 \x20\x20For example: --loader .html=file --loader .md=text"
            )
        })?;
        // Lowercased because a filesystem hands back whatever case the file was
        // created with, and `.PNG`/`.JPG` are routine on macOS and Windows.
        // Matching case-exactly failed the build on them, which reads as the
        // loader being broken rather than as a spelling rule.
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        if ext.is_empty() {
            bail!("--loader expects an extension before the `=`, got {token:?}");
        }
        if name == "file" {
            module_types.remove(&ext);
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
        module_types.insert(ext, module_type);
    }

    file.sort();
    file.dedup();
    Ok(Loaders { module_types, file })
}

// ---- the `file` loader --------------------------------------------------------

/// One emitted asset: the payload name it takes, and its bytes.
pub struct FileAsset {
    pub name: String,
    pub bytes: Vec<u8>,
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
    /// Every mapped extension → whether the `file` loader owns it. Both families
    /// are present so [`FilePlugin::claims`] can resolve specificity across them;
    /// see its doc comment.
    by_ext: BTreeMap<String, bool>,
    collected: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl FilePlugin {
    pub fn new(loaders: &Loaders) -> Self {
        let mut by_ext: BTreeMap<String, bool> = loaders
            .module_types
            .keys()
            .map(|ext| (ext.clone(), false))
            .collect();
        by_ext.extend(loaders.file.iter().map(|ext| (ext.clone(), true)));
        Self {
            by_ext,
            collected: Mutex::new(BTreeMap::new()),
        }
    }

    /// Every asset this build emitted, in a deterministic order — `app_sha256`
    /// hashes the payload in order, so a nondeterministic one would re-key the
    /// extraction dir on every compile of identical inputs.
    pub fn take(&self) -> Vec<FileAsset> {
        self.collected
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
            .into_iter()
            .map(|(name, bytes)| FileAsset { name, bytes })
            .collect()
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
    fn claims(&self, id: &str) -> bool {
        let id = id.to_ascii_lowercase();
        id.match_indices('.')
            .find_map(|(i, _)| self.by_ext.get(&id[i + 1..]))
            .copied()
            .unwrap_or(false)
    }
}

/// The payload name for an asset: its stem, a content hash, its extension.
///
/// Content-hashed, not path-hashed, so two imports of the same bytes dedupe to
/// one payload entry while two different files that merely share a basename
/// (`icons/logo.png` and `brand/logo.png`) cannot collide.
fn asset_name(source: &Path, bytes: &[u8]) -> String {
    let stem: String = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
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
    match source.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}-{}.{ext}", &hash[..8]),
        None => format!("{stem}-{}", &hash[..8]),
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
        "import {{ fileURLToPath as __nubFileURLToPath }} from \"node:url\";\n\
         export default __nubFileURLToPath(new URL({}, import.meta.url));\n",
        serde_json::to_string(&format!("./{name}")).expect("a file name serializes")
    )
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
        async move {
            if !claimed {
                return Ok(None);
            }
            let bytes = std::fs::read(&id)
                .map_err(|e| anyhow::anyhow!("reading the imported asset {id}: {e}"))?;
            let name = asset_name(Path::new(&id), &bytes);
            let code = file_module(&name);
            // A silent drop here would ship a chunk naming a file that is not in
            // the payload — a runtime ENOENT with nothing to attribute it to.
            self.collected
                .lock()
                .map_err(|_| {
                    anyhow::anyhow!("the asset collector was poisoned by an earlier panic")
                })?
                .insert(name, bytes);
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
        let p = FilePlugin::new(&plan(&[".tar.gz=file".into()]).expect("valid"));
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
        let p = FilePlugin::new(&loaders);
        assert!(
            !p.claims("/proj/bundle.tar.gz"),
            "the text mapping is more specific and must win"
        );
        assert!(p.claims("/proj/blob.gz"), "the broad mapping still applies");
    }

    // A path component containing a dot must not be read as an extension.
    #[test]
    fn a_dot_in_a_directory_name_does_not_decide_the_loader() {
        let p = FilePlugin::new(&plan(&[]).expect("valid"));
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

    #[test]
    fn extensions_match_case_insensitively() {
        let p = FilePlugin::new(&plan(&[]).expect("valid"));
        assert!(
            p.claims("/proj/PHOTO.PNG"),
            "a screenshot named .PNG is routine"
        );
        assert!(p.claims("/proj/photo.png"));

        let upper = plan(&[".HTML=file".into()]).expect("valid");
        assert!(
            upper.file.iter().any(|e| e == "html"),
            "a flag spelled in caps must map the same extension"
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

    // The emitted module is what every `file` import evaluates to, so its two
    // load-bearing properties are worth pinning: an absolute path (not the
    // chunk-relative one Rolldown's built-in would give), and a base of
    // `import.meta.url` rather than cwd.
    #[test]
    fn the_file_module_resolves_against_the_module_not_the_cwd() {
        let code = file_module("logo-a1b2c3d4.png");
        assert!(code.contains("import.meta.url"), "{code}");
        assert!(code.contains("fileURLToPath"), "{code}");
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
