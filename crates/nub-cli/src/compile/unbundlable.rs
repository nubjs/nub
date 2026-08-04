//! Deciding, from a package's manifest alone, that it cannot be bundled.
//!
//! Some packages resolve a native artifact at runtime by computing its path, so
//! no import graph can see it:
//!
//! ```text
//! require(path.join(__dirname, 'build', 'Release', 'foo.node'))
//! require('node-gyp-build')(__dirname)
//! require('bindings')('foo.node')
//! ```
//!
//! The ecosystem's answer to this is not analysis. `@vercel/nft` is a 1,424-line
//! partial evaluator that EXECUTES `bindings()` / `nodeGypBuild()` at build time to
//! recover the path, backed by 21 hand-written per-package AST rewrites — and
//! Next.js, built on it, still ships a 79-entry hand-maintained default list
//! (`packages/next/src/lib/server-external-packages.jsonc`, read as
//! `EXTERNAL_PACKAGES` in `build/webpack-config.ts`). Nothing in the surveyed prior
//! art detects these statically.
//!
//! The observation this module rests on: a package that CALLS those resolvers
//! DECLARES them, and a package built by napi-rs advertises its per-platform
//! binaries as optional dependencies. Both are ordinary manifest fields. So the
//! expensive question ("what path will this compute?") is replaced by a cheap one
//! ("does this package look like it builds or loads native code?"), which the
//! manifest answers without reading a byte of source or running anything.
//!
//! Measured against real registry manifests: every one of `bcrypt`, `sqlite3`,
//! `canvas`, `isolated-vm`, `better-sqlite3`, `sharp`, `@node-rs/argon2` and
//! `cpu-features` is caught, and `pino`, `keyv` and `@prisma/client` are not.
//!
//! This is deliberately not the whole answer. Packages that are pure JS yet still
//! unbundlable — `pino` handing `join(__dirname, 'worker.js')` to a worker thread,
//! `keyv` requiring a computed backend — carry no manifest signal at all and are
//! covered by the curated list instead. Over-firing here costs a package its
//! tree-shaking; under-firing ships a binary that fails at runtime, so each rule is
//! kept narrow enough to name what it saw.

use std::path::Path;

use serde_json::Value;

/// Packages whose whole purpose is locating a native addon at runtime. Depending
/// on one is a declaration that this package's real entry point is a `.node` file
/// the import graph will never name.
///
/// `node-addon-api` and `nan` are headers rather than resolvers, so they prove the
/// package COMPILES native code rather than that it locates it — same conclusion
/// for our purposes, and they catch `better-sqlite3`, which declares no resolver.
const RESOLVER_DEPENDENCIES: &[&str] = &[
    "node-gyp-build",
    "bindings",
    "node-pre-gyp",
    "@mapbox/node-pre-gyp",
    "prebuild-install",
    "node-addon-api",
    "nan",
];

/// Platform tokens as they appear in a napi-rs sidecar package name. Node's own
/// `process.platform` spelling, which is what the generators emit.
const PLATFORM_TOKENS: &[&str] = &[
    "darwin", "linux", "win32", "android", "freebsd", "openbsd", "sunos", "aix",
];

