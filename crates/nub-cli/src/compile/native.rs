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
use rolldown::plugin::__inner::SharedPluginable;
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
pub(super) fn install_tree_root(package: &Path) -> Option<PathBuf> {
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

/// Where an unbundled package's own root belongs in the payload.
///
/// A root is what BUNDLED code imports, and bundled code runs from the app root,
/// so its lookup starts there whatever the importer's path was in the source tree.
/// Any root below the top level is out of its reach: a transitive dependency of an
/// isolated install, whose real directory is
/// `node_modules/.bun/<owner>@<version>/node_modules/<name>`, and a nested install
/// of a hoisted one, at `node_modules/<owner>/node_modules/<name>`. Preserved
/// verbatim, the package ships and nothing can find it — measured compiling
/// opencode, where fastify, pino, msgpackr-extract and @npmcli/run-script all
/// landed in the store and the binary died on `Cannot find fastify`.
///
/// So such a root is flattened to the top-level `node_modules/<name>`, which is
/// where a bare specifier from the bundle looks. Only the ROOT moves; the dependency
/// walk below still places each dependency where its own dependent can reach it.
/// [`NativeAddons::out_of_reach`] is what keeps two installs from wanting that path.
fn flattened_root(rel: &Path) -> Option<String> {
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let first = parts.iter().position(|part| part == "node_modules")?;
    let last = parts.iter().rposition(|part| part == "node_modules")?;
    if last == first {
        return None;
    }
    let name = match parts.len() - last - 1 {
        // `node_modules/@scope/name`
        2 => format!("{}/{}", parts[last + 1], parts[last + 2]),
        1 => parts[last + 1].clone(),
        _ => return None,
    };
    Some(format!("node_modules/{name}"))
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

/// Every file a package ships from `dir`. A nested `node_modules` is another
/// package's business: each is materialised, and compared, on its own account.
fn package_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
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
            found.push(path);
        }
    }
    found
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
    for path in package_files(dir) {
        let Ok(inner) = path.strip_prefix(dir) else {
            continue;
        };
        let name = rel.join(inner).to_string_lossy().replace('\\', "/");
        let bytes =
            std::fs::read(&path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
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
        // Never a contest between packages: each claims its directory before it
        // is copied, and this walk stops at a nested `node_modules` — see `Payload`.
        files.entry(name).or_insert((bytes, is_executable(&path)));
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
pub(super) fn resolve_package_root(importer: &Path, specifier: &str) -> Option<PathBuf> {
    let mut dir = importer.parent()?;
    loop {
        let candidate = dir.join("node_modules").join(specifier);
        if candidate.join("package.json").is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

pub(super) fn addon_module(name: &str) -> String {
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
    /// Which of those are shipped: reached by the resolver or inside a closure, or
    /// resolved from the entry when nothing reached them — see
    /// [`Self::unreached_forced_roots`] and [`Self::unreached_forced_unbundled`].
    forced_seen: Mutex<BTreeSet<String>>,
    /// Names the user forced into the bundle, overriding the manifest rules.
    forced_bundled: Vec<String>,
    /// String-literal pino transport targets in bundled code, with the module
    /// that named each — see [`transport_targets`].
    transport_targets: Mutex<BTreeSet<(String, String)>>,
    /// Every package shipped VERBATIM: where it lands, and its directory. Its code
    /// runs from that placement, so a transport target it names is resolved from
    /// there — see [`Self::transport_roots`].
    verbatim_members: Mutex<BTreeSet<(String, PathBuf)>>,
    /// Every package any closure materialised, roots and members alike. Whether
    /// pino ships is read from this, not from the roots: a wrapper that carries
    /// pino in its closure ships it just the same.
    shipped_names: Mutex<BTreeSet<String>>,
    /// Closure members shipped as files because their chunks could not reach an
    /// install they import — see [`Payload::stranded_by`].
    stranded: Mutex<BTreeSet<PathBuf>>,
    /// `--external`: nothing shipped carries these, so a shipped package looks them up
    /// when it runs, like the bundle does — see `external::shim`.
    external: Vec<String>,
    /// Packages an earlier bundle found out of reach, by real directory, each with
    /// the dependency that decided it — see [`Self::out_of_reach`].
    own_installs: BTreeMap<PathBuf, String>,
    /// Every external this bundle kept: the specifier, who named it, and the install
    /// that importer resolves.
    external_edges: Mutex<BTreeSet<(String, String, PathBuf)>>,
}

impl NativeAddons {
    pub fn new(
        target: TargetPlatform,
        user_mapped: bool,
        forced_unbundled: Vec<String>,
        forced_bundled: Vec<String>,
        external: Vec<String>,
        own_installs: BTreeMap<PathBuf, String>,
    ) -> Self {
        Self {
            target,
            user_mapped,
            seeds: Mutex::new(BTreeMap::new()),
            rejections: Mutex::new(BTreeSet::new()),
            unbundled: Mutex::new(BTreeMap::new()),
            forced_seen: Mutex::new(BTreeSet::new()),
            forced_unbundled,
            forced_bundled,
            transport_targets: Mutex::new(BTreeSet::new()),
            verbatim_members: Mutex::new(BTreeSet::new()),
            shipped_names: Mutex::new(BTreeSet::new()),
            external,
            stranded: Mutex::new(own_installs.keys().cloned().collect()),
            own_installs,
            external_edges: Mutex::new(BTreeSet::new()),
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
        at: Option<String>,
        caller: &str,
        payload: &mut Payload,
        closure: &mut super::closure::Plan,
    ) -> Result<()> {
        let Ok(root_rel) = root.strip_prefix(anchor) else {
            return Ok(());
        };
        let seed_at = at
            .unwrap_or_else(|| flattened_root(root_rel).unwrap_or_else(|| payload_path(root_rel)));
        let mut walk = self.walk_closure(root, anchor, &seed_at, caller, false)?;
        // A bundled package is placed as if its code had moved, so a closure that
        // ships as files after all needs walking again: each member now looks up its
        // dependencies from its own directory.
        if !plan_closure(&walk.members, closure, payload)? {
            walk = self.walk_closure(root, anchor, &seed_at, caller, true)?;
        }
        let Walk {
            members,
            shadowed,
            mut saw_addon,
            mut unloadable,
        } = walk;
        self.record_shipped(&members);
        let mut kept_addon = false;
        // Asked of the whole closure for the same reason the tally is: a napi-rs
        // package puts each libc in its OWN sidecar, so no single package holds
        // both builds and a per-package answer is always "no". That shipped the
        // musl sidecar into every glibc artifact — harmless, since sharp picks its
        // sidecar by name, but ~10 MB of an artifact that cannot use it.
        let verbatim = || members.iter().filter(|member| member.verbatim);
        let disambiguate_libc = self.target.os == TargetOs::Linux
            && verbatim().any(|member| has_libc_match(&member.dir, &self.target));
        for member in verbatim() {
            match payload.claim_full(&member.rel, member.holder(None))? {
                Claim::Reuse => continue,
                // Whatever imported through the stub now resolves against the
                // package's own files, as it does uncompiled — so its chunks go.
                Claim::ReplacedStub => {
                    let chunks = format!("{}/__nub/", member.rel);
                    closure
                        .entries
                        .retain(|entry| !entry.chunk.starts_with(&chunks));
                }
                Claim::Copy => {}
            }
            let (dir, rel) = (&member.dir, &member.rel);
            let tally = copy_package_tree(
                dir,
                Path::new(rel),
                &self.target,
                disambiguate_libc,
                &mut payload.files,
            )?;
            saw_addon |= tally.saw;
            kept_addon |= tally.kept;
            if tally.saw && !tally.kept {
                unloadable.get_or_insert(dir.clone());
            }
        }

        // A package the walk skipped because another one had its placement. Its
        // code is inlined wherever it is reached unless it ships as files, and
        // then it needs that directory to itself.
        for (rel, dir, reached_from) in shadowed {
            let Some(manifest) = std::fs::read_to_string(dir.join("package.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            else {
                continue;
            };
            if !manifest_runs_on(&manifest, &self.target)
                || !self.closure_member_ejects(&dir, &manifest)
            {
                continue;
            }
            let skipped = Holder {
                name: manifest["name"].as_str().unwrap_or_default().to_string(),
                version: manifest["version"].as_str().map(str::to_string),
                instance: dir,
                caller: reached_from,
                stub: None,
            };
            let kept = members
                .iter()
                .find(|member| member.rel == rel)
                .map(|member| member.holder(None));
            if let Some(held) = payload.holders.get(&rel).or(kept.as_ref()) {
                if !same_instance(&held.instance, &skipped.instance) {
                    return Err(collision(&rel, held, &skipped));
                }
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

    /// Find one closure's members and where each lands, writing nothing.
    ///
    /// `all_files` is a closure [`plan_closure`] refused: every member ships as files.
    fn walk_closure(
        &self,
        root: &Path,
        anchor: &Path,
        seed_at: &str,
        caller: &str,
        all_files: bool,
    ) -> Result<Walk> {
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
            VecDeque::from([(root.to_path_buf(), seed_at.to_string(), None)]);
        let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
        // A different package reaching a placement already taken in this walk.
        let mut shadowed: Vec<(String, PathBuf, String)> = Vec::new();
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
            match seen.get(&rel) {
                Some(kept) if *kept == identity => continue,
                Some(_) => {
                    let reached_from = dependent.clone().unwrap_or_else(|| caller.to_string());
                    shadowed.push((rel, dir, reached_from));
                    continue;
                }
                None => {
                    seen.insert(rel.clone(), identity.clone());
                }
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
            placed
                .entry(identity.clone())
                .or_default()
                .push(rel.clone());

            let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            // The ROOT is what the eject rule convicted; every other member is
            // asked on its own account. Before this, one verdict at the root made
            // the whole closure verbatim by association — jsdom reads a stylesheet
            // from a path built at run time and must ship as files, but parse5 never had to.
            let verbatim =
                all_files || dependent.is_none() || self.closure_member_ejects(&dir, &manifest);
            members.push(Member {
                dir: dir.clone(),
                real: identity,
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
                caller: dependent.clone().unwrap_or_else(|| caller.to_string()),
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
                    if self.is_external(name) {
                        continue;
                    }
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
        Ok(Walk {
            members,
            shadowed,
            saw_addon,
            unloadable,
        })
    }

    fn materialise_root(
        &self,
        root: &Path,
        at: Option<String>,
        caller: &str,
        payload: &mut Payload,
        closure: &mut super::closure::Plan,
    ) -> Result<()> {
        // NOT canonicalized, and that is deliberate. Under an isolated install
        // this is the SYMLINK path — `node_modules/sharp` — which is where the
        // bundle's own `require("sharp")` looks. Resolving it to the real
        // `.store/sharp@<v>/node_modules/sharp` makes the package land there
        // instead, and the artifact then cannot find it at all. Measured:
        // canonicalizing here shipped sharp into `.store` and the native-islands
        // gate failed with `Cannot find module 'sharp'`.
        //
        // So the root keeps the path Node resolves it by, while dependencies are
        // canonicalized during the walk to find what they really are. The two
        // forms differing is what `placement_for` is for.
        match install_tree_root(root) {
            Some(anchor) => self.materialise_unbundled(root, &anchor, at, caller, payload, closure),
            None => Ok(()),
        }
    }

    /// Note what a closure shipped, for [`Self::transport_roots`].
    fn record_shipped(&self, members: &[Member]) {
        if let Ok(mut names) = self.shipped_names.lock() {
            names.extend(members.iter().map(|member| member.name.clone()));
        }
        if let Ok(mut verbatim) = self.verbatim_members.lock() {
            verbatim.extend(
                members
                    .iter()
                    // pino's own source names targets only in its examples, and
                    // `pino/file` is pino resolving itself.
                    .filter(|member| member.verbatim && member.name != "pino")
                    .map(|member| (member.rel.clone(), member.dir.clone())),
            );
        }
    }

    fn is_external(&self, specifier: &str) -> bool {
        self.external
            .iter()
            .any(|package| super::bundle::is_package_specifier(specifier, package))
    }

    /// `--unbundled` names nothing in the build reached, resolved from the entry
    /// as a top-level import of them would be.
    fn unreached_forced_roots(&self, entry: &Path) -> Vec<(PathBuf, String)> {
        let mut found = Vec::new();
        if let Ok(mut seen) = self.forced_seen.lock() {
            for name in &self.forced_unbundled {
                if seen.contains(name) {
                    continue;
                }
                if self.is_external(name) {
                    seen.insert(name.clone());
                    continue;
                }
                if let Some(root) = resolve_package_root(entry, name) {
                    seen.insert(name.clone());
                    let reason = crate::compile::unbundlable::Reason::Forced.describe();
                    found.push((root, format!("{name} — {reason}")));
                }
            }
        }
        found
    }

    /// The pino transport targets to ship, each with the payload path it has to
    /// occupy — empty unless pino itself ships.
    ///
    /// pino resolves a target with `createRequire(caller).resolve(target)`, from
    /// wherever the calling code runs IN THE ARTIFACT. Bundled code runs from the
    /// app root, so a target it names has to sit at the top-level
    /// `node_modules/<name>` whatever nested install it resolved to in the source
    /// tree. A verbatim package keeps its layout, so a target it names is placed
    /// where its own lookup finds it. Bundled callers that resolved different
    /// installs of one name cannot all be served from the root, and shipping one
    /// of them would hand the others the wrong package, so that is refused.
    fn transport_roots(&self) -> Result<Vec<(PathBuf, String, String, String)>> {
        if !self
            .shipped_names
            .lock()
            .is_ok_and(|names| names.contains("pino"))
        {
            return Ok(Vec::new());
        }
        // Placement -> (real directory, the path it was found at, who named it).
        let mut wanted: BTreeMap<String, (PathBuf, PathBuf, String)> = BTreeMap::new();
        let mut want = |at: String, root: PathBuf, caller: String| -> Result<()> {
            let real = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            if let Some((other, _, first)) = wanted.get(&at) {
                if !same_instance(other, &real) {
                    bail!(
                        "a pino transport resolves to two different installs that would both sit at {at}:\n\
                         \x20 {} (named in {first})\n\
                         \x20 {} (named in {caller})\n\
                         \x20 pino resolves a target from where its caller runs in the compiled binary,\n\
                         \x20 which for bundled code is the root, so one caller would load the wrong\n\
                         \x20 package. Install a single version of it.",
                        other.display(),
                        real.display()
                    );
                }
                return Ok(());
            }
            wanted.insert(at, (real, root, caller));
            Ok(())
        };
        let bundled = self
            .transport_targets
            .lock()
            .map(|targets| targets.clone())
            .unwrap_or_default();
        for (importer, target) in bundled {
            let name = super::closure::package_of(&target);
            if self.forced_bundled.iter().any(|n| n == name) || self.is_external(name) {
                continue;
            }
            if let Some(root) = resolve_package_root(Path::new(&importer), name) {
                want(format!("node_modules/{name}"), root, importer)?;
            }
        }
        let verbatim = self
            .verbatim_members
            .lock()
            .map(|members| members.clone())
            .unwrap_or_default();
        for (rel, dir) in verbatim {
            let real = std::fs::canonicalize(&dir).unwrap_or(dir);
            for file in super::unbundlable::loadable_code(&real).unwrap_or_default() {
                let Ok(text) = std::fs::read_to_string(real.join(&file)) else {
                    continue;
                };
                if !text.contains("transport") {
                    continue;
                }
                for target in transport_targets(&text) {
                    let name = super::closure::package_of(&target);
                    if self.is_external(name) {
                        continue;
                    }
                    let Some(root) = resolve_package_root(&real.join(&file), name) else {
                        continue;
                    };
                    if std::fs::canonicalize(&root).is_ok_and(|target| target == real) {
                        continue;
                    }
                    let real_rel = install_tree_root(&root)
                        .and_then(|anchor| root.strip_prefix(anchor).ok().map(payload_path));
                    let at = placement_for(&rel, real_rel.as_deref(), name);
                    want(at, root, format!("{rel}/{file}"))?;
                }
            }
        }
        let mut found = Vec::new();
        for (at, (_, root, caller)) in wanted {
            let name = at
                .rsplit_once("node_modules/")
                .map_or(&*at, |(_, name)| name);
            let summary = format!("{name} — it is named as a pino transport target");
            let caller = format!("a pino transport target in {caller}");
            found.push((root, at, summary, caller));
        }
        Ok(found)
    }

    /// Every eject closure in the build, and the second bundle that serves them.
    fn plan_closures(
        &self,
        selected: &Selected,
        entry: &Path,
        external: Option<&SharedPluginable>,
        minify: bool,
        keep_names: bool,
    ) -> Result<(Payload, super::closure::Bundled)> {
        if let Ok(mut unbundled) = self.unbundled.lock() {
            unbundled.clone_from(&selected.roots);
        }
        if let Ok(mut seen) = self.forced_seen.lock() {
            seen.clone_from(&selected.reached);
        }
        if let Ok(mut names) = self.shipped_names.lock() {
            names.clear();
        }
        if let Ok(mut verbatim) = self.verbatim_members.lock() {
            verbatim.clear();
        }
        let mut payload = Payload::default();
        // ONE plan across every root. Two closures can overlap, and a package one
        // of them ships verbatim has to be external to the other's chunks — a
        // second copy inside a chunk would be a different module object reading a
        // different `__dirname`.
        let mut closure = super::closure::Plan::default();
        for root in selected.roots.keys() {
            self.materialise_root(
                root,
                None,
                "an import in the bundle",
                &mut payload,
                &mut closure,
            )?;
        }
        // Only after every closure has been walked: a `--unbundled` name is marked
        // reached when it turns up INSIDE a closure, so not until then does
        // "unreached" mean that nothing in the build names it.
        let mut shipped: BTreeSet<PathBuf> = selected
            .roots
            .keys()
            .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
            .collect();
        for (root, summary) in self.unreached_forced_roots(entry) {
            if !shipped.insert(std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone())) {
                continue;
            }
            if let Ok(mut unbundled) = self.unbundled.lock() {
                unbundled.insert(root.clone(), summary);
            }
            self.materialise_root(&root, None, "--unbundled", &mut payload, &mut closure)?;
        }
        // Last, so that whether pino ships counts every closure above.
        for (root, at, summary, caller) in self.transport_roots()? {
            if let Ok(mut unbundled) = self.unbundled.lock() {
                unbundled.insert(root.clone(), summary);
            }
            self.materialise_root(&root, Some(at), &caller, &mut payload, &mut closure)?;
        }
        let bundled = super::closure::bundle(&closure, external, minify, keep_names)?;
        Ok((payload, bundled))
    }

    pub fn plan_survivors(
        &self,
        chunks: &mut [super::bundle::BundledFile],
        entry: &Path,
        external: Option<&SharedPluginable>,
        minify: bool,
        keep_names: bool,
    ) -> Result<PlannedNative> {
        let seeds = self
            .seeds
            .lock()
            .map_err(|_| anyhow::anyhow!("the native-addon seed collector was poisoned"))?
            .clone();
        let mut summaries = BTreeSet::new();
        let selected = Selected {
            roots: self
                .unbundled
                .lock()
                .map(|roots| roots.clone())
                .unwrap_or_default(),
            reached: self
                .forced_seen
                .lock()
                .map(|seen| seen.clone())
                .unwrap_or_default(),
        };
        // Again from the top each time a bundled package turns out to need shipping as
        // files, since that moves whatever it depends on. Each round adds a package,
        // and a closure with none left to add has no chunks to get wrong.
        let (mut payload, closure_files) = loop {
            let (payload, bundled) =
                self.plan_closures(&selected, entry, external, minify, keep_names)?;
            let stranded = payload.stranded_by(&bundled.imports);
            let mut known = self
                .stranded
                .lock()
                .map_err(|_| anyhow::anyhow!("the stranded package set was poisoned"))?;
            let before = known.len();
            known.extend(stranded.keys().cloned());
            if known.len() == before {
                if let Some(why) = stranded.values().next() {
                    bail!(
                        "{why}.\n\
                         \x20 Pass --unbundled for the package that imports it so it ships as files."
                    );
                }
                payload.reject_shadowed_imports()?;
                break (payload, bundled.files);
            }
        };

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
                match payload.files.entry(file.name) {
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
        let placed = payload
            .holders
            .iter()
            .map(|(at, holder)| (at.clone(), holder.instance.clone()))
            .collect();
        Ok(PlannedNative {
            placed,
            files: payload
                .files
                .into_iter()
                .map(|(name, (bytes, executable))| IslandFile {
                    name,
                    bytes,
                    executable,
                })
                .chain(closure_files)
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
        // By package rather than by specifier: its files no longer go through the
        // bundle, so a subpath of it has nothing to be inlined from. `--bundled` still
        // wins, and leaves two installs to claim one directory.
        let package = super::closure::package_of(specifier);
        if !self.own_installs.is_empty() && !self.forced_bundled.iter().any(|n| n == package) {
            let moved = resolve_package_root(Path::new(importer), package).and_then(|root| {
                let real = std::fs::canonicalize(&root).ok()?;
                let dependency = self.own_installs.get(&real)?;
                Some((root, dependency))
            });
            if let Some((root, dependency)) = moved {
                let reason = crate::compile::unbundlable::Reason::OwnInstall(dependency.clone());
                self.record_external(package, importer, root, &reason);
                return true;
            }
        }
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
            if let Ok(mut seen) = self.forced_seen.lock() {
                seen.insert(specifier.to_string());
            }
            crate::compile::unbundlable::Reason::Forced
        } else {
            match crate::compile::unbundlable::classify(&root, &manifest) {
                Some(reason) => reason,
                None => return false,
            }
        };
        self.record_external(specifier, importer, root, &reason);
        true
    }

    fn record_external(
        &self,
        package: &str,
        importer: &str,
        root: PathBuf,
        reason: &crate::compile::unbundlable::Reason,
    ) {
        if let Ok(mut edges) = self.external_edges.lock() {
            edges.insert((package.to_string(), importer.to_string(), root.clone()));
        }
        if let Ok(mut roots) = self.unbundled.lock() {
            roots.insert(root, format!("{package} — {}", reason.describe()));
        }
    }

    /// The packages to take out of the bundle, because their code cannot load the
    /// install it resolves from where bundled code runs.
    ///
    /// An external stays a bare specifier, and every module in the bundle looks it up
    /// from the app root, where one directory answers for each name. That belongs to
    /// the install the entry itself resolves, or to the first one named when the
    /// entry resolves none. A module that wants any other install used to load that
    /// one with no error, or fail on a missing module if nothing held the name.
    ///
    /// Shipping the module's package as files serves it exactly: the code runs from
    /// its own directory, so its lookups, `require.resolve` and any self-reference
    /// behave as they do uncompiled. A rewritten specifier would have kept the package
    /// bundled, and answered only the imports the bundler can see.
    ///
    /// A package that is not an installed one, such as a workspace member shipping
    /// source, cannot move. It stays, and both installs then claim the one directory,
    /// which fails the build naming them.
    pub fn out_of_reach(&self, entry: &Path) -> BTreeMap<PathBuf, String> {
        let edges = self
            .external_edges
            .lock()
            .map(|edges| edges.clone())
            .unwrap_or_default();
        let mut holders: BTreeMap<&str, PathBuf> = BTreeMap::new();
        let mut moved = BTreeMap::new();
        for (package, importer, root) in &edges {
            let holder = holders
                .entry(package)
                .or_insert_with(|| resolve_package_root(entry, package).unwrap_or(root.clone()));
            if same_instance(holder, root) {
                continue;
            }
            let Some(owner) = owning_package(Path::new(importer)) else {
                continue;
            };
            let Ok(real) = std::fs::canonicalize(&owner) else {
                continue;
            };
            if real
                .components()
                .any(|part| part.as_os_str() == "node_modules")
                && !self.own_installs.contains_key(&real)
            {
                moved.entry(real).or_insert_with(|| package.clone());
            }
        }
        moved
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
        //
        // Recorded as REACHED, or `unreached_forced_unbundled` would report a name
        // that did its job: a closure member is never resolved by the bundler, so
        // it can only ever be seen here.
        if self.forced_unbundled.iter().any(|n| n == name) {
            if let Ok(mut seen) = self.forced_seen.lock() {
                seen.insert(name.to_string());
            }
            return true;
        }
        if self.forced_bundled.iter().any(|n| n == name) {
            return false;
        }
        if self.stranded.lock().is_ok_and(|stranded| {
            std::fs::canonicalize(dir).is_ok_and(|real| stranded.contains(&real))
        }) {
            return true;
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

    /// `--unbundled` names that shipped nothing: nothing in the build reached
    /// them, and the entry cannot resolve them either.
    ///
    /// Silently doing nothing there produces a binary that fails on the user's
    /// machine with the build reporting success.
    pub fn unreached_forced_unbundled(&self) -> Vec<String> {
        let seen = match self.forced_seen.lock() {
            Ok(seen) => seen,
            Err(_) => return Vec::new(),
        };
        self.forced_unbundled
            .iter()
            .filter(|name| !seen.contains(*name))
            .cloned()
            .collect()
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
    /// Each package directory in the payload, and the installed one it came from.
    pub placed: Vec<(String, PathBuf)>,
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
    /// `dir` with its symlinks resolved: which installed package this is.
    real: PathBuf,
    rel: String,
    name: String,
    version: Option<String>,
    /// Ships as files. Otherwise it is bundled into the chunk of whichever
    /// specifier reached it, and its own files never enter the payload.
    verbatim: bool,
    /// What reached it — the dependent's placement, or what asked for the root —
    /// for a collision to name.
    caller: String,
}

impl Member {
    fn holder(&self, stub: Option<BTreeMap<String, BTreeMap<&'static str, String>>>) -> Holder {
        Holder {
            name: self.name.clone(),
            version: self.version.clone(),
            instance: self.real.clone(),
            caller: self.caller.clone(),
            stub,
        }
    }
}

/// What the bundle itself left to planning. Every pass starts from exactly this:
/// the roots nothing imports, the transports and the `--unbundled` names a closure
/// reached are all derived, and a pass that moved a package can derive them
/// differently, so one pass's answers must not carry into the next.
struct Selected {
    roots: BTreeMap<PathBuf, String>,
    reached: BTreeSet<String>,
}

/// The package a module belongs to: the nearest directory whose manifest has a name,
/// since a `dist/esm/package.json` holding only `type` is not one.
fn owning_package(module: &Path) -> Option<PathBuf> {
    module
        .ancestors()
        .skip(1)
        .find(|dir| {
            std::fs::read_to_string(dir.join("package.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .is_some_and(|manifest| manifest["name"].is_string())
        })
        .map(Path::to_path_buf)
}

/// One closure's members and where each lands, before any of it is written.
struct Walk {
    members: Vec<Member>,
    /// A different package reaching a placement already taken in this walk.
    shadowed: Vec<(String, PathBuf, String)>,
    saw_addon: bool,
    unloadable: Option<PathBuf>,
}

/// What occupies one package directory of the payload, and what put it there.
struct Holder {
    name: String,
    version: Option<String>,
    /// The installed directory it came from. See [`same_instance`].
    instance: PathBuf,
    caller: String,
    /// A closure stub's `exports` map; `None` for the package's own files.
    stub: Option<BTreeMap<String, BTreeMap<&'static str, String>>>,
}

/// Whether two installed directories are one package to anything that loads them:
/// the same files, and each dependency and peer resolving to the same thing from
/// both, all the way down.
///
/// A real directory alone is too strict: npm hoists byte-identical copies of `x@1`
/// side by side, and refusing those would fail ordinary trees. A version alone is too
/// loose, twice over: a peer-suffixed store holds `foo@1` with `peer@1` beside `foo@1`
/// with `peer@2`, and two local or tarball installs can share a manifest and no code.
///
/// Files are read only when two different directories land on one path, so a build
/// without such a pair reads none.
fn same_instance(a: &Path, b: &Path) -> bool {
    fn compare(a: &Path, b: &Path, seen: &mut BTreeSet<(PathBuf, PathBuf)>) -> bool {
        let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
        let (a, b) = (real(a), real(b));
        // A pair already being compared is a dependency cycle, and answers true so
        // that the rest of the walk decides.
        if a == b || !seen.insert((a.clone(), b.clone())) {
            return true;
        }
        let manifest = |dir: &Path| {
            let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
            serde_json::from_str::<serde_json::Value>(&text).ok()
        };
        let (Some(left), Some(right)) = (manifest(&a), manifest(&b)) else {
            return false;
        };
        if left != right || !same_files(&a, &b) {
            return false;
        }
        let mut names = BTreeSet::new();
        for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
            if let Some(deps) = left.get(field).and_then(|deps| deps.as_object()) {
                names.extend(deps.keys().cloned());
            }
        }
        names.iter().all(|name| {
            let from = |dir: &Path| resolve_package_root(&dir.join("x"), name);
            match (from(&a), from(&b)) {
                (None, None) => true,
                (Some(left), Some(right)) => compare(&left, &right, seen),
                _ => false,
            }
        })
    }
    compare(a, b, &mut BTreeSet::new())
}

/// Whether two package directories hold the same files, by name and bytes.
fn same_files(a: &Path, b: &Path) -> bool {
    let inner = |dir: &Path| -> BTreeSet<PathBuf> {
        package_files(dir)
            .iter()
            .filter_map(|path| path.strip_prefix(dir).ok().map(Path::to_path_buf))
            .collect()
    };
    let files = inner(a);
    files == inner(b)
        && files.iter().all(|file| {
            matches!(
                (std::fs::read(a.join(file)), std::fs::read(b.join(file))),
                (Ok(left), Ok(right)) if left == right
            )
        })
}

/// The payload as it is assembled, and every package directory placed in it.
///
/// Placement used to be checked up front while the writer kept whatever reached a
/// path first, so a second package landing on a taken directory shipped none of
/// its own files and its callers loaded the first one's code, silently. Each
/// package now claims its directory as it is written: a different package there
/// fails the build, the same package's files are reused, and its files replace a
/// closure stub of it. Never the other way round — a stub answers only the
/// subpaths the scan saw, where the package's own files answer every one.
#[derive(Default)]
struct Payload {
    files: BTreeMap<String, (Vec<u8>, bool)>,
    holders: BTreeMap<String, Holder>,
    /// Each package a verbatim one names: where it lands, the name, and the install
    /// that answers uncompiled. See [`Self::reject_shadowed_imports`].
    named: Vec<(String, String, PathBuf)>,
}

enum Claim {
    Copy,
    Reuse,
    ReplacedStub,
}

impl Payload {
    fn claim_full(&mut self, at: &str, holder: Holder) -> Result<Claim> {
        let Some(held) = self.holders.get_mut(at) else {
            self.holders.insert(at.to_string(), holder);
            return Ok(Claim::Copy);
        };
        if !same_instance(&held.instance, &holder.instance) {
            return Err(collision(at, held, &holder));
        }
        if held.stub.is_none() {
            return Ok(Claim::Reuse);
        }
        *held = holder;
        self.files.remove(&format!("{at}/package.json"));
        Ok(Claim::ReplacedStub)
    }

    /// Place a closure stub, returning the `exports` entries it now answers with
    /// a chunk of its own. A second closure stubbing the same package adds the
    /// specifiers the first did not name; on the package's own files it adds
    /// nothing, since those already answer every one.
    fn claim_stub(
        &mut self,
        at: &str,
        mut holder: Holder,
    ) -> Result<BTreeSet<(String, &'static str)>> {
        let exports = holder.stub.take().unwrap_or_default();
        if let Some(held) = self.holders.get(at) {
            if !same_instance(&held.instance, &holder.instance) {
                return Err(collision(at, held, &holder));
            }
            if held.stub.is_none() {
                return Ok(BTreeSet::new());
            }
        }
        let held = self.holders.entry(at.to_string()).or_insert(holder);
        let existing = held.stub.get_or_insert_with(BTreeMap::new);
        let mut added = BTreeSet::new();
        for (key, conditions) in exports {
            let answered = existing.entry(key.clone()).or_default();
            for (condition, target) in conditions {
                if !answered.contains_key(condition) {
                    answered.insert(condition, target);
                    added.insert((key.clone(), condition));
                }
            }
        }
        let manifest = super::closure::stub_manifest(&held.name, held.version.as_deref(), existing);
        self.files
            .insert(format!("{at}/package.json"), (manifest, false));
        Ok(added)
    }

    /// What Node finds for `package`, walking up from `from` in the artifact.
    fn lookup(&self, from: &str, package: &str) -> Option<(String, &Holder)> {
        let mut dir = from;
        loop {
            let at = if dir.is_empty() {
                format!("node_modules/{package}")
            } else {
                format!("{dir}/node_modules/{package}")
            };
            if let Some(held) = self.holders.get(&at) {
                return Some((at, held));
            }
            if dir.is_empty() {
                return None;
            }
            dir = dir.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    /// The packages whose bundled code landed where it cannot reach an install it
    /// imports, so that those packages can ship as files, where their own lookups
    /// work: a chunk runs from its stub or the app root, not from where the module sat.
    fn stranded_by(&self, imports: &[super::closure::VerbatimImport]) -> BTreeMap<PathBuf, String> {
        let mut stranded = BTreeMap::new();
        for import in imports {
            let from = import.chunk.rsplit_once('/').map_or("", |(dir, _)| dir);
            let reached = self
                .lookup(from, &import.package)
                .is_some_and(|(_, held)| same_instance(&held.instance, &import.instance));
            if reached {
                continue;
            }
            if let Some(owner) = owning_package(&import.module) {
                let why = format!(
                    "{} imports {} from {}, and {} cannot reach that",
                    import.module.display(),
                    import.package,
                    import.instance.display(),
                    import.chunk
                );
                stranded.insert(owner, why);
            }
        }
        stranded
    }

    /// Fail when a verbatim package would load a different install than it does
    /// uncompiled: the same directory can be on two packages' lookup paths, and each
    /// was placed without knowing what the other would find there.
    fn reject_shadowed_imports(&self) -> Result<()> {
        for (from, package, wanted) in &self.named {
            let Some((at, held)) = self.lookup(from, package) else {
                continue;
            };
            if !same_instance(&held.instance, wanted) {
                bail!(
                    "{from} resolves {package} to {}, but in the compiled binary it would load \
                     {} from {at}.\n\
                     \x20 Install a single version of it, or pass --unbundled for the package that\n\
                     \x20 depends on the other copy so that each keeps its own.",
                    wanted.display(),
                    held.instance.display()
                );
            }
        }
        Ok(())
    }
}

fn collision(at: &str, held: &Holder, other: &Holder) -> anyhow::Error {
    let describe = |holder: &Holder| {
        let name = match &holder.version {
            Some(version) => format!("{}@{version}", holder.name),
            None => holder.name.clone(),
        };
        format!(
            "{name} from {}, reached from {}",
            holder.instance.display(),
            holder.caller
        )
    };
    anyhow::anyhow!(
        "two different packages would occupy {at} in the compiled binary:\n\
         \x20 {}\n\
         \x20 {}\n\
         \x20 Only one can ship there, and the other's callers would load it instead.\n\
         \x20 Install a single copy of it.",
        describe(held),
        describe(other)
    )
}

/// Turn one walked closure into a bundling plan, writing the stub manifests that
/// let Node resolve what the plan replaces.
///
/// Returns false, having written nothing, on either refusal — a package whose
/// specifiers cannot be enumerated, or one named through a subpath no `.js` file
/// can answer. The whole closure then ships as files. Partial is not an option: a
/// stub set missing one entry produces a binary that builds clean and dies on the
/// machine it ships to, which is the one failure this pipeline exists to avoid.
fn plan_closure(
    members: &[Member],
    closure: &mut super::closure::Plan,
    payload: &mut Payload,
) -> Result<bool> {
    let mut named: Vec<(usize, String, super::closure::Uses)> = Vec::new();
    let mut refuse = false;
    for (index, member) in members.iter().enumerate().filter(|(_, m)| m.verbatim) {
        match super::closure::boundary_specifiers(&member.dir) {
            Some(found) => named.extend(found.into_iter().map(|(s, u)| (index, s, u))),
            None => refuse = true,
        }
    }

    // Each entry with the stub and `exports` key it answers.
    let mut entries: Vec<(String, String, &'static str, super::closure::Entry)> = Vec::new();
    let mut stubs: BTreeMap<String, Holder> = BTreeMap::new();
    let mut imports: Vec<(String, String, PathBuf)> = Vec::new();
    for (importer, specifier, uses) in named {
        let package = super::closure::package_of(&specifier);
        let from = &members[importer];
        // The install this importer resolves uncompiled, placed where it can reach it:
        // an isolated install puts one package under several dependents, and the name
        // alone would answer with whichever of them the walk met first. A bundled install
        // with no such placement is a refusal rather than a skip — the specifier is real
        // and nothing would answer it. A specifier that resolves to nothing uncompiled (a
        // builtin, a devDependency named in a test file) is left alone, as is a package
        // outside the closure.
        let Some(wanted) = resolve_package_root(&from.real.join("x"), package)
            .and_then(|root| std::fs::canonicalize(root).ok())
        else {
            continue;
        };
        let Some(member) = members
            .iter()
            .find(|member| member.real == wanted && reachable_from(&from.rel, &member.rel))
        else {
            refuse |= members
                .iter()
                .any(|member| !member.verbatim && member.real == wanted);
            continue;
        };
        imports.push((from.rel.clone(), package.to_string(), member.real.clone()));
        if member.verbatim {
            continue;
        }
        if !super::closure::answerable(&specifier, uses) {
            refuse = true;
            break;
        }
        let stub = stubs.entry(member.rel.clone()).or_insert_with(|| Holder {
            caller: from.rel.clone(),
            ..member.holder(Some(BTreeMap::new()))
        });
        for esm in [false, true] {
            if !(if esm { uses.imported } else { uses.required }) {
                continue;
            }
            let number = closure.next_chunk;
            let entry = super::closure::Entry {
                // Names no real file. The bundler needs only its DIRECTORY, so the
                // specifier resolves from the package that wrote it — from its REAL
                // directory, as Node resolves from a module's realpath. The root's
                // `dir` is the `node_modules/<name>` symlink under an isolated
                // install, and none of its `.store/` siblings is reachable from
                // there: every jsdom entry resolved to nothing.
                importer: super::bundle::canonicalize_for_bundler(&from.dir)
                    .join(format!("__nub_closure_{number}.js")),
                chunk: super::closure::chunk_path(&member.rel, number, esm),
                specifier: specifier.clone(),
                esm,
            };
            let (key, condition, target) = super::closure::export_entry(&member.rel, &entry);
            // Two ejected packages naming the same specifier share one chunk. The
            // map is written only alongside the push that BUILDS that chunk —
            // overwriting it on the second sighting left `restructure`'s manifest
            // pointing at a chunk number nothing had emitted, and the artifact died
            // on `Cannot find module`.
            let exports = stub.stub.get_or_insert_with(BTreeMap::new);
            let targets = exports.entry(key.clone()).or_default();
            if !targets.contains_key(condition) {
                targets.insert(condition, target);
                entries.push((member.rel.clone(), key, condition, entry));
                closure.next_chunk += 1;
            }
        }
    }

    // Nothing is written until both refusals have had their say, so giving up
    // leaves the payload exactly as it was before closure bundling existed.
    if refuse {
        return Ok(false);
    }
    let mut answered = BTreeSet::new();
    for (at, stub) in stubs {
        for (key, condition) in payload.claim_stub(&at, stub)? {
            answered.insert((at.clone(), key, condition));
        }
    }
    closure.entries.extend(
        entries
            .into_iter()
            .filter(|(at, key, condition, _)| {
                answered.contains(&(at.clone(), key.clone(), *condition))
            })
            .map(|(.., entry)| entry),
    );
    closure.verbatim.extend(
        members
            .iter()
            .filter(|member| member.verbatim)
            .map(|member| member.real.clone()),
    );
    payload.named.extend(imports);
    Ok(true)
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
        super::warn(
            &format!(
                "{} optionally depends on {}, which is not installed",
                edge.owner, edge.name
            ),
            &[
                "Nothing from it is embedded. If this addon loads a companion shared",
                "library from that package, the compiled binary will fail at run time:",
                &addon.display().to_string(),
            ],
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
        let code_module = matches!(
            args.module_type,
            ModuleType::Js | ModuleType::Jsx | ModuleType::Ts | ModuleType::Tsx
        );
        if code_module && args.code.contains("transport") {
            let named = transport_targets(args.code);
            if !named.is_empty() {
                if let Ok(mut seen) = self.transport_targets.lock() {
                    let importer = clean_url(args.id).to_string();
                    seen.extend(named.into_iter().map(|target| (importer.clone(), target)));
                }
            }
        }
        let scannable = code_module && args.code.contains("createRequire");
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

/// The bare specifiers a module passes as a `target` property's string literal.
///
/// pino names a transport by string — `transport: { target: "pino-pretty" }`,
/// `pino.transport({ targets: [{ target: … }] })`, the same objects inside a
/// `pipeline` — and resolves it inside a worker, so no import ever reaches the
/// package and the binary ships without it. Each of those shapes is a `target`
/// property, so that is the whole pattern; the caller only looks in modules that
/// say `transport` at all. It over-reports in the safe direction: a stray
/// `target: "_blank"` names no installed package and is dropped when it fails to
/// resolve. A path target is pino's to resolve and stays out.
fn transport_targets(code: &str) -> Vec<String> {
    use super::closure::{is_word_byte, literal, skip_spaces};
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = code[from..].find("target") {
        let start = from + offset;
        from = start + "target".len();
        if start > 0 && is_word_byte(bytes[start - 1]) {
            continue;
        }
        // `"target": …` as well as `target: …`, but never `targets`.
        let mut i = from + usize::from(matches!(bytes.get(from), Some(b'"' | b'\'')));
        if bytes.get(i).is_some_and(|b| is_word_byte(*b)) {
            continue;
        }
        i = skip_spaces(bytes, i);
        if bytes.get(i) != Some(&b':') {
            continue;
        }
        let Some((value, _)) = literal(code, skip_spaces(bytes, i + 1)) else {
            continue;
        };
        if !value.is_empty() && !value.starts_with(['.', '/', '\\', '#']) && !value.contains(':') {
            found.push(value.to_string());
        }
    }
    found
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
    // Names only Nub's own `--os` / `--cpu` / `--libc` install flags, which work
    // in every project. The persistent `supportedArchitectures` setting is not
    // offered: the engine reads it only from an incumbent pnpm or yarn's config,
    // so for a nub, npm or bun project it is advice that cannot be followed.
    let advice = "\x20\x20A native addon is machine code for one platform, and a compiled binary \
                  loads it\n\x20\x20from a real file at run time — there is no later step that \
                  could translate it. The\n\x20\x20install has to put the target's own platform \
                  package on disk before you compile.\n\
                  \n\x20\x20Select it with nub install --os <os> --cpu <cpu> --libc <libc>, then \
                  compile again.\n\x20\x20If no prebuilt exists for the target, install on the \
                  target platform itself — a\n\x20\x20container is the usual way — or drop \
                  --platform to build for this machine.";
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
    use super::flattened_root;
    use std::path::Path;

    /// A root below the top level is out of reach of the bundle that imports it, whether
    /// a store symlink or a nested hoisted install put it there. Preserving its path
    /// ships the package where nothing can find it.
    #[test]
    fn a_nested_root_is_flattened_to_where_a_bare_specifier_looks() {
        for (store, expected) in [
            (
                "node_modules/.bun/opencode-gitlab-auth@2.1.0/node_modules/fastify",
                "node_modules/fastify",
            ),
            (
                "node_modules/.pnpm/pino@9.5.0/node_modules/pino",
                "node_modules/pino",
            ),
            (
                "node_modules/.store/owner@1.0.0/node_modules/@scope/name",
                "node_modules/@scope/name",
            ),
            (
                "node_modules/holder/node_modules/better-sqlite3",
                "node_modules/better-sqlite3",
            ),
        ] {
            assert_eq!(
                flattened_root(Path::new(store)).as_deref(),
                Some(expected),
                "{store} must land where the bundle's own resolution looks"
            );
        }
    }

    /// A hoisted or workspace-level package is ALREADY at the path Node reaches
    /// it by. Moving it would break the placement the walk depends on.
    #[test]
    fn a_hoisted_root_keeps_the_path_node_reaches_it_by() {
        for kept in [
            "node_modules/clipboardy",
            "node_modules/@opentui/core",
            "packages/app/node_modules/web-tree-sitter",
        ] {
            assert_eq!(
                flattened_root(Path::new(kept)),
                None,
                "{kept} is already reachable and must not move"
            );
        }
    }

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

    /// Every shape pino accepts a transport in is a `target` property, and only a
    /// literal naming a package is shipped — a path is pino's to resolve, and a
    /// value the scan cannot read as a constant is not guessed at.
    #[test]
    fn a_transport_target_is_read_from_a_target_propertys_string_literal() {
        let code = r#"
            pino({ transport: { target: "pino-pretty", options: {} } });
            pino.transport({ targets: [{ target: '@scope/pino-x' }, { "target": "pino-roll" }] });
            pino.transport({ pipeline: [{ target: "./local.mjs" }, { target: "/abs/t.js" }] });
            const not = { target: name, targets: "x", retarget: "y", t: { target: "file:///t.js" } };
        "#;
        assert_eq!(
            transport_targets(code),
            ["pino-pretty", "@scope/pino-x", "pino-roll"]
        );
    }

    fn install(dir: &Path, version: &str, deps: &[&str]) -> PathBuf {
        let name = dir.file_name().unwrap().to_str().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let deps: Vec<String> = deps.iter().map(|dep| format!(r#""{dep}":"*""#)).collect();
        std::fs::write(
            dir.join("package.json"),
            format!(
                r#"{{"name":"{name}","version":"{version}","dependencies":{{{}}}}}"#,
                deps.join(",")
            ),
        )
        .unwrap();
        dir.to_path_buf()
    }

    /// The rules for two packages meeting at one payload directory. A stub answers
    /// only the subpaths the scan saw, so it never stands in for the package's own
    /// files, while they stand in for it; anything else is a different package and
    /// one of its callers would load the wrong code.
    #[test]
    fn a_package_directory_holds_one_package_and_its_own_files_beat_a_stub() {
        let tree = tempfile::tempdir().unwrap();
        let one = install(&tree.path().join("one/node_modules/x"), "1", &[]);
        let two = install(&tree.path().join("two/node_modules/x"), "2", &[]);
        let at = "node_modules/x";
        let holder = |instance: &Path, version: &str, caller: &str, stub| Holder {
            name: "x".to_string(),
            version: Some(version.to_string()),
            instance: instance.to_path_buf(),
            caller: caller.to_string(),
            stub,
        };
        let stub = |key: &str, caller: &str| {
            let exports = BTreeMap::from([(
                key.to_string(),
                BTreeMap::from([("require", format!("./__nub/{}.js", key.len()))]),
            )]);
            holder(&one, "1", caller, Some(exports))
        };
        let keys = |added: BTreeSet<(String, &'static str)>| -> Vec<String> {
            added.into_iter().map(|(key, _)| key).collect()
        };

        let mut payload = Payload::default();
        assert_eq!(
            keys(payload.claim_stub(at, stub("./a", "one")).unwrap()),
            ["./a"]
        );
        // A second closure's stub adds what the first did not name.
        assert_eq!(
            keys(payload.claim_stub(at, stub("./bb", "two")).unwrap()),
            ["./bb"]
        );
        assert!(
            keys(payload.claim_stub(at, stub("./a", "two")).unwrap()).is_empty(),
            "a specifier already answered needs no second chunk"
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&payload.files[&format!("{at}/package.json")].0).unwrap();
        assert!(manifest["exports"]["./a"].is_object() && manifest["exports"]["./bb"].is_object());

        // The package's own files replace the stub, and its manifest with it.
        assert!(matches!(
            payload
                .claim_full(at, holder(&one, "1", "three", None))
                .unwrap(),
            Claim::ReplacedStub
        ));
        assert!(!payload.files.contains_key(&format!("{at}/package.json")));
        // From then on the same package is reused, and a stub adds nothing.
        assert!(matches!(
            payload
                .claim_full(at, holder(&one, "1", "four", None))
                .unwrap(),
            Claim::Reuse
        ));
        assert!(keys(payload.claim_stub(at, stub("./c", "five")).unwrap()).is_empty());

        // Anything else fails, in either order, and names both sides.
        let text = match payload.claim_full(at, holder(&two, "2", "six", None)) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a different version must not share a directory"),
        };
        assert!(
            ["x@1", "x@2", "three", "six", at]
                .iter()
                .all(|part| text.contains(part)),
            "{text}"
        );
        let mut stubbed = Payload::default();
        stubbed.claim_stub(at, stub("./a", "seven")).unwrap();
        assert!(
            stubbed
                .claim_stub(at, holder(&two, "2", "eight", Some(BTreeMap::new())))
                .is_err()
        );

        // One version with other code is another package, against files and stubs alike.
        let other = install(&tree.path().join("three/node_modules/x"), "1", &[]);
        std::fs::write(other.join("index.js"), "other code\n").unwrap();
        assert!(
            payload
                .claim_full(at, holder(&other, "1", "nine", None))
                .is_err()
        );
        assert!(
            stubbed
                .claim_stub(at, holder(&other, "1", "ten", Some(BTreeMap::new())))
                .is_err()
        );
    }

    /// One version in a peer-suffixed store is several installed packages, a local
    /// install can reuse a published manifest, and npm's byte-identical hoisted copies
    /// are one package. Only the files and the dependencies can tell them apart.
    #[test]
    fn an_installed_package_is_its_files_and_what_its_dependencies_resolve_to() {
        let tree = tempfile::tempdir().unwrap();
        let foo = |project: &str, peer: Option<&str>| {
            let modules = tree.path().join(project).join("node_modules");
            if let Some(version) = peer {
                install(&modules.join("peer"), version, &[]);
            }
            install(&modules.join("foo"), "1", &["peer"])
        };
        let peer_one = foo("a", Some("1"));
        assert!(same_instance(&peer_one, &peer_one));
        assert!(same_instance(&peer_one, &foo("copy", Some("1"))));
        assert!(!same_instance(&peer_one, &foo("other", Some("2"))));
        assert!(!same_instance(&peer_one, &foo("absent", None)));

        let code = |project: &str, name: &str, body: &str| {
            let dir = tree.path().join(project).join("node_modules/foo");
            std::fs::write(dir.join(name), body).unwrap();
        };
        // A file on one side, then the same name with other bytes, then the same bytes.
        foo("first", Some("1"));
        foo("second", Some("1"));
        let (first, second) = (
            tree.path().join("first/node_modules/foo"),
            tree.path().join("second/node_modules/foo"),
        );
        code("first", "index.js", "one\n");
        assert!(!same_instance(&first, &second));
        code("second", "index.js", "two\n");
        assert!(!same_instance(&first, &second));
        code("second", "index.js", "one\n");
        assert!(same_instance(&first, &second));
        // A nested package is judged as a package, or not at all if nothing depends on it.
        install(&first.join("node_modules/unrelated"), "1", &[]);
        assert!(same_instance(&first, &second));
        // Different code one level down makes the dependent different too.
        std::fs::write(
            tree.path().join("first/node_modules/peer/index.js"),
            "other code\n",
        )
        .unwrap();
        assert!(!same_instance(&first, &second));

        // Through a cycle, which has to end, and still see the difference past it.
        let cycle = |project: &str, leaf: &str| {
            let modules = tree.path().join(project).join("node_modules");
            install(&modules.join("back"), "1", &["start", "leaf"]);
            install(&modules.join("leaf"), leaf, &[]);
            install(&modules.join("start"), "1", &["back"])
        };
        let start = cycle("c1", "1");
        assert!(same_instance(&start, &cycle("c2", "1")));
        assert!(!same_instance(&start, &cycle("c3", "2")));
    }
}
