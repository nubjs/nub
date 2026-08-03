//! Native addon (`.node`) embedding.
//!
//! A `.node` file is a shared library, and `dlopen` will only take a real path on
//! a real filesystem — which is why a compiler that serves its app from a virtual
//! filesystem cannot load one at all. nub extracts the app to an ordinary
//! directory before running it, so the addon is just a file at a path and the
//! platform loader needs nothing special: no chmod (dlopen wants READ, not exec)
//! and no re-signing (a Mach-O's ad-hoc signature survives a byte copy). Both
//! verified on macOS/arm64, 2026-07-30.
//!
//! A native addon is carried as a reached package island rather than a flat
//! content-hashed asset. The island preserves the owning package and its
//! installed production dependency geometry, which is required by addons such
//! as sharp whose loader finds companion shared libraries relative to the
//! `.node` file. See [`super::native_layout`].
//!
//! THE EMITTED MODULE IS CommonJS ON PURPOSE. Every real addon is reached by a
//! `require()` from a CJS loader, and Rolldown's ESM→CJS interop hands a CJS
//! consumer the module NAMESPACE — so an `export default binding` would arrive as
//! `{ default: binding }` and every `binding.someFn` would be `undefined`, with no
//! error to attribute. `module.exports = …` makes `require()` yield the addon's
//! own exports, exactly as it does uncompiled. `import.meta.url` stays legal
//! inside Rolldown's CJS wrapper (it is a region of the ESM chunk), which is what
//! lets the module locate itself without `__filename`.
//!
//! WHAT MAKES THE PLATFORM PACKAGES RESOLVE AT ALL is not here — it is
//! `target_defines` in [`super`]. A modern NAPI-RS loader dispatches through
//! `if (process.platform === …) { if (process.arch === …) require("@scope/pkg-…") }`
//! and `@parcel/watcher` builds its specifier as
//! `` `@parcel/watcher-${process.platform}-${process.arch}` ``; with both operands
//! defined to the TARGET's values, Rolldown folds the template, dead-code-
//! eliminates the ladder, and resolves the one surviving literal. The addon that
//! gets embedded is therefore always the target's, never the build host's — which
//! is also why [`check_target`] can be a hard error rather than a warning.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, bail};
use nub_core::compile::{TargetArch, TargetOs, TargetPlatform};
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookTransformArgs, HookTransformOutput, HookTransformOutputMap,
    HookTransformReturn, HookUsage, Plugin, PluginContext, SharedLoadPluginContext,
    SharedTransformPluginContext,
};
use rolldown_common::ModuleType;
use rolldown_common::ResolvedExternal;
use rolldown_common::side_effects::HookSideEffects;
use rolldown_utils::url::clean_url;

use super::native_layout::{DroppedEdge, IslandFile, Seed};

/// The module a `.node` import evaluates to: the addon, `dlopen`ed from its
/// extracted package-island copy.
///
/// `createRequire(import.meta.url)` rather than a baked path — the build
/// machine's directory layout has nothing to do with the deploy machine's, and
/// the app dir is content-hash-keyed, so the only base that is right on both is
/// the chunk's own URL. `name` is already a nested, payload-relative island path.
/// The directory containing the `node_modules` tree `package` is installed in.
fn install_tree_root(package: &Path) -> Option<PathBuf> {
    let mut parts: Vec<&std::ffi::OsStr> = package.components().map(|c| c.as_os_str()).collect();
    let index = parts.iter().rposition(|part| *part == "node_modules")?;
    parts.truncate(index);
    Some(parts.iter().collect())
}

/// Copy one package directory into the payload at `rel`, skipping the nested
/// `node_modules` each package is materialised through on its own account.
fn copy_package_tree(
    dir: &Path,
    rel: &Path,
    target: &TargetPlatform,
    files: &mut BTreeMap<String, (Vec<u8>, bool)>,
) -> Result<()> {
    // A package carrying addons must contribute at least one this target can load.
    // Skipping every foreign build is right when a matching one exists beside them,
    // and silently WRONG when none does: that is a cross-compile against a tree
    // installed for the build host, and shipping the package with no loadable addon
    // produces a binary that fails on the user's machine having said nothing here.
    let mut saw_addon = false;
    let mut kept_addon = false;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if entry.file_name() != "node_modules" {
                    stack.push(path);
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let Ok(inner) = path.strip_prefix(dir) else {
                continue;
            };
            let name = rel.join(inner).to_string_lossy().replace('\\', "/");
            let bytes = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            // A foreign-platform addon is SKIPPED, not an error. Packages routinely
            // ship prebuilds for every platform they support — better-sqlite3 carries
            // a Windows one — and only the matching build is ever loaded, so failing
            // the compile because an irrelevant prebuild exists rejects perfectly
            // good packages. Dropping them also keeps the artifact to the one
            // platform it targets.
            //
            // The island path checks rather than skips because it is handed the ONE
            // addon the bundler resolved, where this walks a whole tree.
            if path.extension().is_some_and(|e| e == "node") {
                saw_addon = true;
                match check_target(&bytes, &path, target) {
                    Ok(()) => kept_addon = true,
                    Err(_) => continue,
                }
            }
            files.entry(name).or_insert((bytes, is_executable(&path)));
        }
    }
    if saw_addon && !kept_addon {
        // Re-run the check on one of them purely to reuse its diagnostic, which
        // names the platform found, the platform wanted, and how to install for
        // the target. Reporting that beats a bespoke message that would drift.
        if let Some(addon) = first_addon(dir) {
            let bytes = std::fs::read(&addon)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", addon.display()))?;
            check_target(&bytes, &addon, target)?;
        }
    }
    Ok(())
}