/// Packages that cannot be bundled for reasons no manifest field records.
///
/// Every rule above reads a declaration the package made about itself. These made
/// none: they are ordinary JavaScript whose behaviour at run time — handing a
/// worker thread a path, requiring a backend chosen from a string, replacing the
/// module loader — defeats bundling without leaving a trace in `package.json`.
///
/// A list is where the ecosystem lands when analysis runs out: Next.js still
/// maintains 79 default entries after years of investment in static analysis. But
/// a list is not where it has to STAY, and the entries below are only the ones
/// [`computed_asset_read`] cannot reach — every one of them defeats bundling
/// through a specifier no static form carries, not through a file it reads.
///
/// Importing Next's list wholesale was considered and measured. It does not
/// transfer: roughly half its entries are native packages the manifest rules
/// already catch, a third are build tools it externalizes to keep a dev server's
/// rebuilds fast rather than for correctness, and it names `express` — which
/// bundles correctly, and whose eject cost a compiled server 0.5 MB and its
/// tree-shaking for nothing. An entry never imported is free, because this
/// classifier only ever runs on a package the bundler actually resolved; an entry
/// that IS imported is not. So entries arrive one at a time, with a reproduction.
///
/// A list is a maintenance cost with no principled end, so it stays small and
/// each entry carries the behaviour that put it here. `--unbundled` covers
/// anything not yet listed, which is what keeps this from being load-bearing.
const KNOWN_UNBUNDLABLE: &[(&str, &str)] = &[
    // Hands `join(__dirname, 'worker.js')` to thread-stream, which requires that
    // path from a worker thread — a path that exists only in the source tree.
    (
        "pino",
        "it loads its transport worker from a path built at run time",
    ),
    // Named as a string in pino config and required inside the worker, so the
    // specifier never appears as an import anywhere the bundler can see.
    (
        "pino-pretty",
        "it is named as a string and required inside a worker",
    ),
    (
        "pino-roll",
        "it is named as a string and required inside a worker",
    ),
    (
        "thread-stream",
        "it starts a worker from a path given at run time",
    ),
    // Requires a backend chosen from a connection string.
    ("keyv", "it requires a storage backend chosen at run time"),
    // Requires an undeclared dependency unconditionally.
    ("config", "it requires a dependency it does not declare"),
    // Both replace the module loader itself, which a bundle has already resolved
    // past by the time they run.
    (
        "import-in-the-middle",
        "it patches the module loader, which a bundle has already bypassed",
    ),
    (
        "require-in-the-middle",
        "it patches the module loader, which a bundle has already bypassed",
    ),
    // Reads its default stylesheet with readFileSync from a path built off
    // __dirname, which points at the payload root once bundled. Listed rather
    // than left to the flag because the failure it produces is the worst shape
    // available: bundling jsdom but unbundling its css-tree dependency — the
    // obvious first move, since css-tree is what the build error names —
    // COMPILES CLEAN and dies at run time on the stylesheet.
    (
        "jsdom",
        "it reads a data file from a path built at run time",
    ),
    // Both ship data beside their source and reach it through __dirname: pdfkit
    // its built-in fonts, geoip-lite its address database. Same failure as jsdom
    // and the same reason for listing it — the build succeeds and the binary
    // fails, so there is no build-time error to attach advice to.
    (
        "pdfkit",
        "it reads its bundled fonts from a path built at run time",
    ),
    (
        "geoip-lite",
        "it reads its address database from a path built at run time",
    ),
    // The WebAssembly shape of the same thing: the payload is a .wasm module
    // rather than a font or a table, but it is still a file read from a computed
    // path. Listed for the same reason — the build succeeds and the read fails.
    (
        "sql.js",
        "it reads its WebAssembly module from a path built at run time",
    ),
];

