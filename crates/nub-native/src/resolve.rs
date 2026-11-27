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

use napi::Either;
use napi_derive::napi;

use nub_tsconfig as tsconfig;

const TS_PARENT_EXTS: [&str; 4] = [".ts", ".tsx", ".mts", ".cts"];

/// The additive TS resolution. `Some(absolute path)` ⇒ nub short-circuits; `None`
/// ⇒ fall through to Node (the compat boundary); a [`PackageRedirect`] ⇒ ask Node
/// for the installed packages a `paths` target names. `parent_path` is the importer's
/// absolute filesystem path (empty for the entry).
/// `tsconfig_path` is the project's configured tsconfig when one is set, so
/// `paths` resolution reads the file the project names instead of rediscovering
/// one by walking up from the importer.
/// `preserve_symlinks` mirrors Node's flag of the same name. The caller passes it
/// because the flag reaches a process two ways — argv and `NODE_OPTIONS` — and only
/// the JS side sees both.
#[napi(catch_unwind)]
pub fn resolve_ts(
    specifier: String,
    parent_path: String,
    tsconfig_path: Option<String>,
    preserve_symlinks: Option<bool>,
) -> Option<Either<String, PackageRedirect>> {
    let preserve_symlinks = preserve_symlinks.unwrap_or(false);
    // Classify and DISCOVER from where the importer really lives. Under
    // `--preserve-symlinks` Node hands over the un-realpathed path, so a file inside a
    // workspace package symlinked into node_modules would read as a dependency and
    // lose the additive resolution its own imports get without the flag. Both uses
    // have to move together: `parent_dir` drives tsconfig discovery and the
    // node_modules walk, so testing the real path while discovering from the symlink
    // would find the project root's tsconfig instead of the package's own. What the
    // resolver RETURNS is a separate question, settled per-case below.
    let parent_path = if preserve_symlinks && !parent_path.is_empty() {
        real_path(&parent_path)
    } else {
        parent_path
    };
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
        let candidates = tsconfig::match_paths(&parent_dir, &specifier, tsconfig_path.as_deref());
        let mut packages = Vec::new();
        for candidate in candidates {
            if let Some(mut target) = package_target(&candidate) {
                target.file = try_resolve_file(&candidate, true, probe_order(&parent_ext));
                packages.push(target);
                continue;
            }
            if let Some(resolved) = try_resolve_file(&candidate, true, probe_order(&parent_ext)) {
                return redirect_or_file(packages, Some(resolved));
            }
        }
        // No alias matched. A bare package NAME still belongs entirely to Node
        // (`main`/`exports`); only a SUBPATH into an `exports`-less dependency is
        // ours to probe.
        let rest = resolve_bare_subpath(&specifier, &parent_dir, preserve_symlinks);
        return redirect_or_file(packages, rest);
    }

    // Extensionless / emit-swap branch — only when the importer is itself a TS
    // file and the specifier is relative.
    if is_ts_parent(&parent_ext) && is_relative && !names_a_directory(&specifier) {
        let target = path_join_resolve(&parent_dir, &specifier);
        if let Some(resolved) = try_resolve_file(&target, true, probe_order(&parent_ext)) {
            return Some(Either::A(resolved));
        }
    }

    None
}

/// The `paths` targets that land in installed packages, for Node to resolve in
/// order, and what answers when none of them does.
///
/// `"*": ["./node_modules/*"]` is a common tsconfig line, and probing its target as a
/// path reads the package's legacy `main` and never its `exports` — so `import "dep"`
/// silently loaded a different file than plain Node does (a dual package's CommonJS
/// build under `import`, say). `tsc` and tsconfig-paths take `main` there too, but
/// they answer a different question: `tsc` is looking for TYPES (`types` first), and
/// the runtime module is Node's to choose.
///
/// The caller asks Node for each [`PackageTarget`] in `paths` order. Where Node
/// refuses one (not exported, or nothing at the mapped path), that target's own
/// `file` answers if it has one, else the next target is tried, and `otherwise`
/// (a later candidate's file) closes the list. With nothing left, Node resolves the
/// original specifier, so the error is the one plain Node gives.
#[napi(object)]
pub struct PackageRedirect {
    pub packages: Vec<PackageTarget>,
    pub otherwise: Option<String>,
}

