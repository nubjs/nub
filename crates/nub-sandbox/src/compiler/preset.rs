//! The closed preset table. A `"sandbox": "<preset>"` string opts into a
//! nub-implemented named policy set. The resolver is a CLOSED table — an unknown
//! preset is a hard error naming the supported set (same discipline as the env
//! type grammar), so adding a preset later is non-breaking.
//!
//! A preset expands to the equivalent granular surface `Value`, which the pipeline
//! then folds — one code path, no separate preset→IR translator to keep in sync.
//!
//! The `build-jail` preset is nub's dependency-lifecycle-script confinement. It is
//! reachable two ways: as a bare `--sandbox build-jail` STATIC policy (the skeleton
//! below), and — the production path — via [`compile_build_jail`], which the aube
//! lifecycle interposition drives per spawn with the script's own package dir,
//! provisioned interpreter, and constructed env.

use super::{CompileCtx, CompileError, ScopeCapabilities, compile, defaults};
use crate::matcher::path::{Homes, canonicalize_glob_prefix};
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, SandboxPolicy};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Resolve a preset name to its granular surface object. `"build-jail"` is the
/// only preset today (the lifecycle-script baseline).
pub fn resolve(name: &str) -> Result<Value, CompileError> {
    match name {
        "build-jail" => Ok(build_jail_surface(None, None, None, None)),
        other => Err(CompileError::unknown_preset(other, &["build-jail"])),
    }
}

/// Make the build jail a PURE ALLOWLIST: strip every deny the fold produced, so the
/// compiled policy carries grants only.
///
/// The build jail is not a deny model compiled into grants — it never had denies to begin
/// with. Its shape is "install into a temp dir, run the script there with access to only
/// that dir plus specific deps", and under a `default_effect == Deny` base a path is
/// unreadable because nothing granted it. A deny rule on top is therefore either redundant
/// (the path is outside every grant — the `~/.ssh` family, `/etc/shadow`) or a sign that a
/// GRANT is too broad, which is fixed by narrowing the grant, never by carving it.
///
/// This is not cosmetic. Deny-inside-allow is inexpressible on the zero-privilege
/// mechanisms: Landlock unions rules and has no deny primitive at any ABI, and an explicit
/// deny-ACE naming a Windows AppContainer's own SID is inert against that AppContainer's
/// child. Because the fold's `.env*`/`.npmrc` band is depth-independent, its literal prefix
/// is empty, so `backend::windows::deny_shadows_grant` sees it shadow every grant and
/// REJECTS the policy — which is why every read-granting build-jail policy fails closed on
/// Windows today with `build-jail could not be applied`. Emitting no denies is what lets the
/// allowlist backends enforce the jail at all.
///
/// Enforced by stripping rather than by suppressing each finalizer, so a deny added to the
/// surface or to a fold finalizer later cannot silently reintroduce the rejection; the
/// invariant lives in one place and is asserted by `build_jail_emits_no_deny_rules`.
///
/// THE INVARIANT BINDS THE BACKENDS, NOT JUST THIS FUNCTION. Stripping every `Effect::Deny`
/// is worth nothing if a backend then SYNTHESIZES one out of an allow, which is exactly what
/// the Seatbelt write loop did — a read-only Allow became `(deny file-write* …)`, so the
/// jail's own grants cancelled each other while the IR looked deny-free. A backend may
/// render an Allow only as permission.
pub fn enforce_pure_allowlist(name: &str, policy: &mut SandboxPolicy) {
    if name != "build-jail" {
        return;
    }
    policy.fs.rules.entries.retain(|r| r.effect != Effect::Deny);
}

/// Grant the build-jail interpreter closure (the provisioned Node + the PATH-prepended
/// shim) READ. nub provisions its own Node under its store rather than `/usr`, so the
/// tight-read base (Linux `RootView::Minimal` auto-mounting `ESSENTIAL_READ_PATHS`,
/// macOS's Seatbelt system base) does NOT reach it — the read-set spike proved this is
/// load-bearing (a node-gyp build with the interpreter ungranted fails). Under nub a
/// bare `node` hits the shim (`$NODE`) while `npm_node_execpath` names the real binary,
/// so the interposition supplies BOTH; each is granted the FILE and its bin DIR
/// (siblings a re-spawning build tool reaches). Front-inserted as a base allow so the
/// `.env`/secret floor entries (later) still win — the interpreter paths never overlap
/// them, so the position is safe either way.
pub fn grant_build_jail_interpreter(name: &str, policy: &mut SandboxPolicy, ctx: &CompileCtx) {
    if name != "build-jail" || ctx.interpreter.is_empty() {
        return;
    }
    let mut grants = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for interpreter in &ctx.interpreter {
        if seen.insert(interpreter.clone()) {
            push_read_path(&mut grants, interpreter, FsOrigin::Authored);
        }
        if let Some(bin_dir) = interpreter.parent()
            && seen.insert(bin_dir.to_path_buf())
        {
            push_read_path(&mut grants, bin_dir, FsOrigin::Authored);
        }
    }
    policy.fs.rules.entries.splice(0..0, grants);
}

/// Grant the build-jail's per-spawn extra READ subtrees that the interpreter grant misses.
/// The embedder derives them; today they are the provisioned Node's toolchain subtrees
/// (below) plus the resolved Python's own closure, whose derivation and bounds live with
/// the embedder because it owns where each toolchain comes from.
///
/// The Node pair is the GLOBAL PACKAGE TREE and the C/C++ HEADERS, and both are spelled by
/// the embedder because BOTH DIFFER BY DISTRIBUTION SHAPE — the layouts are not variants of
/// one path, they are two different answers:
///
/// - The global tree is `<node-root>/lib/node_modules` on POSIX and `<node-root>/node_modules`
///   on Windows, whose archive is FLAT (`node.exe` with `node_modules` beside it, no `bin/`
///   and no `lib/`). It is what makes `npm`/`npx`/`corepack` resolvable at all — each is a
///   symlink into that tree on POSIX, a `%~dp0`-relative `.cmd` shim on Windows — and on POSIX
///   it is genuinely load-bearing, because it sits OUTSIDE the granted bin dir (`../lib/…`):
///   without it all three dangle and the standard `prebuild-install || npm run build` fallback
///   dies at `npm: not found` (measured on `keytar`: rc 127 → rc 0 once the target is
///   readable). On the FLAT layout it is redundant rather than load-bearing, since read grants
///   are subtree grants ([`push_read_path`]) and the interpreter's own directory IS the
///   distribution root — spelled anyway, so the grant states what the jail needs instead of
///   depending on that coincidence.
/// - The headers are `<node-root>/include/node` **only where the distribution ships them**,
///   which the Windows one does not — verified against `node-v24.18.1-win-x64.zip`'s central
///   directory (zero `include/`, `.h` or `.lib` entries). So `include/node` is a POSIX answer,
///   and the embedder asks the DISK rather than the platform: where the directory exists it
///   names the distribution root, and where it does not it prefetches the `-headers.tar.gz`
///   plus `node.lib` OUT of jail into a tree nub owns and names that instead.
///
/// Either way `npm_config_nodedir` names the granted tree, which is what makes the grant
/// load-bearing rather than an optimisation. node-gyp reads `<nodedir>/include/node/*` and
/// `<nodedir>/$(Configuration)/<name>.lib` when nodedir is set, and when it is NOT set it
/// DOWNLOADS the headers into its own `%LOCALAPPDATA%\node-gyp\Cache` / `~/.cache/node-gyp`
/// devDir (node-gyp 13 `lib/configure.js` `getNodeDir`, `lib/install.js`) — a fetch the jail's
/// deny-all egress refuses, so an ungranted nodedir is not a slow native build, it is no
/// native build at all. nub provisions Node under its version store (`~/.cache/nub/node/
/// <ver>`), a path in neither `$tooldirs` nor the interpreter grant, so nothing else covers it.
///
/// The embedder keeps the grant on SUBTREES rather than the bare root, which for a system Node
/// is a shared prefix carrying unrelated `etc/`/`var/`. A nonexistent path yields an inert
/// allow, which is what lets the embedder pass a speculative spelling without probing first.
/// Front-inserted as base allows so the reasserted secret/`.env` floor stays authoritative;
/// these paths never overlap a secret.
fn grant_build_jail_extra_reads(policy: &mut SandboxPolicy, extra_reads: &[PathBuf]) {
    let mut grants = Vec::new();
    for dir in extra_reads {
        push_read_path(&mut grants, dir, FsOrigin::Speculative);
    }
    policy.fs.rules.entries.splice(0..0, grants);
}