/// Why a package must ship unbundled. Kept as distinct variants rather than a
/// boolean so a wrong verdict can be traced to the exact rule that produced it —
/// these rules are heuristics over a hostile corpus and will need tuning against
/// packages nobody here has seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Depends on a package whose job is resolving a native addon at runtime.
    ResolverDependency(String),
    /// Advertises per-platform sidecar packages, the napi-rs shape.
    ///
    /// Worth more than it looks: a napi-rs package splits into a JS-only wrapper
    /// and one binary package per platform, and only the binary one holds a
    /// `.node`. A forward dependency walk therefore misses the package the
    /// application actually imports. Reading the fan off the wrapper's own
    /// manifest answers that without walking anything.
    NapiPlatformFan(usize),
    /// Carries a build step that produces a native artifact.
    BuildMarker(&'static str),
    /// On the list of packages whose run-time behaviour defeats bundling.
    Known(&'static str),
    /// Builds a path from `__dirname` / `import.meta.url` and ships a file that
    /// is not code — so the path names something the bundle does not carry.
    ///
    /// Carries the two halves of the evidence so a wrong verdict names what
    /// convinced it, rather than leaving someone to guess which file did.
    ComputedAssetRead { code: String, asset: String },
    /// Named by the user, not by any rule.
    ///
    /// No detector reaches every package — a pure-JS package handing a worker a
    /// path built at run time carries no manifest signal at all — so the flag is
    /// part of the design rather than an admission of one.
    Forced,
}

impl Reason {
    /// User-facing explanation. These end up in a build error telling someone why
    /// their package was left unbundled, so they name the evidence, not the rule.
    pub fn describe(&self) -> String {
        match self {
            Reason::ResolverDependency(dep) => {
                format!("it depends on `{dep}`, which resolves a native addon at runtime")
            }
            Reason::NapiPlatformFan(count) => {
                format!("it declares {count} per-platform binary packages (napi-rs layout)")
            }
            Reason::BuildMarker(marker) => format!("its manifest declares {marker}"),
            Reason::Known(why) => (*why).to_string(),
            Reason::ComputedAssetRead { code, asset } => format!(
                "`{code}` builds a path at run time and it ships `{asset}`, which is not code"
            ),
            Reason::Forced => "you asked for it with --unbundled".to_string(),
        }
    }
}

/// Decide whether the package installed at `root` can be bundled.
///
/// Returns the FIRST rule that fires; the rules overlap heavily on real packages
/// (`sqlite3` trips three) and the caller only needs one reason to report. The
/// manifest rules run first because they are free — [`computed_asset_read`] is
/// the only one that reads the package tree, so it never runs for a package a
/// declaration already settled.
pub fn classify(root: &Path, manifest: &Value) -> Option<Reason> {
    // Checked first: an entry here is a fact somebody established by hand, where
    // every rule below is an inference from a declaration.
    if let Some(name) = manifest.get("name").and_then(Value::as_str) {
        if let Some((_, why)) = KNOWN_UNBUNDLABLE.iter().find(|(n, _)| *n == name) {
            return Some(Reason::Known(why));
        }
        // A resolver is judged by the rule it earns its dependents. Its whole body
        // is `require(<a path it just computed>)`, so bundling it moves that call
        // into a chunk while the addon stays where the caller's package is. Only
        // reachable since packages INSIDE an eject closure are classified on their
        // own — a resolver is never the package an application imports.
        if RESOLVER_DEPENDENCIES.contains(&name) {
            return Some(Reason::ResolverDependency(name.to_string()));
        }
    }
    if let Some(dep) = resolver_dependency(manifest) {
        return Some(Reason::ResolverDependency(dep));
    }
    if let Some(count) = napi_platform_fan(manifest) {
        return Some(Reason::NapiPlatformFan(count));
    }
    if let Some(marker) = build_marker(manifest) {
        return Some(Reason::BuildMarker(marker));
    }
    computed_asset_read(root)
}

fn resolver_dependency(manifest: &Value) -> Option<String> {
    let deps = manifest.get("dependencies")?.as_object()?;
    RESOLVER_DEPENDENCIES
        .iter()
        .find(|name| deps.contains_key(**name))
        .map(|name| (*name).to_string())
}

/// The count of optional dependencies that look like this package's own
/// per-platform binaries.
///
/// Matching on the package's own base name is what keeps this from firing on an
/// ordinary optional dependency that merely happens to be platform-specific: the
/// sidecars are named after their parent (`sharp` -> `@img/sharp-darwin-arm64`,
/// `@node-rs/argon2` -> `@node-rs/argon2-linux-x64-gnu`), so the parent's name
/// appearing inside the child's is the signal.
///
/// Two is the floor. A single platform-named optional dependency is an ordinary
/// dependency; a FAN across platforms is the generator's fingerprint.
fn napi_platform_fan(manifest: &Value) -> Option<usize> {
    let optional = manifest.get("optionalDependencies")?.as_object()?;
    let name = manifest.get("name")?.as_str()?;
    let base = name.rsplit('/').next()?;
    if base.is_empty() {
        return None;
    }
    let count = optional
        .keys()
        .filter(|dep| dep.contains(base) && PLATFORM_TOKENS.iter().any(|token| dep.contains(token)))
        .count();
    (count >= 2).then_some(count)
}

/// A manifest field that only exists because the package builds something native.
///
/// `gypfile` is set by npm when the package ships a `binding.gyp`. `binary` is
/// node-pre-gyp's remote-artifact descriptor. An install-phase script is the
/// weakest of the three — plenty of pure-JS packages run one — but it is the only
/// signal `cpu-features` carries, and a false positive costs tree-shaking on one
/// package where a false negative ships a broken binary.
fn build_marker(manifest: &Value) -> Option<&'static str> {
    if manifest.get("gypfile").and_then(Value::as_bool) == Some(true) {
        return Some("gypfile");
    }
    if manifest.get("binary").is_some_and(|v| v.is_object()) {
        return Some("a `binary` block");
    }
    let scripts = manifest.get("scripts")?.as_object()?;
    ["install", "preinstall", "postinstall"]
        .iter()
        .find(|phase| scripts.contains_key(**phase))
        .map(|phase| match *phase {
            "install" => "an `install` script",
            "preinstall" => "a `preinstall` script",
            _ => "a `postinstall` script",
        })
}

