//! nub's dependency-lifecycle build-jail — the embedder side of the aube
//! `EngineContext::lifecycle_sandbox` interposition.
//!
//! aube's own build jail is neutralized under the NUB profile
//! (`embedder_owns_lifecycle_sandbox = true`); this module supplies the replacement.
//! When a dependency build/postinstall script runs, aube hands the fully-configured
//! spawn to [`NubBuildJail::run`], which compiles nub-sandbox's tight build-jail
//! policy for that package and launches the script confined:
//!
//! - WRITE confined to a private per-run tmp + the script's own package dir.
//! - READ confined to the consumer's DEPENDENCY TREE and top-level manifest, nub's own
//!   PM cache (where it bootstraps node-gyp), and the provisioned interpreter (the OS
//!   backends supply the system/toolchain closure under a minimal root). The consumer's
//!   source, config, `.git/`, and `.github/` are outside it.
//! - egress curated to the install-time artifact hosts (`$downloads`) and denied
//!   everywhere else; the home-secret + `.env*` floors applied; `/etc/shadow` denied.
//! - the constructed lifecycle env minus credential-shaped keys.
//!
//! The user's OWN root-package scripts are NOT routed here — aube passes them no
//! sandbox scope, so `run_script` never reaches this hook for them. A git dependency's
//! root scripts ARE: its `prepare` runs through a nested install whose root is the
//! fetched checkout, which aube marks `RootProvenance::Fetched` and confines here with
//! BOTH anchors on that checkout. The project anchor matters as much as the write one:
//! the read grants are anchored on it, and a checkout's own `workspaces` globs choose
//! the importer directory, so anchoring reads there would let the fetched tree grant
//! itself a read on a sibling of its scratch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nub_sandbox::RuntimeCapability;

/// The installed hook. Holds the process-lifetime sandbox runtime capability (Linux
/// needs the sealed bwrap authority from `earliest_bootstrap`; other OSes a unit).
#[derive(Debug)]
struct NubBuildJail {
    runtime: &'static RuntimeCapability,
}

/// Install nub's build-jail as the engine's lifecycle-spawn confiner. Called once at
/// startup with the process-lifetime runtime capability. Idempotent-safe to call
/// once; a second install would replace the hook (only the first is expected).
pub(crate) fn install(runtime: &'static RuntimeCapability) {
    let hook: Arc<dyn aube_util::LifecycleSandbox> = Arc::new(NubBuildJail { runtime });
    aube_util::update_engine_context(|c| c.lifecycle_sandbox = Some(hook));
}

impl aube_util::LifecycleSandbox for NubBuildJail {
    fn run(
        &self,
        spawn: aube_util::LifecycleSandboxSpawn,
    ) -> std::io::Result<std::process::ExitStatus> {
        // Reconstruct the effective child env the UNCONFINED spawn would have had: the
        // aube-process env (inherited — the non-jailed lifecycle command never clears
        // it) with the command's explicit operations layered on. Non-UTF-8 entries are
        // dropped (nub-sandbox's env IR is `String`-keyed/valued), matching nub's other
        // ambient-env capture; a build script never needs a non-UTF-8 var.
        let mut ambient = reconstruct_child_env(&spawn.env_delta);

        // The interpreter closure to grant READ. nub provisions its own Node under its
        // store (not `/usr`), so the tight-read base can't reach it. Under nub a bare
        // `node` resolves via the PATH-prepended shim (`NODE`) which re-execs the real
        // binary (`npm_node_execpath`), so BOTH must be readable/executable — grant each
        // (compile_build_jail dedups and adds each one's bin dir).
        let interpreter: Vec<PathBuf> = ["npm_node_execpath", "NODE"]
            .iter()
            .filter_map(|k| ambient.get(*k))
            .map(PathBuf::from)
            .collect();

        // Make node-gyp compile offline. It reads Node headers from `npm_config_nodedir/
        // include/node` (default devdir `~/.cache/node-gyp/<ver>`, unreadable → network
        // fallback the jail denies). Point nodedir at the provisioned Node root and grant
        // that root's toolchain subtrees (the store path is outside `$tooldirs` + the
        // interpreter grant). Set-if-absent: an explicit ambient nodedir is a deliberate
        // build-against-custom-node choice; the case we fix (nub's own Node) carries none.
        let mut extra_reads = Vec::new();
        // npm's builtin `lib/node_modules/npm/npmrc` (no leading dot) sits inside the
        // `lib/node_modules` grant below; the Linux deny-search walk must be SEEDED there
        // (or at `npm/` itself) rather than at an ancestor, because it skips descending
        // into any directory literally named `node_modules` for cost
        // (`DENY_WALK_SKIP_DIRS` in the Linux backend) — a skip that only blocks descent
        // INTO such a child, not enumeration of a root that already IS one. Recorded
        // separately from `extra_reads` (which stays read-only plumbing) and only added
        // when the dir actually exists, since `deny_search_roots` is strict — an absent
        // root is a hard compile error, unlike the read grants above, which are best-effort
        // `Speculative`.
        let mut npm_builtin_config_deny_root = None;
        if let Some((nodedir, reads)) = node_toolchain_grant(&ambient) {
            ambient
                .entry("npm_config_nodedir".to_string())
                .or_insert(nodedir);
            if let Some(lib_node_modules) = reads.get(1) {
                npm_builtin_config_deny_root = npm_builtin_config_deny_root_for(lib_node_modules);
            }
            extra_reads.extend(reads);
        }

        let homes = sandbox_homes(&spawn.project_root);
        let policy = nub_sandbox::compile_build_jail(
            homes,
            &spawn.package_dir,
            interpreter,
            extra_reads,
            ambient,
        )
        .map_err(|e| {
            std::io::Error::other(format!("compiling build-jail for lifecycle script: {e}"))
        })?;

        let mut spec = nub_sandbox::CommandSpec::new(&spawn.program)
            .args(&spawn.args)
            .cwd(&spawn.cwd);
        // The `.env*` deny floor is a bounded glob, so the backend needs the dirs whose
        // immediate children it may materialize to enforce it. The PACKAGE DIR is the
        // primary such root: it is the one place the jail both reads and writes. The
        // project root is deliberately NOT passed — the read set no longer reaches it, so
        // walking it would build masks for files the script cannot open, and each mask
        // makes bwrap materialize its parent directories inside the jail, disclosing the
        // shape of the consumer's tree along exactly the paths that hold secrets. For a
        // fetched git dependency the two are the same directory anyway. npm's own
        // `node_modules/npm` dir (above) is added on the same basis: it is exactly the
        // read-granted subtree the floor must reach, no wider.
        if nub_sandbox::requires_deny_search_roots(&policy) {
            let mut roots = vec![spawn.package_dir.clone()];
            roots.extend(npm_builtin_config_deny_root);
            spec = spec.deny_search_roots(roots);
        }

        let prepared =
            nub_sandbox::apply_with_runtime(&policy, spec, self.runtime).map_err(|d| {
                let detail = d
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("could not enforce {}", d.lost.join(", ")));
                std::io::Error::other(format!(
                    "build-jail could not be applied (fail-closed): {detail}"
                ))
            })?;
        if let Some(warning) = prepared.degradation.warning() {
            eprintln!("warning: {warning}");
        }
        // `status()` spawns, waits, and (Linux) reaps the whole process tree via the
        // retained monitor on drop — descendant reaping without aube's job object.
        prepared.status()
    }
}