/// The two subtrees of nub's own PM cache the jail grants, as `$tooldirs`-style surface
/// patterns. Held here rather than inlined so they and the broad `$tooldirs` set resolve
/// against the same anchor on every platform — `builtin_sets` carries `$cache/nub/pm` for
/// its nub entry, and `the_narrowed_toolchain_grant_stays_inside_tooldirs` pins that these
/// stay inside it.
///
/// The grant used to be the cache ROOT. That was wider than anything the jail reads and it
/// leaked: a git dependency is cloned to `$cache/nub/pm/git/<key>`, whose `.git/config`
/// records the fetch URL — so a private dep fetched over HTTPS with a token in the URL put
/// that token in reach of EVERY lifecycle script on the machine (reproduced under the real
/// jail on macOS and Linux, 2026-07-28). Naming the subtrees the jail actually needs is a
/// GRANT NARROWING, not a deny, so the pure-allowlist invariant and the Windows backend are
/// untouched.
///
/// Why exactly these two, and why the rest of the cache is not needed:
/// - `store` is the global virtual store (`identity.rs` sets `virtual_store_subdir` to
///   `store`), the directory packages actually materialize into. A project's
///   `node_modules/<name>` is a SYMLINK out of it, and both Landlock and Seatbelt match on
///   the canonicalized path, so the project `node_modules` grant does not reach the content
///   behind the link. Nothing runs without this.
/// - `tools` holds the node-gyp nub bootstraps for itself
///   (`node_gyp_bootstrap.rs` → `<cache>/tools/node-gyp`). The read-ladder study measured 16
///   of 33 packages failing `rc=127` without it.
/// - `git` stays UNGRANTED. A git dep's own `prepare` still reaches its checkout, because
///   the checkout is that spawn's `package_dir` rw grant — so the narrowing costs a git dep
///   nothing and closes every CROSS-package read. (A git dep's script can still see its own
///   repo's fetch URL; that credential is one it was fetched with, not another project's.)
/// - `packuments-v1`, `packuments-full-v1`, `trust-policy-v1` are registry metadata the
///   resolver consumes UNCONFINED, before any script spawns. No lifecycle script reads them.
const NUB_PM_CACHE_PATTERNS: &[&str] = &[NUB_GLOBAL_VIRTUAL_STORE_PATTERN, "$cache/nub/pm/tools"];

/// The GLOBAL virtual store, as a `$cache`-anchored surface pattern — the directory aube
/// materializes every package into when `use_global_virtual_store` is on (the default off
/// CI). Named once so the read grant above and the store-containment guard in
/// [`store_entry_write_root`] can never drift apart.
const NUB_GLOBAL_VIRTUAL_STORE_PATTERN: &str = "$cache/nub/pm/store";

/// The PROJECT-LOCAL virtual store's leaf under `node_modules` — the OTHER root the same
/// package can materialize into, since `use_global_virtual_store` is a per-install toggle
/// and `with_project_local_dep_paths` flips it per PACKAGE. A `file:` dependency takes this
/// path on an otherwise-global install, so both roots are live in one tree.
///
/// THE SINGLE SOURCE OF TRUTH FOR THE NAME, which nub-cli's `nub_setting_defaults` reuses
/// when it sets the engine's `virtualStoreDir`. It lives here rather than there so the
/// grant and the layout cannot drift: this leaf was `.nub` until it moved to the
/// vendor-neutral `.store`, and a jail holding its own stale copy of the name declines
/// SILENTLY — the grant compiles to nothing and the package dies on gyp's laundered
/// ENOENT, indistinguishable from having no fix at all.
pub const PROJECT_VIRTUAL_STORE_LEAF: &str = ".store";

/// Where the per-package private HOMEs live, as a `$cache`-anchored surface pattern.
///
/// A 66-package lifecycle corpus put an unwritable `$HOME` behind 7 of 11 filesystem
/// failures — `npx only-allow`'s `~/.npm` (the widest, a common preinstall idiom),
/// `~/.cache/Cypress`, `~/.stc`, `~/.cache/ffmpeg-static-nodejs`. Each wants a
/// home-anchored scratch dir; none wants the USER's home. Cypress also shows the tiers
/// are ordered: its CDNs are already on `$downloads` and it still failed, because the
/// write is refused before the fetch is attempted.
///
/// A sibling of, not a child of, `$cache/nub/pm`: the PM cache is read-granted to every
/// jailed script, and the one writable home must not nest inside the store it could then
/// shadow.
const NUB_JAIL_HOME_ROOT_PATTERN: &str = "$cache/nub/jail-home";

/// Resolve and materialize the private HOME for ONE package's lifecycle spawns.
///
/// PER-PACKAGE, not shared. A shared home is a config root two different dependencies
/// both write: package A drops `$HOME/.npmrc` naming `script-shell`/`node-gyp` under its
/// own control, package B's `prebuild-install || npm run build` fallback honours it, and
/// the attacker now runs inside B's jail with write access to B's `build/Release/*.node`
/// — which the user later `require()`s UNCONFINED. Per-package removes that channel
/// outright, and it is what aube's own jail chose (`aube-scripts::jail_home`).
///
/// PERSISTENT across runs, which is the one divergence from `$tmp` and from aube. It buys
/// only install-time cache reuse — re-installing the same package skips a ~250 MB Cypress
/// or ~70 MB ffmpeg-static re-download and works offline the second time. It does NOT
/// make those artifacts resolvable at RUN time: the app's `HOME` is the user's own, and
/// nub sets no `CYPRESS_CACHE_FOLDER`. Persistence is safe here only because the home is
/// per-package — the sole writer is the package that reads it.
///
/// `None` — a cache root the engine never established, or anything but a real directory
/// squatting the leaf — leaves the jail compiling exactly as before. That is the
/// conservative direction: no grant, no redirect, today's behavior.
fn private_home_dir(homes: &Homes, package_dir: &Path) -> Option<PathBuf> {
    // Gate on the engine's cache root already existing, so a policy compiled against a
    // synthetic `Homes` (every unit test) neither materializes a tree nor silently
    // changes shape with the caller's privileges.
    if !homes.cache.is_dir() {
        return None;
    }
    let root = PathBuf::from(crate::matcher::path::expand_symbolic(
        NUB_JAIL_HOME_ROOT_PATTERN,
        homes,
    ));
    std::fs::create_dir_all(&root).ok()?;
    // Canonicalize the ROOT and append the leaf literally — never canonicalize the leaf.
    // The leaf is the one node a jailed script can unlink and recreate (a Seatbelt subpath
    // grant matches its own root), so following it would let a script replace its home
    // with a symlink to the real one and have the NEXT compile grant that. Resolving the
    // parent instead is what makes the grant unforgeable; `symlink_metadata` then refuses
    // a leaf that is anything but a real directory. The helper also strips Windows'
    // `\\?\` verbatim prefix, whose `?` a bounded-literal grant reads as a glob and drops.
    let root = crate::matcher::path::canonicalize_including_nonexistent(&root);
    let dir = root.join(package_home_slug(package_dir));
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => Some(dir),
        Ok(_) => None,
        Err(_) => create_private_dir(&dir).is_ok().then_some(dir),
    }
}

/// Create `dir` owner-only. The home holds one package's install scratch; nothing else on
/// the machine has business in it, and the umask default (0755) would publish it.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    // Only the `unix` arm mutates the builder; elsewhere the binding is immutable.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// A stable, filesystem-safe directory name for one package's home: its readable basename
/// plus a hash of the resolved package dir, so two packages never collide and the same
/// package finds its own cache again on the next install. Hash stability across Rust
/// releases is not required — a changed hash only means a cold cache.
fn package_home_slug(package_dir: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let resolved = crate::matcher::path::canonicalize_including_nonexistent(package_dir);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    resolved.hash(&mut hasher);
    let name: String = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("package")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}-{:016x}", hasher.finish())
}