/// The first `.node` directly beneath `dir`, ignoring nested `node_modules`.
/// Used only to produce a diagnostic, so which one it finds does not matter.
fn first_addon(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).ok()?.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if entry.file_name() != "node_modules" {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "node") {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

/// The installed root of `specifier` as seen from `importer`.
///
/// Walks the importer's ancestors for `node_modules/<specifier>`, which is Node's
/// own lookup order, and stops at the first hit. Only the DIRECTORY is resolved —
/// the manifest inside it is all the caller needs — so none of `exports`, `main`
/// or condition resolution applies.
fn resolve_package_root(importer: &Path, specifier: &str) -> Option<PathBuf> {
    let mut dir = importer.parent()?;
    loop {
        let candidate = dir.join("node_modules").join(specifier);
        if candidate.join("package.json").is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

fn addon_module(name: &str) -> String {
    format!(
        "const record = process[Symbol.for(\"nub.compile.bootstrap\")];\n\
         module.exports = record.createRequire(import.meta.url)({});\n",
        serde_json::to_string(&format!("./{name}")).expect("an asset name serializes")
    )
}

/// Embeds `.node` addons, and clears the one CJS idiom that stops their loaders
/// from running once bundled.
#[derive(Debug)]
pub struct NativeAddons {
    /// The platform the artifact will run on. Every embedded addon is checked
    /// against it, because an addon for the wrong platform is unloadable and
    /// nothing later in the pipeline would notice.
    target: TargetPlatform,
    /// Whether `--loader` claimed `.node` itself. A user who maps it wants the
    /// other behavior (a path to hand `process.dlopen`, say), and silently
    /// overriding their flag is worse than not having the default.
    user_mapped: bool,
    /// Fixed-width wrapper token → cheap seed metadata. Package traversal and
    /// copying happen only in [`Self::plan_survivors`], after tree-shaking.
    seeds: Mutex<BTreeMap<String, Seed>>,
    /// Why an addon was refused, for `bundle` to attach to the failure.
    ///
    /// Kept here rather than carried by the hook's error because Rolldown
    /// replaces a plugin error with its own "plugin `X` threw an error" and DROPS
    /// the message — so a refusal reported only through the hook fails the build
    /// while telling the user nothing about which platform, or what to do
    /// (measured 2026-07-30). Same problem and same fix as
    /// `FilePlugin::case_hints`.
    rejections: Mutex<BTreeSet<String>>,
    /// Package roots excluded from the bundle, to be shipped in place.
    unbundled: Mutex<BTreeMap<PathBuf, String>>,
    /// Names the user forced unbundled, beyond what the manifest rules find.
    forced_unbundled: Vec<String>,
    /// Names the user forced into the bundle, overriding the manifest rules.
    forced_bundled: Vec<String>,
}

impl NativeAddons {
    pub fn new(
        target: TargetPlatform,
        user_mapped: bool,
        forced_unbundled: Vec<String>,
        forced_bundled: Vec<String>,
    ) -> Self {
        Self {
            target,
            user_mapped,
            seeds: Mutex::new(BTreeMap::new()),
            rejections: Mutex::new(BTreeSet::new()),
            unbundled: Mutex::new(BTreeMap::new()),
            forced_unbundled,
            forced_bundled,
        }
    }

    /// What to add to a failed bundle's error, if an addon was refused.
    pub fn rejections(&self) -> Vec<String> {
        self.rejections
            .lock()
            .map(|r| r.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Plan only islands whose fixed-width seed token survived into emitted
    /// JavaScript. Replacing it with the equal-length content digest does not
    /// move generated lines or columns, so source-map geometry stays valid.
    /// Materialise a package that was excluded from the bundle, at the position it
    /// already occupies on disk, together with everything it needs there.
    ///
    /// The point of leaving it unbundled is that its own layout is already correct:
    /// `__dirname` lands where its author expected, a sibling it reaches by walking
    /// up is where it was, and the addon it computes a path to is at the end of
    /// that path. So this copies rather than rearranges.
    ///
    /// It must NOT require the package to contain an addon. A napi-rs package is a
    /// JS-only wrapper whose per-platform sidecar holds the `.node` — `sharp` has
    /// none of its own, and `@img/sharp-darwin-arm64` has it — so a
    /// find-the-addon-first approach ships nothing at all for exactly the packages
    /// this exists to support. The dependency closure below is what reaches the
    /// sidecar, and `optionalDependencies` is load-bearing because that is where a
    /// napi-rs package declares them.
    ///
    /// Only what npm actually installed travels: an optional dependency for another
    /// platform never resolves here, so the per-platform fan costs nothing.
    fn materialise_unbundled(
        &self,
        root: &Path,
        anchor: &Path,
        files: &mut BTreeMap<String, (Vec<u8>, bool)>,
    ) -> Result<()> {
        let mut queue = vec![root.to_path_buf()];
        let mut seen = BTreeSet::new();
        while let Some(dir) = queue.pop() {
            if !seen.insert(dir.clone()) {
                continue;
            }
            let Ok(rel) = dir.strip_prefix(anchor) else {
                continue;
            };
            copy_package_tree(&dir, rel, &self.target, files)?;

            let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            for field in ["dependencies", "optionalDependencies"] {
                let Some(deps) = manifest.get(field).and_then(|v| v.as_object()) else {
                    continue;
                };
                for name in deps.keys() {
                    if let Some(next) = resolve_package_root(&dir.join("x"), name) {
                        queue.push(next);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn plan_survivors(
        &self,
        chunks: &mut [super::bundle::BundledFile],
    ) -> Result<PlannedNative> {
        let seeds = self
            .seeds
            .lock()
            .map_err(|_| anyhow::anyhow!("the native-addon seed collector was poisoned"))?
            .clone();
        let mut files = BTreeMap::<String, (Vec<u8>, bool)>::new();
        let mut summaries = BTreeSet::new();
        for root in self.unbundled_roots() {
            // The tree root the payload paths are relative to: the directory
            // holding the `node_modules` this package was installed into, so a
            // package lands at exactly the path Node will look for it at.
            let Some(anchor) = install_tree_root(&root) else {
                continue;
            };
            self.materialise_unbundled(&root, &anchor, &mut files)?;
        }
        for seed in seeds.values() {
            let token = seed.token.as_bytes();
            if !chunks
                .iter()
                .any(|chunk| contains_bytes(&chunk.bytes, token))
            {
                continue;
            }
            let bytes = std::fs::read(&seed.source).map_err(|e| {
                anyhow::anyhow!("reading native addon {}: {e}", seed.source.display())
            })?;
            check_target(&bytes, &seed.source, &self.target)?;
            let planned = seed.plan(&self.target)?;
            debug_assert_eq!(planned.token.len(), planned.digest.len());
            // Only for a survivor: an island whose wrapper was shaken out ships
            // nothing, so its missing companions cannot fail anything.
            warn_dropped_edges(&seed.source, &planned.dropped);
            for chunk in chunks.iter_mut() {
                replace_bytes(
                    &mut chunk.bytes,
                    planned.token.as_bytes(),
                    planned.digest.as_bytes(),
                );
            }
            summaries.insert(planned.summary);
            for file in planned.files {
                let body = (file.bytes, file.executable);
                match files.entry(file.name) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(body);
                    }
                    std::collections::btree_map::Entry::Occupied(entry) if entry.get() == &body => {
                    }
                    std::collections::btree_map::Entry::Occupied(entry) => bail!(
                        "native package island path {:?} identifies different bytes",
                        entry.key()
                    ),
                }
            }
        }
        Ok(PlannedNative {
            files: files
                .into_iter()
                .map(|(name, (bytes, executable))| IslandFile {
                    name,
                    bytes,
                    executable,
                })
                .collect(),
            summaries: summaries.into_iter().collect(),
        })
    }

    /// Whether a bare specifier names a package that must ship unbundled.
    ///
    /// Only bare specifiers: a relative or absolute request is the application's
    /// own file, and an already-resolved id has no package name to look up.
    fn classify_bare_specifier(&self, specifier: &str, importer: Option<&str>) -> bool {
        if specifier.starts_with('.') || specifier.starts_with('/') || specifier.contains('\0') {
            return false;
        }
        let Some(importer) = importer else {
            return false;
        };
        let Some(root) = resolve_package_root(Path::new(importer), specifier) else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(root.join("package.json")) else {
            return false;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        // The user's word beats the rules in both directions. Forcing a package
        // INTO the bundle is checked first: a false positive costs that package
        // its tree-shaking for no reason, and the alternative to a flag is waiting
        // on a nub release.
        if self.forced_bundled.iter().any(|n| n == specifier) {
            return false;
        }
        let reason = if self.forced_unbundled.iter().any(|n| n == specifier) {
            crate::compile::unbundlable::Reason::Forced
        } else {
            match crate::compile::unbundlable::classify(&manifest) {
                Some(reason) => reason,
                None => return false,
            }
        };
        if let Ok(mut roots) = self.unbundled.lock() {
            roots.insert(root, format!("{specifier} — {}", reason.describe()));
        }
        true
    }

    /// Package roots left unbundled, for the caller to materialise.
    pub fn unbundled_roots(&self) -> Vec<PathBuf> {
        self.unbundled
            .lock()
            .map(|roots| roots.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// What shipped unbundled and which rule selected it.
    ///
    /// Read from the RESOLVE-time set, not the transform-time one: excluding a
    /// package means none of its modules is ever loaded, so anything keyed on
    /// having seen its code reports nothing for exactly the packages that shipped.
    pub fn unbundled_summaries(&self) -> Vec<String> {
        self.unbundled
            .lock()
            .map(|roots| roots.values().cloned().collect())
            .unwrap_or_default()
    }

    fn claims(&self, id: &str) -> bool {
        !self.user_mapped && Path::new(id).extension().is_some_and(|e| e == "node")
    }
}

pub struct PlannedNative {
    pub files: Vec<IslandFile>,
    pub summaries: Vec<String>,
}

/// Say when an island shipped without an optional dependency that is not
/// installed.
///
/// Refusing is not available. An optional edge is also how a package names
/// companions for the platforms and configurations this target does NOT need —
/// the same mechanism that makes a cross-compile resolve — and a manifest gives
/// no way to tell those apart from a companion the addon genuinely loads (sharp
/// against a system libvips is the case a refusal would break). So the build says
/// what it left out while the author can still act, instead of either failing
/// correct builds or letting the artifact die at `dlopen` on a user's machine.
fn warn_dropped_edges(addon: &Path, dropped: &[DroppedEdge]) {
    for edge in dropped {
        eprintln!(
            "note: {} optionally depends on {}, which is not installed, so nothing\n\
             \x20\x20from it is embedded. If this addon loads a companion shared library from\n\
             \x20\x20that package, the compiled binary will fail at run time: {}",
            edge.owner,
            edge.name,
            addon.display()
        );
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn replace_bytes(bytes: &mut [u8], from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len(), "native island tokens are fixed-width");
    if from.is_empty() {
        return;
    }
    let mut offset = 0;
    while let Some(index) = bytes[offset..]
        .windows(from.len())
        .position(|window| window == from)
    {
        let start = offset + index;
        bytes[start..start + from.len()].copy_from_slice(to);
        offset = start + to.len();
    }
}

impl Plugin for NativeAddons {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:native-addons")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::Load | HookUsage::Transform | HookUsage::ResolveId
    }

    fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        // A package that computes its addon path cannot be bundled, so it is left
        // as a bare specifier and shipped beside the bundle in its own installed
        // layout — where Node's ordinary resolution finds it and `__dirname` means
        // what the package's author expected.
        //
        // Decided here rather than after resolution because excluding a module is
        // a resolve-time answer, and classification only needs the package ROOT:
        // `<ancestor>/node_modules/<specifier>/package.json`. That is a short walk,
        // not a reimplementation of Node resolution — no exports, main, or
        // condition handling is involved in reading a manifest.
        let external = self.classify_bare_specifier(args.specifier, args.importer);
        let specifier = args.specifier.to_string();
        async move {
            Ok(external.then(|| HookResolveIdOutput {
                id: specifier.into(),
                external: Some(ResolvedExternal::Bool(true)),
                ..Default::default()
            }))
        }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        // Rolldown hands `load` the id with any `?query` still attached, and a
        // query is legal in a specifier — so both the extension test and the read
        // run against the cleaned path.
        let id = clean_url(args.id).to_string();
        let claimed = self.claims(&id);
        async move {
            if !claimed {
                return Ok(None);
            }
            let path = Path::new(&id);
            // Ownership is the only work done for every resolved variant. Object
            // and package target checks, dependency traversal, and package reads
            // wait until emitted chunks prove this wrapper survived.
            let seed = match Seed::discover(path) {
                Ok(seed) => seed,
                Err(why) => {
                    if let Ok(mut seen) = self.rejections.lock() {
                        seen.insert(format!("{why:#}"));
                    }
                    return Err(why);
                }
            };
            let payload = match seed.wrapper_path() {
                Ok(path) => path,
                Err(why) => {
                    if let Ok(mut seen) = self.rejections.lock() {
                        seen.insert(format!("{why:#}"));
                    }
                    return Err(why);
                }
            };
            if let Ok(mut seen) = self.seeds.lock() {
                seen.insert(seed.token.clone(), seed);
            } else {
                let why = anyhow::anyhow!("the native-addon seed collector was poisoned");
                if let Ok(mut seen) = self.rejections.lock() {
                    seen.insert(format!("{why:#}"));
                }
                return Err(why);
            }
            let code = addon_module(&payload);
            Ok(Some(HookLoadOutput {
                code: code.into(),
                module_type: Some(ModuleType::Js),
                // Asserted, not left to `Default` (which means "decide for me" and
                // consults the addon package's own `sideEffects` field). Loading an
                // addon RUNS its initializer, and plenty are imported for exactly
                // that — a package declaring `"sideEffects": false` would otherwise
                // make the addon shakeable and silently drop it from the payload.
                side_effects: Some(HookSideEffects::True),
                ..Default::default()
            }))
        }
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        // Every module reaches `transform`, where `load` stops at the first plugin
        // to claim one — so this is the hook that can see the whole graph, and the
        // packages worth warning about are exactly the ones no other hook notices.
        // Real JavaScript only — a `text`/`json` module's "code" IS the file's
        // content, and blanking something inside it would corrupt user data.
        //
        // The substring test is what keeps this hook off the critical path: it is
        // a necessary condition for any match (the assignment's value must be a
        // `createRequire` call), and it holds for a handful of modules in a graph
        // of thousands — so almost nothing pays for the extra parse.
        let scannable = matches!(
            args.module_type,
            ModuleType::Js | ModuleType::Jsx | ModuleType::Ts | ModuleType::Tsx
        ) && args.code.contains("createRequire");
        let spans = if scannable {
            require_rebindings(args.id, args.code)
        } else {
            Vec::new()
        };
        let source = (!spans.is_empty()).then(|| args.code.to_string());
        async move {
            let Some(source) = source else {
                return Ok(None);
            };
            Ok(Some(HookTransformOutput {
                code: Some(blank_spans(&source, &spans)),
                // The rewrite replaces bytes with spaces and keeps every newline,
                // so no position moves — which is what makes `Null` (rather than
                // `Omitted`, which would drop the module's mapping entirely)
                // the honest answer.
                map: HookTransformOutputMap::Null,
                ..Default::default()
            }))
        }
    }
}

// ---- the `require = createRequire(…)` rebinding -------------------------------

/// Byte ranges of every module-level `require = createRequire(…)` statement.
///
/// WHY THIS HAS TO GO. NAPI-RS emits `require = createRequire(__filename)` at the
/// top of every generated loader so the file works when some other bundler turns
/// it into ESM. Rolldown rewrites the `require("…")` CALLS in that module anyway —
/// measured, the reassignment does not stop it — but it leaves the assignment
/// itself, and assigning to an undeclared `require` inside an ES module throws
/// `ReferenceError: require is not defined in ES module scope` before any of the
/// rewritten calls run. So the addon loader dies on line 3 of a module whose every
/// interesting line the bundler already got right.
///
/// Blanking it is not merely the fix that works, it is the only one that does.
/// Turning it into a DECLARATION (`var require = …`) also compiles — and is worse:
/// Rolldown then sees a module-level `require` binding, stops rewriting the calls,
/// and the loader fails at run time looking for `node_modules` in a cache
/// directory. Verified all three ways against Rolldown 1.2.0 on 2026-07-30.
///
/// What is left behind is `require` meaning Rolldown's `__require`, i.e.
/// `createRequire(import.meta.url)` of the chunk. That is the closest thing to
/// `createRequire(__filename)` that survives bundling, so the statement is not
/// just removable — it is redundant.
///
/// NARROW ON PURPOSE, four ways: the statement must be at module top level, the
/// target must be a bare `require` the module never declares, the callee must be
/// a binding imported from Node's `module` builtin, and its one argument must be
/// the side-effect-free `__filename` or `import.meta.url`. A module that declares
/// its own `require` is writing to a real local binding and is left alone; a local
/// function coincidentally named `createRequire` is not evidence that removing its
/// call preserves the program.
fn require_rebindings(id: &str, source: &str) -> Vec<(usize, usize)> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{
        AssignmentOperator, AssignmentTarget, BindingPattern, Declaration, Expression,
        ImportDeclarationSpecifier, ImportOrExportKind, Statement, VariableDeclarationKind,
    };
    use oxc_parser::Parser;
    use oxc_span::{GetSpan, SourceType};

    /// Whether a top-level statement introduces a `require` binding of its own.
    fn declares_require(stmt: &Statement<'_>) -> bool {
        match stmt {
            Statement::VariableDeclaration(d) => d.declarations.iter().any(|decl| {
                matches!(&decl.id, BindingPattern::BindingIdentifier(id) if id.name == "require")
            }),
            Statement::FunctionDeclaration(f) => {
                f.id.as_ref().is_some_and(|id| id.name == "require")
            }
            Statement::ClassDeclaration(c) => c.id.as_ref().is_some_and(|id| id.name == "require"),
            Statement::ImportDeclaration(i) => i.specifiers.as_ref().is_some_and(|specs| {
                specs.iter().any(|s| s.local().name == "require")
            }),
            // An exported `const require = …` is still a declaration.
            Statement::ExportNamedDeclaration(e) => e.declaration.as_ref().is_some_and(|d| {
                matches!(d, Declaration::VariableDeclaration(v) if v.declarations.iter().any(|decl| {
                    matches!(&decl.id, BindingPattern::BindingIdentifier(id) if id.name == "require")
                }))
            }),
            _ => false,
        }
    }

    fn is_module_builtin(specifier: &str) -> bool {
        matches!(specifier, "module" | "node:module")
    }

    /// A direct CommonJS import from Node's `module` builtin. `require` is only
    /// accepted after the module has established that it has not been rebound;
    /// otherwise this syntactically identical call could load arbitrary code.
    fn is_module_builtin_require(expr: &Expression<'_>) -> bool {
        let Expression::CallExpression(call) = expr else {
            return false;
        };
        matches!(&call.callee, Expression::Identifier(id) if id.name == "require")
            && call.arguments.len() == 1
            && matches!(
                call.arguments[0].as_expression(),
                Some(Expression::StringLiteral(specifier)) if is_module_builtin(specifier.value.as_str())
            )
    }

    /// `createRequire` has no observable work when called with one of the two
    /// ordinary module-location values NAPI-RS emits. Anything more expressive
    /// might run user code, so the whole assignment must remain intact.
    fn is_napi_rs_base(expr: &Expression<'_>) -> bool {
        matches!(expr, Expression::Identifier(id) if id.name == "__filename")
            || matches!(
                expr,
                Expression::StaticMemberExpression(member)
                    if member.property.name == "url"
                        && matches!(
                            &member.object,
                            Expression::MetaProperty(meta)
                                if meta.meta.name == "import" && meta.property.name == "meta"
                        )
            )
    }

    fn add_module_bindings(
        declaration: &oxc_ast::ast::VariableDeclaration<'_>,
        create_require_bindings: &mut BTreeSet<String>,
        module_namespaces: &mut BTreeSet<String>,
        require_rebound: bool,
    ) {
        if declaration.kind != VariableDeclarationKind::Const || require_rebound {
            return;
        }
        for declarator in &declaration.declarations {
            if !declarator
                .init
                .as_ref()
                .is_some_and(is_module_builtin_require)
            {
                continue;
            }
            match &declarator.id {
                BindingPattern::BindingIdentifier(id) => {
                    module_namespaces.insert(id.name.to_string());
                }
                BindingPattern::ObjectPattern(pattern) => {
                    for property in &pattern.properties {
                        if !property.computed
                            && property.key.is_specific_static_name("createRequire")
                            && let BindingPattern::BindingIdentifier(id) = &property.value
                        {
                            create_require_bindings.insert(id.name.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Only the two NAPI-RS shapes are eligible: a named `createRequire`
    /// binding, or `.createRequire` on a namespace imported from the same Node
    /// builtin. Tracking the binding avoids treating a custom API with the same
    /// spelling as Node's side-effect-free helper.
    fn is_napi_rs_create_require(
        expr: &Expression<'_>,
        create_require_bindings: &BTreeSet<String>,
        module_namespaces: &BTreeSet<String>,
    ) -> bool {
        let Expression::CallExpression(call) = expr else {
            return false;
        };
        if call.arguments.len() != 1
            || !call.arguments[0]
                .as_expression()
                .is_some_and(is_napi_rs_base)
        {
            return false;
        }
        match &call.callee {
            Expression::Identifier(id) => create_require_bindings.contains(id.name.as_str()),
            Expression::StaticMemberExpression(member) => {
                member.property.name == "createRequire"
                    && matches!(
                        &member.object,
                        Expression::Identifier(id) if module_namespaces.contains(id.name.as_str())
                    )
            }
            _ => false,
        }
    }

    let source_type = SourceType::from_path(id).unwrap_or_else(|_| SourceType::mjs());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    // A parse failure yields nothing: Rolldown's own error is the better
    // diagnostic, and this pass must never be the thing that fails a build.
    if parsed.panicked {
        return Vec::new();
    }
    if parsed.program.body.iter().any(declares_require) {
        return Vec::new();
    }

    let mut create_require_bindings = BTreeSet::new();
    let mut module_namespaces = BTreeSet::new();

    // ESM imports are hoisted, so their bindings are available irrespective of
    // where their declaration text appears in the module.
    for stmt in &parsed.program.body {
        let Statement::ImportDeclaration(import) = stmt else {
            continue;
        };
        if !is_module_builtin(import.source.value.as_str())
            || import.import_kind != ImportOrExportKind::Value
        {
            continue;
        }
        let Some(specifiers) = import.specifiers.as_ref() else {
            continue;
        };
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier)
                    if specifier.import_kind == ImportOrExportKind::Value
                        && specifier.imported.name() == "createRequire" =>
                {
                    create_require_bindings.insert(specifier.local.name.to_string());
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    module_namespaces.insert(specifier.local.name.to_string());
                }
                _ => {}
            }
        }
    }

    let mut require_rebound = false;
    parsed
        .program
        .body
        .iter()
        .filter_map(|stmt| {
            if let Statement::VariableDeclaration(declaration) = stmt {
                add_module_bindings(
                    declaration,
                    &mut create_require_bindings,
                    &mut module_namespaces,
                    require_rebound,
                );
                return None;
            }
            let Statement::ExpressionStatement(st) = stmt else {
                return None;
            };
            let Expression::AssignmentExpression(assign) = &st.expression else {
                return None;
            };
            let targets_require = matches!(
                &assign.left,
                AssignmentTarget::AssignmentTargetIdentifier(id) if id.name == "require"
            );
            let eligible = !require_rebound
                && assign.operator == AssignmentOperator::Assign
                && targets_require
                && is_napi_rs_create_require(
                    &assign.right,
                    &create_require_bindings,
                    &module_namespaces,
                );
            require_rebound |= targets_require;
            eligible.then(|| (st.span().start as usize, st.span().end as usize))
        })
        .collect()
}

/// Replace each range with whitespace, keeping every newline where it was.
///
/// Line-preserving, which is what lets the transform report an unchanged source
/// map. Ranges arrive in source order (they come from a single forward walk), so
/// one pass suffices.
fn blank_spans(source: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(source.len());
    let mut last = 0usize;
    for (start, end) in spans {
        out.push_str(&source[last..*start]);
        out.extend(
            source[*start..*end]
                .chars()
                .map(|c| if c == '\n' { '\n' } else { ' ' }),
        );
        last = *end;
    }
    out.push_str(&source[last..]);
    out
}

// ---- target matching ----------------------------------------------------------

/// What a `.node` file's object header says it is.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Object {
    os: TargetOs,
    arches: TargetArches,
}

/// The supported architectures an object can load on. Unlike an `Option`, this
/// never turns an unrecognized machine value into a wildcard match.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TargetArches(u8);

impl TargetArches {
    const X64: Self = Self(1);
    const ARM64: Self = Self(2);

    fn from_cpu(cpu: u32) -> Option<Self> {
        match cpu {
            // CPU_TYPE_ARM64 / CPU_TYPE_X86_64 (`mach/machine.h`).
            0x0100_000C => Some(Self::ARM64),
            0x0100_0007 => Some(Self::X64),
            _ => None,
        }
    }

    fn contains(self, arch: TargetArch) -> bool {
        self.0
            & match arch {
                TargetArch::X64 => Self::X64.0,
                TargetArch::Arm64 => Self::ARM64.0,
            }
            != 0
    }

    fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// The three native formats have recognizable headers even when their machine
/// field is not one nub can target. Keep that distinct from malformed or wholly
/// unrecognized input so unsupported machines never become a wildcard match.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Classification {
    Object(Object),
    UnsupportedMachine { os: TargetOs },
}

/// Refuse an addon the target cannot load.
///
/// This is a HARD ERROR, not a warning, and it is the whole reason a cross-compile
/// is trustworthy. `--platform linux-x64` on a macOS host resolves whatever
/// `node_modules` happens to hold; without this check a Mach-O addon would be
/// embedded silently and the binary would die on a Linux machine — the failure
/// furthest in time and space from the mistake. It can be strict precisely because
/// the platform defines make resolution pick the TARGET's platform package
/// whenever one is installed, so a mismatch means it genuinely is not there.
fn check_target(bytes: &[u8], path: &Path, target: &TargetPlatform) -> Result<()> {
    let advice = "\x20\x20A native addon is machine code for one platform, and a compiled binary \
                  loads it\n\x20\x20from a real file at run time — there is no later step that \
                  could translate it.\n\x20\x20Install this dependency for the target (its \
                  platform package) and compile again. For cross-platform installs,\n\x20\x20configure \
                  supportedArchitectures.os, supportedArchitectures.cpu, and\n\x20\x20supportedArchitectures.libc \
                  so the target's native packages are present;\n\x20\x20or drop --platform to build for \
                  this machine.";
    let found = match classify(bytes) {
        Some(Classification::Object(found)) => found,
        Some(Classification::UnsupportedMachine { os }) => bail!(
            "this native addon is built for an unsupported {} architecture: {}\n\
             \x20\x20nub can embed native addons only for x64 or arm64 targets.",
            os_name(os),
            path.display()
        ),
        None => bail!(
            "this file is not a native addon any supported platform can load: {}\n\
             \x20\x20Expected a Mach-O, ELF, or PE shared library; its header is none of those.",
            path.display()
        ),
    };
    if found.os != target.os {
        bail!(
            "this native addon is built for {}, but the binary targets {}: {}\n{advice}",
            os_name(found.os),
            target.triple(),
            path.display()
        );
    }
    if !found.arches.contains(target.arch) {
        bail!(
            "this native addon has no {} slice for {}, but the binary targets {}: {}\n{advice}",
            os_name(found.os),
            arch_name(target.arch),
            target.triple(),
            path.display()
        );
    }
    Ok(())
}

fn os_name(os: TargetOs) -> &'static str {
    match os {
        TargetOs::Darwin => "macOS",
        TargetOs::Linux => "Linux",
        TargetOs::Win32 => "Windows",
    }
}

fn arch_name(arch: TargetArch) -> &'static str {
    match arch {
        TargetArch::X64 => "x64",
        TargetArch::Arm64 => "arm64",
    }
}

/// Read the object header. Covers exactly the three formats nub targets, so a
/// file matching none of them cannot load anywhere and is reported as such.
///
/// glibc-vs-musl is deliberately NOT distinguished here: both are ordinary ELF
/// and the difference lives in the dynamic-link requirements, not the header.
/// [`super::native_layout`] checks package `libc` metadata against the target;
/// packages that omit it retain Node's ordinary best-effort behavior.
fn classify(bytes: &[u8]) -> Option<Classification> {
    let magic: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let le32 = |at: usize| -> Option<u32> {
        bytes
            .get(at..at + 4)
            .and_then(|s| s.try_into().ok())
            .map(u32::from_le_bytes)
    };
    let le16 = |at: usize| -> Option<u16> {
        bytes
            .get(at..at + 2)
            .and_then(|s| s.try_into().ok())
            .map(u16::from_le_bytes)
    };
    let classified = |os: TargetOs, arches: Option<TargetArches>| {
        arches
            .map(|arches| Classification::Object(Object { os, arches }))
            .unwrap_or(Classification::UnsupportedMachine { os })
    };
    match magic {
        // Mach-O, 64-bit, either byte order.
        [0xcf, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xcf] => {
            let cpu = thin_macho_cpu(bytes)?;
            Some(classified(TargetOs::Darwin, TargetArches::from_cpu(cpu)))
        }
        // Mach-O universal ("fat"), 32- and 64-bit headers, either byte order.
        [0xca, 0xfe, 0xba, 0xbe] => Some(classified(
            TargetOs::Darwin,
            fat_arches(bytes, Endian::Big, 20)?,
        )),
        [0xbe, 0xba, 0xfe, 0xca] => Some(classified(
            TargetOs::Darwin,
            fat_arches(bytes, Endian::Little, 20)?,
        )),
        [0xca, 0xfe, 0xba, 0xbf] => Some(classified(
            TargetOs::Darwin,
            fat_arches(bytes, Endian::Big, 32)?,
        )),
        [0xbf, 0xba, 0xfe, 0xca] => Some(classified(
            TargetOs::Darwin,
            fat_arches(bytes, Endian::Little, 32)?,
        )),
        [0x7f, b'E', b'L', b'F'] => Some(classified(
            TargetOs::Linux,
            match le16(18)? {
                // EM_X86_64 / EM_AARCH64.
                0x3E => Some(TargetArches::X64),
                0xB7 => Some(TargetArches::ARM64),
                _ => None,
            },
        )),
        // PE: the DOS stub points at the real header, whose Machine field is the
        // arch. A truncated or non-PE `MZ` file falls through to `None`.
        [b'M', b'Z', ..] => {
            let pe = le32(0x3C)? as usize;
            let sig = pe.checked_add(4)?;
            (bytes.get(pe..sig)? == b"PE\0\0".as_slice()).then_some(())?;
            Some(classified(
                TargetOs::Win32,
                match le16(sig)? {
                    // IMAGE_FILE_MACHINE_AMD64 / _ARM64.
                    0x8664 => Some(TargetArches::X64),
                    0xAA64 => Some(TargetArches::ARM64),
                    _ => None,
                },
            ))
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Endian {
    Big,
    Little,
}

impl Endian {
    fn u32(self, bytes: &[u8]) -> Option<u32> {
        let bytes: [u8; 4] = bytes.try_into().ok()?;
        Some(match self {
            Self::Big => u32::from_be_bytes(bytes),
            Self::Little => u32::from_le_bytes(bytes),
        })
    }

    fn u64(self, bytes: &[u8]) -> Option<u64> {
        let bytes: [u8; 8] = bytes.try_into().ok()?;
        Some(match self {
            Self::Big => u64::from_be_bytes(bytes),
            Self::Little => u64::from_le_bytes(bytes),
        })
    }
}

/// Read the CPU type from a loadable 64-bit Mach-O header. Fat slice records are
/// only dispatch metadata; the inner header is the authoritative object the
/// platform loader will actually open.
fn thin_macho_cpu(bytes: &[u8]) -> Option<u32> {
    let magic: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    match magic {
        [0xcf, 0xfa, 0xed, 0xfe] => Endian::Little.u32(bytes.get(4..8)?),
        [0xfe, 0xed, 0xfa, 0xcf] => Endian::Big.u32(bytes.get(4..8)?),
        _ => None,
    }
}

/// Parse a Mach-O universal header and every declared slice record. The parser
/// deliberately validates the slice ranges too: a header that merely claims a
/// target CPU but has no bytes for that slice is not a usable target addon.
fn fat_arches(bytes: &[u8], endian: Endian, record_size: usize) -> Option<Option<TargetArches>> {
    let count = usize::try_from(endian.u32(bytes.get(4..8)?)?).ok()?;
    let table_len = count.checked_mul(record_size)?.checked_add(8)?;
    bytes.get(..table_len)?;

    let mut arches: Option<TargetArches> = None;
    for index in 0..count {
        let start = 8usize.checked_add(index.checked_mul(record_size)?)?;
        let record = bytes.get(start..start.checked_add(record_size)?)?;
        let cpu = endian.u32(record.get(..4)?)?;
        let (offset, size) = if record_size == 20 {
            (
                u64::from(endian.u32(record.get(8..12)?)?),
                u64::from(endian.u32(record.get(12..16)?)?),
            )
        } else {
            (
                endian.u64(record.get(8..16)?)?,
                endian.u64(record.get(16..24)?)?,
            )
        };
        // A zero-length range is just another header-only claim, not a loadable
        // architecture slice.
        (size != 0).then_some(())?;
        // A fat-arch offset names an inner Mach-O object, so it cannot point
        // back into the fat header or its slice table.
        (offset >= table_len as u64).then_some(())?;
        let end = offset.checked_add(size)?;
        let start = usize::try_from(offset).ok()?;
        let end = usize::try_from(end).ok()?;
        let slice = bytes.get(start..end)?;
        let Some(arch) = TargetArches::from_cpu(cpu) else {
            continue;
        };
        if thin_macho_cpu(slice) != Some(cpu) {
            continue;
        }
        if let Some(arches) = &mut arches {
            arches.insert(arch);
        } else {
            arches = Some(arch);
        }
    }
    Some(arches)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(triple: &str) -> TargetPlatform {
        TargetPlatform::parse(triple).expect("a supported triple")
    }

    #[test]
    fn survivor_token_replacement_is_fixed_width_and_complete() {
        let mut bytes = b"aa1111bb1111cc".to_vec();
        replace_bytes(&mut bytes, b"1111", b"2222");
        assert_eq!(bytes, b"aa2222bb2222cc");
        assert!(!contains_bytes(&bytes, b"1111"));
    }

    fn macho(cpu: u32) -> Vec<u8> {
        let mut v = vec![0xcf, 0xfa, 0xed, 0xfe];
        v.extend_from_slice(&cpu.to_le_bytes());
        v.resize(64, 0);
        v
    }

    fn macho_big_endian(cpu: u32) -> Vec<u8> {
        let mut v = vec![0xfe, 0xed, 0xfa, 0xcf];
        v.extend_from_slice(&cpu.to_be_bytes());
        v.resize(64, 0);
        v
    }

    fn elf(machine: u16) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        v[18..20].copy_from_slice(&machine.to_le_bytes());
        v
    }

    fn pe(machine: u16) -> Vec<u8> {
        let mut v = vec![0u8; 256];
        v[..2].copy_from_slice(b"MZ");
        v[0x3C..0x40].copy_from_slice(&128u32.to_le_bytes());
        v[128..132].copy_from_slice(b"PE\0\0");
        v[132..134].copy_from_slice(&machine.to_le_bytes());
        v
    }

    fn write_u32(bytes: &mut [u8], value: u32, endian: Endian) {
        bytes.copy_from_slice(&match endian {
            Endian::Big => value.to_be_bytes(),
            Endian::Little => value.to_le_bytes(),
        });
    }

    fn write_u64(bytes: &mut [u8], value: u64, endian: Endian) {
        bytes.copy_from_slice(&match endian {
            Endian::Big => value.to_be_bytes(),
            Endian::Little => value.to_le_bytes(),
        });
    }

    fn fat(uses_64_bit_records: bool, endian: Endian, cpus: &[u32]) -> Vec<u8> {
        let record_size = if uses_64_bit_records { 32 } else { 20 };
        let table_len = 8 + record_size * cpus.len();
        let mut v = vec![0u8; table_len + 64 * cpus.len()];
        v[..4].copy_from_slice(match (uses_64_bit_records, endian) {
            (false, Endian::Big) => &[0xca, 0xfe, 0xba, 0xbe],
            (false, Endian::Little) => &[0xbe, 0xba, 0xfe, 0xca],
            (true, Endian::Big) => &[0xca, 0xfe, 0xba, 0xbf],
            (true, Endian::Little) => &[0xbf, 0xba, 0xfe, 0xca],
        });
        write_u32(&mut v[4..8], cpus.len() as u32, endian);
        for (index, cpu) in cpus.iter().enumerate() {
            let start = 8 + record_size * index;
            write_u32(&mut v[start..start + 4], *cpu, endian);
            let offset = table_len + 64 * index;
            if uses_64_bit_records {
                write_u64(&mut v[start + 8..start + 16], offset as u64, endian);
                write_u64(&mut v[start + 16..start + 24], 64, endian);
            } else {
                write_u32(&mut v[start + 8..start + 12], offset as u32, endian);
                write_u32(&mut v[start + 12..start + 16], 64, endian);
            }
            v[offset..offset + 64].copy_from_slice(&macho(*cpu));
        }
        v
    }

    #[test]
    fn every_targetable_object_format_is_recognized() {
        assert_eq!(
            classify(&macho(0x0100_000C)),
            Some(Classification::Object(Object {
                os: TargetOs::Darwin,
                arches: TargetArches::ARM64,
            }))
        );
        assert_eq!(
            classify(&elf(0x3E)),
            Some(Classification::Object(Object {
                os: TargetOs::Linux,
                arches: TargetArches::X64,
            }))
        );
        assert_eq!(
            classify(&pe(0xAA64)),
            Some(Classification::Object(Object {
                os: TargetOs::Win32,
                arches: TargetArches::ARM64,
            }))
        );
        assert_eq!(classify(b"#!/bin/sh\n"), None);
        assert_eq!(classify(b""), None);
    }

    #[test]
    fn fat_macho_only_accepts_arches_in_its_slice_table() {
        let x64 = fat(false, Endian::Big, &[0x0100_0007]);
        check_target(&x64, Path::new("x.node"), &target("darwin-x64")).expect("x64 slice");
        assert!(
            check_target(&x64, Path::new("x.node"), &target("darwin-arm64")).is_err(),
            "an x64-only universal binary cannot load on arm64"
        );

        let arm64 = fat(false, Endian::Big, &[0x0100_000C]);
        check_target(&arm64, Path::new("x.node"), &target("darwin-arm64")).expect("arm64 slice");
        assert!(
            check_target(&arm64, Path::new("x.node"), &target("darwin-x64")).is_err(),
            "an arm64-only universal binary cannot load on x64"
        );

        let universal = fat(false, Endian::Big, &[0x0100_0007, 0x0100_000C]);
        for triple in ["darwin-x64", "darwin-arm64"] {
            check_target(&universal, Path::new("x.node"), &target(triple))
                .expect("both-slice universal binary");
        }
    }

    #[test]
    fn fat_macho_parses_32_and_64_bit_headers_in_both_byte_orders() {
        for (uses_64_bit_records, endian) in [
            (false, Endian::Big),
            (false, Endian::Little),
            (true, Endian::Big),
            (true, Endian::Little),
        ] {
            let universal = fat(uses_64_bit_records, endian, &[0x0100_0007, 0x0100_000C]);
            for triple in ["darwin-x64", "darwin-arm64"] {
                check_target(&universal, Path::new("x.node"), &target(triple)).unwrap_or_else(
                    |err| panic!("{triple} must parse from this universal header: {err:#}"),
                );
            }
        }
    }

    #[test]
    fn fat_macho_accepts_a_matching_big_endian_inner_slice() {
        let mut universal = fat(false, Endian::Big, &[0x0100_0007]);
        let inner = 8 + 20;
        universal[inner..inner + 64].copy_from_slice(&macho_big_endian(0x0100_0007));
        check_target(&universal, Path::new("x.node"), &target("darwin-x64"))
            .expect("a valid big-endian x64 inner Mach-O slice");
    }

    #[test]
    fn fat_macho_rejects_truncated_slice_tables_and_overflowing_counts() {
        let header_only = [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 1];
        assert_eq!(
            classify(&header_only),
            None,
            "a header is not a universal binary"
        );

        let mut truncated_record = fat(false, Endian::Big, &[0x0100_0007]);
        truncated_record.truncate(8 + 19);
        assert_eq!(classify(&truncated_record), None, "every record must fit");

        let overflowing_count = [0xca, 0xfe, 0xba, 0xbe, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            classify(&overflowing_count),
            None,
            "a count whose table cannot fit is not trusted"
        );

        let mut truncated_slice = fat(false, Endian::Big, &[0x0100_0007]);
        truncated_slice.truncate(8 + 20 + 63);
        assert_eq!(
            classify(&truncated_slice),
            None,
            "a declared slice must be present in the file"
        );

        let mut empty_slice = fat(false, Endian::Big, &[0x0100_0007]);
        empty_slice[20..24].fill(0);
        assert_eq!(
            classify(&empty_slice),
            None,
            "a target slice needs bytes to load"
        );

        let mut overlapping_slice = fat(false, Endian::Big, &[0x0100_0007]);
        overlapping_slice[16..20].fill(0);
        assert_eq!(
            classify(&overlapping_slice),
            None,
            "a target slice cannot overlap the fat header or slice table"
        );

        let mut zero_filled_inner = fat(false, Endian::Big, &[0x0100_0007]);
        zero_filled_inner[28..].fill(0);
        assert!(
            check_target(
                &zero_filled_inner,
                Path::new("x.node"),
                &target("darwin-x64"),
            )
            .is_err(),
            "a fat record alone cannot make a zero-filled slice targetable"
        );

        let mut mismatched_inner = fat(false, Endian::Big, &[0x0100_0007]);
        mismatched_inner[28..].copy_from_slice(&macho(0x0100_000C));
        assert!(
            check_target(
                &mismatched_inner,
                Path::new("x.node"),
                &target("darwin-x64"),
            )
            .is_err(),
            "the inner Mach-O CPU must match the fat record"
        );
    }

    #[test]
    fn unsupported_elf_and_pe_machines_are_rejected() {
        for (bytes, platform, description) in [
            (elf(0xF3), "linux-x64", "RISC-V ELF"),
            (pe(0x014C), "win32-x64", "PE i386"),
        ] {
            assert!(matches!(
                classify(&bytes),
                Some(Classification::UnsupportedMachine { .. })
            ));
            let err = check_target(&bytes, Path::new("x.node"), &target(platform))
                .expect_err("{description} is not a supported target architecture");
            assert!(
                format!("{err:#}").contains("unsupported"),
                "must explain why {description} was refused: {err:#}"
            );
        }
    }

    // The cross-compile guarantee: a host-built addon must not ride into a
    // foreign-platform binary, because nothing downstream would notice.
    #[test]
    fn a_foreign_addon_is_refused_and_the_message_names_both_sides() {
        let err = check_target(
            &macho(0x0100_000C),
            Path::new("/p/node_modules/x/x.node"),
            &target("linux-x64"),
        )
        .expect_err("a Mach-O cannot ship in a linux binary");
        let msg = format!("{err:#}");
        assert!(msg.contains("macOS"), "must name what the addon is: {msg}");
        assert!(
            msg.contains("linux-x64"),
            "must name what was targeted: {msg}"
        );
        assert!(msg.contains("x.node"), "must name the file: {msg}");

        // Right OS, wrong arch is the same class of unloadable.
        assert!(
            check_target(&elf(0x3E), Path::new("x.node"), &target("linux-arm64")).is_err(),
            "an x64 ELF cannot ship in an arm64 binary"
        );
        // …and the matching cases pass, including a universal binary against
        // either arch.
        check_target(&macho(0x0100_0007), Path::new("x"), &target("darwin-x64")).expect("match");
        check_target(&pe(0x8664), Path::new("x"), &target("win32-x64")).expect("match");
        for triple in ["darwin-x64", "darwin-arm64"] {
            check_target(
                &fat(false, Endian::Big, &[0x0100_0007, 0x0100_000C]),
                Path::new("x"),
                &target(triple),
            )
            .expect("a universal binary satisfies either arch");
        }
    }

    #[test]
    fn a_file_that_is_not_an_addon_at_all_says_so() {
        let err = check_target(
            b"not an object file",
            Path::new("x.node"),
            &target("darwin-arm64"),
        )
        .expect_err("unrecognized headers are refused");
        assert!(
            format!("{err:#}").contains("Mach-O, ELF, or PE"),
            "must say what was expected: {err:#}"
        );
    }

    // The NAPI-RS preamble, verbatim. Left in place it throws
    // `ReferenceError: require is not defined in ES module scope` before any
    // rewritten call runs.
    #[test]
    fn the_napi_rs_require_rebinding_is_blanked_and_nothing_else_moves() {
        let source = "const { createRequire } = require('node:module')\n\
                      require = createRequire(__filename)\n\
                      module.exports = require('@scope/pkg-darwin-arm64')\n";
        let spans = require_rebindings("index.js", source);
        assert_eq!(spans.len(), 1, "exactly the rebinding: {spans:?}");
        let out = blank_spans(source, &spans);
        assert!(
            !out.contains("require = createRequire"),
            "the rebinding must be gone: {out}"
        );
        assert!(
            out.contains("const { createRequire } = require('node:module')")
                && out.contains("module.exports = require('@scope/pkg-darwin-arm64')"),
            "everything else must survive byte-for-byte: {out}"
        );
        assert_eq!(
            out.lines().count(),
            source.lines().count(),
            "line geometry must be preserved, or the Null source map lies"
        );
        assert_eq!(out.len(), source.len(), "only bytes inside the span change");
    }

    #[test]
    fn only_a_bare_require_assigned_a_node_module_create_require_call_is_touched() {
        // A module that DECLARES require owns that binding; rewriting there would
        // also stop Rolldown rewriting its calls, which is strictly worse.
        for declared in [
            "let require = createRequire(import.meta.url)\nrequire = createRequire(import.meta.url)\n",
            "import { createRequire } from 'node:module'\nfunction require() {}\nrequire = createRequire(1)\n",
        ] {
            assert!(
                require_rebindings("index.js", declared).is_empty(),
                "a declared require must be left alone: {declared}"
            );
        }
        // A nested assignment is out of scope — see `require_rebindings`.
        assert!(
            require_rebindings("i.js", "function f() { require = createRequire(1) }\n").is_empty()
        );
    }

    #[test]
    fn napi_rs_node_module_bindings_are_blanked() {
        // NAPI-RS's generated preamble uses the first spelling. The other cases
        // prove the same binding-aware treatment for Node's compatible `module`
        // specifier, aliases, namespaces, and ESM import bindings.
        for source in [
            "const { createRequire } = require('node:module')\nrequire = createRequire(__filename)\n",
            "const { createRequire: makeRequire } = require('module')\nrequire = makeRequire(__filename)\n",
            "const Module = require('node:module')\nrequire = Module.createRequire(__filename)\n",
            "import { createRequire as makeRequire } from 'node:module'\nrequire = makeRequire(import.meta.url)\n",
            "import * as Module from 'module'\nrequire = Module.createRequire(import.meta.url)\n",
        ] {
            assert_eq!(
                require_rebindings("index.js", source).len(),
                1,
                "the Node builtin binding is the generated-loader idiom: {source}"
            );
        }
    }

    #[test]
    fn custom_create_require_bindings_and_side_effects_are_preserved() {
        for source in [
            // Matching a local helper by spelling deleted real program behavior.
            "const { createRequire } = require('./module')\nrequire = createRequire(__filename)\n",
            "import { createRequire } from './module'\nrequire = createRequire(import.meta.url)\n",
            "const Module = require('./module')\nrequire = Module.createRequire(__filename)\n",
            "const createRequire = makeRequire()\nrequire = createRequire(__filename)\n",
            // Even Node's own helper must remain when evaluating its base does
            // work; blanking the entire assignment would skip that call.
            "const { createRequire } = require('node:module')\nrequire = createRequire(recordSideEffect())\n",
            "const { createRequire } = require('node:module')\nrequire = createRequire(__filename, recordSideEffect())\n",
        ] {
            assert!(
                require_rebindings("index.js", source).is_empty(),
                "only a side-effect-free Node builtin binding is removable: {source}"
            );
        }
    }

    #[test]
    fn a_prior_require_rebinding_preserves_later_canonical_assignments() {
        for source in [
            "const { createRequire } = require('node:module')\n\
             require = custom\n\
             require = createRequire(__filename)\n",
            "const { createRequire } = require('node:module')\n\
             require += custom\n\
             require = createRequire(__filename)\n",
        ] {
            assert!(
                require_rebindings("index.js", source).is_empty(),
                "a prior simple or compound assignment makes later require behavior observable: {source}"
            );
        }
    }

    // The interop contract: a CJS consumer must receive the addon's own exports,
    // never a `{ default: … }` namespace.
    /// Only the target platform's addon travels.
    ///
    /// Packages ship prebuilds for every platform they support — better-sqlite3
    /// carries a Windows one — so the foreign builds must be DROPPED rather than
    /// copied or treated as an error. Copying them makes a cross-compiled artifact
    /// carry binaries it can never load; erroring on them rejects packages that are
    /// perfectly fine, which is what a first attempt at this did: it failed
    /// better-sqlite3 on its own host because a win32 prebuild sat in the tree.
    ///
    /// The same-platform half is the control: without it this passes for any
    /// implementation that copies nothing.
    #[test]
    fn only_the_target_platforms_addon_is_materialised() {
        let dir = std::env::temp_dir().join(format!(
            "nub-xcompile-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A darwin-arm64 addon, as an installed tree on this host would hold.
        std::fs::write(dir.join("addon.node"), macho(0x0100_000C)).unwrap();

        let mut files = BTreeMap::new();
        let native = copy_package_tree(
            &dir,
            Path::new("node_modules/pkg"),
            &target("darwin-arm64"),
            &mut files,
        );
        assert!(
            native.is_ok() && files.contains_key("node_modules/pkg/addon.node"),
            "control: the addon must be accepted for the platform it was built for"
        );

        let mut foreign_files = BTreeMap::new();
        let foreign = copy_package_tree(
            &dir,
            Path::new("node_modules/pkg"),
            &target("linux-x64"),
            &mut foreign_files,
        );
        assert!(
            foreign.is_err(),
            "when NOTHING matches the target the compile must fail — that is a \
             cross-compile against a tree installed for the build host, and shipping \
             the package with no loadable addon is a binary that dies on the user's \
             machine having said nothing here"
        );
        assert!(
            !foreign_files.contains_key("node_modules/pkg/addon.node"),
            "and the foreign addon must not have travelled"
        );

        // The other half, and the reason this is a skip rather than a hard reject:
        // a foreign prebuild BESIDE a matching one is every package that ships
        // prebuilds for the platforms it supports. Erroring on those failed
        // better-sqlite3 on its own host, which carries a win32 build next to the
        // darwin one.
        std::fs::write(dir.join("other.node"), elf(0x3e)).unwrap();
        let mut mixed = BTreeMap::new();
        copy_package_tree(
            &dir,
            Path::new("node_modules/pkg"),
            &target("darwin-arm64"),
            &mut mixed,
        )
        .expect("a foreign prebuild beside a matching one is not an error");
        assert!(
            mixed.contains_key("node_modules/pkg/addon.node")
                && !mixed.contains_key("node_modules/pkg/other.node"),
            "the matching addon travels and the foreign one does not"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_emitted_module_is_commonjs_and_locates_itself_from_the_chunk() {
        let code = addon_module("watcher-a1b2c3d4.node");
        assert!(
            code.contains("module.exports ="),
            "must be CJS-shaped or Rolldown's interop wraps it: {code}"
        );
        assert!(
            code.contains("import.meta.url"),
            "must locate itself from the chunk, not a build-machine path: {code}"
        );
        assert!(
            code.contains(r#"process[Symbol.for("nub.compile.bootstrap")]"#)
                && code.contains("record.createRequire(import.meta.url)"),
            "the captured factory must create the loader-visible require: {code}"
        );
        assert!(
            !code.contains(r#"require("node:module")"#),
            "the addon helper must not perform a late builtin require: {code}"
        );
        assert!(
            code.contains(r#""./watcher-a1b2c3d4.node""#),
            "must name the payload entry: {code}"
        );
        let allocator = oxc_allocator::Allocator::default();
        let parsed =
            oxc_parser::Parser::new(&allocator, &code, oxc_span::SourceType::mjs()).parse();
        assert!(
            !parsed.panicked && parsed.diagnostics.is_empty(),
            "the generated module must parse: {:?}",
            parsed.diagnostics
        );
    }
}
