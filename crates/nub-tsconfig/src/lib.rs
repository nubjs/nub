//! In-process tsconfig discovery / parse / `extends` resolution + a `paths`
//! matcher, mirroring `get-tsconfig@4.14.0` (the npm package nub used to call).
//!
//! Only the `tsc`/`findConfigFile` nearest-config path is reproduced (get-tsconfig's
//! `getTsconfig(dir)` with `includes=false`), NOT the language-server
//! `createFilesMatcher` mode. The pieces nub's resolver/transpiler exercise are:
//!   * `findUp` discovery of the nearest `tsconfig.json`,
//!   * the `extends` chain (string-or-array, arrays reverse-merged, nested
//!     `compilerOptions`/`watchOptions` merge, `references` dropped),
//!   * `${configDir}` (TS 5.5) interpolation against the FINAL consuming dir,
//!   * `createPathsMatcher` (exact + longest-prefix wildcard, implicit baseUrl).
//!
//! **Yarn PnP divergence (intentional gap):** get-tsconfig resolves package
//! `extends` (`extends: "@tsconfig/node20/tsconfig.json"`) through
//! `findPnpApi(process.cwd())` when a `.pnp.cjs` is present. This port omits PnP —
//! a package `extends` under Yarn PnP resolves via the plain node_modules walk
//! instead (effectively unsupported). Documented per the simple-over-defensive
//! rule; the node_modules path covers the overwhelmingly common npm/pnpm install.
//!
//! Slash-normalization, `${configDir}`, and the byte-for-byte `tsconfig_hash`
//! (the cache-key component) all match get-tsconfig's output so warm transpile
//! caches survive the JS→Rust move.
//!
//! **Two callers, one answer.** The addon reads this per importer directory to drive
//! transpilation and the additive `paths` resolver; the CLI reads it once before
//! spawn for [`custom_conditions`], because `compilerOptions.customConditions` has to
//! reach Node as `--conditions=` and nothing in-process is early enough to decide
//! that. Sharing the crate is what keeps the `extends` chain from being implemented
//! twice.

// `collapsible_if` fires on intentional nested `if let { if let }` sites; collapsing
// every site is cosmetic churn in what is a verbatim get-tsconfig mirror, so allow it.
#![allow(clippy::collapsible_if)]

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use jsonc_parser::ParseOptions;
use serde_json::{Map, Value};

/// Pin JSONC acceptance to the pre-0.32 dialect. New parser versions default new
/// extensions on, but tsconfig and data imports must not silently become more
/// permissive when this dependency moves.
///
/// Lives here rather than in the addon because a tsconfig `extends` chain reads
/// `package.json` metadata with the same dialect, and the addon's `.jsonc` data
/// importer must stay on the identical pin — one definition, so a future relaxation
/// cannot land on one reader and miss the other.
pub fn jsonc_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: true,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: true,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// Deepest `{`/`[` nesting an externally-authored config or data document may carry.
///
/// Higher than the CLI's bound on its own config files, because these readers take
/// arbitrary user data — a `.json5` fixture is not a `nub.jsonc` — but far enough
/// below the measured abort thresholds (json5 dies between 2500 and 3000 levels on an
/// 8 MiB stack and between 300 and 400 on Windows' 1 MiB) that the margin holds on the
/// smallest stack nub runs on.
pub const MAX_NESTING_DEPTH: usize = 128;

/// The transform-relevant `compilerOptions` slice surfaced to the JS transpiler.
/// `baseUrl`/`paths` are NOT here — they live only in the cached matcher, and
/// `customConditions` is not here either: it is the CLI's, read via
/// [`custom_conditions`], and the transpiler has no use for it.
#[derive(Default, Clone)]
pub struct TsCompilerOptions {
    pub jsx: Option<String>,
    pub jsx_import_source: Option<String>,
    pub jsx_factory: Option<String>,
    pub jsx_fragment_factory: Option<String>,
    pub experimental_decorators: Option<bool>,
    pub emit_decorator_metadata: Option<bool>,
}

/// Result of [`load_tsconfig`]. `path`/`compiler_options` are `None` when no
/// tsconfig was found walking up from `dir` (identical to get-tsconfig → null).
pub struct TsconfigResult {
    /// Absolute, slash-normalized path of the resolved tsconfig.json (for
    /// watch-dep reporting). `None` when no tsconfig found.
    pub path: Option<String>,
    /// The transform-relevant compilerOptions slice. `None` when no tsconfig.
    pub compiler_options: Option<TsCompilerOptions>,
    /// `JSON.stringify`-equivalent of the FULL merged compilerOptions — the exact
    /// string the cache key folds in (`tsconfigHash`). Empty when no tsconfig.
    pub tsconfig_hash: String,
}

/// The cached, fully-resolved tsconfig for one importer directory.
struct Loaded {
    path: Option<String>,
    compiler_options: Option<TsCompilerOptions>,
    tsconfig_hash: String,
    matcher: Option<PathsMatcher>,
    /// Merged `compilerOptions.customConditions`, verbatim. Empty when unset.
    custom_conditions: Vec<String>,
    /// Problems found while parsing this config, already rendered for a user.
    /// Empty on a clean parse. A config that reaches here with entries still
    /// carries whatever options DID resolve — see [`build_loaded`].
    diagnostics: Vec<String>,
}

/// A compiled `paths` matcher (get-tsconfig's `createPathsMatcher` closure state).
struct PathsMatcher {
    base_url: String,
    /// `(pattern, substitutions)`. `pattern` is `Exact(s)` or `Wildcard{prefix,suffix}`.
    paths: Vec<(Pattern, Vec<String>)>,
    /// Whether `baseUrl` was explicitly set (controls the no-match fallback).
    has_base_url: bool,
}

enum Pattern {
    Exact(String),
    Wildcard { prefix: String, suffix: String },
}

/// Process-lifetime cache (mirrors the old JS `tsconfigCache`), keyed on the
/// importer's directory plus the configured tsconfig path if the project names
/// one — not on the *resolved* tsconfig path, which is the lookup's result.
fn cache() -> &'static Mutex<HashMap<String, Arc<Loaded>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Loaded>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Entry ceiling for the memo above. A directory-keyed memo is unbounded BY
/// CONSTRUCTION, and this one lives in the addon — i.e. inside the user's own
/// long-lived Node process, not the one-shot CLI. A process that materializes
/// directories at RUNTIME (codegen, a dev server's generated routes, per-case
/// temp dirs) therefore adds an entry per directory forever; each holds the
/// merged `compilerOptions` plus the compiled `paths` matcher, measured at
/// ~2.6 KB against an ordinary tsconfig, so 100k directories cost ~250 MB.
///
/// Clearing wholesale instead of evicting an LRU is deliberate: this is a PURE
/// memo, so a miss costs only a re-read, and a real project re-warms the handful
/// of directories it actually imports from within the next few resolves. An LRU
/// would buy a better hit rate on a pathological key stream in exchange for
/// per-lookup bookkeeping on the hot path, which is the wrong trade for a cache
/// whose realistic working set is far below the cap.
const CACHE_MAX_ENTRIES: usize = 8192;