/// Directories a published package carries but an importing application never
/// loads. `bin` and `scripts` matter most: a dependency's CLI is exactly where a
/// legitimate `readFileSync(join(__dirname, 'usage.txt'))` lives, and counting
/// it flags libraries — `ejs` and `mathjs` both, measured — that bundle fine.
const UNREACHED_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "spec",
    "__snapshots__",
    "fixture",
    "fixtures",
    "example",
    "examples",
    "doc",
    "docs",
    "man",
    "benchmark",
    "benchmarks",
    "bench",
    "coverage",
    "types",
    "typings",
    "flow-typed",
    "e2e",
    "bin",
    "scripts",
    "script",
    "tools",
];

/// Extensions the bundler consumes, so a file with one is never a runtime asset.
const CODE_EXTENSIONS: &[&str] = &["js", "cjs", "mjs", "jsx", "ts", "tsx", "mts", "cts"];

/// Extensions that are metadata rather than payload. `json` is here because the
/// bundler inlines it, and `map` because a source map is build output.
const METADATA_EXTENSIONS: &[&str] = &[
    "md", "markdown", "map", "json", "yml", "yaml", "lock", "flow",
];

/// Words in a filename that mark it as paperwork. Without these, one
/// `ThirdPartyNotices.txt` or `unicode-license.txt` is enough to unbundle a
/// package that reads nothing — measured on `mathjs` and `full-icu`.
const PAPERWORK: &[&str] = &[
    "license",
    "licence",
    "notice",
    "readme",
    "changelog",
    "history",
    "authors",
    "contributors",
    "code_of_conduct",
    "thirdparty",
    "third-party",
    "third_party",
];

/// Functions that turn a base directory into a path. The verdict rests on one of
/// these RECEIVING `__dirname` — not on the name appearing anywhere — which is
/// what separates a real read from `ejs`, whose only `__filename` is a token it
/// concatenates into the source of a template it is compiling.
const PATH_BUILDERS: &[&str] = &[
    "join",
    "resolve",
    "relative",
    "dirname",
    "normalize",
    "readFileSync",
    "readFile",
    "createReadStream",
    "pathToFileURL",
    "fileURLToPath",
    "URL",
];

/// The expressions that name the directory a module was loaded from.
const BASES: &[&str] = &["__dirname", "__filename", "import.meta.url"];

/// How far back from a base expression the opening `(` of its call may sit. Long
/// enough for `path.resolve(process.env.HOME || __dirname` and no further.
const CALL_WINDOW: usize = 80;

/// Bounds on the tree walk. A package is scanned once per compile, and the walk
/// stops the moment both halves of the evidence are in hand, so these only cap
/// the pathological case.
///
/// The byte cap belongs to [`computed_asset_read`], not to the walk: it is a
/// budget on reading, and a walk that dropped the file would tell
/// [`loadable_code`] a package ships nothing it in fact ships.
const MAX_ENTRIES: usize = 4000;
const MAX_DEPTH: usize = 8;
const MAX_FILE_BYTES: u64 = 8 << 20;

