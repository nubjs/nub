//! The ADDITIVE TypeScript resolution layer — the part of nub's resolver that
//! Node has no opinion on. Mirrors the `tryResolveFile` + matcher-driven steps of
//! the old JS `resolveSpec`/`resolveCjsPath`.
//!
//! **The boundary is load-bearing.** [`resolve_ts`] returns `Some(absolute path)`
//! ONLY for the additive cases nub owns: tsconfig `paths` aliases, `.ts/.tsx/.mts/
//! .cts/.jsx` extension probing, the `.js`→`.ts` (and `.jsx→.tsx`, `.mjs→.mts`,
//! `.cjs→.cts`) emit-convention swap, directory-index probing, reading a
//! directory's `package.json#main` (main ONLY), and — see
//! [`resolve_bare_subpath`] — extension probing of a bare package SUBPATH into a
//! dependency that declares no `exports`. For EVERYTHING else — package-name
//! resolution, `exports`/`imports` maps, export conditions — it returns `None`,
//! and the JS hook turns that into `nextResolve(...)` /
//! `origResolveFilename(...)`. Reimplementing Node's own resolution here is
//! forbidden; `None` is where byte-for-byte compat is preserved.
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
            if let Some(resolved) = try_resolve_file(&candidate, true, probe_order(&parent_ext)) {
                return Some(resolved);
            }
        }
        // No alias matched. A bare package NAME still belongs entirely to Node
        // (`main`/`exports`); only a SUBPATH into an `exports`-less dependency is
        // ours to probe.
        return resolve_bare_subpath(&specifier, &parent_dir);
    }

    // Extensionless / emit-swap branch — only when the importer is itself a TS
    // file and the specifier is relative.
    if is_ts_parent(&parent_ext) && is_relative {
        let target = path_join_resolve(&parent_dir, &specifier);
        if let Some(resolved) = try_resolve_file(&target, true, probe_order(&parent_ext)) {
            return Some(resolved);
        }
    }

    None
}

/// Extension probing for a bare package SUBPATH (`pkg/sub`, `@scope/pkg/a/b`).
///
/// Node's ESM resolver gives a subpath no probing at all, so `import
/// "prismjs/components/prism-python"` is a hard `ERR_MODULE_NOT_FOUND` on plain
/// Node even though the CJS `require()` of the same specifier works. tsc accepts
/// it, tsx resolves it, and a TS monorepo whose workspace packages point `main`
/// at `.ts` source depends on it — so this is nub's additive layer. (#562)
///
/// THE ENCAPSULATION RULE: a dependency that declares `exports` owns its own
/// subpath map completely — we return `None` and let Node apply it, so a probe
/// can never reach a path the package deliberately did not export. Only the
/// legacy `exports`-less shape, where a subpath is just a path join, is probed.
/// tsx draws the line in the same place.
///
/// Not gated on a TS importer: a `checkJs`/`allowJs` project's `.js` files sit in
/// the same graph as its `.ts` files and import the same package subpaths, so
/// gating would split resolution down the middle of one project.
fn resolve_bare_subpath(specifier: &str, parent_dir: &str) -> Option<String> {
    // `#foo` is an `imports` specifier — the package's own private map, Node's.
    if specifier.starts_with('#') {
        return None;
    }

    let mut segs = specifier.split('/');
    let first = segs.next()?;
    let pkg = if first.starts_with('@') {
        format!("{first}/{}", segs.next()?)
    } else {
        first.to_string()
    };
    let subpath = segs.collect::<Vec<_>>().join("/");
    // A bare NAME (no subpath) resolves through `main`/`exports` — never ours.
    // A traversing subpath would escape the package root.
    if subpath.is_empty() || subpath.split('/').any(|s| s == "..") {
        return None;
    }

    let pkg_dir = tsconfig::find_up_dir(parent_dir, &format!("node_modules/{pkg}"))?;
    let pkg_json = Path::new(&pkg_dir).join("package.json");
    let text = std::fs::read_to_string(&pkg_json).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    // The mere PRESENCE of the key hands the package its own subpath map. Note
    // `"exports": null` is the deliberate "export nothing" form, not an absent
    // map — so this tests presence, never truthiness.
    if manifest.get("exports").is_some() {
        return None;
    }

    let target = path_join_resolve(&pkg_dir, &subpath);
    // An exact-extension hit is not an additive case — Node resolves `pkg/dist/foo.js`
    // in an `exports`-less package identically, down to its own symlink decision. Leave
    // it to Node so this function owns only what genuinely needed probing.
    if !extname(&target).is_empty() && is_file(&target) {
        return None;
    }

    // Node realpaths a resolved module by default, and nub's load hooks REFUSE to
    // transpile anything under a `node_modules/` path. A workspace package symlinked
    // into node_modules (the `main: ./index.ts` monorepo shape) is only loadable once
    // that hop is resolved away.
    let resolved = real_path(&try_resolve_file(&target, true, &NODE_MODULES_PROBE)?);

    // A TS hit STILL under node_modules after the symlink hop is unshipped source the
    // load hooks refuse, so winning here is worse than not resolving at all: it turns
    // Node's ERR_MODULE_NOT_FOUND into a misleading
    // ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING, and — because probing settles on a
    // file before it ever considers a directory — `sub.ts` would outrank a sibling
    // `sub/index.js` that Node's CJS resolver loads today. Fall through instead, so
    // Node's own precedence and its own error both survive.
    if is_node_modules(&resolved) && matches!(extname(&resolved).as_str(), ".ts" | ".tsx") {
        return None;
    }
    Some(resolved)
}