/// Parse + resolve the project's tsconfig: `explicit` when the project names
/// one, else the nearest found walking up from `dir`. Memoized per pair.
/// Returns null-ish fields when there is none.
pub fn load_tsconfig(dir: &str, explicit: Option<&str>) -> TsconfigResult {
    let loaded = load_for_dir(dir, explicit);
    TsconfigResult {
        path: loaded.path.clone(),
        compiler_options: loaded.compiler_options.clone(),
        tsconfig_hash: loaded.tsconfig_hash.clone(),
    }
}

/// The merged `compilerOptions.customConditions`, in declaration order, for the
/// tsconfig governing `dir` (`explicit` when the project names one). Empty when there
/// is no tsconfig, when it is unparseable, or when the option is unset.
///
/// TypeScript uses these only to resolve TYPES out of a package's `exports`/`imports`
/// map. nub hands them to Node as `--conditions` as well, so the module the type
/// checker followed is the module that actually loads — the whole point of a
/// `customConditions` setup, and the thing every other runner makes you configure
/// twice ([privatenumber/tsx#574](https://github.com/privatenumber/tsx/issues/574) is
/// this exact request, still open).
///
/// Entries are returned VERBATIM: condition names are case-sensitive, and
/// `normalize_compiler_options` deliberately leaves this key alone (unlike the
/// enum-ish options it lower-cases). Non-string array members are dropped rather than
/// coerced — a malformed value should narrow resolution, never invent a condition.
pub fn custom_conditions(dir: &str, explicit: Option<&str>) -> Vec<String> {
    load_for_dir(dir, explicit).custom_conditions.clone()
}

/// Problems found parsing the tsconfig governing `dir`, already rendered for a
/// user. Empty on a clean parse or when there is no tsconfig.
///
/// [`report_diagnostics`] has already written these to stderr by the time a caller
/// can ask; this is the same list without the capture, which is what lets a test
/// assert the wording. A tsconfig named in `nub.jsonc` never needs it — the config
/// layer refuses an unreadable or unparseable one before the runtime gets here
/// (`project_config.rs`, the `tsconfig` value check).
pub fn diagnostics(dir: &str, explicit: Option<&str>) -> Vec<String> {
    load_for_dir(dir, explicit).diagnostics.clone()
}

/// Config paths this process has already written a warning for. The CLI hands
/// these to the child through [`REPORTED_ENV`] so the addon does not repeat them.
pub fn reported_config_paths() -> Vec<String> {
    reported().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Carries [`reported_config_paths`] across the CLI→Node boundary, newline-separated.
///
/// Both processes parse the same tsconfig on an ordinary run, so without this the
/// user reads the identical warning twice. tsx hit exactly this and gave up on
/// warning altogether over it (its `loadTsconfig` says so in a comment) — but that
/// is a consequence of warning from inside two Node loaders, and nub has a CLI that
/// parses once, before spawn, and can simply say what it already said. Suppression
/// is keyed on the PATH rather than a global quiet flag because the CLI anchors at
/// the CWD while the addon anchors at the importing file's directory, so the two
/// genuinely do read different configs — and the one the CLI never saw still warns.
pub const REPORTED_ENV: &str = "__NUB_TSCONFIG_REPORTED";

fn reported() -> &'static Mutex<Vec<String>> {
    static REPORTED: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    REPORTED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Write each diagnostic to stderr once per process, then remember the path.
///
/// Deduped on the config path, because `build_loaded` runs once per importer
/// DIRECTORY and a project resolves the same tsconfig from many of them. Each
/// message already names the file it belongs to, so nothing is prefixed here.
fn report_diagnostics(config_path: &str, diags: &[String]) {
    if diags.is_empty() {
        return;
    }
    let path = slash(config_path);
    if std::env::var(REPORTED_ENV).is_ok_and(|v| v.split('\n').any(|already| already == path)) {
        return;
    }
    let mut seen = reported().lock().unwrap_or_else(|e| e.into_inner());
    if seen.iter().any(|p| p == &path) {
        return;
    }
    seen.push(path);
    drop(seen);
    for diag in diags {
        eprintln!("Nub: {diag}");
    }
}

/// Shared internal entry — returns the cached `Loaded` (with its matcher) so the
/// resolver can reuse the same state without re-reading the FS.
fn load_for_dir(dir: &str, explicit: Option<&str>) -> Arc<Loaded> {
    // NUL cannot occur in a path on any platform nub runs on, so it separates the
    // two halves of the key without a collision an ordinary path could produce.
    let key = format!("{dir}\0{}", explicit.unwrap_or_default());
    if let Some(hit) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return hit.clone();
    }
    let loaded = Arc::new(build_loaded(dir, explicit));
    let mut entries = cache().lock().unwrap_or_else(|e| e.into_inner());
    if entries.len() >= CACHE_MAX_ENTRIES {
        entries.clear();
    }
    entries.insert(key, loaded.clone());
    loaded
}

