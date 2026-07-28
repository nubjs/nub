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
        "build-jail" => Ok(build_jail_surface(None)),
        other => Err(CompileError::unknown_preset(other, &["build-jail"])),
    }
}

/// Re-assert a preset's built-in secret floor AFTER its surface object has folded,
/// closing the last-match-wins hole a broad subtree grant opens.
///
/// build-jail's dependency-tree read re-allows every path under `<proj>/node_modules` —
/// a vendored `.env` or `.npmrc` included — because it is a later matching entry.
/// Re-appending the built-in secret denies makes them authoritative again. Under the
/// tight (default-deny) read set these are mostly belt-and-suspenders — an ungranted
/// secret path is unreadable by construction — but they still close an overlap between
/// a granted cache root and an adjacent secret (e.g. `~/.npmrc` beside `~/.npm/_cacache`).
/// Uses [`defaults::secret_read_denies`] directly so the floor is byte-consistent across
/// the policy.
pub fn reassert_secret_floor(name: &str, policy: &mut SandboxPolicy, ctx: &CompileCtx) {
    if name != "build-jail" {
        return;
    }
    let denies = defaults::secret_read_denies(&ctx.homes);
    let entries = &mut policy.fs.rules.entries;
    // ESTABLISH the floor if the fold skipped it. `fold::finalize_env_deny` appends it only
    // when the FOLDED surface already granted a read, but build-jail's reads are all
    // post-fold now (they need SPECULATIVE origin, which the surface cannot express), so the
    // static `--sandbox build-jail` skeleton folds to denies-only, the fold declines, and the
    // grants then arrive with nothing trailing them. Re-establishing here — where every
    // build-jail path converges — is what keeps "grants a read" and "carries the secret
    // floor" from diverging. A `None` here means genuinely ABSENT rather than displaced:
    // every build-jail post-fold grant splices at the FRONT, so nothing appends after the
    // fold and position still implies presence.
    if defaults::env_deny_floor_start(entries).is_none() {
        entries.extend(defaults::env_deny_leaf_rules());
        entries.extend(defaults::env_deny_subtree_rules());
    }
    // Same gate, same fix, second finalizer: `fold::finalize_policy_file_deny` also
    // declines on a denies-only fold, which would leave the very `nub.jsonc` that
    // configures this jail outside its own self-exclusion. Not reachable today — the
    // config sits at the project root, outside every post-fold grant — but "any read grant
    // implies the policy file is denied" must not be conditional on which finalizer ran.
    for policy_file in &ctx.policy_files {
        let rule = defaults::policy_file_deny_rule(policy_file);
        if !entries.contains(&rule) {
            entries.push(rule);
        }
    }
    // Splice in BEFORE the trailing `.env*`/`.npmrc` floor rather than appending: appending
    // would leave the floor no longer last, and it is located POSITIONALLY
    // (`defaults::env_deny_floor_start`, which the Linux backend calls to tell an explicit
    // user deny from the builtin floor). These home-secret denies still land after every
    // band-1 allow, which is all their re-assertion needs; ordering among denies does not
    // affect any verdict.
    let at = defaults::env_deny_floor_start(entries).unwrap_or(entries.len());
    entries.splice(at..at, denies);
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
/// The Node pair is its C/C++ header dir
/// (`<node-root>/include/node`) and `<node-root>/lib/node_modules`. node-gyp compiles an
/// addon against the headers under `npm_config_nodedir/include/node`, and `npm`/`npx`/
/// `corepack` are each a symlink into `lib/node_modules`, so without it they dangle and
/// the standard `prebuild-install || npm run build` fallback dies at `npm: not found`.
/// nub provisions Node under its version store (`~/.cache/nub/node/<ver>`) — a path in
/// neither `$tooldirs` nor the interpreter grant (which covers only `bin/node` + the bin
/// dir). Without these grants node-gyp finds no local headers and falls back to a network
/// header download — reachable now that `nodejs.org` is allowed, but it re-fetches the
/// headers on a cold cache for every native build the jail runs, and on an offline or
/// air-gapped host the whole native-compile ecosystem fails outright. The grant is what
/// keeps the offline path working. The embedder supplies the concrete paths — it owns where nub puts Node,
/// and keeps the grant on SUBTREES rather than the bare root, which for a system Node is
/// a shared prefix. A nonexistent path (a system Node shipping no headers) yields an
/// inert allow.
/// Front-inserted as base allows so the reasserted secret/`.env` floor stays authoritative;
/// these paths never overlap a secret.
fn grant_build_jail_extra_reads(policy: &mut SandboxPolicy, extra_reads: &[PathBuf]) {
    let mut grants = Vec::new();
    for dir in extra_reads {
        push_read_path(&mut grants, dir, FsOrigin::Speculative);
    }
    policy.fs.rules.entries.splice(0..0, grants);
}

/// nub's own PM cache root, as a `$tooldirs`-style surface pattern. Held here rather
/// than inlined so the narrowed build-jail grant and the broad `$tooldirs` set resolve
/// to the same directory on every platform — `builtin_sets` carries the same anchor for
/// its nub entry, and `the_narrowed_toolchain_grant_stays_inside_tooldirs` pins that they
/// stay in agreement.
const NUB_PM_CACHE_PATTERN: &str = "$cache/nub/pm";

/// Grant the build jail's narrowed READ set: the consumer's DEPENDENCY TREE, the
/// consumer's top-level MANIFEST, and nub's own PM cache. Together these replace what
/// were once two much broader grants — `"./"` (the entire consuming project) and
/// `$tooldirs` (16 ecosystem cache patterns).
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
    let mut roots = vec![
        ctx.homes.project.join("package.json"),
        ctx.homes.project.join("node_modules"),
        PathBuf::from(crate::matcher::path::expand_symbolic(
            NUB_PM_CACHE_PATTERN,
            &ctx.homes,
        )),
    ];
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
/// private per-run tmp and, via [`compile_build_jail`], the script's own package dir.
/// Egress curated down to the install-time artifact hosts (see [`build_jail_net`]).
///
/// `package_dir` is the per-spawn WRITE grant. It stays LAST so its read-write access
/// wins over the front-inserted dependency-tree read for the package subtree
/// (last-match-wins, preserved by the `preserve_order` serde_json map). `None` yields
/// the static `--sandbox build-jail` skeleton (the per-package write is a production-
/// interposition concern). The env axis is the strip-all floor here;
/// [`compile_build_jail`] replaces it with the scrubbed lifecycle env.
fn build_jail_surface(package_dir: Option<&Path>) -> Value {
    let mut fs = serde_json::Map::new();
    // `$tmp` sets the private per-run tmp MODE (TmpMode::Private) — a writable scratch,
    // shared host tmp hidden. It emits no ordinary fs rule.
    fs.insert("$tmp".to_string(), json!("rw"));
    if let Some(dir) = package_dir {
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
    // D6, and now a CROSS-PLATFORM floor rather than a Linux carve-out: the Linux minimal
    // root no longer mounts `/etc` wholesale, but macOS's Seatbelt base still grants
    // `/etc` + `/private/etc` as a subpath, so these denies are what keeps the two
    // password-hash files unreadable there.
    fs.insert("/etc/shadow".to_string(), json!(false));
    fs.insert("/etc/gshadow".to_string(), json!(false));
    json!({
        "fs": Value::Object(fs),
        "net": build_jail_net(),
        // Strip-all here; the interposition supplies the scrubbed lifecycle env.
        "vars": []
    })
}

/// The build-jail's net axis: the curated install-time artifact hosts (`$downloads`),
/// everything else denied. A lifecycle script that legitimately fetches its own binary —
/// Node headers for a native compile, the Prisma engines, the Cypress binary — reaches
/// exactly those hosts and nothing more; the set is wildcard-free and carries no host that
/// accepts a write, so an attacker-authored postinstall gains no way to send bytes out.
///
/// WINDOWS keeps the deny-all. Its backend refuses a per-host policy outright
/// (`WinNetPlan::PerHostUnsupported`) because the available AppContainer exemption exposes
/// every loopback listener, so a local forwarder could bypass the hostname gate — and an
/// unappliable jail fails the install rather than degrading. Deny-all is the STRICTER
/// posture, so the divergence loses a capability, never enforcement.
fn build_jail_net() -> Value {
    #[cfg(not(windows))]
    {
        json!(["$downloads"])
    }
    #[cfg(windows)]
    {
        json!(false)
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
/// subtrees the embedder derives (the provisioned Node's `include/node` headers so node-gyp
/// compiles offline, and its `lib/node_modules` so `npm`/`npx` resolve) — see
/// [`grant_build_jail_extra_reads`].
pub fn compile_build_jail(
    homes: Homes,
    package_dir: &Path,
    interpreter: Vec<PathBuf>,
    extra_reads: Vec<PathBuf>,
    ambient_env: BTreeMap<String, String>,
) -> Result<SandboxPolicy, CompileError> {
    let surface = build_jail_surface(Some(package_dir));
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
    reassert_secret_floor("build-jail", &mut policy, &ctx);
    policy.env = defaults::lifecycle_scrubbed_env(&ambient_env);
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

    /// The secret-file floor must remain the LAST fs entries after the preset re-asserts
    /// its home-secret denies. The Linux backend reads that boundary positionally to decide
    /// whether a denied dotenv file is masked unreadable or present-but-empty, so a floor
    /// displaced by the re-assert reads as absent and silently downgrades an explicit deny.
    #[test]
    fn build_jail_secret_reassert_keeps_the_env_floor_trailing() {
        let policy = build_jail_policy();
        let entries = &policy.fs.rules.entries;
        let floor_len =
            defaults::ENV_DENY_LEAF_GLOBS.len() + defaults::ENV_DENY_SUBTREE_GLOBS.len();
        assert_eq!(
            defaults::env_deny_floor_start(entries),
            Some(entries.len() - floor_len),
            "the build-jail preset must leave the {floor_len} secret-file floor entries last; \
             found trailing entries {:?}",
            entries
                .iter()
                .rev()
                .take(8)
                .map(|r| r.matcher.as_str())
                .collect::<Vec<_>>()
        );
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
        let grant = crate::matcher::path::expand_symbolic(NUB_PM_CACHE_PATTERN, &homes);
        let inside = crate::compiler::builtin_sets::tooldir_patterns()
            .iter()
            .map(|p| crate::matcher::path::expand_symbolic(p, &homes))
            .any(|t| grant == t || grant.starts_with(&format!("{t}/")));
        assert!(
            inside,
            "the build jail's {NUB_PM_CACHE_PATTERN} grant expanded to {grant}, which no \
             $tooldirs pattern covers — the carve-out has drifted from the set it came from"
        );
    }

    /// Guards the other half: the re-assert must still HAPPEN. Without it a home secret
    /// overlapping a `$tooldirs` grant would stay readable, and the test above would pass
    /// vacuously if the re-assert were simply deleted.
    #[test]
    fn build_jail_reasserts_the_home_secret_denies() {
        let policy = build_jail_policy();
        let ssh_denies = policy
            .fs
            .rules
            .entries
            .iter()
            .filter(|r| r.effect == Effect::Deny && r.matcher.as_str().contains("/.ssh"))
            .count();
        assert!(
            ssh_denies > 0,
            "build-jail must re-assert the home-secret read denies"
        );
    }
}