/// Probe order for a bare package subpath — JS FIRST, and deliberately NOT
/// parent-extension-aware like [`probe_order`].
///
/// What decides the right file inside a dependency is the package's own shape,
/// not the importer's extension. In a PUBLISHED package a `.ts` sibling is
/// unshipped source, so JS must win; in a symlinked WORKSPACE package there is no
/// `.js` at all, so JS-first still lands on the `.ts`. Both shapes want the same
/// order. Matches tsx's set exactly — `.mjs`/`.cjs`/`.mts`/`.cts` are not probed,
/// as an extensionless import of one is not a pattern that occurs.
///
/// Ordering alone is not sufficient: a `.ts` with no `.js` sibling still wins the
/// probe, which is why [`resolve_bare_subpath`] discards a TS hit that is still
/// under node_modules after realpathing.
const NODE_MODULES_PROBE: [&str; 4] = [".js", ".json", ".ts", ".tsx"];

/// Resolve symlinks, keeping a plain path on Windows (`std::fs::canonicalize`
/// yields a `\\?\` verbatim path there, which Node and `pathToFileURL` choke on).
/// Falls back to the input when the path cannot be canonicalized.
///
/// The UNC arm mirrors `strip_verbatim` in nub-core: a canonicalized network path
/// is `\\?\UNC\server\share\x`, so stripping only `\\?\` would leave
/// `UNC\server\share\x` — not an absolute path at all.
fn real_path(path: &str) -> String {
    let Ok(canonical) = std::fs::canonicalize(path) else {
        return path.to_string();
    };
    let s = canonical.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match s.strip_prefix(r"\\?\") {
        Some(stripped) => stripped.to_string(),
        None => s.into_owned(),
    }
}

/// Port of the JS `tryResolveFile`: existing-extension hit, emit-convention
/// swaps, extensionless probing, and directory `main`/index resolution. `probe`
/// is the extensionless-probe order (see [`probe_order`], [`NODE_MODULES_PROBE`]).
fn try_resolve_file(
    target: &str,
    allow_dir_main: bool,
    probe: &'static [&'static str],
) -> Option<String> {
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
    if existing_ext.is_empty() || !TS_PARENT_EXTS.contains(&existing_ext.as_str()) {
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
                    if let Some(resolved) = try_resolve_file(&main_target, false, probe) {
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
    let p = path.split_once('?').map_or(path, |(head, _)| head);
    // Match Node's path.extname: only count a dot in the final path segment.
    let base = p.rsplit(['/', '\\']).next().unwrap_or(p);
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
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn is_dir(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}
