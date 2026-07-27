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
        // immediate children it may materialize to enforce it. The PACKAGE DIR is the only
        // such root now: it is the one place the jail both reads and writes. The project
        // root is deliberately NOT passed — the read set no longer reaches it, so walking
        // it would build masks for files the script cannot open, and each mask makes bwrap
        // materialize its parent directories inside the jail, disclosing the shape of the
        // consumer's tree along exactly the paths that hold secrets. For a fetched git
        // dependency the two are the same directory anyway.
        if nub_sandbox::requires_deny_search_roots(&policy) {
            spec = spec.deny_search_roots([spawn.package_dir.clone()]);
        }

        let prepared =
            nub_sandbox::apply_with_runtime(&policy, spec, self.runtime).map_err(|d| {
                let detail = d
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("could not enforce {}", d.lost.join(", ")));
                std::io::Error::other(refusal(&detail))
            })?;
        if let Some(warning) = prepared.degradation.warning() {
            eprintln!("warning: {warning}");
        }
        // `status()` spawns, waits, and (Linux) reaps the whole process tree via the
        // retained monitor on drop — descendant reaping without aube's job object.
        prepared.status()
    }
}

/// The message a user sees when an install refuses because the build jail cannot be applied.
///
/// THE REFUSAL IS THE PRODUCT HERE. The build jail is opted into per project, so a refusal only
/// ever reaches someone whose repository asked for it — which makes fail-closed correct, and
/// makes the message the entire remaining surface. A raw Bubblewrap error at this point is a
/// design failure: the reader has to learn, without leaving the terminal, that the requirement
/// comes from the project rather than from nub, what their own machine is missing, and the one
/// command that fixes it.
///
/// The cause comes from [`nub_sandbox::preflight::diagnose`], which asks the host directly. The
/// launcher's own `detail` is kept as the last line rather than being the whole message: it is
/// the ground truth when the preflight has no opinion, and the thing to paste into a bug report
/// when it does.
fn refusal(detail: &str) -> String {
    let mut out = headline();
    match nub_sandbox::preflight::diagnose() {
        Some(missing) => {
            out.push_str(&remedy(&missing));
            out.push_str("\nThen run `nub install` again.\n");
            // The launcher writes its own remedy prose for the same conditions, so appending
            // its whole reason printed the fix twice — once structured, once as a paragraph.
            // Keep only its candidate ledger, which is the part the remedy above does not
            // carry and the part a bug report needs.
            out.push_str(&format!("\n{}\n", evidence(detail)));
        }
        // No prerequisite is missing, so this is not a machine-setup problem and offering a
        // setup command would send the reader somewhere that cannot help. The launcher's
        // reason is the only real information available, so it is printed whole.
        None => {
            out.push_str("  The sandbox could not be applied on this host.\n");
            out.push_str(&format!("\n{detail}\n"));
        }
    }
    out
}

/// The evidence tail of a launcher reason: the per-candidate ledger it parenthesizes, without
/// the remedy paragraph that precedes it. Falls back to the whole reason when there is no such
/// tail, so a message shape this does not recognize is passed through rather than truncated.
fn evidence(detail: &str) -> String {
    for marker in ["(underlying: ", "("] {
        if let Some(start) = detail.find(marker)
            && detail.ends_with(')')
        {
            return detail[start + marker.len()..detail.len() - 1].to_string();
        }
    }
    detail.to_string()
}

/// The first line, which has to be TRUE about where the requirement came from.
///
/// A reader whose install just refused needs to know whether their own repository asked for
/// this or whether nub decided it — those lead to completely different next actions (talk to
/// the team vs. file a bug). So the project is named only when `nub.jsonc` actually carries the
/// opt-in; otherwise the line says nothing about a project that did not ask.
///
/// The config is READ here, not consumed: `install.sandbox` remains inert as a policy input,
/// and this is attribution for a message, not a gate. When the opt-in becomes the thing that
/// arms the jail, this function is already asking the right question.
fn headline() -> String {
    let opted_in = crate::project_config::effective_config()
        .and_then(|config| config.values.install.sandbox.as_ref())
        .is_some();
    if opted_in {
        return String::from(
            "nub install: this project requires the build sandbox (nub.jsonc → install.sandbox)\n\n",
        );
    }
    String::from(
        "nub install: the build sandbox could not confine a dependency's install script\n\n",
    )
}