/// Grant the build jail's narrowed READ set: the consumer's DEPENDENCY TREE, the
/// consumer's top-level MANIFEST, and the two [`NUB_PM_CACHE_PATTERNS`] subtrees of nub's
/// own PM cache. Together these replace what were once two much broader grants — `"./"`
/// (the entire consuming project) and `$tooldirs` (16 ecosystem cache patterns).
///
/// Measured, not reasoned. A 34-package read-ladder study
/// (`.fray/sandbox-minimum-readset.md`) isolated which grants are load-bearing, and a
/// 311-package trust-list corpus (`.fray/sandbox-readset-fullcorpus.md`) then ran the
/// whole set at scale: of the 219 packages that pass today, 217 are unaffected and the
/// 2 that regressed drove the manifest grant below. What the narrowing buys is the
/// credential surface those broad grants carried — under `"./"` a dependency's install
/// script could read the consumer's source, config, `.git/hooks/`, and
/// `.github/workflows/`.
///
/// Why each is irreducible:
/// - a lifecycle script's OWN dependencies are HOISTED to the consumer's `node_modules`,
///   so `node-gyp-build` and `prebuild-install` resolve out of `<project>/node_modules/.bin`
///   rather than the package's own directory. Dropping the project read outright fails 27
///   of 33 packages; keeping only `node_modules` costs nothing.
/// - `package.json` is granted as ONE FILE, never the directory that holds it. Two
///   packages at scale read the consumer's top-level manifest and crash with an uncaught
///   `ENOENT` without it: `@sentry/capacitor` cross-checks its version against sibling
///   `@sentry/*` entries, and `simple-git-hooks` looks for its own config field. It is a
///   non-secret manifest the package is already declared in, so the exposure is
///   negligible — and confining it to the file is what keeps the rest of the project out.
/// - nub bootstraps its OWN node-gyp into `<cache>/nub/pm/tools/node-gyp`
///   (`node_gyp_bootstrap.rs`) — a TOOLCHAIN grant wearing a cache-directory name.
///   Under nub a confined script skips the ambient-PATH probe entirely, so this subtree
///   (including the `lazy-bin` shim) is the ONLY node-gyp a native build can reach. The
///   other 15 `$tooldirs` patterns (`~/.cargo/registry`, `~/.m2/repository`, the
///   pnpm/yarn/bun stores, …) were reached by no package in either corpus.
/// - `<cache>/nub/pm/store` is where the dependency tree physically lives under the global
///   virtual store; the `node_modules` grant above only covers the symlinks pointing at it.
///
/// SPECULATIVE origin is load-bearing, not incidental: every root here is legitimately
/// absent on a real host — a project whose dependencies are not installed, a manifest-less
/// directory, a machine where nub has never bootstrapped node-gyp — and
/// `compile_mount_plan` REFUSES a missing AUTHORED source, which would abort every
/// confined script there.
///
/// ORDER: outermost path first, so each later grant nests INSIDE the one before it in
/// bwrap's argv. The project root is deliberately absent today (nothing needs it), which
/// leaves bwrap to auto-create it as writable scaffolding; if that has to become an empty
/// read-only bind to restore a loud `EROFS`, it slots in at the head of this list and the
/// nested grants keep working unchanged.
///
/// Front-inserted so the surface's `package_dir` rw entry stays later and keeps winning.
pub fn grant_build_jail_dependency_reads(
    name: &str,
    policy: &mut SandboxPolicy,
    ctx: &CompileCtx,
    package_dir: Option<&Path>,
) {
    if name != "build-jail" {
        return;
    }
    // The two project roots are the ONLY grants the experimental arm can withhold, and
    // withholding is all it can do — see `crate::arm` for why a drop-only boolean is what
    // keeps the allowlist's nub-curated authorship invariant structural.
    let mut roots = if crate::arm::project_grant_dropped() {
        Vec::new()
    } else {
        vec![
            ctx.homes.project.join("package.json"),
            ctx.homes.project.join("node_modules"),
        ]
    };
    roots.extend(
        NUB_PM_CACHE_PATTERNS
            .iter()
            .map(|p| PathBuf::from(crate::matcher::path::expand_symbolic(p, &ctx.homes))),
    );
    // The `node_modules` the package ACTUALLY sits in, which is not always the project's.
    // aube's hoisted planner is per-IMPORTER, so a workspace member's dependency
    // materializes at `<root>/packages/<m>/node_modules/<name>` and resolves its own
    // tooling through the sibling `<root>/packages/<m>/node_modules/.bin` — outside
    // `<project>/node_modules` entirely. Missing it reproduces exactly the failure the
    // read ladder measured at 27 of 33 packages when the project read is dropped, but
    // only in workspaces, which is how it would have escaped a single-project corpus.
    // Redundant for the root-hoisted and isolated layouts (both anchor under the
    // project's own `node_modules`), where this resolves to a path already covered.
    if let Some(dir) = package_dir
        && let Some(own) = enclosing_node_modules(dir)
    {
        roots.push(own);
    }
    let mut grants = Vec::new();
    for root in roots {
        push_read_path(&mut grants, &root, FsOrigin::Speculative);
    }
    policy.fs.rules.entries.splice(0..0, grants);
}

/// The nearest ancestor of `package_dir` named `node_modules`. That is the directory a
/// lifecycle script's own dependency closure and `.bin` shims are installed into,
/// whichever linker placed it.
fn enclosing_node_modules(package_dir: &Path) -> Option<PathBuf> {
    package_dir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "node_modules"))
        .map(Path::to_path_buf)
}

/// The package's own STORE-ENTRY ROOT — the extra subtree a confined native build must be
/// able to write — or `None` when the package was not materialized into a virtual store.
///
/// THE INVARIANT, and it is arithmetic rather than a choice any tool makes: a
/// `node-addon-api` dependant hands gyp a `..`-relative `.gyp` path (`path.relative` in
/// that package's `index.js`), and gyp joins `depth(".") + generator_output("build") +
/// base_path("../../../..")` — **`build/` absorbs exactly one `..`**, so the join collapses
/// one level too shallow and lands on the package's store-entry root, scoped and unscoped
/// alike (a scoped name's extra `..` is cancelled by its extra directory level). npm and
/// pnpm compute the same escaping path and both build fine, so the layout is not the fault;
/// only confinement turns it into a failure. It presented for weeks as an ABI problem
/// because gyp's `EnsureDirExists` is a bare `except OSError: pass` one line above the
/// `open()` that reports, laundering the jail's EPERM into a misleading ENOENT.
///
/// GUARDED ON STORE CONTAINMENT, because under a HOISTED linker the identical arithmetic
/// lands on the PROJECT ROOT (or a workspace member's root), which must never be writable.
/// The candidate qualifies only when its parent is a virtual store aube itself materializes
/// into, so a hoisted layout declines structurally rather than by luck. `package_dir` is
/// resolved first so the derivation does not depend on whether aube handed over the store
/// path or the project's symlink into it — both backends match the resolved path anyway.
///
/// KNOWN RESIDUAL: a SCOPED package under a hoisted linker escapes into
/// `node_modules/@scope/`, which this correctly declines and deliberately does not fix —
/// granting the scope dir would hand a build write access to its sibling packages.
fn store_entry_write_root(homes: &Homes, package_dir: &Path) -> Option<PathBuf> {
    let resolved = crate::matcher::path::canonicalize_including_nonexistent(package_dir);
    let candidate = enclosing_node_modules(&resolved)?.parent()?.to_path_buf();
    let parent = crate::matcher::path::canonicalize_including_nonexistent(candidate.parent()?);
    let global = PathBuf::from(crate::matcher::path::expand_symbolic(
        NUB_GLOBAL_VIRTUAL_STORE_PATTERN,
        homes,
    ));
    let project_local = homes
        .project
        .join("node_modules")
        .join(PROJECT_VIRTUAL_STORE_LEAF);
    [global, project_local]
        .iter()
        .any(|root| crate::matcher::path::canonicalize_including_nonexistent(root) == parent)
        .then_some(candidate)
}

/// Push a READ-allow rule per subtree glob for `path` (the node itself + `/**`).
///
/// `origin` decides what an ABSENT path means. The interpreter was resolved from the
/// spawn, so its disappearance is a real error; the extra reads are derived from the
/// interpreter's location without ever being looked up, so a Node laid out differently
/// (distro headers in a separate package, no bundled npm) must leave them inert rather
/// than fail the jail closed.
fn push_read_path(out: &mut Vec<FsRule>, path: &Path, origin: FsOrigin) {
    for g in defaults::subtree_globs(&path.to_string_lossy()) {
        out.push(FsRule {
            matcher: CanonGlob(canonicalize_glob_prefix(&g)),
            effect: Effect::Allow,
            access: FsAccess::Read,
            origin,
        });
    }
}

