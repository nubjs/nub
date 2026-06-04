//! The ADDITIVE TypeScript resolution layer — the part of nub's resolver that
//! Node has no opinion on. Mirrors the `tryResolveFile` + matcher-driven steps of
//! the old JS `resolveSpec`/`resolveCjsPath`.
//!
//! **The boundary is load-bearing.** [`resolve_ts`] returns `Some(absolute path)`
//! ONLY for the additive cases nub owns: tsconfig `paths` aliases, `.ts/.tsx/.mts/
//! .cts/.jsx` extension probing, the `.js`→`.ts` (and `.jsx→.tsx`, `.mjs→.mts`,
//! `.cjs→.cts`) emit-convention swap, directory-index probing, and reading a
//! directory's `package.json#main` (main ONLY). For EVERYTHING else — node_modules
//! resolution, `exports`/`imports` maps, export conditions, scoped packages, bare
//! specifiers — it returns `None`, and the JS hook turns that into
//! `nextResolve(...)` / `origResolveFilename(...)`. Reimplementing Node's own
//! resolution here is forbidden; `None` is where byte-for-byte compat is preserved.
//!
//! The `node:`/`data:`/builtin guards, the nub-internal-graph bypass, vendored
//! packages, and the clobber map all stay in JS and run BEFORE this is called.

use std::path::{Path, PathBuf};

use napi_derive::napi;

use crate::tsconfig;

const TS_PARENT_EXTS: [&str; 4] = [".ts", ".tsx", ".mts", ".cts"];

/// The additive TS resolution. `Some(absolute path)` ⇒ nub short-circuits; `None`
/// ⇒ fall through to Node (the compat boundary). `parent_path` is the importer's
/// absolute filesystem path (empty for the entry).
#[napi]
pub fn resolve_ts(specifier: String, parent_path: String) -> Option<String> {
    let parent_ext = extname(&parent_path);
    let parent_dir = if parent_path.is_empty() {
        std::env::current_dir().ok()?.to_string_lossy().into_owned()
    } else {
        dirname(&parent_path)
    };

    let is_relative = specifier.starts_with("./") || specifier.starts_with("../");
    let is_absolute = specifier.starts_with('/') || Path::new(&specifier).is_absolute();
    let is_file_url = specifier.starts_with("file:");

    // tsconfig `paths` branch — non-relative, non-absolute specifiers from a file
    // outside node_modules. (Not gated on a TS parent: a plain .js with a paths
    // alias resolves too.)
    if !is_relative && !is_absolute && !is_file_url && !is_node_modules(&parent_path) {
        let candidates = tsconfig::match_paths(&parent_dir, &specifier);
        for candidate in candidates {
            if let Some(resolved) = try_resolve_file(&candidate, &parent_ext, true) {
                return Some(resolved);
            }
        }
        // A bare package with no matching alias → let Node resolve from
        // node_modules. NEVER probe node_modules ourselves.
        return None;
    }

    // Extensionless / emit-swap branch — only when the importer is itself a TS
    // file and the specifier is relative.
    if is_ts_parent(&parent_ext) && is_relative {
        let target = path_join_resolve(&parent_dir, &specifier);
        if let Some(resolved) = try_resolve_file(&target, &parent_ext, true) {
            return Some(resolved);
        }
    }

    None
}