/// The per-cause remedy block. Each cause gets the command that actually fixes IT — a package
/// install, a one-time host setup, or a fresh login — because the three are not
/// interchangeable and offering the wrong one costs the reader a round trip.
fn remedy(missing: &nub_sandbox::preflight::Missing) -> String {
    use nub_sandbox::preflight::Missing;
    match missing {
        Missing::Bubblewrap => format!(
            "  Missing: bubblewrap\n\n{}\n",
            bubblewrap_install_hint(host_distro())
        ),
        // PLACEHOLDER REMEDY, pending the apt-route investigation: whether Ubuntu 24.04 can be
        // satisfied by a package alone is still open, so this points at nub's own setup, which
        // is known to work. If an apt-only route lands, it replaces this arm and nothing else.
        Missing::NamespacePermission => format!(
            "  Missing: permission to create user namespaces\n\n  This kernel restricts \
             unprivileged user namespaces. Nub grants that one capability to its own bundled \
             bubblewrap, and to nothing else:\n\n    {}\n",
            nub_sandbox::preflight::LINUX_SETUP_COMMAND
        ),
        Missing::SessionGroup => format!(
            "  Missing: the {} group in this shell\n\n  The host is set up. This shell's group \
             set was fixed when it started, so it does not carry the group and neither will \
             anything it launches. Start a fresh login, or run the install through:\n\n    sg {} \
             -c 'nub install'\n",
            nub_sandbox::preflight::LINUX_HELPER_GROUP,
            nub_sandbox::preflight::LINUX_HELPER_GROUP
        ),
        Missing::SeatbeltUnavailable => String::from(
            "  Missing: /usr/bin/sandbox-exec\n\n  Nub confines a build script through the stock \
             macOS Seatbelt entry point, which is missing or not executable here. No setup \
             command installs it — restore it from a stock macOS system volume.\n",
        ),
    }
}

/// The distro family, for the package line. `ID_LIKE` is checked after `ID` so a derivative
/// (Linux Mint, Pop!_OS, Manjaro) gets its parent's package manager rather than falling through
/// to the generic list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Distro {
    Debian,
    Fedora,
    Arch,
    Suse,
    Alpine,
    Unknown,
}

/// Read the host's distro identity. Not Linux-gated: the file simply does not exist elsewhere,
/// which lands on `Unknown` — and a `cfg` here would make the classifier dead code on macOS and
/// leave the one platform that needs it the only one that compiles it.
fn host_distro() -> Distro {
    std::fs::read_to_string("/etc/os-release")
        .map(|release| classify_distro(&release))
        .unwrap_or(Distro::Unknown)
}

fn classify_distro(os_release: &str) -> Distro {
    let field = |key: &str| -> String {
        os_release
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(|value| value.trim_matches('"').to_ascii_lowercase())
            .unwrap_or_default()
    };
    // ID is one token; ID_LIKE is a space-separated list, so both are matched by word.
    let words: Vec<String> = format!("{} {}", field("ID="), field("ID_LIKE="))
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let has = |name: &str| words.iter().any(|word| word == name);
    if has("debian") || has("ubuntu") {
        return Distro::Debian;
    }
    if has("fedora") || has("rhel") || has("centos") {
        return Distro::Fedora;
    }
    if has("arch") {
        return Distro::Arch;
    }
    if has("suse") || has("opensuse") {
        return Distro::Suse;
    }
    if has("alpine") {
        return Distro::Alpine;
    }
    Distro::Unknown
}