/// The build-jail baseline surface. Tight, default-deny read — the dependency tree and
/// nub's own toolchain cache ([`grant_build_jail_dependency_reads`], front-inserted after
/// this folds) plus the OS backends' minimal-root closure — with WRITE confined to a
/// per-run tmp (private everywhere but Windows, which takes the shared tmp; see the `$tmp`
/// comment below) and, via [`compile_build_jail`], the script's own package dir.
/// Egress gated on package identity, coarsely (see [`build_jail_net`]).
///
/// `package_dir` is the per-spawn WRITE grant. It stays LAST so its read-write access
/// wins over the front-inserted dependency-tree read for the package subtree
/// (last-match-wins, preserved by the `preserve_order` serde_json map). `None` yields
/// the static `--sandbox build-jail` skeleton (the per-package write is a production-
/// interposition concern), as does `private_home`. The env axis is the strip-all floor
/// here; [`compile_build_jail`] replaces it with the scrubbed lifecycle env.
fn build_jail_surface(
    package_dir: Option<&Path>,
    private_home: Option<&Path>,
    package_name: Option<&str>,
    store_entry_root: Option<&Path>,
) -> Value {
    let mut fs = serde_json::Map::new();
    // `$tmp` sets the private per-run tmp MODE (TmpMode::Private) — a writable scratch,
    // shared host tmp hidden. It emits no ordinary fs rule.
    //
    // WINDOWS takes the SHARED tmp instead, spelled as the key's ABSENCE (`Shared` is the
    // default and the sentinel deliberately cannot name it). It is not a concession: the
    // AppContainer backend cannot enforce `Private` at all, so asking for it only ever
    // produced a standing `tmp-private` lost axis plus a per-run scratch dir the confined
    // launch path never points the child at. The child is not left without scratch — the
    // OS redirects an AppContainer's TEMP into its own
    // `…\AppData\Local\Packages\<profile>\AC` profile, writable by construction (measured).
    // Free by construction, which is the load-bearing part: a tmp MODE emits no fs rule, so
    // no inheritable ACE and no DACL propagation. Granting the literal `%TEMP%` PATH instead
    // would cost a full inheritable-ACE tree walk over an enormous directory on every
    // lifecycle spawn (`set_ace` propagates; ~1000 ms on a populated tree vs 3 ms on an
    // empty one) — that is why the widening is a mode change and not a path grant.
    #[cfg(not(windows))]
    fs.insert("$tmp".to_string(), json!("rw"));
    if let Some(dir) = private_home {
        // This package's own HOME (see `private_home_dir`), read-write.
        fs.insert(dir.to_string_lossy().into_owned(), json!("rw"));
    }
    if let Some(dir) = package_dir {
        // The package's own store-entry root, read-write. FIRST so the `package_dir` entry
        // below stays last and keeps winning under last-match-wins for its own subtree.
        // See [`store_entry_write_root`] for why gyp writes here and why this is guarded on
        // store containment rather than emitted unconditionally.
        //
        // COST, weighed and accepted: the store entry also holds `node_modules/.bin` for a
        // package with binary deps, which this makes writable. Those shims execute only
        // while THIS package's own lifecycle scripts run — a context the attacker already
        // owns — and it does not reach the project's `node_modules/.bin`. The store is
        // machine-global, so it shares the cross-project reach the store already has. A
        // tighter variant (enumerate `<store-entry-root>/<dep_dirname>` per direct dep,
        // which nub knows at spawn time) is real machinery for a jail whose stated posture
        // is to loosen liberally when packages break.
        if let Some(root) = store_entry_root {
            fs.insert(root.to_string_lossy().into_owned(), json!("rw"));
        }
        // Own-package-dir READ-WRITE: the one subtree a dep build may write (its
        // `build/`, the compiled `.node`).
        //
        // The write ladder found no outcome changed by widening this to `node_modules` rw
        // or `./` rw — but it installed only the LATEST version of every package, so that
        // is a result about the versions measured, NOT a general equivalence. Known
        // counterexample: `@prisma/client` 3.x initializes `node_modules/.prisma/client`
        // from its postinstall (`path.join(__dirname, '../../../.prisma/client')`), a
        // SIBLING of the package dir, which this grant denies. Older versions can need
        // writes the corpus never exercised; read the ladder as evidence about a version
        // sample, not about the ecosystem.
        //
        // DO NOT "fix" that by allowing writes to dot-directories at the `node_modules`
        // root. That generalization is strictly WORSE than the whole-project write grant
        // it looks like a tightening of, because the dot-entries there are not scratch
        // space — they are the install itself:
        //   - `.aube/<dep_path>/node_modules/<name>` is nub's own virtual store, where
        //     EVERY materialized package in the dependency tree lives (`.pnpm/` likewise).
        //     Write access there is write access to every dependency's source, before that
        //     source is executed.
        //   - `.bin/` is the shim directory later tooling executes UNCONFINED — the exact
        //     persistence vector the jail exists to close.
        // Covering the codegen case therefore needs an ENUMERATED namespace (`.prisma`),
        // never a pattern over dot-entries. Such a grant is a pure positive rw nested in
        // the dependency-tree read — the same shape this `package_dir` entry already is —
        // so it costs nothing on Windows: `deny_shadows_grant` rejects a DENY that overlaps
        // a grant, and an allow-list introduces no deny. The deny-inside-allow form (grant
        // the dot-entries, refuse `.bin`) is what Windows cannot express and would fail
        // closed on every install there.
        fs.insert(dir.to_string_lossy().into_owned(), json!("rw"));
    }
    // NO `/etc/shadow` deny here any more. The build jail is a PURE ALLOWLIST — it emits no
    // deny rules at all — so the password-hash files are protected by not being granted:
    // free on Linux (the minimal root binds `/etc` LEAVES, never the directory), and on
    // macOS by the Seatbelt base granting the specific `/private/etc` files it needs instead
    // of the whole subpath (`macos_seatbelt_base.sbpl`).
    json!({
        "fs": Value::Object(fs),
        "net": build_jail_net(package_name),
        // Strip-all here; the interposition supplies the scrubbed lifecycle env.
        "vars": []
    })
}