/// Detect a package that resolves one of its own shipped files through a path it
/// computes at run time.
///
/// This is the failure the manifest rules cannot reach and the one with the worst
/// shape: `tiktoken`, `esbuild-wasm`, `web-tree-sitter`, `yoga-wasm-web`,
/// `tesseract.js-core` and `@jimp/plugin-print` each COMPILE CLEAN and die on the
/// user's machine, because `join(__dirname, 'x.wasm')` points into the bundle's
/// extracted payload where no `x.wasm` was ever written. There is no build error
/// to attach advice to, which is why every such package found so far arrived as a
/// bug report and then as a hand-written list entry.
///
/// Both halves are required. The path-building expression alone fires on any
/// package that locates a sibling MODULE (which the bundler already followed);
/// a non-code file alone fires on anything shipping a `.wasm` it inlines. The
/// conjunction, measured over 473 installed packages, flagged 8 packages that
/// genuinely read a shipped file and 1 that does not (`fontkit`, whose `src/`
/// tries are inlined into the `dist/` its manifest actually points at).
///
/// The scan is textual, so a path builder inside a comment or a string would
/// count. That is the safe direction to be wrong in — the cost is one package's
/// tree-shaking, where the cost of missing one is a binary that fails on a user's
/// machine — and no such case appeared in the corpus.
fn computed_asset_read(root: &Path) -> Option<Reason> {
    let mut code = Vec::new();
    let mut asset = None;
    collect(root, root, 0, &mut 0, &mut code, &mut asset)?;
    // The cheap half decides whether the expensive half runs at all: most
    // packages ship no asset, and those pay only the directory walk.
    let asset = asset?;
    for file in code {
        let path = root.join(&file);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if builds_a_path(&text) {
            return Some(Reason::ComputedAssetRead { code: file, asset });
        }
    }
    None
}

/// Every file in `root` an importing application could load, package-relative.
///
/// The same reachability rules [`computed_asset_read`] scans under — no tests, no
/// type declarations, nothing under a directory a dependent never enters — which
/// is why it shares the walk rather than repeating it. `None` means the walk was
/// truncated, so a caller reasoning about what the package DOESN'T contain has to
/// treat the answer as unknown.
pub fn loadable_code(root: &Path) -> Option<Vec<String>> {
    let mut code = Vec::new();
    collect(root, root, 0, &mut 0, &mut code, &mut None)?;
    Some(code)
}

/// Walk the package, splitting what it ships into code to scan and the first
/// asset found. Returns `None` only when the walk was truncated, which makes a
/// truncated scan report nothing rather than a verdict from partial evidence.
fn collect(
    root: &Path,
    dir: &Path,
    depth: usize,
    seen: &mut usize,
    code: &mut Vec<String>,
    asset: &mut Option<String>,
) -> Option<()> {
    if depth > MAX_DEPTH {
        return Some(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Some(());
    };
    for entry in entries.flatten() {
        *seen += 1;
        if *seen > MAX_ENTRIES {
            return None;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // A dot-prefixed entry is tooling (`.github`, `.bin`), never a published
        // asset, and `node_modules` belongs to a different package.
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if !UNREACHED_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name)) {
                collect(root, &path, depth + 1, seen, code, asset)?;
            }
            continue;
        }
        if !kind.is_file() || is_unreached_file(name) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if is_code(name) {
            code.push(rel);
        } else if asset.is_none() && is_asset(name) {
            *asset = Some(rel);
        }
    }
    Some(())
}

/// A test or type-declaration file: shipped, but never on the path an importing
/// application takes. `jimp` publishes its `src/*.node.test.ts` files, and their
/// `__dirname` reads of snapshot images are what flagged four of its plugins.
fn is_unreached_file(name: &str) -> bool {
    if name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts") {
        return true;
    }
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    stem.ends_with(".test") || stem.ends_with(".spec")
}

fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.').map(|(_, ext)| ext)
}

fn is_code(name: &str) -> bool {
    extension(name).is_some_and(|ext| CODE_EXTENSIONS.iter().any(|c| ext.eq_ignore_ascii_case(c)))
}

