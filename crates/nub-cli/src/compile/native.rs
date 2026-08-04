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
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
/// Whether an ELF addon is linked against musl, glibc, or neither detectably.
///
/// The ELF header records machine and OS but NOT which libc, so a glibc build and
/// a musl build of the same addon are indistinguishable to the platform check —
/// and both then travel into a musl artifact, where the loader picks whichever
/// comes first and fails if that is the glibc one. Observed on Alpine with
/// better-sqlite3, which ships both.
///
/// The distinction is in the symbol and string tables rather than the header:
/// a glibc build carries versioned symbols (`__cxa_finalize@GLIBC_2.17`), and a
/// musl build names its interpreter (`libc.musl-aarch64.so.1`). Matched as raw
/// bytes because that is all this needs — parsing the dynamic section to learn one
/// bit would be a lot of machinery for a string that is right there.
///
/// `None` means neither marker is present, which is treated as "cannot tell" and
/// left to travel: refusing an addon we merely failed to classify would reject
/// working packages, and the platform check above has already agreed on OS and
/// architecture.
fn elf_libc_is_musl(bytes: &[u8]) -> Option<bool> {
    if contains_bytes(bytes, b"libc.musl") {
        return Some(true);
    }
    if contains_bytes(bytes, b"GLIBC_") {
        return Some(false);
    }
    None
}

/// The project directory the whole `node_modules` tree hangs off.
///
/// The FIRST `node_modules` component, not the last. A nested install lives at
/// `<proj>/node_modules/holder/node_modules/pkg`, and anchoring at the last one
/// would make its payload path `node_modules/pkg` — identical to the top-level
/// copy's. The two then collide, the first written wins, and the loser's version
/// silently is not in the artifact: the binary either loads the wrong version or
/// fails outright.
///
/// Matches `native_layout::install_anchor`, which has always taken the first.
fn install_tree_root(package: &Path) -> Option<PathBuf> {
    let mut parts: Vec<&std::ffi::OsStr> = package.components().map(|c| c.as_os_str()).collect();
    let index = parts.iter().position(|part| *part == "node_modules")?;
    parts.truncate(index);
    Some(parts.iter().collect())
}

/// Copy one package directory into the payload at `rel`, skipping the nested
/// `node_modules` each package is materialised through on its own account.
/// A payload path: relative, `/`-separated, whatever the host uses.
fn payload_path(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Whether a package at `at` is on `dependent`'s own upward lookup path.
///
/// Node resolves a bare specifier by walking up from the importer looking for
/// `node_modules/<name>`, so `at` has to be `<ancestor of dependent>/node_modules/<name>`.
/// Shared by placement and by the dedupe that reuses an existing placement.
fn reachable_from(dependent: &str, at: &str) -> bool {
    let Some((base, _)) = at.rsplit_once("node_modules/") else {
        return false;
    };
    let base = base.trim_end_matches('/');
    base.is_empty() || dependent == base || dependent.starts_with(&format!("{base}/"))
}

/// Where a dependency has to sit for its dependent to find it.
///
/// Node resolves a bare specifier by walking up from the importer looking for
/// `node_modules/<name>`, so a dependency is only reachable from its dependent
/// when its path is `<some ancestor of the dependent>/node_modules/<name>`.
///
/// A flat install already satisfies that — `node_modules/detect-libc` is
/// reachable from `node_modules/sharp` — so its packages keep the exact paths
/// they occupy on disk, which is what makes `__dirname` and sibling lookups work.
///
/// An isolated install does not. There `node_modules/sharp` is a symlink and the
/// real package sits at `node_modules/.store/sharp@0.35.3/node_modules/sharp`
/// with its dependencies beside it, so the dependency's own path is reachable
/// only from inside `.store/` — not from the symlink the bundle resolves through.
/// Since the payload cannot carry a symlink (the launcher rejects one in its
/// extraction as tampering), such a dependency is nested directly under its
/// dependent, which resolves from anywhere the dependent ends up.
fn placement_for(dependent: &str, dep_real: Option<&str>, name: &str) -> String {
    let reachable = dep_real.is_some_and(|dep| reachable_from(dependent, dep));
    match (reachable, dep_real) {
        (true, Some(dep)) => dep.to_string(),
        _ => format!("{dependent}/node_modules/{name}"),
    }
}

/// Whether this tree holds an addon built against the target's own C library.
///
/// Gates the libc check, which would otherwise be a regression rather than a fix.
/// Some packages ship ONE addon linked statically against musl precisely so it
/// runs under both libcs; dropping it for a glibc target would fail a package
/// that works today, and loudly, since a package left with no loadable addon
/// fails the build.
///
/// So the check only ever DISAMBIGUATES: it runs when the tree already contains
/// a build for the target's libc, which is exactly the case it exists for — two
/// candidates where the loader would otherwise pick by directory order.
fn has_libc_match(dir: &Path, target: &TargetPlatform) -> bool {
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
            if path.extension().is_none_or(|e| e != "node") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if check_target(&bytes, &path, target).is_ok()
                && elf_libc_is_musl(&bytes) == Some(target.musl)
            {
                return true;
            }
        }
    }
    false
}