/// The effective child env: the current (aube) process env with the command's explicit
/// operations applied (`Some` = set/override, `None` = removed). Non-UTF-8 keys/values
/// are skipped.
fn reconstruct_child_env(
    delta: &[(std::ffi::OsString, Option<std::ffi::OsString>)],
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    for (key, value) in delta {
        let Ok(key) = key.clone().into_string() else {
            continue;
        };
        match value {
            Some(value) => {
                if let Ok(value) = value.clone().into_string() {
                    env.insert(key, value);
                }
            }
            None => {
                env.remove(&key);
            }
        }
    }
    env
}

/// The per-OS home anchors for the build-jail compile, with the project anchored at
/// the install's project root. Mirrors `cli::sandbox_homes`, differing only in the
/// project field.
fn sandbox_homes(project_root: &std::path::Path) -> nub_sandbox::Homes {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| project_root.to_path_buf());
    // Resolve the cache home the way the ENGINE does (`aube_store::dirs::cache_dir`),
    // %LOCALAPPDATA% branch included. The jail grants nub's own node-gyp through a
    // `$cache`-anchored pattern, so a divergence here aims that grant at a directory the
    // engine never bootstrapped into — on Windows that silently removes the only node-gyp
    // a confined native build can reach, since the interposition no longer falls back to
    // an ambient one.
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            cfg!(windows)
                .then(|| std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from))
                .flatten()
        })
        .unwrap_or_else(|| home.join(".cache"));
    nub_sandbox::Homes {
        home,
        tmp: std::env::temp_dir(),
        cache,
        project: project_root.to_path_buf(),
    }
}