fn build_loaded(dir: &str, explicit: Option<&str>) -> Loaded {
    let Some(config_path) = explicit
        .map(str::to_string)
        .or_else(|| find_up(dir, "tsconfig.json"))
    else {
        return Loaded {
            path: None,
            compiler_options: None,
            tsconfig_hash: String::new(),
            matcher: None,
            custom_conditions: Vec::new(),
            diagnostics: Vec::new(),
        };
    };
    let mut diagnostics = Vec::new();
    let parsed = match parse_tsconfig(&config_path, &mut diagnostics) {
        Ok(p) => p,
        Err(e) => {
            // The config's OWN body is unreadable, so there is nothing to keep —
            // unlike a failed `extends`, which leaves this file's options intact
            // (see `inner_parse`). The reason travels out with the empty result so
            // the CLI can refuse the run; a silent fallback to defaults here is
            // indistinguishable from having no tsconfig at all (#731).
            diagnostics.push(e);
            report_diagnostics(&config_path, &diagnostics);
            return Loaded {
                path: Some(slash(&config_path)),
                compiler_options: None,
                tsconfig_hash: String::new(),
                matcher: None,
                custom_conditions: Vec::new(),
                diagnostics,
            };
        }
    };
    report_diagnostics(&config_path, &diagnostics);

    let co = parsed
        .get("compilerOptions")
        .and_then(Value::as_object)
        .cloned();

    let tsconfig_hash = match &co {
        Some(map) => stringify_compiler_options(map),
        None => String::new(),
    };

    let compiler_options = co.as_ref().map(extract_compiler_options);
    let matcher = build_matcher(&slash(&config_path), co.as_ref());
    let custom_conditions = co
        .as_ref()
        .and_then(|map| map.get("customConditions"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Loaded {
        path: Some(slash(&config_path)),
        compiler_options,
        tsconfig_hash,
        matcher,
        custom_conditions,
        diagnostics,
    }
}

/// get-tsconfig's `findUp` (`O`): posix.join the dir + filename, first existing
/// hit wins, stop at FS root.
fn find_up(start: &str, filename: &str) -> Option<String> {
    let mut dir = PathBuf::from(start);
    loop {
        let candidate = dir.join(filename);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
}

// ── `${configDir}` (TS 5.5) ─────────────────────────────────────────
const CONFIG_DIR: &str = "${configDir}";

/// get-tsconfig's `interpolateConfigDir` (`X`): when a value starts with
/// `${configDir}`, replace it with `slash(join(dir, rest))`; else `None`.
fn interpolate_config_dir(value: &str, dir: &str) -> Option<String> {
    value.strip_prefix(CONFIG_DIR).map(|rest| {
        slash(
            &Path::new(dir)
                .join(rest.trim_start_matches('/'))
                .to_string_lossy(),
        )
    })
}

// ── Path helpers (get-tsconfig's `slash` / `normalizeRelativePath`) ──

/// get-tsconfig's `slash` (`h`): `\` → `/` except `\\?\` extended-length prefixes.
fn slash(p: &str) -> String {
    if p.starts_with("\\\\?\\") {
        p.to_string()
    } else {
        p.replace('\\', "/")
    }
}

/// get-tsconfig's `normalizeRelativePath` (`Q`): slash, then ensure a leading
/// `./` when the path is not already `.`/`..`-prefixed.
fn normalize_relative(p: &str) -> String {
    let s = slash(p);
    if is_relative_dotted(&s) {
        s
    } else {
        format!("./{s}")
    }
}

/// get-tsconfig's `C` regex: `^\.{1,2}(/.*)?$` — a `.`/`..` relative specifier.
fn is_relative_dotted(s: &str) -> bool {
    let after = if let Some(r) = s.strip_prefix("..") {
        r
    } else if let Some(r) = s.strip_prefix('.') {
        r
    } else {
        return false;
    };
    after.is_empty() || after.starts_with('/')
}

/// Node `path.resolve(base, p)` for absolute-or-relative `p` (lexical, no FS).
fn path_resolve(base: &str, p: &str) -> String {
    let joined = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        Path::new(base).join(p)
    };
    normalize_lexical(&joined)
}

/// Lexically normalize `.`/`..` segments without touching the filesystem,
/// matching Node's `path.resolve` collapse behavior.
fn normalize_lexical(p: &Path) -> String {
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    let mut result = PathBuf::new();
    for c in out {
        result.push(c.as_os_str());
    }
    result.to_string_lossy().into_owned()
}

/// get-tsconfig's `pathRelative` (`ie`): `normalizeRelativePath(relative(from, to))`.
fn path_relative_normalized(from: &str, to: &str) -> String {
    normalize_relative(&relative(from, to))
}

/// Node `path.relative(from, to)` (lexical).
fn relative(from: &str, to: &str) -> String {
    let from = normalize_lexical(Path::new(from));
    let to = normalize_lexical(Path::new(to));
    let from_segs: Vec<&str> = from.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let to_segs: Vec<&str> = to.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let mut i = 0;
    while i < from_segs.len() && i < to_segs.len() && from_segs[i] == to_segs[i] {
        i += 1;
    }
    let mut parts: Vec<&str> = vec![".."; from_segs.len() - i];
    parts.extend_from_slice(&to_segs[i..]);
    parts.join("/")
}

// ── Parse + extends (get-tsconfig's `parseTsconfig` / `_parseTsconfig`) ──

/// get-tsconfig's `parseTsconfig` (`oe`): the outermost parse for the consuming
/// config — runs `_parseTsconfig`, then `${configDir}` interpolation on the
/// scalar option fields + `rootDirs`/`typeRoots`/`paths` + `files`/`include`/
/// `exclude`, against the FINAL config's dir.
fn parse_tsconfig(config_path: &str, diags: &mut Vec<String>) -> Result<Value, String> {
    let abs = normalize_lexical(Path::new(config_path));
    let mut config = inner_parse(&abs, &[], diags)?;
    let dir = parent_dir(&abs);

    if let Some(co) = config
        .get_mut("compilerOptions")
        .and_then(Value::as_object_mut)
    {
        // qe = ["outDir","declarationDir","outFile","rootDir","baseUrl","tsBuildInfoFile"]
        for field in [
            "outDir",
            "declarationDir",
            "outFile",
            "rootDir",
            "baseUrl",
            "tsBuildInfoFile",
        ] {
            if let Some(s) = co.get(field).and_then(Value::as_str) {
                if let Some(interp) = interpolate_config_dir(s, &dir) {
                    let rel = path_relative_normalized(&dir, &interp);
                    co.insert(field.to_string(), Value::String(rel));
                }
            }
        }
        for field in ["rootDirs", "typeRoots"] {
            if let Some(arr) = co.get(field).and_then(Value::as_array).cloned() {
                let mapped: Vec<Value> = arr
                    .iter()
                    .map(|v| {
                        let s = v.as_str().unwrap_or("");
                        match interpolate_config_dir(s, &dir) {
                            Some(interp) => Value::String(path_relative_normalized(&dir, &interp)),
                            None => Value::String(normalize_relative(s)),
                        }
                    })
                    .collect();
                co.insert(field.to_string(), Value::Array(mapped));
            }
        }
        if let Some(paths) = co.get_mut("paths").and_then(Value::as_object_mut) {
            for (_k, subs) in paths.iter_mut() {
                if let Some(arr) = subs.as_array_mut() {
                    for v in arr.iter_mut() {
                        if let Some(s) = v.as_str() {
                            if let Some(interp) = interpolate_config_dir(s, &dir) {
                                *v = Value::String(interp);
                            }
                        }
                    }
                }
            }
        }
        normalize_compiler_options(co);
    }

    // ve = ["files","include","exclude"] — interpolated for completeness/parity.
    for field in ["files", "include", "exclude"] {
        if let Some(arr) = config.get(field).and_then(Value::as_array).cloned() {
            let mapped: Vec<Value> = arr
                .iter()
                .map(|v| {
                    let s = v.as_str().unwrap_or("");
                    match interpolate_config_dir(s, &dir) {
                        Some(interp) => Value::String(interp),
                        None => v.clone(),
                    }
                })
                .collect();
            config[field] = Value::Array(mapped);
        }
    }

    Ok(config)
}

/// get-tsconfig's `_parseTsconfig` (`pe`): read JSONC, stamp implicit-baseUrl,
/// resolve the `extends` chain (reverse-merge), then relativize `baseUrl`/`rootDir`.
fn inner_parse(
    config_path: &str,
    stack: &[String],
    diags: &mut Vec<String>,
) -> Result<Value, String> {
    let mut config = read_jsonc(config_path)?;
    if !config.is_object() {
        return Err(format!("Failed to parse tsconfig at: {config_path}"));
    }
    let dir = parent_dir(config_path);

    // Implicit baseUrl marker: paths set but baseUrl unset → remember the config
    // dir so the matcher resolves substitutions against it. We stash it under a
    // private key the matcher reads (get-tsconfig uses a Symbol; a non-tsc key
    // string is fine here because we never re-serialize this back to JSON for the
    // user, and `stringify_compiler_options` skips unknown bookkeeping keys).
    if let Some(co) = config
        .get_mut("compilerOptions")
        .and_then(Value::as_object_mut)
    {
        if co.contains_key("paths") && !co.contains_key("baseUrl") {
            co.insert(IMPLICIT_BASE_URL.to_string(), Value::String(dir.clone()));
        }
    }

    // extends chain.
    if let Some(extends_val) = config.get("extends").cloned() {
        let list: Vec<String> = match &extends_val {
            Value::String(s) => vec![s.clone()],
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        if let Some(obj) = config.as_object_mut() {
            obj.remove("extends");
        }
        // arrays processed in reverse (later entries win).
        //
        // A base that will not resolve is RECORDED AND SKIPPED here, not propagated
        // (#731). get-tsconfig throws, which meant one missing `extends` target
        // discarded the whole file — `paths`, decorator flags and `customConditions`
        // alike — leaving nub indistinguishable from "no tsconfig".
        //
        // Skipping is NOT the whole answer, because a run that proceeds on half a
        // config is wrong in the same quiet way. The recorded diagnostic is what the
        // CLI refuses to run on, so the salvage below only ever governs a config the
        // CLI never saw — a dependency's own tsconfig, reached mid-resolve by the
        // addon, where aborting the process is not available. Keeping what the file
        // declares is the better answer there, and matches `tsc` (TS5083) and bun.
        //
        // Circularity lands here too; `stack` is what actually stops the recursion.
        for entry in list.into_iter().rev() {
            match resolve_extends(&entry, &dir, &mut stack.to_vec(), diags) {
                Ok(base) => config = merge_extends(base, config),
                Err(e) => diags.push(format!("{config_path}: {e}")),
            }
        }
    }

    // Relativize baseUrl/rootDir against this config's dir (skip ${configDir}).
    if let Some(co) = config
        .get_mut("compilerOptions")
        .and_then(Value::as_object_mut)
    {
        for field in ["baseUrl", "rootDir"] {
            if let Some(s) = co.get(field).and_then(Value::as_str) {
                if !s.starts_with(CONFIG_DIR) {
                    let abs = path_resolve(&dir, s);
                    let rel = path_relative_normalized(&dir, &abs);
                    co.insert(field.to_string(), Value::String(rel));
                }
            }
        }
    } else {
        config["compilerOptions"] = Value::Object(Map::new());
    }

    Ok(config)
}

/// Private bookkeeping key for the implicit-baseUrl directory (get-tsconfig uses
/// a JS Symbol; we use a key string that can never collide with a real
/// compilerOption and is filtered out of the hash).
const IMPLICIT_BASE_URL: &str = "\u{0}implicitBaseUrl";

/// get-tsconfig's extends merge: top-level shallow spread parent-then-child, with
/// `compilerOptions` (and `watchOptions`) merged NESTED rather than replaced;
/// `references` dropped from extended configs.
fn merge_extends(mut base: Value, child: Value) -> Value {
    if let Some(obj) = base.as_object_mut() {
        obj.remove("references");
    }
    let base_co = base
        .get("compilerOptions")
        .and_then(Value::as_object)
        .cloned();
    let base_wo = base.get("watchOptions").and_then(Value::as_object).cloned();

    let mut merged = base;
    if let (Some(child_obj), Some(merged_obj)) = (child.as_object(), merged.as_object_mut()) {
        for (k, v) in child_obj {
            merged_obj.insert(k.clone(), v.clone());
        }
    }

    // Nested compilerOptions merge: parent then child.
    let child_co = child
        .get("compilerOptions")
        .and_then(Value::as_object)
        .cloned();
    if base_co.is_some() || child_co.is_some() {
        let mut co = base_co.unwrap_or_default();
        if let Some(cc) = child_co {
            for (k, v) in cc {
                co.insert(k, v);
            }
        }
        merged["compilerOptions"] = Value::Object(co);
    }

    // Nested watchOptions merge — only when the parent had one (get-tsconfig
    // gates the merge on `f.watchOptions`).
    if let Some(bwo) = base_wo {
        let child_wo = child
            .get("watchOptions")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut wo = bwo;
        for (k, v) in child_wo {
            wo.insert(k, v);
        }
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("watchOptions".to_string(), Value::Object(wo));
        }
    }

    merged
}

/// get-tsconfig's `resolveExtends` (`Ge`): resolve the extends target path, guard
/// circularity, parse it, drop `references`, and relativize its path-shaped
/// options against the extends-config dir (leaving `${configDir}` untouched).
fn resolve_extends(
    entry: &str,
    from_dir: &str,
    stack: &mut Vec<String>,
    diags: &mut Vec<String>,
) -> Result<Value, String> {
    let resolved = resolve_extends_path(entry, from_dir)
        .ok_or_else(|| format!("File '{entry}' not found."))?;
    if stack.contains(&resolved) {
        return Err(format!(
            "Circularity detected while resolving configuration: {resolved}"
        ));
    }
    stack.push(resolved.clone());
    let parent_config_dir = parent_dir(&resolved);
    let mut config = inner_parse(&resolved, stack, diags)?;
    if let Some(obj) = config.as_object_mut() {
        obj.remove("references");
    }

    if let Some(co) = config
        .get_mut("compilerOptions")
        .and_then(Value::as_object_mut)
    {
        // baseUrl/outDir/declarationDir/rootDir: resolveAndRelativize against the
        // CONSUMING dir (`from_dir`), skipping ${configDir}-prefixed values.
        for field in ["baseUrl", "outDir", "declarationDir", "rootDir"] {
            if let Some(s) = co.get(field).and_then(Value::as_str) {
                if !s.starts_with(CONFIG_DIR) {
                    let v = resolve_and_relativize(from_dir, &parent_config_dir, s);
                    co.insert(field.to_string(), Value::String(v));
                }
            }
        }
        for field in ["rootDirs", "typeRoots"] {
            if let Some(arr) = co.get(field).and_then(Value::as_array).cloned() {
                let mapped: Vec<Value> = arr
                    .iter()
                    .map(|v| {
                        let s = v.as_str().unwrap_or("");
                        if s.starts_with(CONFIG_DIR) {
                            v.clone()
                        } else {
                            Value::String(resolve_and_relativize(from_dir, &parent_config_dir, s))
                        }
                    })
                    .collect();
                co.insert(field.to_string(), Value::Array(mapped));
            }
        }
    }

    // files/include/exclude: prefix-pattern against the consuming dir.
    for field in ["files", "include", "exclude"] {
        if let Some(arr) = config.get(field).and_then(Value::as_array).cloned() {
            let mapped: Vec<Value> = arr
                .iter()
                .map(|v| {
                    let s = v.as_str().unwrap_or("");
                    if s.starts_with(CONFIG_DIR) {
                        v.clone()
                    } else {
                        Value::String(prefix_pattern(from_dir, &parent_config_dir, s))
                    }
                })
                .collect();
            config[field] = Value::Array(mapped);
        }
    }

    Ok(config)
}

/// get-tsconfig's `resolveAndRelativize` (`N`): `relative(consumingDir,
/// join(extendsDir, value))`, slashed, or `"./"` when empty.
fn resolve_and_relativize(consuming_dir: &str, extends_dir: &str, value: &str) -> String {
    let joined = Path::new(extends_dir).join(value);
    let rel = relative(consuming_dir, &joined.to_string_lossy());
    let s = slash(&rel);
    if s.is_empty() { "./".to_string() } else { s }
}

/// get-tsconfig's `prefixPattern` (`ze`): prefix a files/include/exclude glob from
/// the extends config so it resolves against the consuming dir.
fn prefix_pattern(consuming_dir: &str, extends_dir: &str, pattern: &str) -> String {
    let rel = relative(consuming_dir, extends_dir);
    if rel.is_empty() {
        return pattern.to_string();
    }
    let stripped = pattern.strip_prefix("./").unwrap_or(pattern);
    slash(&format!("{rel}/{stripped}"))
}

/// get-tsconfig's `resolveExtendsPath` (`Ve`): the four extends shapes are `".."`,
/// relative (`.`/`..`), absolute, and package/bare. The package case walks the
/// nearest node_modules and honors the package.json `exports`/`tsconfig` field
/// with conditions `require` then `types`. PnP is omitted (see module docs).
fn resolve_extends_path(entry: &str, from_dir: &str) -> Option<String> {
    let mut n = entry.to_string();
    if entry == ".." {
        n = Path::new(&n).join("tsconfig.json").to_string_lossy().into();
    }
    if entry.starts_with('.') {
        n = path_resolve(from_dir, &n);
    }
    if Path::new(&n).is_absolute() {
        if Path::new(&n).is_file() {
            return Some(n);
        }
        if !n.ends_with(".json") {
            let with_json = format!("{n}.json");
            if Path::new(&with_json).is_file() {
                return Some(with_json);
            }
        }
        return None;
    }

    // Package / bare: split scope, walk to nearest node_modules/<pkg>.
    let mut segs = entry.split('/');
    let first = segs.next().unwrap_or("");
    let (pkg, subpath) = if first.starts_with('@') {
        let scope_pkg = segs.next().unwrap_or("");
        let rest: Vec<&str> = segs.collect();
        (format!("{first}/{scope_pkg}"), rest.join("/"))
    } else {
        let rest: Vec<&str> = segs.collect();
        (first.to_string(), rest.join("/"))
    };

    let pkg_dir = find_up_dir(
        &path_resolve(from_dir, "."),
        &Path::new("node_modules").join(&pkg).to_string_lossy(),
    )?;
    if !Path::new(&pkg_dir).is_dir() {
        return None;
    }

    let pkg_json = Path::new(&pkg_dir).join("package.json");
    if pkg_json.is_file() {
        match resolve_from_package_json(&pkg_json.to_string_lossy(), &subpath, false) {
            Some(p) if Path::new(&p).is_file() => return Some(p),
            Some(_) => {}
            None => return None, // get-tsconfig: `===false` short-circuits
        }
    }

    let w = Path::new(&pkg_dir).join(&subpath);
    let w_str = w.to_string_lossy().into_owned();
    let is_json = w_str.ends_with(".json");
    if !is_json {
        let with_json = format!("{w_str}.json");
        if Path::new(&with_json).is_file() {
            return Some(with_json);
        }
    }
    if w.exists() {
        if w.is_dir() {
            let nested_pkg = w.join("package.json");
            if nested_pkg.is_file() {
                if let Some(p) = resolve_from_package_json(&nested_pkg.to_string_lossy(), "", true)
                {
                    if Path::new(&p).is_file() {
                        return Some(p);
                    }
                }
            }
            let nested_tsconfig = w.join("tsconfig.json");
            if nested_tsconfig.is_file() {
                return Some(nested_tsconfig.to_string_lossy().into_owned());
            }
        } else if is_json {
            return Some(w_str);
        }
    }
    None
}

/// get-tsconfig's `resolveFromPackageJsonPath` (`te`): pick the file a package's
/// `exports` (conditions `["require","types"]`) or `tsconfig` field points at.
/// Returns `None` to signal the get-tsconfig `false` (exports lookup failed).
fn resolve_from_package_json(pkg_json_path: &str, subpath: &str, direct: bool) -> Option<String> {
    let pkg = read_jsonc(pkg_json_path).ok()?;
    let mut target = if subpath.is_empty() {
        "tsconfig.json".to_string()
    } else {
        subpath.to_string()
    };

    if !direct {
        if let Some(exports) = pkg.get("exports") {
            // `None` = get-tsconfig's `false` → caller short-circuits
            target = resolve_exports(exports, subpath, &["require", "types"])?;
        } else if subpath.is_empty() {
            if let Some(ts) = pkg.get("tsconfig").and_then(Value::as_str) {
                target = ts.to_string();
            }
        }
    } else if subpath.is_empty() {
        if let Some(ts) = pkg.get("tsconfig").and_then(Value::as_str) {
            target = ts.to_string();
        }
    }

    // get-tsconfig joins `(packageJsonPath, "..", target)`, which path-normalizes
    // to the package directory plus `target`.
    Some(
        Path::new(&parent_dir(pkg_json_path))
            .join(&target)
            .to_string_lossy()
            .into_owned(),
    )
}

/// Minimal subset of `resolve-pkg-maps`'s `resolveExports` covering the shapes a
/// `@tsconfig/*` package uses: a `"."`/subpath key whose value is a string or a
/// conditions object. Returns the first matching string target, or `None` when no
/// condition matched (get-tsconfig treats that as the `false` short-circuit).
fn resolve_exports(exports: &Value, subpath: &str, conditions: &[&str]) -> Option<String> {
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    };
    // exports may be a bare string (only valid for the "." subpath), a conditions
    // object, or a subpath map.
    match exports {
        Value::String(s) => {
            if subpath.is_empty() {
                Some(s.clone())
            } else {
                None
            }
        }
        Value::Object(map) => {
            // Subpath map (keys starting with ".") vs a bare conditions object.
            let is_subpath_map = map.keys().any(|k| k.starts_with('.'));
            if is_subpath_map {
                let target = map.get(&key)?;
                resolve_conditional(target, conditions)
            } else {
                resolve_conditional(exports, conditions)
            }
        }
        _ => None,
    }
}

/// Resolve a conditions object / string / array target against the condition set.
fn resolve_conditional(target: &Value, conditions: &[&str]) -> Option<String> {
    match target {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => arr.iter().find_map(|t| resolve_conditional(t, conditions)),
        Value::Object(map) => {
            // "default" is always eligible; otherwise match a requested condition.
            for (k, v) in map {
                if k == "default" || conditions.contains(&k.as_str()) {
                    if let Some(found) = resolve_conditional(v, conditions) {
                        return Some(found);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// `findUp` for a relative *path* (not just a filename): walk up from `start`
/// joining `rel`, returning the first existing hit (get-tsconfig calls `O` with a
/// `node_modules/<pkg>` relative path).
pub fn find_up_dir(start: &str, rel: &str) -> Option<String> {
    let mut dir = PathBuf::from(start);
    loop {
        let candidate = dir.join(rel);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
}

fn parent_dir(p: &str) -> String {
    Path::new(p)
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Read + parse a JSONC file (tsconfig permits comments / trailing commas) via the
/// same `jsonc-parser` the data parsers use.
fn read_jsonc(path: &str) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| format!("Cannot resolve tsconfig at path: {path}"))?;
    // The CLI's `nub.jsonc` validation already tolerates the Windows/PowerShell
    // UTF-8 BOM, but a configured tsconfig — and the package metadata in its
    // `extends` chain — is re-opened here, so the same one marker has to be
    // stripped again or a valid configuration silently falls back to no matcher.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    nub_json_guard::check_nesting_depth(text, MAX_NESTING_DEPTH)
        .map_err(|e| format!("Failed to parse tsconfig at: {path}: {e}"))?;
    // The `Option` is the deserialization target, not a wrapper the parser adds.
    // It collapses an empty document and a literal `null` into `None`; a tsconfig
    // that is either is unusable, so both fall through to the same error.
    jsonc_parser::parse_to_serde_value::<Option<Value>>(text, &jsonc_parse_options())
        .map_err(|e| format!("Failed to parse tsconfig at: {path}: {e}"))?
        .ok_or_else(|| format!("Failed to parse tsconfig at: {path}"))
}

// ── compilerOptions normalization (get-tsconfig's `normalizeCompilerOptions`) ──

/// Faithful port of get-tsconfig's `normalizeCompilerOptions` (`Qe`): lower-cases
/// enum-ish string options and fills in the derived defaults TypeScript implies
/// (e.g. `target: "esnext"` ⇒ `module: "es6"`). The OLD JS hashed the object
/// AFTER this ran (get-tsconfig returned a normalized config), so reproducing it
/// is required for `tsconfig_hash` byte-parity. Mutates in place; keys are added
/// in the SAME order get-tsconfig adds them (insertion order, preserved by
/// serde_json's `preserve_order` feature).
fn normalize_compiler_options(e: &mut Map<String, Value>) {
    // Helper: set `key` to `val` only if currently absent (the `x ?? (x = y)` idiom).
    fn set_default(e: &mut Map<String, Value>, key: &str, val: Value) {
        if !e.contains_key(key) {
            e.insert(key.to_string(), val);
        }
    }
    fn get_bool(e: &Map<String, Value>, key: &str) -> bool {
        e.get(key).and_then(Value::as_bool).unwrap_or(false)
    }
    fn lower(e: &Map<String, Value>, key: &str) -> Option<String> {
        e.get(key).and_then(Value::as_str).map(str::to_lowercase)
    }

    if get_bool(e, "strict") {
        for k in [
            "noImplicitAny",
            "noImplicitThis",
            "strictNullChecks",
            "strictFunctionTypes",
            "strictBindCallApply",
            "strictPropertyInitialization",
            "strictBuiltinIteratorReturn",
            "alwaysStrict",
            "useUnknownInCatchVariables",
        ] {
            set_default(e, k, Value::Bool(true));
        }
    }
    if get_bool(e, "composite") {
        set_default(e, "declaration", Value::Bool(true));
        set_default(e, "incremental", Value::Bool(true));
    }
    if let Some(mut t) = lower(e, "target") {
        if t == "es2015" {
            t = "es6".to_string();
        }
        e.insert("target".to_string(), Value::String(t.clone()));
        if t == "esnext" {
            set_default(e, "module", Value::String("es6".into()));
            set_default(e, "useDefineForClassFields", Value::Bool(true));
        }
        if matches!(
            t.as_str(),
            "es6"
                | "es2016"
                | "es2017"
                | "es2018"
                | "es2019"
                | "es2020"
                | "es2021"
                | "es2022"
                | "es2023"
                | "es2024"
        ) {
            set_default(e, "module", Value::String("es6".into()));
        }
        if matches!(t.as_str(), "es2022" | "es2023" | "es2024") {
            set_default(e, "useDefineForClassFields", Value::Bool(true));
        }
    }
    if let Some(mut m) = lower(e, "module") {
        if m == "es2015" {
            m = "es6".to_string();
        }
        e.insert("module".to_string(), Value::String(m.clone()));
        if matches!(
            m.as_str(),
            "es6" | "es2020" | "es2022" | "esnext" | "none" | "system" | "umd" | "amd"
        ) {
            set_default(e, "moduleResolution", Value::String("classic".into()));
        }
        if m == "system" {
            set_default(e, "allowSyntheticDefaultImports", Value::Bool(true));
        }
        if matches!(
            m.as_str(),
            "node16" | "node18" | "node20" | "nodenext" | "preserve"
        ) {
            set_default(e, "esModuleInterop", Value::Bool(true));
            set_default(e, "allowSyntheticDefaultImports", Value::Bool(true));
        }
        if matches!(m.as_str(), "node16" | "node18" | "node20" | "nodenext") {
            set_default(e, "moduleDetection", Value::String("force".into()));
        }
        if m == "node16" {
            set_default(e, "target", Value::String("es2022".into()));
            set_default(e, "moduleResolution", Value::String("node16".into()));
        }
        if m == "node18" {
            set_default(e, "target", Value::String("es2022".into()));
            set_default(e, "moduleResolution", Value::String("node16".into()));
        }
        if m == "node20" {
            set_default(e, "target", Value::String("es2023".into()));
            set_default(e, "moduleResolution", Value::String("node16".into()));
            set_default(e, "resolveJsonModule", Value::Bool(true));
        }
        if m == "nodenext" {
            set_default(e, "target", Value::String("esnext".into()));
            set_default(e, "moduleResolution", Value::String("nodenext".into()));
            set_default(e, "resolveJsonModule", Value::Bool(true));
        }
        if matches!(m.as_str(), "node16" | "node18" | "node20" | "nodenext") {
            let target = e.get("target").and_then(Value::as_str).unwrap_or("");
            if matches!(target, "es3" | "es2022" | "es2023" | "es2024" | "esnext") {
                set_default(e, "useDefineForClassFields", Value::Bool(true));
            }
        }
        if m == "preserve" {
            set_default(e, "moduleResolution", Value::String("bundler".into()));
        }
    }
    if let Some(mut mr) = lower(e, "moduleResolution") {
        if mr == "node" {
            mr = "node10".to_string();
        }
        e.insert("moduleResolution".to_string(), Value::String(mr.clone()));
        if matches!(mr.as_str(), "node16" | "nodenext" | "bundler") {
            set_default(e, "resolvePackageJsonExports", Value::Bool(true));
            set_default(e, "resolvePackageJsonImports", Value::Bool(true));
        }
        if mr == "bundler" {
            set_default(e, "allowSyntheticDefaultImports", Value::Bool(true));
            set_default(e, "resolveJsonModule", Value::Bool(true));
        }
    }
    for key in [
        "jsx",
        "moduleDetection",
        "importsNotUsedAsValues",
        "newLine",
    ] {
        if let Some(v) = lower(e, key) {
            e.insert(key.to_string(), Value::String(v));
        }
    }
    if get_bool(e, "esModuleInterop") {
        set_default(e, "allowSyntheticDefaultImports", Value::Bool(true));
    }
    if get_bool(e, "verbatimModuleSyntax") {
        set_default(e, "isolatedModules", Value::Bool(true));
        set_default(e, "preserveConstEnums", Value::Bool(true));
    }
    if get_bool(e, "isolatedModules") {
        set_default(e, "preserveConstEnums", Value::Bool(true));
    }
    if get_bool(e, "rewriteRelativeImportExtensions") {
        set_default(e, "allowImportingTsExtensions", Value::Bool(true));
    }
    if let Some(lib) = e.get("lib").and_then(Value::as_array).cloned() {
        let mapped: Vec<Value> = lib
            .iter()
            .map(|v| match v.as_str() {
                Some(s) => Value::String(s.to_lowercase()),
                None => v.clone(),
            })
            .collect();
        e.insert("lib".to_string(), Value::Array(mapped));
    }
    if get_bool(e, "checkJs") {
        set_default(e, "allowJs", Value::Bool(true));
    }
}

// ── compilerOptions extraction + hash ───────────────────────────────

fn extract_compiler_options(map: &Map<String, Value>) -> TsCompilerOptions {
    TsCompilerOptions {
        jsx: map.get("jsx").and_then(Value::as_str).map(str::to_string),
        jsx_import_source: map
            .get("jsxImportSource")
            .and_then(Value::as_str)
            .map(str::to_string),
        jsx_factory: map
            .get("jsxFactory")
            .and_then(Value::as_str)
            .map(str::to_string),
        jsx_fragment_factory: map
            .get("jsxFragmentFactory")
            .and_then(Value::as_str)
            .map(str::to_string),
        experimental_decorators: map.get("experimentalDecorators").and_then(Value::as_bool),
        emit_decorator_metadata: map.get("emitDecoratorMetadata").and_then(Value::as_bool),
    }
}

/// Serialize the merged `compilerOptions` to a `JSON.stringify`-equivalent string
/// — the exact `tsconfigHash` the old JS used (`JSON.stringify(co)`). The old JS
/// passed the SAME object get-tsconfig returned (insertion-ordered) straight to
/// `JSON.stringify`; `serde_json`'s default object representation preserves the
/// insertion order of the JSONC parse + extends merge, so re-serializing it here
/// matches byte-for-byte. The private implicit-baseUrl bookkeeping key is the only
/// thing stripped (get-tsconfig keys it under a Symbol, which `JSON.stringify`
/// omits — so we must omit it too for parity).
fn stringify_compiler_options(map: &Map<String, Value>) -> String {
    if map.contains_key(IMPLICIT_BASE_URL) {
        // Rebuild preserving insertion order, skipping the bookkeeping key.
        // (serde_json's `Map::remove` is a *swap*-remove that would move the last
        // entry into the hole and corrupt the order — and thus the hash.)
        let mut clean = Map::new();
        for (k, v) in map {
            if k != IMPLICIT_BASE_URL {
                clean.insert(k.clone(), v.clone());
            }
        }
        serde_json::to_string(&Value::Object(clean)).unwrap_or_default()
    } else {
        serde_json::to_string(&Value::Object(map.clone())).unwrap_or_default()
    }
}

// ── paths matcher (get-tsconfig's `createPathsMatcher`) ─────────────

fn build_matcher(config_path: &str, co: Option<&Map<String, Value>>) -> Option<PathsMatcher> {
    let co = co?;
    let base_url = co.get("baseUrl").and_then(Value::as_str);
    let paths = co.get("paths").and_then(Value::as_object);
    if base_url.is_none() && paths.is_none() {
        return None;
    }
    let implicit = co.get(IMPLICIT_BASE_URL).and_then(Value::as_str);
    let config_dir = parent_dir(config_path);
    // resolvedBaseUrl = resolve(dirname(path), baseUrl || implicit || ".")
    let base_spec = base_url.or(implicit).unwrap_or(".");
    let resolved_base = path_resolve(&config_dir, base_spec);

    let parsed = match paths {
        Some(p) => parse_paths(p, base_url.is_some(), &resolved_base)?,
        None => Vec::new(),
    };

    Some(PathsMatcher {
        base_url: resolved_base,
        paths: parsed,
        has_base_url: base_url.is_some(),
    })
}

/// get-tsconfig's `parsePaths` (`ln`): validate ≤1 `*`, enforce non-relative
/// substitutions require baseUrl, resolve substitutions against the base dir.
fn parse_paths(
    paths: &Map<String, Value>,
    has_base_url: bool,
    base_dir: &str,
) -> Option<Vec<(Pattern, Vec<String>)>> {
    let mut out = Vec::new();
    for (key, subs_val) in paths {
        assert_star_count(
            key,
            &format!("Pattern '{key}' can have at most one '*' character."),
        )
        .ok()?;
        let pattern = parse_pattern(key);
        let subs = subs_val.as_array()?;
        let mut substitutions = Vec::new();
        for sub in subs {
            let s = sub.as_str()?;
            assert_star_count(
                s,
                &format!(
                    "Substitution '{s}' in pattern '{key}' can have at most one '*' character."
                ),
            )
            .ok()?;
            if !has_base_url && !is_relative_dotted(s) && !Path::new(s).is_absolute() {
                // get-tsconfig throws here; we treat the whole matcher as invalid.
                return None;
            }
            substitutions.push(path_resolve(base_dir, s));
        }
        out.push((pattern, substitutions));
    }
    Some(out)
}

fn assert_star_count(s: &str, msg: &str) -> Result<(), String> {
    if s.matches('*').count() > 1 {
        Err(msg.to_string())
    } else {
        Ok(())
    }
}

fn parse_pattern(key: &str) -> Pattern {
    if let Some(star) = key.find('*') {
        Pattern::Wildcard {
            prefix: key[..star].to_string(),
            suffix: key[star + 1..].to_string(),
        }
    } else {
        Pattern::Exact(key.to_string())
    }
}

impl PathsMatcher {
    /// get-tsconfig's matcher closure: returns candidate absolute paths (slashed)
    /// for a specifier, or `[]`. Empty for relative specifiers.
    fn matches(&self, specifier: &str) -> Vec<String> {
        if is_relative_dotted(specifier) {
            return Vec::new();
        }
        let mut wildcards: Vec<&(Pattern, Vec<String>)> = Vec::new();
        for entry in &self.paths {
            match &entry.0 {
                Pattern::Exact(p) => {
                    if p == specifier {
                        return entry.1.iter().map(|s| slash(s)).collect();
                    }
                }
                Pattern::Wildcard { .. } => wildcards.push(entry),
            }
        }
        // Longest matching prefix wins (ties favor the earlier entry).
        let mut best: Option<&(Pattern, Vec<String>)> = None;
        let mut best_len: isize = -1;
        for entry in &wildcards {
            if let Pattern::Wildcard { prefix, suffix } = &entry.0 {
                if specifier.starts_with(prefix.as_str())
                    && specifier.ends_with(suffix.as_str())
                    && prefix.len() as isize > best_len
                {
                    best_len = prefix.len() as isize;
                    best = Some(entry);
                }
            }
        }
        let Some(entry) = best else {
            return if self.has_base_url {
                vec![slash(
                    &Path::new(&self.base_url).join(specifier).to_string_lossy(),
                )]
            } else {
                Vec::new()
            };
        };
        let Pattern::Wildcard { prefix, suffix } = &entry.0 else {
            unreachable!()
        };
        let captured = &specifier[prefix.len()..specifier.len() - suffix.len()];
        entry
            .1
            .iter()
            .map(|s| slash(&s.replacen('*', captured, 1)))
            .collect()
    }
}

/// Run the cached matcher for `dir`'s tsconfig against `specifier`. Returns the
/// candidate paths (possibly empty) — the resolver probes each with the FS.
pub fn match_paths(dir: &str, specifier: &str, explicit: Option<&str>) -> Vec<String> {
    let loaded = load_for_dir(dir, explicit);
    match &loaded.matcher {
        Some(m) => m.matches(specifier),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::custom_conditions;

    /// Write a `tsconfig.json` plus any extra files, and hand back the dir. A fresh
    /// temp dir per call keeps the process-lifetime cache from crossing tests.
    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    fn read(dir: &tempfile::TempDir) -> Vec<String> {
        custom_conditions(&dir.path().to_string_lossy(), None)
    }

    /// The memo is keyed by DIRECTORY, so it is unbounded by construction — and it
    /// lives in the addon, inside the user's own long-lived process. A program that
    /// materializes directories at runtime (codegen, generated routes, per-case temp
    /// dirs) would otherwise add an entry per directory forever. Drive past the cap
    /// with distinct keys and assert the map never exceeds it.
    #[test]
    fn directory_memo_stays_bounded() {
        for i in 0..(super::CACHE_MAX_ENTRIES + 64) {
            // Directories that do not exist still memoize the "no tsconfig here"
            // answer, which is exactly the entry that used to accumulate.
            super::load_tsconfig(&format!("/nonexistent/nub-cap-probe/{i}"), None);
        }
        let len = super::cache().lock().unwrap().len();
        assert!(
            len <= super::CACHE_MAX_ENTRIES,
            "tsconfig memo grew to {len} entries, past the {} cap",
            super::CACHE_MAX_ENTRIES,
        );
    }

    /// The layout that motivates the feature: leaf configs `extends` a shared base
    /// that declares the conditions, and nothing restates them per package.
    #[test]
    fn inherits_custom_conditions_from_an_extended_base_config() {
        let dir = project(&[
            (
                "tsconfig.base.json",
                r#"{ "compilerOptions": { "customConditions": ["@repo/source", "MixedCase"] } }"#,
            ),
            ("tsconfig.json", r#"{ "extends": "./tsconfig.base.json" }"#),
        ]);
        // Declaration order and case both survive: an `exports` key is matched
        // byte-for-byte, so lower-casing one would silently stop matching.
        assert_eq!(read(&dir), vec!["@repo/source", "MixedCase"]);
    }

    /// TypeScript's own merge rule — a child's `compilerOptions` key REPLACES the
    /// base's rather than concatenating with it.
    #[test]
    fn a_child_config_replaces_the_bases_conditions_rather_than_appending() {
        let dir = project(&[
            (
                "tsconfig.base.json",
                r#"{ "compilerOptions": { "customConditions": ["from-base"] } }"#,
            ),
            (
                "tsconfig.json",
                r#"{ "extends": "./tsconfig.base.json", "compilerOptions": { "customConditions": ["from-leaf"] } }"#,
            ),
        ]);
        assert_eq!(read(&dir), vec!["from-leaf"]);
    }

    #[test]
    fn no_conditions_when_the_option_is_absent_or_the_tsconfig_is_missing() {
        let with_tsconfig = project(&[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "strict": true } }"#,
        )]);
        assert!(read(&with_tsconfig).is_empty());

        // A bare directory still walks UP to the filesystem root looking for a
        // tsconfig, so this asserts the empty answer, not the absence of a walk.
        let bare = tempfile::tempdir().unwrap();
        assert!(custom_conditions(&bare.path().to_string_lossy(), None).is_empty());
    }

    /// A non-string member is dropped, not coerced: the value reaches Node as a
    /// resolution condition, and inventing `"7"` from `7` would make a package's
    /// `exports` match on something the user never wrote.
    #[test]
    fn non_string_members_are_dropped_and_the_rest_survive() {
        let dir = project(&[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "customConditions": ["keep", 7, null, "also-keep"] } }"#,
        )]);
        assert_eq!(read(&dir), vec!["keep", "also-keep"]);
    }

    /// The explicit-path form (`nub.jsonc`'s `tsconfig` field) reads that file
    /// instead of walking up — and `extends` still resolves relative to it.
    #[test]
    fn an_explicitly_named_tsconfig_is_read_instead_of_the_nearest_one() {
        let dir = project(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "customConditions": ["nearest"] } }"#,
            ),
            (
                "tsconfig.runtime.json",
                r#"{ "compilerOptions": { "customConditions": ["explicit"] } }"#,
            ),
        ]);
        let explicit = dir.path().join("tsconfig.runtime.json");
        assert_eq!(
            custom_conditions(
                &dir.path().to_string_lossy(),
                Some(&explicit.to_string_lossy())
            ),
            vec!["explicit"]
        );
    }

    fn diags(dir: &tempfile::TempDir) -> Vec<String> {
        super::diagnostics(&dir.path().to_string_lossy(), None)
    }

    /// #731: one `extends` target that will not resolve used to discard the whole
    /// file, so `paths`, decorator flags and `customConditions` all silently
    /// vanished and nub became indistinguishable from a project with no tsconfig.
    /// `tsc` reports TS5083 and still applies what the file itself declares.
    #[test]
    fn an_unresolvable_extends_keeps_the_options_the_file_itself_declares() {
        let dir = project(&[(
            "tsconfig.json",
            r#"{ "extends": "./absent.json",
                 "compilerOptions": {
                   "baseUrl": ".",
                   "paths": { "@/*": ["./src/*"] },
                   "customConditions": ["kept"]
                 } }"#,
        )]);
        assert_eq!(read(&dir), vec!["kept"]);
        let matched = super::match_paths(&dir.path().to_string_lossy(), "@/util", None);
        assert!(
            matched.iter().any(|p| p.ends_with("src/util")),
            "the alias should still resolve; got {matched:?}",
        );
    }

    /// The other half of the same fix: the run continues, but never in silence.
    /// The message names the config, the target it could not read, and the fact
    /// that the rest of the file survived.
    #[test]
    fn an_unresolvable_extends_reports_the_target_it_could_not_read() {
        let dir = project(&[(
            "tsconfig.json",
            r#"{ "extends": "./absent.json", "compilerOptions": { "customConditions": ["kept"] } }"#,
        )]);
        let reported = diags(&dir);
        assert_eq!(
            reported.len(),
            1,
            "expected exactly one diagnostic, got {reported:?}",
        );
        assert!(
            reported[0].contains("absent.json") && reported[0].contains("tsconfig.json"),
            "diagnostic should name both the config and the target it could not read; got {:?}",
            reported[0],
        );
    }

    /// A config whose own body will not parse has nothing to salvage, so the
    /// no-options fallback stays — but the reason now travels with it instead of
    /// being dropped on the floor.
    #[test]
    fn an_unparseable_config_body_applies_nothing_and_says_why() {
        let dir = project(&[("tsconfig.json", "{ \"compilerOptions\": { ")]);
        assert!(read(&dir).is_empty());
        let reported = diags(&dir);
        assert!(
            reported.len() == 1 && reported[0].contains("tsconfig.json"),
            "expected one diagnostic naming the unparseable config; got {reported:?}",
        );
    }

    /// The control the two tests above need: a config that parses cleanly must
    /// stay silent, or the warning is noise on every run instead of a signal.
    #[test]
    fn a_config_that_parses_cleanly_reports_nothing() {
        let dir = project(&[
            ("base.json", r#"{ "compilerOptions": { "strict": true } }"#),
            (
                "tsconfig.json",
                r#"{ "extends": "./base.json", "compilerOptions": { "customConditions": ["ok"] } }"#,
            ),
        ]);
        assert_eq!(read(&dir), vec!["ok"]);
        assert!(diags(&dir).is_empty());
    }
}