/// One line when the distro is known, the full table only when it is not. Printing three
/// package managers to a reader who is demonstrably on one of them is noise they have to filter
/// before they can act.
fn bubblewrap_install_hint(distro: Distro) -> String {
    let one = |command: &str| format!("    {command}");
    match distro {
        Distro::Debian => one("sudo apt install bubblewrap"),
        Distro::Fedora => one("sudo dnf install bubblewrap"),
        Distro::Arch => one("sudo pacman -S bubblewrap"),
        Distro::Suse => one("sudo zypper install bubblewrap"),
        Distro::Alpine => one("sudo apk add bubblewrap"),
        Distro::Unknown => String::from(
            "    Debian/Ubuntu   sudo apt install bubblewrap\n\
             \x20   Fedora/RHEL     sudo dnf install bubblewrap\n\
             \x20   Arch            sudo pacman -S bubblewrap\n\
             \x20   openSUSE        sudo zypper install bubblewrap\n\
             \x20   Alpine          sudo apk add bubblewrap",
        ),
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
/// `.env*`/`.npmrc` deny floor is re-asserted after these grants and stays authoritative.
/// KNOWN GAP: npm's builtin config file is `lib/node_modules/npm/npmrc` with no leading
/// dot, so the `.npmrc` band does not match it; it is benign by default but can carry a
/// registry token on a managed install.
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

    #[test]
    fn a_derivative_distro_gets_its_parent_package_manager() {
        // ID_LIKE is the whole reason this is not a lookup on ID: Mint, Pop!_OS and Manjaro
        // ship their own ID and would otherwise fall through to the five-line generic table.
        let cases = [
            ("ID=ubuntu\nID_LIKE=debian\n", Distro::Debian),
            ("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n", Distro::Debian),
            ("ID=manjaro\nID_LIKE=arch\n", Distro::Arch),
            ("ID=fedora\n", Distro::Fedora),
            ("ID=\"rhel\"\nID_LIKE=\"fedora\"\n", Distro::Fedora),
            ("ID=alpine\n", Distro::Alpine),
            ("ID=plan9\n", Distro::Unknown),
        ];
        for (release, expected) in cases {
            assert_eq!(classify_distro(release), expected, "{release:?}");
        }
    }

    #[test]
    fn a_known_distro_gets_exactly_one_install_line() {
        // Printing five package managers to someone demonstrably on one of them is noise they
        // have to filter before they can act, so the table is the UNKNOWN fallback only.
        let debian = bubblewrap_install_hint(Distro::Debian);
        assert_eq!(debian.lines().count(), 1, "{debian}");
        assert!(debian.contains("apt install bubblewrap"), "{debian}");
        assert!(
            bubblewrap_install_hint(Distro::Unknown).lines().count() > 1,
            "an unidentified host still needs the full table"
        );
    }

    #[test]
    fn each_cause_offers_only_the_remedy_that_fixes_it() {
        use nub_sandbox::preflight::Missing;

        // The three Linux causes are NOT interchangeable, and offering the wrong command is a
        // wasted round trip: no package install grants a namespace, no host setup reaches a
        // shell whose group set is already fixed.
        let package = remedy(&Missing::Bubblewrap);
        assert!(package.contains("bubblewrap"), "{package}");
        assert!(
            !package.contains(nub_sandbox::preflight::LINUX_SETUP_COMMAND),
            "an absent bubblewrap is not fixed by the host setup: {package}"
        );

        let namespace = remedy(&Missing::NamespacePermission);
        assert!(
            namespace.contains(nub_sandbox::preflight::LINUX_SETUP_COMMAND),
            "{namespace}"
        );

        let session = remedy(&Missing::SessionGroup);
        assert!(session.contains("sg "), "{session}");
        assert!(
            !session.contains(nub_sandbox::preflight::LINUX_SETUP_COMMAND),
            "re-running setup cannot change a live shell's group set: {session}"
        );
    }

    #[test]
    fn the_refusal_keeps_the_launchers_own_reason() {
        // The preflight names a cause and a remedy; the launcher's raw reason is what goes in a
        // bug report when the two disagree, so it must survive rather than being replaced.
        let message = refusal("bwrap: setting up uid map: Permission denied");
        assert!(
            message.contains("bwrap: setting up uid map: Permission denied"),
            "{message}"
        );
        assert!(message.starts_with("nub install:"), "{message}");
    }

    #[test]
    fn a_structured_remedy_does_not_repeat_the_launchers_own_prose() {
        // The launcher writes a remedy paragraph for the same conditions the preflight names,
        // so printing its whole reason showed the fix twice. Only its candidate ledger should
        // survive alongside a structured remedy.
        let reason = "the sandbox needs a one-time setup on this system. Run:\n\n    sudo nub \
                      setup-sandbox\n\n(underlying: /usr/bin/bwrap: candidate probe failed)";
        assert_eq!(
            evidence(reason),
            "/usr/bin/bwrap: candidate probe failed",
            "only the ledger should survive"
        );
        // A shape with no parenthesized tail is passed through rather than truncated.
        assert_eq!(evidence("no candidates found"), "no candidates found");
    }

    #[test]
    fn the_headline_blames_the_project_only_when_the_project_opted_in() {
        // No snapshot is initialized in a unit test, so nothing opted in — and the line must
        // NOT claim a `nub.jsonc` requirement that the reader's repository never wrote.
        let line = headline();
        assert!(
            !line.contains("nub.jsonc"),
            "an un-opted-in project must not be told it requires the sandbox: {line}"
        );
    }
}
