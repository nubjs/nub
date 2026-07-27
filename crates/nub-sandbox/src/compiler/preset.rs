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
/// build-jail's `"./"` project read re-allows every path under the project —
/// `<proj>/.env` included, and the home-secret set if a grant overlaps it — because
/// it is a later matching entry. Re-appending the built-in secret denies makes them
/// authoritative again. Under the tight (default-deny) read set these are mostly
/// belt-and-suspenders — an ungranted secret path is unreadable by construction — but
/// they still close an overlap between a `$tooldirs` grant and an adjacent secret
/// (e.g. `~/.npmrc` beside `~/.npm/_cacache`). Uses [`defaults::secret_read_denies`]
/// directly so the floor is byte-consistent across the policy.
pub fn reassert_secret_floor(name: &str, policy: &mut SandboxPolicy, ctx: &CompileCtx) {
    if name == "build-jail" {
        policy
            .fs
            .rules
            .entries
            .extend(defaults::secret_read_denies(&ctx.homes));
    }
}

/// Grant the build-jail interpreter closure (the provisioned Node + the PATH-prepended
/// shim) READ. nub provisions its own Node under its store rather than `/usr`, so the
/// tight-read base (Linux `RootView::Minimal` auto-mounting `ESSENTIAL_READ_DIRS`,
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
            push_read_path(&mut grants, interpreter);
        }
        if let Some(bin_dir) = interpreter.parent()
            && seen.insert(bin_dir.to_path_buf())
        {
            push_read_path(&mut grants, bin_dir);
        }
    }
    policy.fs.rules.entries.splice(0..0, grants);
}

/// Grant the build-jail's per-spawn extra READ subtrees that the interpreter grant misses.
/// Today this is the provisioned Node's C/C++ header dir (`<node-root>/include/node`):
/// node-gyp compiles an addon against the headers under `npm_config_nodedir/include/node`,
/// and nub provisions Node under its version store (`~/.cache/nub/node/<ver>`) — a path in
/// neither `$tooldirs` nor the interpreter grant (which covers only `bin/node` + the bin
/// dir). Without this grant node-gyp finds no local headers and falls back to a network
/// header download the egress-denied jail blocks, so the entire native-compile ecosystem
/// fails offline. The embedder supplies the concrete paths — it owns where nub puts Node.
/// A nonexistent path (a system Node shipping no headers) yields an inert allow.
/// Front-inserted as base allows so the reasserted secret/`.env` floor stays authoritative;
/// these paths never overlap a secret.
fn grant_build_jail_extra_reads(policy: &mut SandboxPolicy, extra_reads: &[PathBuf]) {
    let mut grants = Vec::new();
    for dir in extra_reads {
        push_read_path(&mut grants, dir);
    }
    policy.fs.rules.entries.splice(0..0, grants);
}

/// Push a READ-allow rule per subtree glob for `path` (the node itself + `/**`).
fn push_read_path(out: &mut Vec<FsRule>, path: &Path) {
    for g in defaults::subtree_globs(&path.to_string_lossy()) {
        out.push(FsRule {
            matcher: CanonGlob(canonicalize_glob_prefix(&g)),
            effect: Effect::Allow,
            access: FsAccess::Read,
            origin: FsOrigin::Authored,
        });
    }
}

/// The build-jail baseline surface. Tight, default-deny read (project + `$tooldirs`
/// plus the toolchain closure the OS backends supply under a minimal root) with WRITE
/// confined to a private per-run tmp and — via [`compile_build_jail`] — the script's
/// own package dir. Egress denied. `/etc` is granted read-only by the Linux minimal
/// root (it is in `ESSENTIAL_READ_DIRS`); `/etc/shadow` + `/etc/gshadow` are denied
/// within it (grant-directory-then-deny) so a lifecycle script can read the benign
/// `/etc` files it may legitimately need (`resolv.conf`, `localtime`, `ssl/`) without
/// the two password-hash files ever being readable.
///
/// `package_dir` is the per-spawn WRITE grant, inserted AFTER `"./"` so its
/// read-write access wins over the project's read-only grant for the package subtree
/// (last-match-wins, preserved by the `preserve_order` serde_json map). `None` yields
/// the static `--sandbox build-jail` skeleton (project read only; the per-package
/// write is a production-interposition concern). The env axis is the strip-all floor
/// here; [`compile_build_jail`] replaces it with the scrubbed lifecycle env.
fn build_jail_surface(package_dir: Option<&Path>) -> Value {
    let mut fs = serde_json::Map::new();
    // `$tmp` sets the private per-run tmp MODE (TmpMode::Private) — a writable scratch,
    // shared host tmp hidden. It emits no ordinary fs rule.
    fs.insert("$tmp".to_string(), json!("rw"));
    // The store + PM/toolchain caches the script resolves deps and tools from.
    fs.insert("$tooldirs".to_string(), json!("r"));
    // Project READ (source, sibling `node_modules/.bin`, config the build legitimately
    // reads) — NOT write. Authored before the package-dir grant so the latter wins.
    fs.insert("./".to_string(), json!("r"));
    if let Some(dir) = package_dir {
        // Own-package-dir READ-WRITE: the one subtree a dep build may write (its
        // `build/`, the compiled `.node`). Keyed after `"./"` so its rw beats the
        // project's read-only under last-match-wins.
        fs.insert(dir.to_string_lossy().into_owned(), json!("rw"));
    }
    // D6: `/etc` is granted read-only by the minimal root; deny the two password-hash
    // files within so a whole-`/etc` read never exposes them.
    fs.insert("/etc/shadow".to_string(), json!(false));
    fs.insert("/etc/gshadow".to_string(), json!(false));
    json!({
        "fs": Value::Object(fs),
        // Deny all egress (prefetch / curated-egress is deferred).
        "net": false,
        // Strip-all here; the interposition supplies the scrubbed lifecycle env.
        "vars": []
    })
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
/// compiles offline) — see [`grant_build_jail_extra_reads`].
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
    grant_build_jail_interpreter("build-jail", &mut policy, &ctx);
    grant_build_jail_extra_reads(&mut policy, &extra_reads);
    reassert_secret_floor("build-jail", &mut policy, &ctx);
    policy.env = defaults::lifecycle_scrubbed_env(&ambient_env);
    Ok(policy)
}