/// The build-jail's net axis, gated on PACKAGE IDENTITY: a package the catalog names may reach
/// the network, and a package it does not name reaches nothing.
///
/// NO ENTRY MEANS NO NETWORK, and that default is the point rather than a fallback. The
/// attack this jail exists to blunt is the Shai-Hulud shape — an attacker publishes a new
/// `postinstall` into a package that never had one, and it runs with the user's access. That
/// package is not one anybody vetted for network use, so it gets nothing; a package that
/// needs egress arrives through a pull request against `data/build-jail-catalog.json` first,
/// and that PR is the whole opt-in mechanism. Host granularity is a separate axis and a much
/// smaller one: narrowing an admitted host would constrain a package somebody already
/// reviewed, while doing nothing about the unvetted one, and a global egress set handed that
/// unvetted package `github.com` — ample for exfiltration on its own.
///
/// The grant is therefore a BOOLEAN, deliberately. Both catalog spellings — a package named in
/// `networkHosts[].fetchedBy` and one listed in `packageNetwork.full` — mean "this package was
/// reviewed and admitted", so both resolve to the same grant. A package recorded in
/// `notGranted.packages` is denied whichever way it was observed: the catalog parser subtracts
/// the refused set from the union, so a refusal cannot be contradicted by an observation of
/// what the package was seen fetching.
///
/// PER-HOST EGRESS IS DELIBERATELY NOT ENFORCED HERE, AND `$downloads` IS DELIBERATELY NOT
/// REFERENCED. Do not restore either as a missing feature. Two of the three backends cannot
/// enforce a host list at all: Linux needs a network namespace to force the child through nub's
/// proxy, which needs an unprivileged user namespace — denied by default on Ubuntu 24.04, and
/// requiring it is the one thing this product cannot do; Windows' loopback exemption
/// (`NetworkIsolationSetAppContainerConfig`) is admin-only. macOS alone COULD enforce it, via
/// Seatbelt confining egress to the proxy port, and enforcing it there was withdrawn on purpose:
/// macOS is the platform most developers use, so being stricter there means an incomplete host
/// list throws errors that Linux and Windows users never see, and confidence in the list is low.
/// A host list that gates one platform and is provenance on two is a compat liability, not a
/// defense. `$downloads` still serves `nub sandbox`, a different mechanism where per-host DOES
/// work — this jail simply no longer consults it.
///
/// The catalog's per-package `hosts` arrays are retained for the same reason and enforce nothing
/// either: a CHANGING host list is a detection signal, so a package that used to fetch from its
/// own CDN and now reaches somewhere else shows up as a reviewable diff on
/// `data/build-jail-catalog.json`. They are provenance, never a gate.
///
/// The surviving contract is uniform: no catalog entry ⇒ no egress; an entry ⇒ coarse egress.
/// Denial is `false` everywhere. The admitted case is coarse everywhere too, but SPELLED per
/// platform, because `enforce` carries more than egress on Linux:
///
/// - **macOS** and **Windows** get `true` (`enforce = false`). Seatbelt emits `(allow network*)`
///   and starts no proxy; Windows' unprivileged egress lever is the AppContainer
///   `internetClient` capability, which the backend grants on exactly `!net.enforce`, so
///   coarse-allow is the ONLY spelling that reaches it. A host array there would instead select
///   the per-host tier via `needs_account_backend` / `plan_net`, which needs an admin-registered
///   loopback exemption, so an ordinary `nub install` would fail closed (or divert to the
///   admin-only dedicated-account backend) for every catalogued package. `true` is still not
///   "unrestricted" on Windows: WFP refuses an AppContainer's loopback whatever its capabilities
///   and `privateNetworkClientServer` stays withheld, so the grant is public outbound only.
/// - **Linux** gets `["*"]` — a catch-all Allow, naming no host. It CANNOT be `true`, because
///   `backend::linux::build_seccomp` gates the whole socket-family ceiling and the io_uring
///   triple-block on `net.enforce`: `enforce = false` would re-permit `AF_UNIX` (a path to host
///   daemons like `docker.sock` that neither Landlock nor a netns scopes), `AF_VSOCK`,
///   `AF_PACKET`, and io_uring's socket-creation side door. Keeping `enforce = true` with a
///   catch-all Allow lifts exactly `AF_INET`/`AF_INET6` out of that ceiling and leaves the rest
///   denied, which is the coarse grant with the non-egress protections intact. Zero named hosts
///   means nothing here reads as a gate, and zero *authored* hosts keeps `mode` off the
///   proxy-starting path (`apply_inner`'s Landlock arm skips the proxy outright).
fn build_jail_net(package_name: Option<&str>) -> Value {
    if !super::package_network::build_jail_net_allowed(package_name) {
        return json!(false);
    }
    if cfg!(target_os = "linux") {
        json!(["*"])
    } else {
        json!(true)
    }
}