/// One installed package a `paths` target names, as the package's own
/// self-reference (`specifier` is its manifest `name` plus the subpath) resolved from
/// its `package.json`. Anchoring inside the package is what makes Node answer from
/// that exact copy: from the importer, a nested copy or the project's own name
/// could win instead. Only a package that declares `exports` is a target: without
/// one, Node's own answer IS `main` plus probing, which [`try_resolve_file`] already
/// gives, and a self-reference needs `exports` and `name` to resolve at all.
#[napi(object)]
pub struct PackageTarget {
    pub specifier: String,
    pub package_json: String,
    /// The file the target names by path, loaded where `exports` refuses it.
    pub file: Option<String>,
}

fn redirect_or_file(
    packages: Vec<PackageTarget>,
    otherwise: Option<String>,
) -> Option<Either<String, PackageRedirect>> {
    if packages.is_empty() {
        return otherwise.map(Either::A);
    }
    Some(Either::B(PackageRedirect {
        packages,
        otherwise,
    }))
}

fn package_target(candidate: &str) -> Option<PackageTarget> {
    let components: Vec<_> = Path::new(candidate).components().collect();
    let nm = components
        .iter()
        .rposition(|c| c.as_os_str() == "node_modules")?;
    let first = components.get(nm + 1)?.as_os_str().to_str()?;
    let name_end = nm + 1 + if first.starts_with('@') { 2 } else { 1 };
    if components.len() < name_end {
        return None;
    }
    // A target naming an existing file by its full name is a path the author spelled
    // out, not a package lookup.
    if components.len() > name_end && !extname(candidate).is_empty() && is_file(candidate) {
        return None;
    }
    let pkg_dir: PathBuf = components[..name_end].iter().collect();
    let package_json = pkg_dir.join("package.json");
    let text = std::fs::read_to_string(&package_json).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    // `"exports": null` is no exports map to Node, here as there.
    if manifest
        .get("exports")
        .is_none_or(serde_json::Value::is_null)
    {
        return None;
    }
    let name = manifest.get("name")?.as_str()?;
    let mut specifier = name.to_string();
    for c in &components[name_end..] {
        specifier.push('/');
        specifier.push_str(c.as_os_str().to_str()?);
    }
    Some(PackageTarget {
        specifier,
        package_json: package_json.to_string_lossy().into_owned(),
        file: None,
    })
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
fn resolve_bare_subpath(
    specifier: &str,
    parent_dir: &str,
    preserve_symlinks: bool,
) -> Option<String> {
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
    //
    // Beyond that, a subpath carrying a dot-segment or an empty segment belongs to
    // Node, because lexical normalization erases the very thing that gives such a
    // specifier its meaning:
    //   - `..` would escape the package root. Split on BOTH separators, since
    //     `path_join_resolve` goes through `Path::components`, which honors `\` on
    //     Windows — a `/`-only guard would let `pkg/a\..\..\other` walk straight out.
    //   - a trailing `/` or `/.` names a DIRECTORY. Node's CJS resolver goes straight
    //     to LOAD_AS_DIRECTORY and its ESM resolver raises ERR_UNSUPPORTED_DIR_IMPORT,
    //     but normalization drops that empty component — so probing would answer
    //     `pkg/sub/` with a sibling `pkg/sub.js` (a DIFFERENT module than Node loads)
    //     and would resolve `pkg/lone/` where Node reports it missing.
    if subpath.is_empty()
        || subpath
            .split(['/', '\\'])
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
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

    // `allow_dir_main: false`. Directory INDEX probing is deliberate — tsx resolves
    // `pkg/dir` to `dir/index.js` and that is the parity target — but honoring a
    // nested directory's own `package.json` `main` goes beyond both tsx and Node's
    // ESM resolver, and CJS does not need it: declining here falls through to Node,
    // whose LOAD_AS_DIRECTORY already reads that `main`.
    let candidate = try_resolve_file(&target, false, &NODE_MODULES_PROBE)?;

    // Node realpaths a resolved module by default, and nub's load hooks REFUSE to
    // transpile anything under a `node_modules/` path. A workspace package symlinked
    // into node_modules (the `main: ./index.ts` monorepo shape) is only loadable once
    // that hop is resolved away.
    let resolved = real_path(&candidate);

    // A TS hit STILL under node_modules after the symlink hop is unshipped source the
    // load hooks refuse, so winning here is worse than not resolving at all: it turns
    // Node's ERR_MODULE_NOT_FOUND into a misleading
    // ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING, and — because probing settles on a
    // file before it ever considers a directory — `sub.ts` would outrank a sibling
    // `sub/index.js` that Node's CJS resolver loads today. Fall through instead, so
    // Node's own precedence and its own error both survive.
    //
    // The whole TS family, not just what [`NODE_MODULES_PROBE`] can land on: the
    // emit-convention swaps above reach `.mts` and `.cts` too (`pkg/sub.mjs` →
    // `sub.mts`), and those are exactly as unloadable inside a dependency.
    if is_node_modules(&resolved) && TS_PARENT_EXTS.contains(&extname(&resolved).as_str()) {
        return None;
    }

    // Decide on `resolved` above, but hand back what NODE would hand back. Those
    // differ only under `--preserve-symlinks`, and there the distinction is the whole
    // ballgame: every other resolution in the process keys a symlinked workspace
    // package by its un-realpathed path, so returning the real one here would key the
    // SAME file two ways and instantiate its module twice.
    Some(if preserve_symlinks {
        candidate
    } else {
        resolved
    })
}

/// The TypeScript source behind a missing build output, for the unbuilt workspace
/// package. Called only after Node has failed to resolve, so a built file always
/// wins. `missing` is the absolute path Node could not find: what a package's
/// `main`, `exports` or `imports` named, or the entry file. When Node's error does not
/// carry that path, `missing` is absent and `specifier` is a bare package specifier
/// whose path is re-derived (see [`package_path`]): from the `node_modules`
/// directories Node searched when `lookup_paths` is given (CommonJS, which a
/// `require.resolve` `paths` option can point anywhere), else up from `parent_path`
/// (the importer, empty for the entry), as ESM searches.
///
/// `probe` is for a path Node itself probes: a legacy `main`, a CommonJS subpath or
/// the entry, where `build` means `build.js` or `build/index.js`. An `exports` or
/// `imports` target is exact.
///
/// Returns the source file, or `None` to keep Node's error.
#[napi(catch_unwind)]
pub fn resolve_out_dir_source(
    missing: Option<String>,
    specifier: String,
    parent_path: String,
    probe: Option<bool>,
    tsconfig_path: Option<String>,
    preserve_symlinks: Option<bool>,
    lookup_paths: Option<Vec<String>>,
) -> Option<String> {
    let preserve_symlinks = preserve_symlinks.unwrap_or(false);
    let tsconfig_path = tsconfig_path.as_deref();
    match missing {
        Some(missing) => source_for_output(
            &missing,
            probe.unwrap_or(false),
            tsconfig_path,
            preserve_symlinks,
        ),
        None => {
            let parent_dir = if parent_path.is_empty() {
                std::env::current_dir().ok()?.to_string_lossy().into_owned()
            } else {
                dirname(&parent_path)
            };
            let path = package_path(&specifier, &parent_dir, lookup_paths.as_deref())?;
            source_for_output(&path, true, tsconfig_path, preserve_symlinks)
        }
    }
}

/// The path a bare specifier names inside a package with no `exports`, found as
/// Node finds the package: the first of `lookup_paths` holding `<name>`, or with
/// none given the nearest `node_modules/<name>` directory up from the importer. A
/// bare name means its legacy `main`; a subpath is joined on. Node's
/// error carries the exact path for an `exports` or `imports` target, so this covers
/// only the two shapes it does not: ESM's failed legacy `main` (reported without the
/// path on every version, and on Node 18 as an internal assertion), and a CommonJS
/// subpath (`Cannot find module 'pkg/dist/x'`).
fn package_path(
    specifier: &str,
    parent_dir: &str,
    lookup_paths: Option<&[String]>,
) -> Option<String> {
    if specifier.starts_with('#') || specifier.starts_with('.') {
        return None;
    }
    let mut segs = specifier.split('/');
    let first = segs.next()?;
    let name = if first.starts_with('@') {
        format!("{first}/{}", segs.next()?)
    } else {
        first.to_string()
    };
    let subpath = segs.collect::<Vec<_>>().join("/");
    // The same guard as [`resolve_bare_subpath`]: a dot or empty segment is a path
    // Node normalizes differently, or one that leaves the package.
    if !subpath.is_empty()
        && subpath
            .split(['/', '\\'])
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return None;
    }
    let pkg_dir = match lookup_paths {
        Some(dirs) => dirs.iter().find_map(|dir| {
            let pkg = Path::new(dir).join(&name);
            pkg.is_dir().then(|| pkg.to_string_lossy().into_owned())
        })?,
        None => tsconfig::find_up_dir(parent_dir, &format!("node_modules/{name}"))?,
    };
    let text = std::fs::read_to_string(Path::new(&pkg_dir).join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    if manifest.get("exports").is_some() {
        return None;
    }
    if !subpath.is_empty() {
        return Some(path_join_resolve(&pkg_dir, &subpath));
    }
    let main = manifest.get("main")?.as_str()?;
    if main.trim().is_empty() {
        return None;
    }
    Some(path_join_resolve(&pkg_dir, main))
}

/// tsc's output-to-source mapping (`tryLoadInputFileForPath`), run only where Node
/// came up empty. A path inside the `outDir` of the tsconfig that owns it maps to the
/// same path under `rootDir`, `.js` becoming `.ts`/`.tsx`, `.mjs` `.mts` and `.cjs`
/// `.cts`. The built file always wins: this is reached only once it is missing.
///
/// The owning tsconfig is the target package's, never the importer's, and `extends`
/// is followed with `tsc`'s path semantics, so an `outDir` inherited from a monorepo
/// root names the root's directory, not the package's. A config qualifies only from
/// inside the package (the nearest `package.json` above the target), which is also
/// `tsc`'s condition. The project's configured tsconfig is tried first, then the
/// nearest `tsconfig.json`.
///
/// A path inside `node_modules` once symlinks are resolved never maps, so a workspace
/// package linked into `node_modules` qualifies and a published dependency does not.
fn source_for_output(
    missing: &str,
    probe: bool,
    tsconfig_path: Option<&str>,
    preserve_symlinks: bool,
) -> Option<String> {
    if is_file(missing) {
        return None;
    }
    // The output directory usually does not exist, so the symlink hop is resolved on
    // the deepest ancestor that does.
    let mut lexical = PathBuf::from(missing);
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::metadata(&lexical).is_err() {
        rest.push(lexical.file_name()?.to_os_string());
        lexical = lexical.parent()?.to_path_buf();
    }
    let real_base = PathBuf::from(real_path(&lexical.to_string_lossy()));
    let mut real = real_base.clone();
    for part in rest.iter().rev() {
        real.push(part);
    }
    let real = real.to_string_lossy().into_owned();
    if is_node_modules(&real) {
        return None;
    }

    let pkg_root = Path::new(&real)
        .ancestors()
        .skip(1)
        .find(|dir| dir.join("package.json").is_file())?
        .to_path_buf();
    let mut configs: Vec<String> = Vec::new();
    if let Some(explicit) = tsconfig_path {
        let explicit = real_path(explicit);
        if Path::new(&explicit).starts_with(&pkg_root) {
            configs.push(explicit);
        }
    }
    let nearest = Path::new(&real)
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(&pkg_root))
        .map(|dir| dir.join("tsconfig.json"))
        .find(|config| config.is_file());
    if let Some(config) = nearest {
        let config = config.to_string_lossy().into_owned();
        if !configs.contains(&config) {
            configs.push(config);
        }
    }

    let mut outputs = Vec::new();
    if !probe || !extname(&real).is_empty() {
        outputs.push(real.clone());
    }
    if probe {
        outputs.push(format!("{real}.js"));
        outputs.push(
            Path::new(&real)
                .join("index.js")
                .to_string_lossy()
                .into_owned(),
        );
    }

    let source = configs.iter().find_map(|config| {
        let dirs = tsconfig::emit_dirs(config)?;
        outputs
            .iter()
            .find_map(|output| map_output_to_source(output, &dirs))
    })?;

    // Node keys a module by its real path, so a symlinked source is canonicalized as
    // Node's own realpath step would. Under `--preserve-symlinks` every module of a
    // linked package is keyed by its path through the link instead, so the answer is
    // spelled that way. Either way, the same file loads once.
    if !preserve_symlinks {
        let source = real_path(&source);
        return (!is_node_modules(&source)).then_some(source);
    }
    let (mut lexical, mut real_base) = (lexical, real_base);
    loop {
        if let Ok(tail) = Path::new(&source).strip_prefix(&real_base) {
            return Some(lexical.join(tail).to_string_lossy().into_owned());
        }
        if lexical.file_name().is_none() || lexical.file_name() != real_base.file_name() {
            return Some(source);
        }
        lexical = lexical.parent()?.to_path_buf();
        real_base = real_base.parent()?.to_path_buf();
    }
}