/// The Node-toolchain additions derived from the effective child env: the
/// `npm_config_nodedir` value to inject (the Node root — `bin/node`'s grandparent) and
/// the read subtrees under it. `None` only when `npm_node_execpath` is absent or has
/// fewer than two parents; the `<root>/bin/node` shape is ASSUMED, not checked, so a
/// Windows layout (`<root>/node.exe`) derives one level too high and yields paths that
/// do not exist. That is inert rather than wrong — the grants are `Speculative`, so an
/// absent path is skipped — but it is why this must not be used to derive anything that
/// has to be correct. Pure over its input, so the derivation is unit-testable without a
/// Node on disk.
///
/// Two subtrees, NOT the whole root. `lib/node_modules` is what makes `<root>/bin/npm`,
/// `npx` and `corepack` resolvable at all: each is a symlink into it, so with only the
/// bin dir granted all three are DANGLING inside the jail and the standard
/// `prebuild-install || npm run build` fallback dies at `npm: not found` (measured on
/// `keytar`: rc 127 → rc 0 once the target is readable). Granting the ROOT instead would
/// be simpler but is unbounded — `npm_node_execpath` is the user's Node, which on a
/// Homebrew or `/usr/local` install makes the root a shared system prefix carrying
/// unrelated `etc/`/`var/` content.
///
/// Scope of what this opens: Node's own toolchain plus any globally installed package's
/// SOURCE (`npm -g` lands in `lib/node_modules`) — third-party code, not user data, and
/// less sensitive than the `~/.npm/_cacache` tarballs `$tooldirs` already grants. The
/// `.env*`/`.npmrc` deny floor is re-asserted after these grants and stays authoritative,
/// including npm's own undotted `lib/node_modules/npm/npmrc` (matched by its own
/// `ENV_DENY_LEAF_GLOBS` band, not the `.npmrc` glob) — the caller additionally passes
/// `reads[1]`'s `npm/` subdir as a Linux deny-search root so the floor's recursive mask
/// walk actually reaches it (see the call site).
fn node_toolchain_grant(ambient: &BTreeMap<String, String>) -> Option<(String, Vec<PathBuf>)> {
    let root = ambient
        .get("npm_node_execpath")
        .and_then(|exec| Path::new(exec).parent()?.parent().map(Path::to_path_buf))?;
    let nodedir = root.to_string_lossy().into_owned();
    let reads = vec![
        root.join("include").join("node"),
        root.join("lib").join("node_modules"),
    ];
    Some((nodedir, reads))
}

/// Whether `lib_node_modules` (the second [`node_toolchain_grant`] read, always
/// `<node-root>/lib/node_modules`) holds npm's own `npm/` package dir — and if so, its
/// path, to pass as the extra Linux `deny_search_roots` entry so the recursive mask walk
/// reaches `npm/npmrc` instead of stopping at the `node_modules`-named ancestor (see the
/// call site doc). Checked against the real filesystem (unlike the `Speculative` read
/// grants above): `deny_search_roots` is strict, so an absent root would be a hard
/// compile error rather than a silently-skipped grant.
fn npm_builtin_config_deny_root_for(lib_node_modules: &Path) -> Option<PathBuf> {
    let npm_dir = lib_node_modules.join("npm");
    npm_dir.is_dir().then_some(npm_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_toolchain_grant_derives_nodedir_headers_and_lib_node_modules() {
        let ambient: BTreeMap<String, String> = [(
            "npm_node_execpath".to_string(),
            "/home/u/.cache/nub/node/v22.14.0/bin/node".to_string(),
        )]
        .into_iter()
        .collect();
        let (nodedir, reads) = node_toolchain_grant(&ambient).expect("derives a grant");
        assert_eq!(nodedir, "/home/u/.cache/nub/node/v22.14.0");
        assert_eq!(
            reads,
            vec![
                PathBuf::from("/home/u/.cache/nub/node/v22.14.0/include/node"),
                PathBuf::from("/home/u/.cache/nub/node/v22.14.0/lib/node_modules"),
            ]
        );
    }

    /// The grant stays SCOPED to toolchain subtrees. Granting the derived root itself
    /// would hand a dependency build script the whole prefix — for a `/usr/local/bin/node`
    /// or Homebrew Node that is a shared system prefix, not nub's own store.
    #[test]
    fn node_toolchain_grant_never_grants_the_bare_root() {
        let ambient: BTreeMap<String, String> = [(
            "npm_node_execpath".to_string(),
            "/usr/local/bin/node".to_string(),
        )]
        .into_iter()
        .collect();
        let (_, reads) = node_toolchain_grant(&ambient).expect("derives a grant");
        assert!(
            !reads.contains(&PathBuf::from("/usr/local")),
            "the shared prefix itself must never be a read grant: {reads:?}"
        );
    }

    #[test]
    fn node_toolchain_grant_absent_without_execpath() {
        let ambient: BTreeMap<String, String> = [("PATH".to_string(), "/usr/bin".to_string())]
            .into_iter()
            .collect();
        assert!(node_toolchain_grant(&ambient).is_none());
    }

    #[test]
    fn npm_builtin_config_deny_root_present_when_npm_dir_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let lib_node_modules = tmp.path().join("lib/node_modules");
        std::fs::create_dir_all(lib_node_modules.join("npm")).unwrap();
        assert_eq!(
            npm_builtin_config_deny_root_for(&lib_node_modules),
            Some(lib_node_modules.join("npm"))
        );
    }

    /// Absence must be tolerated, not just "usually present": a from-source Node build,
    /// or the Windows-layout mis-derivation `node_toolchain_grant`'s own doc calls out,
    /// can hand this a `lib_node_modules` that doesn't exist. `deny_search_roots` is
    /// strict (an absent root is a hard error), so this must return `None`, never a
    /// dangling path.
    #[test]
    fn npm_builtin_config_deny_root_absent_when_npm_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let lib_node_modules = tmp.path().join("lib/node_modules");
        std::fs::create_dir_all(&lib_node_modules).unwrap();
        assert_eq!(npm_builtin_config_deny_root_for(&lib_node_modules), None);

        let nonexistent = tmp.path().join("nowhere/lib/node_modules");
        assert_eq!(npm_builtin_config_deny_root_for(&nonexistent), None);
    }
}
