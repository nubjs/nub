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
//! - READ confined to the project, `$tooldirs`, and the provisioned interpreter (the
//!   OS backends supply the system/toolchain closure under a minimal root).
//! - egress denied; the home-secret + `.env*` floors applied; `/etc/shadow` denied.
//! - the constructed lifecycle env minus credential-shaped keys.
//!
//! The user's OWN root-package scripts are NOT routed here — aube passes them no
//! package dir, so `run_script` never reaches this hook for them. A git dependency's
//! root scripts ARE: its `prepare` runs through a nested install whose root is the
//! fetched checkout, which aube marks `RootProvenance::Fetched` and confines here
//! keyed on that checkout directory.

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
        if let Some((nodedir, reads)) = node_toolchain_grant(&ambient) {
            ambient
                .entry("npm_config_nodedir".to_string())
                .or_insert(nodedir);
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
        // immediate children it may materialize to enforce it. The build-jail reads the
        // project + writes the package dir, so those are the roots a `.env` could sit in.
        if nub_sandbox::requires_deny_search_roots(&policy) {
            spec = spec.deny_search_roots([spawn.project_root.clone(), spawn.package_dir.clone()]);
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
/// the install's project root (what `./` expands against). Mirrors
/// `cli::sandbox_homes`, differing only in the project field.
fn sandbox_homes(project_root: &std::path::Path) -> nub_sandbox::Homes {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| project_root.to_path_buf());
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    nub_sandbox::Homes {
        home,
        tmp: std::env::temp_dir(),
        cache,
        project: project_root.to_path_buf(),
    }
}

/// The Node-toolchain additions derived from the effective child env: the
/// `npm_config_nodedir` value to inject (the provisioned Node root — `bin/node`'s
/// grandparent) and the read subtrees under it. `None` when there is no
/// `npm_node_execpath` or its path has no `<root>/bin/node` shape, so a caller with no
/// resolvable Node adds nothing. Pure over its input so the derivation is unit-testable
/// without a provisioned Node on disk.
///
/// Two subtrees, NOT the whole root. `include/node` carries the C/C++ headers node-gyp
/// compiles against. `lib/node_modules` is what makes `<root>/bin/npm`, `npx` and
/// `corepack` resolvable at all: each is a symlink into it, so with only the bin dir
/// granted all three are DANGLING inside the jail and the standard
/// `prebuild-install || npm run build` fallback dies at `npm: not found` (measured on
/// `keytar`: rc 127 → rc 0 once the target is readable). Granting the ROOT instead would
/// be simpler but is unbounded — `npm_node_execpath` is the user's Node, which on a
/// Homebrew or `/usr/local` install makes the root a shared system prefix carrying
/// unrelated `etc/`/`var/` content. These two subtrees are toolchain-only under every
/// Node layout.
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
}