/// Whether a package's own manifest says it can run on the target.
///
/// `os`, `cpu` and `libc` are what npm and pnpm use to decide whether to install
/// an optional dependency at all, so a per-platform sidecar states plainly that
/// it is for one platform: `@img/sharp-darwin-arm64` declares
/// `os: ["darwin"], cpu: ["arm64"]`, and the musl one adds `libc: ["musl"]`.
///
/// Without this, every installed sidecar travelled. A cross-platform install is
/// exactly the case that makes several of them present, so a linux artifact
/// carried the darwin and musl sidecars too — measured at ~140 MB of payload for
/// sharp where ~36 MB was usable. Filtering `.node` files alone could not fix it,
/// because the bulk of a sidecar is its shared library.
///
/// Absent fields mean "runs anywhere", and a leading `!` negates an entry, as npm
/// defines it. Anything unparseable keeps the package: this only ever drops what
/// a manifest positively rules out.
fn manifest_runs_on(manifest: &serde_json::Value, target: &TargetPlatform) -> bool {
    let matches = |field: &str, want: &str| {
        let Some(values) = manifest.get(field).and_then(serde_json::Value::as_array) else {
            return true;
        };
        let listed: Vec<&str> = values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        if listed.is_empty() {
            return true;
        }
        if listed.iter().any(|v| v.strip_prefix('!') == Some(want)) {
            return false;
        }
        let positive: Vec<&str> = listed
            .iter()
            .copied()
            .filter(|v| !v.starts_with('!'))
            .collect();
        positive.is_empty() || positive.contains(&want)
    };
    let os = match target.os {
        TargetOs::Darwin => "darwin",
        TargetOs::Linux => "linux",
        TargetOs::Win32 => "win32",
    };
    let cpu = match target.arch {
        TargetArch::X64 => "x64",
        TargetArch::Arm64 => "arm64",
    };
    let libc = if target.musl { "musl" } else { "glibc" };
    matches("os", os)
        && matches("cpu", cpu)
        && (target.os != TargetOs::Linux || matches("libc", libc))
}

/// What one package contributed, so the closure can judge the set.
#[derive(Default)]
struct AddonTally {
    saw: bool,
    kept: bool,
}