fn map_output_to_source(output: &str, dirs: &tsconfig::EmitDirs) -> Option<String> {
    let tail = Path::new(output).strip_prefix(&dirs.out_dir).ok()?;
    if tail.as_os_str().is_empty() {
        return None;
    }
    let ext = extname(output);
    let source_exts: &[&str] = match ext.as_str() {
        ".js" => &[".ts", ".tsx"],
        ".mjs" => &[".mts"],
        ".cjs" => &[".cts"],
        _ => return None,
    };
    let mapped = Path::new(&dirs.root_dir)
        .join(tail)
        .to_string_lossy()
        .into_owned();
    let stem = mapped.strip_suffix(ext.as_str())?;
    source_exts
        .iter()
        .map(|source_ext| format!("{stem}{source_ext}"))
        .find(|candidate| is_file(candidate) && !is_node_modules(candidate))
}

/// Does this specifier NAME A DIRECTORY rather than a file — a trailing separator
/// (`./sub/`) or a trailing `.` (`./sub/.`)?
///
/// Node treats that as decisive: its CJS resolver reads the trailing slash off the
/// RAW request and goes straight to LOAD_AS_DIRECTORY, skipping file probing
/// entirely, and its ESM resolver carries it through URL resolution to
/// `ERR_UNSUPPORTED_DIR_IMPORT`. Lexical normalization erases it — `Path::components`
/// yields nothing for a trailing separator — so probing would answer `./sub/` with a
/// sibling `./sub.ts` and resolve `./lone/` where Node reports it missing, both
/// silently.
///
/// Only the LAST segment is consulted, which is what separates this from the
/// stricter rule in [`resolve_bare_subpath`]. An interior dot-segment is legitimate
/// in a relative specifier — Node normalizes `./sub/./x` and resolves it — whereas a
/// package subpath carrying one is Node's to answer.
fn names_a_directory(specifier: &str) -> bool {
    matches!(specifier.rsplit(['/', '\\']).next(), Some("") | Some("."))
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

    // 2. Emit-convention swaps (only when the ext matches). `extname` reads past a
    // `?query`, so the extension is not always the path's tail: a target that does
    // not END in it has no stem to swap onto.
    let swaps: &[&str] = match existing_ext.as_str() {
        ".js" => &[".ts", ".tsx"],
        ".jsx" => &[".tsx"],
        ".mjs" => &[".mts"],
        ".cjs" => &[".cts"],
        _ => &[],
    };
    if let Some(stem) = target.strip_suffix(existing_ext.as_str()) {
        for ext in swaps {
            let swapped = format!("{stem}{ext}");
            if is_file(&swapped) {
                return Some(swapped);
            }
        }
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