/// A shipped file the bundler will not carry: it has an extension, that
/// extension is neither code nor metadata, and its name is not paperwork.
fn is_asset(name: &str) -> bool {
    let Some(ext) = extension(name) else {
        return false;
    };
    if METADATA_EXTENSIONS
        .iter()
        .any(|m| ext.eq_ignore_ascii_case(m))
    {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    !PAPERWORK.iter().any(|w| lower.contains(w))
}

/// Whether `text` hands a module's own location to something that builds a path.
///
/// Two forms, both measured against the corpus: the base expression sits inside a
/// path builder's argument list, or it is concatenated with a leading separator
/// (`__dirname + "/data"`, `` `${__dirname}/data` ``).
fn builds_a_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    for base in BASES {
        let mut from = 0;
        while let Some(offset) = text[from..].find(base) {
            let start = from + offset;
            let end = start + base.len();
            if inside_path_call(bytes, start) || concatenated_with_separator(bytes, end) {
                return true;
            }
            from = end;
        }
    }
    false
}

/// Scan back for the `(` that opens the call this expression is an argument to,
/// and check what it belongs to. A `)` first means the expression sits after a
/// closed group rather than inside an open one, so the scan stops there.
fn inside_path_call(bytes: &[u8], start: usize) -> bool {
    let floor = start.saturating_sub(CALL_WINDOW);
    let mut i = start;
    while i > floor {
        i -= 1;
        match bytes[i] {
            b')' => return false,
            b'(' => {
                let mut end = i;
                while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
                    end -= 1;
                }
                return PATH_BUILDERS
                    .iter()
                    .any(|name| word_ends_at(bytes, end, name));
            }
            _ => {}
        }
    }
    false
}