fn copy_package_tree(
    dir: &Path,
    rel: &Path,
    target: &TargetPlatform,
    disambiguate_libc: bool,
    files: &mut BTreeMap<String, (Vec<u8>, bool)>,
) -> Result<AddonTally> {
    // Reports what it saw and kept rather than deciding: the "must contribute a
    // loadable addon" rule belongs to the whole dependency closure, not to one
    // package. A napi-rs package ships one platform PER PACKAGE, so
    // `@img/sharp-linux-arm64` legitimately contains only a Linux addon and is
    // simply not the sidecar this target uses — its sibling holds that one.
    // Judging it alone failed every cross-platform install, including compiling
    // for the host once a foreign sidecar was present.
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
            // `file_type` is lstat-based, so a SYMLINK is neither dir nor file
            // and used to be dropped with no record — the package then shipped
            // without a file it has on disk, and the artifact failed at run time
            // on a read that works everywhere else. A payload cannot carry a
            // link (the launcher treats one in an extracted tree as tampering),
            // so the link becomes a regular file holding its target's bytes,
            // which is what reading through it returns anyway.
            //
            // Only when it resolves to a FILE. A symlinked directory is left
            // alone: following it would need cycle detection for a shape that
            // does not occur inside a published package, and copying the tree
            // twice under two names is worse than the status quo.
            if !kind.is_file() {
                if !kind.is_symlink() || !path.is_file() {
                    continue;
                }
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
                    Ok(()) => {}
                    Err(_) => continue,
                }
                // The header agrees on OS and architecture but says nothing about
                // libc, so a glibc and a musl build of the same addon both reach
                // here. Shipping both leaves the loader to pick, and on Alpine it
                // picks the glibc one and fails.
                //
                // Only a POSITIVE mismatch is dropped. `None` means unclassifiable
                // and must travel, which is `elf_libc_is_musl`'s stated contract:
                // an addon linked statically against musl so it runs under both
                // libcs carries neither marker, and `!= Some(target.musl)` was
                // true for it — so the one build the gate exists to protect was
                // the one it discarded, silently, whenever any OTHER addon in the
                // closure happened to be classifiable.
                if disambiguate_libc && elf_libc_is_musl(&bytes) == Some(!target.musl) {
                    continue;
                }
                kept_addon = true;
            }
            files.entry(name).or_insert((bytes, is_executable(&path)));
        }
    }
    Ok(AddonTally {
        saw: saw_addon,
        kept: kept_addon,
    })
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
    ///
    /// Copying is what the ROOT needs, not what its dependencies need. Each member
    /// of the closure gets the eject question put to it separately, and the ones
    /// that answer no are handed to [`super::closure`] to be bundled — see
    /// [`plan_closure`].
    fn materialise_unbundled(
        &self,
        root: &Path,
        anchor: &Path,
        files: &mut BTreeMap<String, (Vec<u8>, bool)>,
        closure: &mut super::closure::Plan,
    ) -> Result<()> {
        let Ok(root_rel) = root.strip_prefix(anchor) else {
            return Ok(());
        };
        // Each entry is a package's real directory paired with where it lands in
        // the payload. The two diverge under an isolated install, where the real
        // directory is inside `.store/` and the path Node reaches it by is a
        // symlink — see `placement_for`.
        // Each entry carries the dependent that reached it, so a package already
        // placed somewhere that dependent can also reach is reused instead of
        // copied again. sharp's 18 MB libvips sidecar was copied twice without it.
        //
        // Breadth-first, and that is load-bearing rather than incidental: a
        // shallower placement is reachable from more dependents, so placing it
        // first is what lets the deeper one be dropped. Depth-first placed
        // `sharp/@img/sharp-linux-arm64/@img/sharp-libvips-…` before
        // `sharp/@img/sharp-libvips-…`, and the shallow one cannot reuse a copy
        // buried under a sibling.
        let mut queue: VecDeque<(PathBuf, String, Option<String>)> =
            VecDeque::from([(root.to_path_buf(), payload_path(root_rel), None)]);
        let mut seen = BTreeSet::new();
        // EVERY placement of a package, not just the first. Keeping only the
        // first does not terminate: a dependent outside that placement's subtree
        // cannot reach it, so it places a second copy one level deeper, and a
        // dependency CYCLE then deepens the path forever. `seen` is no backstop —
        // it is keyed on the ever-deepening path, so it never repeats. Measured
        // as a hang on an isolated tree with `c -> d -> c` reached from two
        // branches, which is nub's own default install layout.
        let mut placed: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        // An ejected package must contribute at least one addon this target can
        // load, but the rule is about the CLOSURE. A napi-rs package puts each
        // platform in its own sidecar, so most of them hold nothing for this
        // target and that is correct; only the whole set coming up empty is a
        // cross-compile against a tree installed for the build host, which would
        // otherwise ship a package with nothing loadable and say nothing here.
        let mut saw_addon = false;
        let mut kept_addon = false;
        let mut unloadable: Option<PathBuf> = None;
        let mut members: Vec<Member> = Vec::new();
        while let Some((dir, rel, dependent)) = queue.pop_front() {
            // Keyed on the REAL directory: an isolated store reaches one package
            // through several symlinks, so the paths differ while the package does
            // not, and keying on the link left every copy looking distinct.
            let identity = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            // Already placed where this dependent can reach it: nothing to copy.
            if let (Some(ats), Some(from)) = (placed.get(&identity), dependent.as_deref()) {
                if ats.iter().any(|at| reachable_from(from, at)) {
                    continue;
                }
            }
            if !seen.insert(rel.clone()) {
                continue;
            }
            // A package that says it cannot run here is dropped whole, not merely
            // stripped of its addon: most of a per-platform sidecar's weight is
            // the shared library beside it.
            //
            // A dropped sidecar still COUNTS as an addon seen and not kept, or
            // the closure check below cannot fire — skipping the package means
            // its addon is never walked, and a cross-compile whose every sidecar
            // was foreign would compile clean and die on the user's machine.
            // Measured: sharp for linux against a macOS-only install produced a
            // 31 MB artifact that failed at run time.
            if let Some(manifest) = std::fs::read_to_string(dir.join("package.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            {
                if !manifest_runs_on(&manifest, &self.target) {
                    // The ROOT is different from a dependency: the bundle has
                    // already externalised its specifier, so an artifact without
                    // it carries an import that resolves to nothing. Dropping it
                    // quietly produced a clean build that died on the user's
                    // machine with ERR_MODULE_NOT_FOUND.
                    if dependent.is_none() {
                        let name = manifest
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("this package");
                        anyhow::bail!(
                            "{name} says it does not run on {}: {}\n\
                             \x20 Its package.json restricts os/cpu/libc, and a compiled binary \
                             resolves it\n\x20 from a real file at run time — leaving it out \
                             would ship an import that\n\x20 resolves to nothing. Install it for \
                             the target and compile again, or drop\n\x20 --platform to build for \
                             this machine.",
                            self.target.triple(),
                            dir.display()
                        );
                    }
                    if first_addon(&dir).is_some() {
                        saw_addon = true;
                        unloadable.get_or_insert(dir.clone());
                    }
                    continue;
                }
            }
            placed.entry(identity).or_default().push(rel.clone());

            let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            // The ROOT is what the eject rule convicted; every other member is
            // asked on its own account. Before this, one verdict at the root made
            // the whole closure verbatim by association — pdfkit reads its fonts
            // off `__dirname` and must ship as files, but fontkit never had to.
            let verbatim = dependent.is_none() || self.closure_member_ejects(&dir, &manifest);
            members.push(Member {
                dir: dir.clone(),
                rel: rel.clone(),
                name: manifest
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                version: manifest
                    .get("version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                verbatim,
            });
            // A bundled package is TRANSPARENT for placement: its code ends up in
            // the chunk of whichever boundary specifier reached it, and that chunk
            // sits beside the packages its own dependent can reach — so nesting a
            // dependency under a directory that now holds no code would put it
            // somewhere nothing walks up through.
            let context = if verbatim {
                rel.clone()
            } else {
                dependent.clone().unwrap_or_else(|| rel.clone())
            };
            // Peer dependencies are followed too. A peer is normally supplied by
            // the application rather than installed under the package, so it is
            // easy to read as somebody else's problem — but an ejected package
            // runs from real files and resolves its peer by walking up, exactly
            // like any other require. Left out, the package ships and fails at
            // run time on a module the manifest named.
            //
            // Nothing needs to consult peerDependenciesMeta.optional: an optional
            // peer that was not installed simply does not resolve, and one that
            // was is indistinguishable from a required peer at run time.
            for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
                let Some(deps) = manifest.get(field).and_then(|v| v.as_object()) else {
                    continue;
                };
                for name in deps.keys() {
                    // Resolved from the package's REAL directory. Under an
                    // isolated install a dependency lives beside its dependent
                    // inside `.store/`, which is unreachable by walking up from
                    // the symlink the dependent was found through.
                    let Some(next) = std::fs::canonicalize(&dir)
                        .ok()
                        .and_then(|real| resolve_package_root(&real.join("x"), name))
                    else {
                        continue;
                    };
                    let real_rel = next.strip_prefix(anchor).ok().map(payload_path);
                    let at = placement_for(&context, real_rel.as_deref(), name);
                    queue.push_back((next, at, Some(context.clone())));
                }
            }
        }
        plan_closure(&mut members, closure, files);
        // Asked of the whole closure for the same reason the tally is: a napi-rs
        // package puts each libc in its OWN sidecar, so no single package holds
        // both builds and a per-package answer is always "no". That shipped the
        // musl sidecar into every glibc artifact — harmless, since sharp picks its
        // sidecar by name, but ~10 MB of an artifact that cannot use it.
        let verbatim = || members.iter().filter(|member| member.verbatim);
        let disambiguate_libc = self.target.os == TargetOs::Linux
            && verbatim().any(|member| has_libc_match(&member.dir, &self.target));
        for Member { dir, rel, .. } in verbatim() {
            let tally =
                copy_package_tree(dir, Path::new(rel), &self.target, disambiguate_libc, files)?;
            saw_addon |= tally.saw;
            kept_addon |= tally.kept;
            if tally.saw && !tally.kept {
                unloadable.get_or_insert(dir.clone());
            }
        }

        if saw_addon && !kept_addon {
            // Re-run the check on one of them purely to reuse its diagnostic,
            // which names the platform found, the platform wanted, and how to
            // install for the target. Reporting that beats a bespoke message
            // that would drift.
            if let Some(addon) = unloadable.as_deref().and_then(first_addon) {
                let bytes = std::fs::read(&addon)
                    .map_err(|e| anyhow::anyhow!("reading {}: {e}", addon.display()))?;
                check_target(&bytes, &addon, &self.target)?;
                // Reached only when that check is HAPPY with the addon — which it
                // can be while the package is still unusable, because it reads the
                // ELF header and the header does not record libc. A glibc-only
                // sidecar for a musl target passed it and the compile went on to
                // ship nothing loadable.
                anyhow::bail!(
                    "no native addon here can be loaded on {}: {}\n\
                     \x20 Its platform packages are all for another C library. Install this \
                     dependency\n\x20 for the target and compile again, or drop --platform to \
                     build for this machine.",
                    self.target.triple(),
                    addon.display()
                );
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
        // ONE plan across every root. Two closures can overlap, and a package one
        // of them ships verbatim has to be external to the other's chunks — a
        // second copy inside a chunk would be a different module object reading a
        // different `__dirname`.
        let mut closure = super::closure::Plan::default();
        for root in self.unbundled_roots() {
            // NOT canonicalized, and that is deliberate. Under an isolated
            // install this is the SYMLINK path — `node_modules/sharp` — which is
            // where the bundle's own `require("sharp")` looks. Resolving it to
            // the real `.store/sharp@<v>/node_modules/sharp` makes the package
            // land there instead, and the artifact then cannot find it at all.
            // Measured: canonicalizing here shipped sharp into `.store` and the
            // native-islands gate failed with `Cannot find module 'sharp'`.
            //
            // So the root keeps the path Node resolves it by, while dependencies
            // are canonicalized during the walk to find what they really are.
            // The two forms differing is what `placement_for` is for.
            let Some(anchor) = install_tree_root(&root) else {
                continue;
            };
            self.materialise_unbundled(&root, &anchor, &mut files, &mut closure)?;
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
            closure,
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
            match crate::compile::unbundlable::classify(&root, &manifest) {
                Some(reason) => reason,
                None => return false,
            }
        };
        if let Ok(mut roots) = self.unbundled.lock() {
            roots.insert(root, format!("{specifier} — {}", reason.describe()));
        }
        true
    }

    /// Whether a package reached INSIDE an eject closure has to ship verbatim too.
    ///
    /// The same rules that ejected the root, plus two the root can never trip. A
    /// package holding a `.node`, or holding no JavaScript at all, is not code the
    /// bundler could stand in for: it is a napi-rs sidecar or the shared library
    /// beside one, reached by `dlopen` and a computed path rather than by any
    /// require. Bundling one replaces a real binary with a chunk nothing loads, and
    /// dropping one is how sharp loses its 18 MB of libvips with nothing failing at
    /// build time.
    fn closure_member_ejects(&self, dir: &Path, manifest: &serde_json::Value) -> bool {
        let name = manifest
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        // The user's word beats the rules in both directions, exactly as it does at
        // the root — and the flag has to reach INSIDE the closure, because that is
        // where a wrong verdict now costs correctness rather than tree-shaking.
        if self.forced_unbundled.iter().any(|n| n == name) {
            return true;
        }
        if self.forced_bundled.iter().any(|n| n == name) {
            return false;
        }
        if first_addon(dir).is_some() {
            return true;
        }
        let Some(code) = crate::compile::unbundlable::loadable_code(dir) else {
            return true;
        };
        code.is_empty()
            || builds_its_own_require(dir, &code)
            || crate::compile::unbundlable::classify(dir, manifest).is_some()
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
    /// What the ejected packages still reach across the eject boundary. The
    /// caller bundles it, because [`super::closure`] needs the bundler's flags and
    /// this plugin only knows the tree.
    pub closure: super::closure::Plan,
}

/// Whether the package reaches the filesystem by a path it builds at run time, in
/// the one shape [`crate::compile::unbundlable::classify`] structurally cannot
/// see.
///
/// `createRequire(import.meta.url)` is an ESM package announcing that it will load
/// something by path. `css-tree` does exactly that for `../data/patch.json`, and a
/// `.json` counts as METADATA to `computed_asset_read` — the bundler normally
/// inlines one — so no rule there fires and the package reads as bundlable.
/// Bundled, that require runs from the chunk's directory instead of the module's
/// and dies with `MODULE_NOT_FOUND`; it is what broke jsdom's closure, measured.
///
/// Scoped to closure members rather than added to `classify`, deliberately. The
/// same hazard exists for a package imported directly and is older than this
/// code, but widening a detector that landed hours ago on evidence gathered here
/// is a separate change with its own precision budget.
fn builds_its_own_require(dir: &Path, code: &[String]) -> bool {
    code.iter().any(|file| {
        std::fs::read_to_string(dir.join(file)).is_ok_and(|text| text.contains("createRequire"))
    })
}

/// One package in an eject closure, and where it lands.
struct Member {
    dir: PathBuf,
    rel: String,
    name: String,
    version: Option<String>,
    /// Ships as files. Otherwise it is bundled into the chunk of whichever
    /// specifier reached it, and its own files never enter the payload.
    verbatim: bool,
}

/// Turn one walked closure into a bundling plan, writing the stub manifests that
/// let Node resolve what the plan replaces.
///
/// The whole closure reverts to verbatim on either refusal — a package whose
/// specifiers cannot be enumerated, or one named through a subpath no `.js` file
/// can answer. Partial is not an option: a stub set missing one entry produces a
/// binary that builds clean and dies on the machine it ships to, which is the one
/// failure this pipeline exists to avoid.
fn plan_closure(
    members: &mut [Member],
    closure: &mut super::closure::Plan,
    files: &mut BTreeMap<String, (Vec<u8>, bool)>,
) {
    let mut named: Vec<(usize, String, super::closure::Uses)> = Vec::new();
    let mut refuse = false;
    for (index, member) in members.iter().enumerate().filter(|(_, m)| m.verbatim) {
        match super::closure::boundary_specifiers(&member.dir) {
            Some(found) => named.extend(found.into_iter().map(|(s, u)| (index, s, u))),
            None => refuse = true,
        }
    }

    let mut entries: Vec<super::closure::Entry> = Vec::new();
    #[allow(clippy::type_complexity)]
    let mut stubs: BTreeMap<
        String,
        (
            String,
            Option<String>,
            BTreeMap<String, BTreeMap<&'static str, String>>,
        ),
    > = BTreeMap::new();
    for (importer, specifier, uses) in named {
        let package = super::closure::package_of(&specifier);
        let bundled = |member: &&Member| !member.verbatim && member.name == package;
        // Which placement the importer would actually reach: Node walks up from the
        // file that ran, and an isolated install puts the same package under
        // several dependents. A named package with no reachable placement is a
        // refusal rather than a skip — the specifier is real and nothing would
        // answer it. A package outside the closure entirely (a builtin, an
        // undeclared dependency) has no placements at all and is left alone.
        let Some(member) = members
            .iter()
            .filter(bundled)
            .find(|member| reachable_from(&members[importer].rel, &member.rel))
        else {
            refuse |= members.iter().any(|member| bundled(&member));
            continue;
        };
        if !super::closure::answerable(&specifier, uses) {
            refuse = true;
            break;
        }
        let stub = stubs
            .entry(member.rel.clone())
            .or_insert_with(|| (member.name.clone(), member.version.clone(), BTreeMap::new()));
        for esm in [false, true] {
            if !(if esm { uses.imported } else { uses.required }) {
                continue;
            }
            let entry = super::closure::Entry {
                // Names no real file. The bundler needs only its DIRECTORY, so the
                // specifier resolves from the package that wrote it.
                importer: members[importer]
                    .dir
                    .join(format!("__nub_closure_{}.js", entries.len())),
                chunk: super::closure::chunk_path(&member.rel, entries.len(), esm),
                specifier: specifier.clone(),
                esm,
            };
            let (key, condition, target) = super::closure::export_entry(&member.rel, &entry);
            // Two ejected packages naming the same specifier share one chunk. The
            // map is written only alongside the push that BUILDS that chunk —
            // overwriting it on the second sighting left `restructure`'s manifest
            // pointing at a chunk number nothing had emitted, and the artifact died
            // on `Cannot find module`.
            let targets = stub.2.entry(key).or_default();
            if !targets.contains_key(condition) {
                targets.insert(condition, target);
                entries.push(entry);
            }
        }
    }

    // Nothing is written until both refusals have had their say, so giving up
    // leaves the payload exactly as it was before closure bundling existed.
    if refuse {
        for member in members.iter_mut() {
            member.verbatim = true;
        }
        return;
    }
    for (at, (name, version, exports)) in stubs {
        files.entry(format!("{at}/package.json")).or_insert((
            super::closure::stub_manifest(&name, version.as_deref(), &exports),
            false,
        ));
    }
    closure.entries.extend(entries);
    closure.verbatim.extend(
        members
            .iter()
            .filter(|member| member.verbatim && !member.name.is_empty())
            .map(|member| member.name.clone()),
    );
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
    // Names WHERE supportedArchitectures works, because it does not work
    // everywhere: the engine reads it from an incumbent pnpm or yarn's own
    // config, so a project using neither has no equivalent setting and the
    // advice was unactionable for exactly the reader most likely to hit this.
    let advice = "\x20\x20A native addon is machine code for one platform, and a compiled binary \
                  loads it\n\x20\x20from a real file at run time — there is no later step that \
                  could translate it. The\n\x20\x20install has to put the target's own platform \
                  package on disk before you compile.\n\
                  \n\x20\x20If this project uses pnpm or yarn, set supportedArchitectures.os, \
                  .cpu and .libc\n\x20\x20in its config and install again. Otherwise install on \
                  the target platform itself —\n\x20\x20a container of that platform is the \
                  usual way — or drop --platform to build for\n\x20\x20this machine.";
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
            false,
            &mut files,
        );
        assert!(
            native.as_ref().is_ok_and(|t| t.saw && t.kept)
                && files.contains_key("node_modules/pkg/addon.node"),
            "control: the addon must be accepted for the platform it was built for"
        );

        let mut foreign_files = BTreeMap::new();
        let foreign = copy_package_tree(
            &dir,
            Path::new("node_modules/pkg"),
            &target("linux-x64"),
            false,
            &mut foreign_files,
        );
        // Reported, not decided here. Whether this is fatal depends on the rest of
        // the closure: a napi-rs package puts each platform in its own sidecar, so
        // a package holding nothing for this target is the normal case and only the
        // whole closure coming up empty is the cross-compile failure. Judging it
        // per package failed every cross-platform install — including compiling for
        // the HOST once a foreign sidecar was installed beside the right one.
        // `materialise_unbundled` owns the verdict; the corpus covers it end to end.
        assert!(
            foreign.as_ref().is_ok_and(|t| t.saw && !t.kept),
            "a package with nothing for this target reports saw-but-kept-nothing"
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
            false,
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

    /// A nested install keeps its nesting in the payload.
    ///
    /// The anchor has to be the FIRST `node_modules`. Taking the last one makes
    /// `<proj>/node_modules/holder/node_modules/pkg` land at `node_modules/pkg` —
    /// exactly where the top-level copy lands — and one silently replaces the
    /// other in the payload.
    ///
    /// That failure is invisible from the outside, which is what makes it worth a
    /// test rather than a fixture run. Compiling a real project with both copies
    /// present produced a binary that exited 0 and printed the right answer with
    /// only one version shipped, because the two happened to be compatible. A
    /// genuinely different major would have loaded the wrong one just as quietly.
    #[test]
    fn a_nested_install_does_not_collide_with_the_top_level_one() {
        let proj = Path::new("/p");
        let top = Path::new("/p/node_modules/pkg");
        let nested = Path::new("/p/node_modules/holder/node_modules/pkg");

        assert_eq!(install_tree_root(top).as_deref(), Some(proj));
        assert_eq!(
            install_tree_root(nested).as_deref(),
            Some(proj),
            "both anchor at the project, so the nested one keeps its depth"
        );

        // What the payload names become, which is where the collision would land.
        let rel_top = top.strip_prefix(install_tree_root(top).unwrap()).unwrap();
        let rel_nested = nested
            .strip_prefix(install_tree_root(nested).unwrap())
            .unwrap();
        assert_ne!(
            rel_top, rel_nested,
            "distinct installs must produce distinct payload paths, or one \
             overwrites the other and the binary silently carries the wrong version"
        );
        assert_eq!(
            rel_nested,
            Path::new("node_modules/holder/node_modules/pkg")
        );
    }

    /// A glibc addon must not travel into a musl artifact, or the reverse.
    ///
    /// The ELF header agrees on OS and architecture for both, so the platform
    /// check passes either way and both builds of the same addon reach the
    /// payload. The loader then picks whichever comes first — observed on Alpine
    /// with better-sqlite3, which ships `linux-arm64.node` beside
    /// `linuxmusl-arm64.node`, where the glibc one was chosen and could not load.
    ///
    /// Both directions are asserted, and so is the undetectable case: an addon
    /// carrying neither marker must still travel, because refusing something we
    /// merely failed to classify would reject working packages.
    #[test]
    fn a_foreign_libc_addon_does_not_travel() {
        let glibc = b"....__cxa_finalize@GLIBC_2.17....".as_slice();
        let musl = b"....libc.musl-aarch64.so.1....".as_slice();
        let neither = b"....no libc marker here....".as_slice();

        assert_eq!(
            elf_libc_is_musl(glibc),
            Some(false),
            "GLIBC_ marks a glibc build"
        );
        assert_eq!(
            elf_libc_is_musl(musl),
            Some(true),
            "libc.musl marks a musl build"
        );
        assert_eq!(
            elf_libc_is_musl(neither),
            None,
            "an unclassifiable addon must not be claimed either way — it travels"
        );

        // The call site's own predicate, which is where this went wrong: the
        // helper returning None is only half of "it travels". `!= Some(want)` is
        // true for None and dropped it; `== Some(!want)` keeps it.
        for (bytes, label) in [
            (glibc, "glibc"),
            (musl, "musl"),
            (neither, "unclassifiable"),
        ] {
            let drop_for_musl_target = elf_libc_is_musl(bytes) == Some(false);
            let drop_for_gnu_target = elf_libc_is_musl(bytes) == Some(true);
            match label {
                "glibc" => assert!(drop_for_musl_target && !drop_for_gnu_target),
                "musl" => assert!(!drop_for_musl_target && drop_for_gnu_target),
                _ => assert!(
                    !drop_for_musl_target && !drop_for_gnu_target,
                    "an unclassifiable addon is dropped for NEITHER target"
                ),
            }
        }
    }

    /// A lone addon travels even when its libc looks wrong for the target.
    ///
    /// The libc check must only DISAMBIGUATE. Some packages ship one addon linked
    /// statically against musl so it runs under both libcs, and dropping it for a
    /// glibc target would fail a package that works today — loudly, since a package
    /// left with no loadable addon fails the build. So the check is gated on the
    /// tree already holding a build for the target's own libc.
    #[test]
    fn the_libc_check_only_fires_when_there_is_a_real_choice() {
        const AARCH64: u16 = 183;
        let musl_target = target("linux-arm64-musl");
        let gnu_target = target("linux-arm64");

        let write = |dir: &Path, name: &str, marker: &[u8]| {
            let mut bytes = elf(AARCH64);
            bytes.extend_from_slice(marker);
            std::fs::write(dir.join(name), bytes).expect("writing the fixture addon");
        };

        // Both libcs present — the case the check exists for.
        let both = tempfile::tempdir().expect("a temp dir");
        write(
            both.path(),
            "linux-arm64.node",
            b"__cxa_finalize@GLIBC_2.17",
        );
        write(
            both.path(),
            "linuxmusl-arm64.node",
            b"libc.musl-aarch64.so.1",
        );
        assert!(
            has_libc_match(both.path(), &musl_target),
            "a musl build is present, so the check may drop the glibc one"
        );
        assert!(
            has_libc_match(both.path(), &gnu_target),
            "and the reverse, for a glibc target"
        );

        // Only a musl build, targeting glibc: nothing to disambiguate, so the
        // check stays off and the addon travels rather than failing the build.
        let lone = tempfile::tempdir().expect("a temp dir");
        write(lone.path(), "linux-arm64.node", b"libc.musl-aarch64.so.1");
        assert!(
            !has_libc_match(lone.path(), &gnu_target),
            "a statically-linked musl addon is the only candidate — it must not be dropped"
        );

        // Neither marker: unclassifiable, so it cannot be the match that licenses
        // dropping anything. Distinguishes `== Some(musl)` from `!= Some(!musl)`,
        // which agree on every case above.
        let unmarked = tempfile::tempdir().expect("a temp dir");
        write(
            unmarked.path(),
            "linux-arm64.node",
            b"no libc marker at all",
        );
        assert!(
            !has_libc_match(unmarked.path(), &gnu_target),
            "an addon we cannot classify must not license dropping its neighbours"
        );
    }

    /// A dependency lands where its dependent can actually resolve it.
    ///
    /// Two install shapes, and only one of them puts a dependency somewhere the
    /// dependent reaches by walking up. Getting this wrong produced a binary that
    /// compiled clean and died on `Cannot find module 'detect-libc'`, because
    /// sharp shipped without the dependencies it was installed with.
    #[test]
    fn a_dependency_lands_where_its_dependent_can_resolve_it() {
        // Flat install: already reachable, so the package keeps its own path.
        assert_eq!(
            placement_for(
                "node_modules/sharp",
                Some("node_modules/detect-libc"),
                "detect-libc"
            ),
            "node_modules/detect-libc",
            "a flat install is already correct — do not move it"
        );

        // Isolated install: the real package is inside .store/ with its
        // dependencies beside it, reachable only from within .store/ and not from
        // the symlink the bundle resolved through.
        assert_eq!(
            placement_for(
                "node_modules/sharp",
                Some("node_modules/.store/sharp@0.35.3/node_modules/detect-libc"),
                "detect-libc"
            ),
            "node_modules/sharp/node_modules/detect-libc",
            "unreachable through the symlink, so it nests under its dependent"
        );

        // Nested under a dependent that is itself nested, and the case where the
        // dependency could not be located at all.
        assert_eq!(
            placement_for("node_modules/a/node_modules/b", None, "c"),
            "node_modules/a/node_modules/b/node_modules/c"
        );

        // Reachable from a deeper dependent, because the base is an ancestor.
        assert_eq!(
            placement_for(
                "node_modules/a/node_modules/b",
                Some("node_modules/a/node_modules/c"),
                "c"
            ),
            "node_modules/a/node_modules/c",
            "an ancestor's node_modules is on the dependent's own lookup path"
        );

        // A sibling's private tree is NOT on the lookup path, so it must nest.
        assert_eq!(
            placement_for("node_modules/a", Some("node_modules/b/node_modules/c"), "c"),
            "node_modules/a/node_modules/c",
            "a sibling's nested copy is unreachable from here"
        );
    }

    /// A sidecar that says it is for another platform does not travel.
    ///
    /// Filtering `.node` files alone left the rest of a foreign sidecar in the
    /// payload, and most of one is its shared library — measured at ~140 MB of
    /// sharp sidecars in a linux artifact where ~36 MB was usable. `os`/`cpu`/
    /// `libc` are what npm and pnpm use to decide whether to install an optional
    /// dependency at all, so the package states the answer itself.
    #[test]
    fn a_sidecar_for_another_platform_does_not_travel() {
        let m = |json: &str| serde_json::from_str::<serde_json::Value>(json).expect("valid json");
        let linux = target("linux-arm64");
        let musl = target("linux-arm64-musl");
        let darwin = target("darwin-arm64");

        let darwin_sidecar = m(r#"{"os":["darwin"],"cpu":["arm64"]}"#);
        assert!(manifest_runs_on(&darwin_sidecar, &darwin));
        assert!(
            !manifest_runs_on(&darwin_sidecar, &linux),
            "a darwin sidecar must not travel into a linux artifact"
        );

        // libc separates two packages that are otherwise identical.
        let musl_sidecar = m(r#"{"os":["linux"],"cpu":["arm64"],"libc":["musl"]}"#);
        assert!(manifest_runs_on(&musl_sidecar, &musl));
        assert!(
            !manifest_runs_on(&musl_sidecar, &linux),
            "a musl sidecar is unusable on glibc"
        );

        // No fields means it runs anywhere — the ordinary case, and the reason
        // this only ever drops what a manifest positively rules out.
        assert!(manifest_runs_on(&m(r#"{"name":"ordinary"}"#), &linux));
        assert!(
            manifest_runs_on(&m(r#"{"os":[]}"#), &linux),
            "an empty list constrains nothing"
        );

        // npm's negation form.
        let not_win = m(r#"{"os":["!win32"]}"#);
        assert!(manifest_runs_on(&not_win, &linux));
        assert!(!manifest_runs_on(&not_win, &target("win32-x64")));
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
