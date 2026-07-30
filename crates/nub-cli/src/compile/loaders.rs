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
        let ext = ext.trim().trim_start_matches('.');
        if ext.is_empty() {
            bail!("--loader expects an extension before the `=`, got {token:?}");
        }
        // Rejected rather than silently mapped: `from_str_with_fallback` would
        // hand back `Custom(_)`, which Rolldown fails on much later with a
        // message that names neither the flag nor the extension.
        if name == "file" {
            module_types.remove(ext);
            if !file.iter().any(|e| e == ext) {
                file.push(ext.to_string());
            }
            continue;
        }
        let module_type = ModuleType::from_known_str(name).map_err(|_| {
            anyhow::anyhow!(
                "--loader {token:?}: unknown loader {name:?}\n\
                 \x20\x20Available: file, text, json, base64, dataurl, binary, empty, js, jsx, ts, tsx"
            )
        })?;
        file.retain(|e| e != ext);
        module_types.insert(ext.to_string(), module_type);
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
    exts: Vec<String>,
    collected: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl FilePlugin {
    pub fn new(loaders: &Loaders) -> Self {
        Self {
            exts: loaders.file.clone(),
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

    fn claims(&self, id: &str) -> bool {
        Path::new(id)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| self.exts.iter().any(|e| e == ext))
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
            if let Ok(mut collected) = self.collected.lock() {
                collected.insert(name, bytes);
            }
            Ok(Some(HookLoadOutput {
                code: code.into(),
                module_type: Some(ModuleType::Js),
                // Nothing in the emitted module observes anything, so an import
                // whose binding goes unused should take the asset out of the
                // payload with it rather than bloating every artifact.
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