/// Compile the build-jail policy for ONE dependency lifecycle spawn — the production
/// interposition entry the aube lifecycle hook drives. Builds the tight read/write
/// skeleton for `package_dir`, grants the provisioned `interpreter`, then REPLACES the
/// env axis with the constructed lifecycle env minus credential-shaped keys (D1: a
/// dep build needs `PATH`/`NODE`/`npm_package_*`/build hints, so strip-all breaks it;
/// the credential family — registry auth, `*TOKEN*`/`*SECRET*`/`*AUTH*` — is withheld).
///
/// `ambient_env` is the effective child env the UNCONFINED spawn would have had (the
/// aube-process env plus the command's overlay), already reconstructed by the caller.
/// `interpreter` is the closure to grant read (the provisioned Node + shim); each
/// path and its bin dir become read grants. `extra_reads` are additional per-spawn read
/// subtrees the embedder derives — the headers node-gyp compiles against and the global
/// package tree `npm`/`npx` resolve through, both spelled per distribution shape (POSIX
/// `include/node` + `lib/node_modules`; Windows a prefetched header tree + a flat
/// `node_modules`) — see [`grant_build_jail_extra_reads`].
///
/// `package_name` is aube's INSTALLER-RESOLVED identity for this spawn and the only key
/// the curated per-package exception table ([`super::curated`]) is looked up by. `None` —
/// aube's root is a checkout it fetched — grants no exception.
pub fn compile_build_jail(
    homes: Homes,
    package_dir: &Path,
    package_name: Option<&str>,
    interpreter: Vec<PathBuf>,
    extra_reads: Vec<PathBuf>,
    ambient_env: BTreeMap<String, String>,
) -> Result<SandboxPolicy, CompileError> {
    let private_home = private_home_dir(&homes, package_dir);
    let store_entry_root = store_entry_write_root(&homes, package_dir);
    let surface = build_jail_surface(
        Some(package_dir),
        private_home.as_deref(),
        package_name,
        store_entry_root.as_deref(),
    );
    // cwd anchors diagnostics/canonicalization; the project root (homes.project) is
    // what `"./"` expands against, so the package dir as cwd does not affect grants.
    let cwd = homes.project.clone();
    // Approved caps: this is nub's own build-jail, not dependency-authored config.
    let ctx = CompileCtx::new(
        homes,
        cwd,
        ScopeCapabilities::approved(),
        ambient_env.clone(),
    )
    .with_interpreter(interpreter);
    // The object surface routes through `compile` (not the string-preset arm), so the
    // interpreter grant + secret-floor reassert are applied here rather than in
    // `compile_scope`'s preset branch.
    let mut policy = compile(&surface, &ctx)?;
    grant_build_jail_dependency_reads("build-jail", &mut policy, &ctx, Some(package_dir));
    grant_build_jail_interpreter("build-jail", &mut policy, &ctx);
    grant_build_jail_extra_reads(&mut policy, &extra_reads);
    // LAST of the grants, so a curated rw wins under last-match-wins over the
    // front-inserted dependency-tree READ it nests inside. `ctx.homes.project` is the
    // consumer's project root, which aube guarantees whenever it hands over a name.
    super::curated::grant_curated_package(
        &mut policy,
        &ctx.homes.project,
        package_dir,
        package_name,
    );
    enforce_pure_allowlist("build-jail", &mut policy);
    policy.env = defaults::lifecycle_scrubbed_env(&ambient_env);
    policy.build_jail = true;
    // AFTER the scrub, which admits the ambient `HOME`/`USERPROFILE` verbatim — without
    // this the grant above is invisible, because the script still spells its home the
    // user's way and still lands outside every rule. Only names the ambient actually
    // carried, so a POSIX child does not acquire a `USERPROFILE` the unconfined spawn
    // never had. `LOCALAPPDATA` is deliberately NOT redirected: the Windows LowBox launch
    // resolves its AppContainer profile dir from it (`defaults::OS_ESSENTIAL_ENV`), so
    // pointing it elsewhere breaks process creation rather than a cache path. That leaves
    // Windows a compat gap of a DIFFERENT SHAPE, not a reachability one: measured on
    // windows-latest, the OS itself redirects the known folder for a LowBox token, so the
    // child's `%LOCALAPPDATA%` is the per-container `…\AppData\Local\Packages\<profile>\AC`
    // and the real one is denied outright. `unique_profile_name` keys that profile on
    // pid+nonce and `ProfileGuard::drop` deletes it, so the redirect target is fresh,
    // empty and destroyed per LAUNCH — Windows tooling that caches there never hard-fails,
    // it just gets zero reuse, ever. The `USERPROFILE` jail-home below IS persistent, so
    // the two axes do not behave alike.
    if let Some(home) = &private_home {
        let value = home.to_string_lossy().into_owned();
        for key in ["HOME", "USERPROFILE"] {
            // Matched the way `insert_env` replaces (case-insensitively on Windows, where
            // the ambient may spell it `Userprofile`), so presence and replacement agree.
            let present = policy.env.constructed.keys().any(|k| {
                if cfg!(windows) {
                    k.eq_ignore_ascii_case(key)
                } else {
                    k == key
                }
            });
            if present {
                defaults::insert_env(&mut policy.env.constructed, key.to_string(), value.clone());
            }
        }
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::compile;

    fn build_jail_policy() -> SandboxPolicy {
        let homes = Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        };
        let ctx = CompileCtx::new(
            homes,
            PathBuf::from("/proj"),
            ScopeCapabilities::approved(),
            BTreeMap::new(),
        );
        compile(&json!("build-jail"), &ctx).expect("build-jail preset compiles")
    }

    /// The PRODUCTION path — what every lifecycle spawn actually runs. Reaches the fold with
    /// a `package_dir` allow present, so the fold's env-deny finalizer fires here where it
    /// declines on the denies-only static skeleton; that difference is exactly why the
    /// pure-allowlist invariant is asserted on both.
    fn production_build_jail_policy() -> SandboxPolicy {
        let (interpreter, extra_reads) = POSIX_LAYOUT;
        production_build_jail_policy_for(interpreter, extra_reads)
    }

    /// The two DISTRIBUTION SHAPES the embedder can hand `compile_build_jail`, as
    /// (interpreter, extra reads). They are not variants of one path — see
    /// [`grant_build_jail_extra_reads`] — so both are exercised rather than the POSIX one
    /// standing in for both, which is how the Windows spelling stayed inert unnoticed.
    ///
    /// Spelled POSIX-style because the compiler is OS-agnostic and the drive letter is not what
    /// differs: the SHAPE is (nested `bin/` + sibling `lib/` + in-tree headers) against (flat
    /// root + headers in a separately-prefetched tree).
    const POSIX_LAYOUT: (&str, &[&str]) = (
        "/testhome/.cache/nub/node/v26/bin/node",
        &[
            "/testhome/.cache/nub/node/v26/include/node",
            "/testhome/.cache/nub/node/v26/lib/node_modules",
        ],
    );
    const FLAT_LAYOUT: (&str, &[&str]) = (
        "/testhome/.cache/nub/pm/jail-bin/24.18.1-x64/node.exe",
        &[
            "/testhome/.cache/nub/pm/node-headers/24.18.1",
            "/testhome/.cache/nub/pm/jail-bin/24.18.1-x64/node_modules",
        ],
    );

    fn production_build_jail_policy_for(interpreter: &str, extra_reads: &[&str]) -> SandboxPolicy {
        let homes = Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        };
        compile_build_jail(
            homes,
            Path::new("/proj/node_modules/somepkg"),
            None,
            vec![PathBuf::from(interpreter)],
            extra_reads.iter().map(PathBuf::from).collect(),
            BTreeMap::new(),
        )
        .expect("build-jail compiles")
    }

    /// Each shape's toolchain reads actually LAND — the headers node-gyp compiles against and
    /// the global package tree `npm` resolves through — while a sibling of the distribution
    /// stays refused.
    ///
    /// The refused sibling is what makes this non-vacuous: asserting only that the two trees are
    /// readable would pass just as well if the grant had widened to the whole cache home. And
    /// asserting it per SHAPE is the point — the Windows spellings were a POSIX path and a
    /// directory that does not exist in that archive at all, which a POSIX-only fixture cannot
    /// distinguish from a working grant.
    #[test]
    fn both_distribution_shapes_grant_their_headers_and_global_package_tree() {
        for (label, (interpreter, extra_reads)) in [("posix", POSIX_LAYOUT), ("flat", FLAT_LAYOUT)]
        {
            let policy = production_build_jail_policy_for(interpreter, extra_reads);
            let m = crate::matcher::PathMatcher::new(&policy.fs.rules);
            for read in extra_reads {
                let deep = Path::new(read).join("npm/bin/npm-cli.js");
                assert_eq!(
                    m.decide(&deep).effect,
                    Effect::Allow,
                    "{label}: {} must be readable",
                    deep.display()
                );
            }
            let sibling = Path::new(interpreter)
                .parent()
                .and_then(Path::parent)
                .expect("the fixture interpreter has a grandparent")
                .join("unrelated/secret.txt");
            assert_ne!(
                m.decide(&sibling).effect,
                Effect::Allow,
                "{label}: the grant widened past the distribution to {}",
                sibling.display()
            );
        }
    }

    /// THE build-jail invariant: the compiled policy is a PURE ALLOWLIST — zero deny rules.
    /// Rationale on [`enforce_pure_allowlist`]. Asserted on BOTH entry points because they
    /// reach the fold differently: the static skeleton folds to denies-only so the env-deny
    /// finalizer declines, while the production path folds with a `package_dir` allow present
    /// and the finalizer fires.
    #[test]
    fn build_jail_emits_no_deny_rules() {
        for (label, policy) in [
            ("static --sandbox build-jail", build_jail_policy()),
            ("compile_build_jail", production_build_jail_policy()),
        ] {
            let denies: Vec<&str> = policy
                .fs
                .rules
                .entries
                .iter()
                .filter(|r| r.effect == Effect::Deny)
                .map(|r| r.matcher.as_str())
                .collect();
            assert!(
                denies.is_empty(),
                "{label} must compile to a pure allowlist; found deny rules {denies:?}"
            );
            assert_eq!(
                policy.fs.rules.default_effect,
                Effect::Deny,
                "{label}: the allowlist only confines over a default-deny base"
            );
        }
    }

    /// The store-entry write grant, in both directions — it FIRES for a package
    /// materialized into a virtual store (either spelling) and DECLINES for every layout
    /// where the same arithmetic would land on user-authored source.
    ///
    /// The decline half is the property, not an assertion: a hoisted package's escape
    /// target IS the project root (or a workspace member's), so a guard that fired there
    /// would hand a dependency build write access to the consumer's own tree. Both hoisted
    /// rows are therefore run against the same compile path as the granted ones — a guard
    /// that declined by accident (wrong helper, absent wiring) would show up as the granted
    /// rows failing, and one that fired too widely as the declined rows failing.
    #[test]
    fn the_store_entry_write_grant_is_scoped_to_a_virtual_store() {
        use crate::matcher::PathMatcher;
        use crate::policy::FsAccess;

        let homes = Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        };
        let escape_target = |package_dir: &str| -> (PathBuf, bool) {
            let package_dir = PathBuf::from(package_dir);
            let policy = compile_build_jail(
                homes.clone(),
                &package_dir,
                None,
                Vec::new(),
                Vec::new(),
                BTreeMap::new(),
            )
            .expect("build-jail compiles");
            // Where gyp's `build/`-absorbed `..` chain actually lands: one level above the
            // package's enclosing `node_modules`.
            let escape = enclosing_node_modules(&package_dir)
                .and_then(|nm| nm.parent().map(Path::to_path_buf))
                .expect("every fixture sits under a node_modules");
            let d = PathMatcher::new(&policy.fs.rules).decide(&escape.join("Makefile"));
            (
                escape,
                d.effect == Effect::Allow && d.access == FsAccess::ReadWrite,
            )
        };

        for (label, package_dir) in [
            // GLOBAL virtual store — `$cache/nub/pm/store`, the default off CI.
            (
                "global store",
                "/testhome/.cache/nub/pm/store/sqlite3@5.1.14/node_modules/sqlite3",
            ),
            // PROJECT-LOCAL virtual store — what `use_global_virtual_store(false)` and
            // `with_project_local_dep_paths` materialize into.
            (
                "project-local store",
                "/proj/node_modules/.store/sqlite3@5.1.14/node_modules/sqlite3",
            ),
            // SCOPED, whose extra `..` is cancelled by its extra directory level, so it
            // lands on the same store-entry root rather than one level further out.
            (
                "scoped, global store",
                "/testhome/.cache/nub/pm/store/@vscode+sqlite3@5.1.14/node_modules/@vscode/sqlite3",
            ),
        ] {
            let (escape, writable) = escape_target(package_dir);
            assert!(
                writable,
                "{label}: the store-entry root {} must be writable",
                escape.display()
            );
        }

        for (label, package_dir) in [
            // HOISTED — the escape target IS the consumer's project root.
            ("hoisted", "/proj/node_modules/sqlite3"),
            // HOISTED in a WORKSPACE — the escape target is a member's source root, which
            // a project-root check alone would miss.
            (
                "hoisted workspace member",
                "/proj/packages/api/node_modules/sqlite3",
            ),
            // A SCOPED package under a hoisted linker escapes into `node_modules/@scope/`,
            // the one shape this deliberately does not fix: granting the scope dir would
            // hand the build write access to its sibling packages.
            ("hoisted scoped", "/proj/node_modules/@vscode/sqlite3"),
        ] {
            let (escape, writable) = escape_target(package_dir);
            assert!(
                !writable,
                "{label}: {} is user-authored source and must stay unwritable",
                escape.display()
            );
        }
    }

    /// The ACCEPTED COST of the grant above, pinned so a future tightening is a deliberate
    /// decision rather than a silent one: the store entry also holds `node_modules/.bin`
    /// for a package with binary deps, and those shims become writable. They execute only
    /// while THIS package's own lifecycle scripts run — a context the attacker already owns
    /// — and the grant does not reach the project's `node_modules/.bin`, which is the shim
    /// directory later tooling runs UNCONFINED.
    #[test]
    fn the_store_entry_grant_reaches_its_own_bin_but_never_the_projects() {
        use crate::matcher::PathMatcher;
        use crate::policy::FsAccess;

        let policy = compile_build_jail(
            Homes {
                home: PathBuf::from("/testhome"),
                tmp: PathBuf::from("/testtmp"),
                cache: PathBuf::from("/testhome/.cache"),
                project: PathBuf::from("/proj"),
            },
            Path::new("/proj/node_modules/.store/sqlite3@5.1.14/node_modules/sqlite3"),
            None,
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
        )
        .expect("build-jail compiles");
        let m = PathMatcher::new(&policy.fs.rules);
        let writable = |p: &str| {
            let d = m.decide(Path::new(p));
            d.effect == Effect::Allow && d.access == FsAccess::ReadWrite
        };
        assert!(
            writable("/proj/node_modules/.store/sqlite3@5.1.14/node_modules/.bin/node-gyp"),
            "the store entry's own .bin is inside the grant (accepted)"
        );
        assert!(
            !writable("/proj/node_modules/.bin/tsc"),
            "the project's .bin is where unconfined tooling resolves and must stay read-only"
        );
        assert!(
            !writable("/proj/node_modules/.store/left-pad@1.0.0/node_modules/left-pad/index.js"),
            "a SIBLING store entry must stay outside the grant"
        );
    }

    /// The curated exception is WIRED and ORDERED, which the `curated` unit tests cannot
    /// show: they call the granting function against an empty policy, so they would pass
    /// just as well if `compile_build_jail` never called it, or called it before the
    /// dependency-tree READ that would then shadow the write under last-match-wins.
    ///
    /// Asserted as the WRITE decision specifically. `.prisma` sits inside the enclosing
    /// `node_modules` read grant, so an ordering bug leaves it readable-but-unwritable —
    /// which is exactly the `EPERM … mkdir` the exception exists to remove, and is
    /// invisible to a rule-presence check.
    ///
    /// HOISTED, where it used to be store-shaped: [`store_entry_write_root`] now makes the
    /// whole store entry writable, which SUBSUMES a `sibling_dirs` grant under the isolated
    /// linker and leaves both of this test's controls unable to fail. Under a hoisted
    /// linker the store grant declines, so the curated exception is again the only thing
    /// that can produce this write — which is what the test is for.
    #[test]
    fn a_curated_exception_is_wired_into_the_production_path_and_wins_the_write() {
        use crate::matcher::PathMatcher;
        use crate::policy::FsAccess;

        let homes = Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        };
        let package_dir = PathBuf::from("/proj/node_modules/@prisma/client");
        let sibling = Path::new("/proj/node_modules/.prisma");
        let compile = |name: Option<&str>| {
            compile_build_jail(
                homes.clone(),
                &package_dir,
                name,
                Vec::new(),
                Vec::new(),
                BTreeMap::new(),
            )
            .expect("build-jail compiles")
        };
        let writable = |policy: &SandboxPolicy, path: &Path| {
            let d = PathMatcher::new(&policy.fs.rules).decide(path);
            d.effect == Effect::Allow && d.access == FsAccess::ReadWrite
        };

        let named = compile(Some("@prisma/client"));
        assert!(
            writable(&named, sibling),
            "the curated sibling must be WRITABLE, not merely inside the dependency read"
        );
        // Two controls, because "writable" alone would also hold for a compiler that made
        // the whole enclosing node_modules writable — which is the `.bin`/virtual-store
        // hazard the enumerated form exists to avoid.
        assert!(
            !writable(&named, Path::new("/proj/node_modules/.bin/x")),
            "the exception must not widen the enclosing node_modules"
        );
        assert!(
            !writable(&compile(None), sibling),
            "an unnamed spawn (a fetched checkout) gets no exception"
        );
    }

    /// One build-jail compile against REAL directories, so `private_home_dir` materializes.
    /// `home` and `cache` are separate tempdirs so "the private home is writable" and "the
    /// user's home is not" are independent facts here rather than two readings of one path.
    struct HomeCase {
        user_home: tempfile::TempDir,
        cache: tempfile::TempDir,
        policy: SandboxPolicy,
    }

    fn home_case(package: &str) -> HomeCase {
        let user_home = tempfile::tempdir().expect("user home");
        let cache = tempfile::tempdir().expect("cache home");
        std::fs::create_dir_all(user_home.path().join(".ssh")).expect(".ssh");
        std::fs::write(user_home.path().join(".ssh/id_rsa"), b"k").expect("key");
        let project = user_home.path().join("proj");
        let package_dir = project.join("node_modules").join(package);
        std::fs::create_dir_all(&package_dir).expect("package dir");
        let policy = compile_build_jail(
            Homes {
                home: std::fs::canonicalize(user_home.path()).expect("canonical home"),
                tmp: PathBuf::from("/testtmp"),
                cache: std::fs::canonicalize(cache.path()).expect("canonical cache"),
                project,
            },
            &package_dir,
            None,
            Vec::new(),
            Vec::new(),
            [("HOME".to_string(), "/the/users/home".to_string())]
                .into_iter()
                .collect(),
        )
        .expect("build-jail compiles");
        HomeCase {
            user_home,
            cache,
            policy,
        }
    }

    /// The one directory under the jail-home root, which is the home the compile just made.
    fn only_jail_home(cache: &tempfile::TempDir) -> PathBuf {
        let mut homes: Vec<PathBuf> = std::fs::read_dir(cache.path().join("nub/jail-home"))
            .expect("the jail home root is materialized at compile time")
            .map(|e| e.expect("entry").path())
            .collect();
        assert_eq!(homes.len(), 1, "one home per package: {homes:?}");
        // Canonical, because that is the spelling the compiler grants and hands the child
        // (on macOS the tempdir root is reached through the `/var -> /private/var` link).
        std::fs::canonicalize(homes.pop().expect("one")).expect("canonical jail home")
    }

    /// The compat half: the script gets a REAL writable home AND is told about it. Neither
    /// half implies the other — a grant the child never spells is invisible, and a redirect
    /// with no grant is worse than nothing. The fixture's ambient `HOME` is the user's, so
    /// this also pins that the redirect lands after the env scrub that passes it through.
    #[test]
    fn the_build_jail_hands_the_script_its_own_writable_home() {
        let case = home_case("somepkg");
        let jail_home = only_jail_home(&case.cache);
        let matcher = crate::matcher::PathMatcher::new(&case.policy.fs.rules);
        for rel in [".npm/_npx", ".cache/Cypress", ".stc"] {
            let decision = matcher.decide(&jail_home.join(rel));
            assert_eq!(
                (decision.effect, decision.access),
                (Effect::Allow, FsAccess::ReadWrite),
                "{rel}: the corpus breakers' home-anchored dirs must be writable"
            );
        }
        assert_eq!(
            case.policy.env.constructed.get("HOME").map(String::as_str),
            Some(jail_home.to_string_lossy().as_ref()),
            "the child's HOME must name the granted dir, not the ambient one"
        );
    }

    /// Two packages never share a home. A shared one is a config root both can write —
    /// package A's `$HOME/.npmrc` steering package B's `node-gyp`/`script-shell` puts A's
    /// code inside B's jail, and B's compiled addon is `require()`d unconfined.
    #[test]
    fn each_package_gets_its_own_home() {
        let first = only_jail_home(&home_case("alpha").cache);
        let second = only_jail_home(&home_case("beta").cache);
        assert_ne!(
            first.file_name(),
            second.file_name(),
            "two packages must not resolve to the same home directory name"
        );
    }

    /// The escape the persistence choice opens if the leaf is trusted: a jailed script can
    /// unlink and recreate its OWN home root (a subpath grant matches its root), so it can
    /// leave a symlink to the user's home there. Resolving that would hand the NEXT compile
    /// a read-write grant on `~`. The leaf must be refused, not followed.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_home_root_is_refused_rather_than_resolved() {
        let case = home_case("somepkg");
        let jail_home = only_jail_home(&case.cache);
        let user_home = std::fs::canonicalize(case.user_home.path()).expect("canonical home");
        std::fs::remove_dir_all(&jail_home).expect("the script can remove its own home");
        std::os::unix::fs::symlink(&user_home, &jail_home).expect("and put a link there");

        let homes = Homes {
            home: user_home.clone(),
            tmp: PathBuf::from("/testtmp"),
            cache: std::fs::canonicalize(case.cache.path()).expect("canonical cache"),
            project: user_home.join("proj"),
        };
        let package_dir = homes.project.join("node_modules/somepkg");
        assert_eq!(
            private_home_dir(&homes, &package_dir),
            None,
            "a leaf that is not a real directory must yield no grant at all"
        );
        let policy = compile_build_jail(
            homes,
            &package_dir,
            None,
            Vec::new(),
            Vec::new(),
            [("HOME".to_string(), "/the/users/home".to_string())]
                .into_iter()
                .collect(),
        )
        .expect("build-jail still compiles");
        let matcher = crate::matcher::PathMatcher::new(&policy.fs.rules);
        assert_eq!(
            matcher.decide(&user_home.join(".ssh/id_rsa")).effect,
            Effect::Deny,
            "the swapped link must not have resolved into a grant on the real home"
        );
    }

    /// The security half, and the regression that would matter most: widening to a writable
    /// home must not have made the USER's home reachable. Nothing here is a deny — the real
    /// home is simply outside every grant, which is the only shape the allowlist backends
    /// can enforce.
    #[test]
    fn the_private_home_leaves_the_users_own_home_unreachable() {
        let case = home_case("somepkg");
        let (cache, policy) = (&case.cache, &case.policy);
        let user_home = std::fs::canonicalize(case.user_home.path()).expect("canonical home");
        let matcher = crate::matcher::PathMatcher::new(&policy.fs.rules);
        for rel in [".ssh/id_rsa", ".aws/credentials", ".npmrc"] {
            assert_eq!(
                matcher.decide(&user_home.join(rel)).effect,
                Effect::Deny,
                "{rel} in the user's real home must stay ungranted"
            );
        }
        assert_eq!(
            matcher.decide(&user_home).effect,
            Effect::Deny,
            "the user's home directory itself must stay ungranted"
        );
        // In production the jail home sits UNDER the real home (`~/.cache/nub/jail-home`),
        // so the grant must be a leaf: reachable itself, conferring nothing on its parents.
        for ancestor in only_jail_home(cache).ancestors().skip(1) {
            assert_eq!(
                matcher.decide(ancestor).effect,
                Effect::Deny,
                "{} must not become writable by way of the home grant",
                ancestor.display()
            );
        }
    }

    /// The strip is BUILD-JAIL ONLY. A general policy still carries the secret floor, and
    /// must — it has no narrow compiler-authored grant set to withhold secrets by shape, so
    /// its `.env` protection genuinely is the deny band. Guards against the `name` gate being
    /// dropped or `enforce_pure_allowlist` being hoisted into the shared compile path.
    #[test]
    fn a_general_policy_keeps_its_secret_floor() {
        let ctx = CompileCtx::new(
            Homes {
                home: PathBuf::from("/testhome"),
                tmp: PathBuf::from("/testtmp"),
                cache: PathBuf::from("/testhome/.cache"),
                project: PathBuf::from("/proj"),
            },
            PathBuf::from("/proj"),
            ScopeCapabilities::approved(),
            BTreeMap::new(),
        );
        for surface in [json!(true), json!({ "fs": ["./"] })] {
            let policy = compile(&surface, &ctx).expect("compiles");
            assert!(
                policy
                    .fs
                    .rules
                    .entries
                    .iter()
                    .any(|r| r.effect == Effect::Deny),
                "a general policy ({surface}) must keep its denies — only build-jail is stripped"
            );
            let m = crate::matcher::PathMatcher::new(&policy.fs.rules);
            assert_eq!(
                m.decide(Path::new("/proj/.env")).effect,
                Effect::Deny,
                "the `.env` floor must still hold for a general policy ({surface})"
            );
        }
    }

    /// The other half of the invariant: dropping the denies must not make anything readable.
    /// Each secret is now withheld by NOT BEING GRANTED, so the matcher must still refuse it
    /// — including the `/etc` password hashes, which lost their explicit deny when the
    /// Seatbelt base stopped granting `/etc` as a subpath.
    #[test]
    fn build_jail_withholds_every_secret_without_a_single_deny() {
        let policy = production_build_jail_policy();
        let m = crate::matcher::PathMatcher::new(&policy.fs.rules);
        for secret in [
            "/testhome/.ssh/id_rsa",
            "/testhome/.aws/credentials",
            "/testhome/.npmrc",
            "/testhome/.config/gh/hosts.yml",
            "/etc/shadow",
            "/etc/gshadow",
            "/proj/.env",
            "/proj/.env.local",
            "/proj/src/index.ts",
            "/proj/.git/config",
            "/proj/.github/workflows/release.yml",
            // The policy file that CONFIGURES this jail. `fold::finalize_policy_file_deny`
            // used to self-exclude it explicitly; the strip removes that, so the guarantee
            // now rests entirely on the project root being ungranted. Pinned here because a
            // future grant that reached the project root would silently reopen it.
            "/proj/nub.jsonc",
        ] {
            assert_eq!(
                m.decide(Path::new(secret)).effect,
                Effect::Deny,
                "{secret} must be unreachable by construction (ungranted), not by a deny rule"
            );
        }
    }

    /// The build jail's toolchain read was CARVED OUT of `$tooldirs`, so it must remain a
    /// subset of it. If the two anchors ever drift apart the narrowing stops being a
    /// narrowing and starts granting a directory the broad set never covered — silently,
    /// and ON ONE PLATFORM AT A TIME, which is the failure this exists to catch.
    ///
    /// `cache` is set per-platform to what the embedder actually resolves (`sandbox_homes`
    /// mirrors the engine's `cache_dir`, `%LOCALAPPDATA%` branch included). Hardcoding the
    /// POSIX spelling would make this pass on Windows while production aimed the grant at a
    /// directory node-gyp was never bootstrapped into.
    #[test]
    fn the_narrowed_toolchain_grant_stays_inside_tooldirs() {
        let homes = Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: if cfg!(windows) {
                PathBuf::from("/testhome/AppData/Local")
            } else {
                PathBuf::from("/testhome/.cache")
            },
            project: PathBuf::from("/proj"),
        };
        let tooldirs: Vec<String> = crate::compiler::builtin_sets::tooldir_patterns()
            .iter()
            .map(|p| crate::matcher::path::expand_symbolic(p, &homes))
            .collect();
        for pattern in NUB_PM_CACHE_PATTERNS {
            let grant = crate::matcher::path::expand_symbolic(pattern, &homes);
            let inside = tooldirs
                .iter()
                .any(|t| &grant == t || grant.starts_with(&format!("{t}/")));
            assert!(
                inside,
                "the build jail's {pattern} grant expanded to {grant}, which no \
                 $tooldirs pattern covers — the carve-out has drifted from the set it came from"
            );
        }
    }

    /// A git dependency's checkout carries a `.git/config` recording the fetch URL, which
    /// for a private HTTPS dep can embed a token — so `$cache/nub/pm/git` must stay outside
    /// the jail's read set while the two subtrees the jail genuinely needs stay inside it.
    /// The positive half is what makes this non-vacuous: an assertion that `git` is denied
    /// would pass just as well if the whole PM-cache grant were dropped, which breaks every
    /// native build and every global-virtual-store resolution.
    #[test]
    fn the_pm_cache_grant_reaches_the_store_and_toolchain_but_not_git_checkouts() {
        let policy = production_build_jail_policy();
        let m = crate::matcher::PathMatcher::new(&policy.fs.rules);
        let cache = Path::new("/testhome/.cache/nub/pm");
        for granted in [
            cache.join("store/left-pad@1.3.0-abc/node_modules/left-pad/index.js"),
            cache.join("tools/node-gyp/lazy-bin/node-gyp"),
        ] {
            assert_eq!(
                m.decide(&granted).effect,
                Effect::Allow,
                "{} is the store/toolchain content every confined build resolves through",
                granted.display()
            );
        }
        assert_eq!(
            m.decide(&cache.join("git/aube-git-deadbeef/.git/config"))
                .effect,
            Effect::Deny,
            "a git dependency's clone config records its fetch URL — a private HTTPS dep's \
             token — and must not be reachable from another package's lifecycle script"
        );
    }
}