/// Port of the JS `tryResolveFile`: existing-extension hit, emit-convention
/// swaps, extensionless probing, and directory `main`/index resolution.
fn try_resolve_file(target: &str, parent_ext: &str, allow_dir_main: bool) -> Option<String> {
    let existing_ext = extname(target);

    // 1. Existing extension that exists → use it (a real .cjs beats a sibling .cts).
    if !existing_ext.is_empty() && is_file(target) {
        return Some(target.to_string());
    }

    // 2. Emit-convention swaps (only when the ext matches).
    match existing_ext.as_str() {
        ".js" => {
            let ts = format!("{}.ts", &target[..target.len() - 3]);
            if is_file(&ts) {
                return Some(ts);
            }
            let tsx = format!("{}.tsx", &target[..target.len() - 3]);
            if is_file(&tsx) {
                return Some(tsx);
            }
        }
        ".jsx" => {
            let tsx = format!("{}.tsx", &target[..target.len() - 4]);
            if is_file(&tsx) {
                return Some(tsx);
            }
        }
        ".mjs" => {
            let mts = format!("{}.mts", &target[..target.len() - 4]);
            if is_file(&mts) {
                return Some(mts);
            }
        }
        ".cjs" => {
            let cts = format!("{}.cts", &target[..target.len() - 4]);
            if is_file(&cts) {
                return Some(cts);
            }
        }
        _ => {}
    }

    // 3. Extensionless: probe in parent-ext-aware order.
    if existing_ext.is_empty() {
        let probe = probe_order(parent_ext);
        for ext in probe {
            let candidate = format!("{target}{ext}");
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
        // Directory: honor package.json `main` (main only) before index probing.
        if is_dir(target) {
            if allow_dir_main {
                if let Some(main) = read_package_main(target) {
                    let main_target = path_join_resolve(target, &main);
                    // Node's LOAD_AS_DIRECTORY does NOT recurse a main target's
                    // own nested main → allow_dir_main=false.
                    if let Some(resolved) = try_resolve_file(&main_target, parent_ext, false) {
                        return Some(resolved);
                    }
                }
            }
            for ext in probe {
                let idx = Path::new(target)
                    .join(format!("index{ext}"))
                    .to_string_lossy()
                    .into_owned();
                if is_file(&idx) {
                    return Some(idx);
                }
            }
        }
    }

    None
}

/// Port of the JS `getProbeOrder`.
fn probe_order(parent_ext: &str) -> &'static [&'static str] {
    match parent_ext {
        ".tsx" => &[".tsx", ".ts", ".jsx", ".js", ".json"],
        ".mts" => &[".mts", ".ts", ".mjs", ".js", ".json"],
        ".cts" => &[".cts", ".ts", ".cjs", ".js", ".json"],
        _ => &[".ts", ".tsx", ".js", ".jsx", ".json"],
    }
}

/// Port of the JS `readPackageMain`: a directory's `package.json#main` (the legacy
/// CJS entry), or `None`. `exports` is deliberately NOT consulted (Node honors
/// `exports` only for package-name resolution, never a directory-path import).
fn read_package_main(dir: &str) -> Option<String> {
    let pkg_path = Path::new(dir).join("package.json");
    if !pkg_path.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&pkg_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let main = value.get("main")?.as_str()?;
    if main.trim().is_empty() {
        None
    } else {
        Some(main.to_string())
    }
}

fn is_ts_parent(ext: &str) -> bool {
    TS_PARENT_EXTS.contains(&ext)
}

/// JS `extname` semantics over an absolute path (strip a `?query` first; ext is
/// the substring from the last `.`).
fn extname(path: &str) -> String {
    let p = match path.find('?') {
        Some(i) => &path[..i],
        None => path,
    };
    // Match Node's path.extname: only count a dot in the final path segment.
    let base = match p.rfind(['/', '\\']) {
        Some(i) => &p[i + 1..],
        None => p,
    };
    match base.rfind('.') {
        // A leading dot (dotfile) has no extension.
        Some(0) => String::new(),
        Some(i) => base[i..].to_string(),
        None => String::new(),
    }
}

fn dirname(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Node `path.resolve(base, spec)` (lexical; spec may be relative or absolute).
fn path_join_resolve(base: &str, spec: &str) -> String {
    let joined = if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        Path::new(base).join(spec)
    };
    normalize_lexical(&joined)
}

fn normalize_lexical(p: &Path) -> String {
    use std::path::Component;
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

fn is_node_modules(path: &str) -> bool {
    path.contains("/node_modules/") || path.contains("\\node_modules\\")
}

fn is_file(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

fn is_dir(path: &str) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}