/// Whether `name` occupies the bytes ending at `end` as a whole identifier —
/// so `resolve` matches `path.resolve` but not `myResolve`.
fn word_ends_at(bytes: &[u8], end: usize, name: &str) -> bool {
    let Some(start) = end.checked_sub(name.len()) else {
        return false;
    };
    if &bytes[start..end] != name.as_bytes() {
        return false;
    }
    start == 0 || !matches!(bytes[start - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
}

/// Whether the bytes after a base expression join a path onto it: `+ "/x"`, or
/// the close of a `${…}` immediately followed by a separator.
fn concatenated_with_separator(bytes: &[u8], end: usize) -> bool {
    let mut i = end;
    let skip_spaces = |bytes: &[u8], mut i: usize| {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        i
    };
    if i < bytes.len() && bytes[i] == b'}' {
        return matches!(bytes.get(i + 1), Some(b'/' | b'\\' | b'.'));
    }
    i = skip_spaces(bytes, i);
    if bytes.get(i) != Some(&b'+') {
        return false;
    }
    i = skip_spaces(bytes, i + 1);
    if !matches!(bytes.get(i), Some(b'\'' | b'"' | b'`')) {
        return false;
    }
    matches!(bytes.get(i + 1), Some(b'/' | b'\\' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    /// A package tree built from `(relative path, contents)` pairs.
    fn tree(label: &str, files: &[(&str, &str)]) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "nub-unbundlable-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// An empty root, for the manifest rules — which read no files, so the tree
    /// they are handed must contribute no evidence of its own.
    fn bare() -> PathBuf {
        tree("bare", &[])
    }

    /// Real manifests, real verdicts.
    ///
    /// The negative half is what makes this a test rather than a demonstration:
    /// `pino`, `keyv` and `@prisma/client` are all on Next.js's unbundlable list
    /// for reasons NO manifest rule can see, so they must come back clean here.
    /// A change that starts catching them has not got smarter, it has started
    /// guessing — and would cost every such package its tree-shaking.
    #[test]
    fn real_native_packages_are_caught_and_pure_js_ones_are_not() {
        // Shapes taken from the published manifests.
        let bcrypt = json!({
            "name": "bcrypt",
            "dependencies": { "node-addon-api": "^8", "node-gyp-build": "^4" },
            "scripts": { "install": "node-gyp-build" },
        });
        let sharp = json!({
            "name": "sharp",
            "optionalDependencies": {
                "@img/sharp-darwin-arm64": "0.34.4",
                "@img/sharp-linux-x64": "0.34.4",
                "@img/sharp-win32-x64": "0.34.4",
            },
        });
        let cpu_features = json!({
            "name": "cpu-features",
            "scripts": { "install": "node-gyp rebuild" },
        });
        let isolated_vm = json!({ "name": "isolated-vm", "gypfile": true });

        assert!(matches!(
            classify(&bare(), &bcrypt),
            Some(Reason::ResolverDependency(_))
        ));
        assert_eq!(classify(&bare(), &sharp), Some(Reason::NapiPlatformFan(3)));
        assert_eq!(
            classify(&bare(), &cpu_features),
            Some(Reason::BuildMarker("an `install` script"))
        );
        assert_eq!(
            classify(&bare(), &isolated_vm),
            Some(Reason::BuildMarker("gypfile"))
        );

        // `pino` and `keyv` are on the curated list, so they are excluded from the
        // must-not-fire set — see the dedicated test below. What belongs here is a
        // package that is pure JS, unlisted, and must be bundled whole.
        for pure in [
            json!({ "name": "@prisma/client" }),
            json!({ "name": "lodash", "dependencies": {} }),
            json!({ "name": "date-fns" }),
        ] {
            assert_eq!(
                classify(&bare(), &pure),
                None,
                "a pure-JS package must not trip a manifest rule: {pure}"
            );
        }
    }

    /// One platform-named optional dependency is not a fan.
    ///
    /// Without the floor this fires on any package with a single optional
    /// platform-specific dependency — `fsevents` is the classic — and quietly
    /// unbundles it. The paired positive proves the floor is what rejects it,
    /// rather than the name matching failing for some other reason.
    #[test]
    fn a_single_platform_sidecar_is_not_a_napi_fan() {
        let one = json!({
            "name": "watcher",
            "optionalDependencies": { "watcher-darwin-arm64": "1.0.0" },
        });
        assert_eq!(
            classify(&bare(), &one),
            None,
            "one sidecar is an ordinary dependency"
        );

        let two = json!({
            "name": "watcher",
            "optionalDependencies": {
                "watcher-darwin-arm64": "1.0.0",
                "watcher-linux-x64": "1.0.0",
            },
        });
        assert_eq!(
            classify(&bare(), &two),
            Some(Reason::NapiPlatformFan(2)),
            "two platforms is the fan, so the rejection above is the floor at work"
        );
    }

    /// A sidecar must be named after ITS OWN parent.
    ///
    /// Otherwise any package depending on two platform-named things — a build tool
    /// pulling in several `@esbuild/*` binaries, say — is misread as native.
    /// The curated list catches what no manifest field records.
    ///
    /// These packages declare nothing that distinguishes them: `pino` looks
    /// exactly like any package with a dependency. They defeat bundling through
    /// run-time behaviour — handing a worker a path, requiring a backend named by
    /// a string — so a rule reading declarations cannot reach them and the list is
    /// the only mechanism that can.
    ///
    /// The negative half is the control. Matching loosely (a prefix, a substring)
    /// would quietly unbundle `pino-http` and `keyv-redis`, which are ordinary
    /// packages, so the match must be on the exact name.
    #[test]
    fn the_curated_list_matches_exact_names_only() {
        for (name, _) in KNOWN_UNBUNDLABLE {
            let manifest = json!({ "name": name });
            assert!(
                matches!(classify(&bare(), &manifest), Some(Reason::Known(_))),
                "{name} is on the list and must be caught by it"
            );
        }

        for near in [
            "pino-http",
            "keyv-redis",
            "config-chain",
            "thread-stream-x",
            "jsdom-global",
        ] {
            assert_eq!(
                classify(&bare(), &json!({ "name": near })),
                None,
                "{near} merely resembles a listed name and must still be bundled"
            );
        }
    }

    #[test]
    fn platform_sidecars_belonging_to_another_package_do_not_count() {
        let unrelated = json!({
            "name": "my-app",
            "optionalDependencies": {
                "@esbuild/darwin-arm64": "0.28.1",
                "@esbuild/linux-x64": "0.28.1",
            },
        });
        assert_eq!(
            classify(&bare(), &unrelated),
            None,
            "sidecars named after a DIFFERENT package say nothing about this one"
        );
    }

    /// The `tiktoken` shape, which compiles clean and dies on `Missing
    /// tiktoken_bg.wasm`: a path built off `__dirname` naming a file the bundler
    /// does not carry.
    ///
    /// The negative half is the whole point of the rule having two halves. A
    /// package that builds a path to a sibling MODULE is doing what every
    /// multi-file package does, and the bundler already followed that edge — so
    /// without the asset it must not fire.
    #[test]
    fn a_path_built_at_run_time_unbundles_only_when_a_non_code_file_is_shipped() {
        let manifest = json!({ "name": "tokenizer" });

        let reads_an_asset = tree(
            "asset",
            &[
                ("index.js", "const p = path.join(__dirname, 'model.wasm');"),
                ("model.wasm", "\0asm"),
            ],
        );
        assert_eq!(
            classify(&reads_an_asset, &manifest),
            Some(Reason::ComputedAssetRead {
                code: "index.js".into(),
                asset: "model.wasm".into(),
            })
        );

        // Code and JSON only. The JSON is what makes this a control rather than a
        // tautology: a code file can never be read as an asset, but a `.json` is a
        // real file the bundler INLINES — so it must not count as payload, or
        // every package with a data-shaped `.json` beside its source unbundles on
        // any path it happens to build.
        let reads_a_sibling_module = tree(
            "sibling",
            &[
                ("index.js", "const p = path.join(__dirname, 'other.js');"),
                ("other.js", "module.exports = 1;"),
                ("vocab.json", "{\"a\":1}"),
            ],
        );
        assert_eq!(
            classify(&reads_a_sibling_module, &manifest),
            None,
            "the bundler already carries both; the asset half is what makes it a finding"
        );
    }

    /// What separates a read from a coincidence, measured on real packages.
    ///
    /// Each case here unbundled a package that works fine before the rule was
    /// narrowed to its current shape: `ejs` concatenates `__filename` into the
    /// source of a template it compiles, `mathjs` reads a `usage.txt` from its
    /// CLI, and `jimp` publishes tests that read snapshot images.
    #[test]
    fn a_base_expression_that_builds_no_path_is_not_a_read() {
        let manifest = json!({ "name": "renderer" });
        let cases = [
            (
                "emitted",
                "a base name emitted into generated source",
                vec![
                    ("index.js", "out += '  , __filename = ' + name + ';';"),
                    ("data.bin", "x"),
                ],
            ),
            (
                "cli",
                "a read from the package's own command line tool",
                vec![
                    ("index.js", "module.exports = 1;"),
                    ("bin/cli.js", "fs.readFileSync(`${__dirname}/../usage.txt`)"),
                    ("usage.txt", "usage"),
                ],
            ),
            (
                "tested",
                "a read from a published test file",
                vec![
                    ("index.js", "module.exports = 1;"),
                    (
                        "src/render.test.js",
                        "fs.readFileSync(join(__dirname, 'snap.png'))",
                    ),
                    ("src/snap.png", "png"),
                ],
            ),
        ];
        for (label, why, files) in cases {
            let root = tree(label, &files);
            assert_eq!(classify(&root, &manifest), None, "{why} must not unbundle");
        }
    }

    /// Paperwork is not payload.
    ///
    /// `mathjs` ships `math.js.LICENSE.txt` and `playwright-core` a
    /// `ThirdPartyNotices.txt`; either alone was enough to unbundle a package
    /// before the filter, on the strength of an unrelated `__dirname` elsewhere.
    #[test]
    fn a_license_file_is_not_an_asset() {
        let root = tree(
            "paperwork",
            &[
                ("index.js", "const p = path.join(__dirname, 'lib');"),
                ("dist/math.js.LICENSE.txt", "MIT"),
                ("ThirdPartyNotices.txt", "notices"),
            ],
        );
        assert_eq!(classify(&root, &json!({ "name": "mathjs" })), None);
    }
}
