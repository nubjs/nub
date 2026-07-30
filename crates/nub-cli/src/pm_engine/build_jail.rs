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
//! - egress gated on PACKAGE IDENTITY: a package the build-jail catalog names may reach the
//!   network; a package it does not name reaches NOTHING. The grant is COARSE on every platform
//!   — no host filtering — because only macOS could ever enforce a host list, and being stricter
//!   there than on Linux or Windows meant an incomplete list erroring for the platform most
//!   developers use (see `nub_sandbox::compiler::preset::build_jail_net`). The DENIAL is the
//!   point — the attack shape is a new `postinstall` published into a package that never had
//!   one, so an unvetted package gets no egress at all and one that needs it arrives through a
//!   catalog PR first.
//!   The fs axis carries NO deny rules at all —
//!   the jail compiles to a pure allowlist (`preset::enforce_pure_allowlist`), so every secret
//!   is withheld by not being granted rather than by a deny the allowlist backends cannot
//!   express.
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
//!
//! The one per-package opt-out is [`opted_out`] — `dependenciesMeta.<name>.sandbox:
//! false` in the CONSUMER's root manifest, read nowhere else.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nub_sandbox::RuntimeCapability;

/// The installed hook. Holds the process-lifetime sandbox runtime capability (Linux
/// needs the sealed bwrap authority from `earliest_bootstrap`; other OSes a unit).
#[derive(Debug)]
struct NubBuildJail {
    runtime: &'static RuntimeCapability,
    /// Roots+packages already announced as running unconfined. aube consults the gate
    /// once per lifecycle PHASE, so a package with `preinstall` + `install` +
    /// `postinstall` would otherwise print the notice three times. Keyed on the root too
    /// so a second install in the same process cannot silence its own notice.
    announced: Mutex<std::collections::BTreeSet<(PathBuf, String)>>,
}

/// Install nub's build-jail as the engine's lifecycle-spawn confiner. Called once at
/// startup with the process-lifetime runtime capability. Idempotent-safe to call
/// once; a second install would replace the hook (only the first is expected).
pub(crate) fn install(runtime: &'static RuntimeCapability) {
    let hook: Arc<dyn aube_util::LifecycleSandbox> = Arc::new(NubBuildJail {
        runtime,
        announced: Mutex::default(),
    });
    aube_util::update_engine_context(|c| c.lifecycle_sandbox = Some(hook));
}

impl aube_util::LifecycleSandbox for NubBuildJail {
    /// The per-package opt-out gate. `false` sends the script back to aube's ordinary
    /// unconfined spawn.
    fn confines(&self, package_name: Option<&str>, project_root: &Path) -> bool {
        if should_confine(package_name, project_root) {
            return true;
        }
        let name = package_name.unwrap_or_default();
        // Unconfined is an auditable decision, never a silent default-path difference:
        // announce it once per package so the reason is visible in the install output,
        // pointing at the line in the user's own manifest that caused it.
        if self
            .announced
            .lock()
            .map(|mut seen| seen.insert((project_root.to_path_buf(), name.to_string())))
            .unwrap_or(true)
        {
            super::present::warn(&format!(
                "warning: {name} build scripts are running without the build sandbox \
                 (dependenciesMeta.{name}.sandbox is false in package.json)"
            ));
        }
        false
    }

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

        // A dependency's lifecycle script runs on VANILLA Node — nub's augmentation is a
        // developer-facing feature for the user's own code, and a published postinstall
        // neither asked for it nor can rely on it. Unconditional, not set-if-absent: this
        // is the jail's contract, not a default an ambient value may relax.
        //
        // It is also what makes the mechanisms agree. Under bubblewrap the preload was
        // never loaded — nub's runtime dir is outside the child's mount view, so discovery
        // found nothing and silently ran unaugmented. Landlock has no mount namespace, so
        // that same dir is VISIBLE but ungranted (Landlock denies with EACCES, never
        // ENOENT): discovery SUCCEEDED, nub injected `--import …/preload.mjs`, and Node
        // died on the unreadable file. Stating the intent here fixes it at the source
        // instead of widening the allowlist to grant nub's own runtime into untrusted code.
        ambient.insert("NODE_COMPAT".to_string(), "1".to_string());

        // NOTHING IS STAMPED FOR THE WINDOWS REALPATH DEFECT, deliberately. Under the
        // AppContainer every absolute `require()` dies on `EPERM: lstat 'C:\'`, and the one
        // NODE_OPTIONS repair that clears it (`--preserve-symlinks`) is REFUSED here: under
        // nub's DEFAULT isolated layout it silently resolves a dependency to the WRONG version
        // rather than failing. Measured, not reasoned — the evidence and the surviving options
        // live on `nub_sandbox::windows_realpath_node_options`. A confined Windows lifecycle
        // script therefore still fails, but it fails LOUDLY, which is the only property worth
        // holding until a real fix lands.

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
        // fallback the jail denies). Point nodedir at a directory that ACTUALLY HOLDS
        // them and grant the toolchain subtrees (the store path is outside `$tooldirs` +
        // the interpreter grant). Set-if-absent: an explicit ambient nodedir is a
        // deliberate build-against-custom-node choice; the case we fix carries none.
        let probe = ProbeScope::new(&spawn);
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
        if let Some(layout) = ambient
            .get("npm_node_execpath")
            .and_then(|exec| node_layout(Path::new(exec)))
        {
            npm_builtin_config_deny_root = npm_builtin_config_deny_root_for(&layout.global_modules);
            extra_reads.push(layout.headers.clone());
            extra_reads.push(layout.global_modules.clone());
            if !ambient.contains_key("npm_config_nodedir") {
                // WHERE THE HEADERS COME FROM is a property of the distribution, asked of
                // the disk rather than of the platform. A POSIX distribution ships them in
                // its own root, so nodedir names that root and nothing is fetched. The
                // Windows distribution ships none — and its jail is net deny-all, so a
                // confined node-gyp can neither find nor fetch them — so there they are
                // prefetched OUT of jail and nodedir names the prefetched tree, which is
                // granted whole (nub-owned, headers and `node.lib` only).
                let nodedir = if layout.headers.is_dir() {
                    Some(layout.root.clone())
                } else {
                    super::build_prefetch::node_headers(&ambient, &probe)
                        .inspect(|dir| extra_reads.push(dir.clone()))
                };
                if let Some(nodedir) = nodedir {
                    ambient.insert(
                        "npm_config_nodedir".to_string(),
                        nodedir.to_string_lossy().into_owned(),
                    );
                }
            }
        }

        // Same shape for node-gyp's OTHER out-of-jail toolchain dependency, Python. See
        // `python_toolchain_grant` for why this pre-resolves rather than pins a fixed
        // interpreter.
        if let Some(python) = python_toolchain_grant(&ambient, &spawn) {
            // `npm_config_python` is the only spelling that needs setting: it is what
            // node-gyp reads as `--python`, and the one key that outranks it
            // (`NODE_GYP_FORCE_PYTHON`) is not on the lifecycle env allowlist, so it never
            // reaches the child at all — it is honoured here only as a resolution input.
            ambient.insert("npm_config_python".to_string(), python.executable);
            extra_reads.extend(python.reads);
        }

        // PREFETCH — the same move `npm_config_nodedir` makes above, applied to the
        // package's own prebuilt binary: resolve the artifact out here, land it on the
        // path the installer checks before it opens a socket, and the confined script
        // completes without the net axis granting its host at all. Runs LAST of the
        // pre-resolution steps because it is the only one that may need the env the
        // others populated. Infallible by contract — it either improves the spawn or
        // leaves it byte-identical.
        extra_reads.extend(super::build_prefetch::prefetch(
            &spawn,
            &mut ambient,
            &probe,
        ));

        let homes = sandbox_homes(&spawn.project_root);
        // The name is aube's `registry_name()` — the SAME identity `opted_out` keys the
        // per-package opt-out on, and for the same reason: a dependency cannot rename
        // itself into it, and aube withholds it entirely once its root is a checkout it
        // fetched. It selects a curated exception only; it can never widen the baseline.
        let policy = nub_sandbox::compile_build_jail(
            homes,
            &spawn.package_dir,
            spawn.package_name.as_deref(),
            interpreter,
            extra_reads,
            ambient,
        )
        .map_err(|e| {
            std::io::Error::other(format!("compiling build-jail for lifecycle script: {e}"))
        })?;

        // The tail crosses TWO re-encodings on Windows (aube's builder → this spec →
        // the backend's own `CreateProcessW`), and `cmd.exe` survives neither: it does
        // not implement the `CommandLineToArgvW` rules, so a re-quoted line reaches it
        // as `\""` and no dependency lifecycle script starts. Carrying aube's
        // already-encoded line through as-is is the whole point of both enums.
        let mut spec = match &spawn.args {
            aube_util::LifecycleSpawnArgs::Argv(args) => {
                nub_sandbox::CommandSpec::new(&spawn.program).args(args)
            }
            aube_util::LifecycleSpawnArgs::WindowsVerbatim(line) => {
                nub_sandbox::CommandSpec::new(&spawn.program).verbatim_command_line(line)
            }
        }
        .cwd(&spawn.cwd)
        // Third-party build tooling detaches by design — `node-gyp` backgrounds `make`
        // — so the shell's pid is no handle on what the script leaves running. Ask for
        // a process group, which is. Honored by the macOS backend; the other platforms
        // already reap through a mechanism of their own (see `CommandSpec`).
        .reap_descendants(true);
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
        // INERT TODAY, deliberately kept. The jail compiles to a pure allowlist, so
        // `requires_deny_search_roots` is always false here and neither this block nor
        // `npm_builtin_config_deny_root` above runs. Both are retained rather than deleted
        // because the guard is a correct, self-reactivating safety net: if a deny ever
        // legitimately returns to a build-jail policy, the Linux mask walk needs these roots
        // or it would silently under-enforce. Do not read the code above as live.
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
                std::io::Error::other(refusal(&detail))
            })?;
        if let Some(warning) = prepared.degradation.warning() {
            eprintln!("warning: {warning}");
        }
        // The launch handle reaps the script's descendants on return and on drop (which
        // mechanism, per platform: `nub_sandbox::CommandSpec::reap_descendants`). What it
        // cannot cover is a `SIGINT`/`SIGTERM` whose default action kills nub, since that
        // runs no `Drop` at all — and where the launch created a process group, that group
        // is a BACKGROUND one, so a terminal Ctrl-C no longer reaches the script by
        // membership either. Enrolling the group in aube's reaper closes both: its handler
        // sweeps every live group, then re-raises so nub still dies with the right status.
        #[cfg(unix)]
        {
            // Declared BEFORE `child` so it drops AFTER it — locals drop in reverse
            // declaration order, and the group must be killed before its registry slot is
            // cleared. Clearing first leaves a signal in between skipping the group.
            let _enrolled;
            // Armed before the spawn: the child leaves the foreground group the instant it
            // starts, and until the handler exists a Ctrl-C reaches neither the script nor
            // anything that would reap it.
            aube_scripts::unix_group::arm_group_reaper();
            let mut child = prepared.spawn()?;
            // `None` unless the kernel confirmed the child leads its own group — the same
            // fail-open the Windows job object takes when the OS refuses it.
            _enrolled = child
                .process_group_id()
                .and_then(aube_scripts::unix_group::register_embedder_group);
            child.wait()
        }
        // Windows owns spawn+wait inside its launch plan and refuses the asynchronous
        // `spawn` seam, so the uniform `status()` verb stays the entry point off unix.
        #[cfg(not(unix))]
        {
            prepared.status()
        }
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

/// Whether this script stays confined. The whole decision, split out from the hook so
/// the two gates that make the opt-out safe are testable without an install: aube must
/// have handed over a package identity at all (`None` = its root is a checkout it
/// fetched, not the consumer's project), and `project_root` must independently look like
/// the user's own project.
fn should_confine(package_name: Option<&str>, project_root: &Path) -> bool {
    should_confine_from(package_name, project_root, std::env::current_dir().ok())
}

/// [`should_confine`] with the process cwd injected, so both gates are testable without
/// mutating a global.
///
/// The cwd gate is the BACKSTOP behind aube's `package_name` gate. aube already withholds
/// the name unless its root is the user's project, so this is redundant TODAY and
/// deliberately kept anyway — it is the check that survives a future aube caller pointing
/// `project_dir` somewhere else, which is exactly how the hole this feature first shipped
/// with was opened (`run_git_dep_prepare` roots a nested install at the fetched clone
/// dir). Exact rather than heuristic: nub's cwd is where the user invoked the install,
/// aube's project root is at or above it, and the nested git-prepare install runs in this
/// same process without chdir — so a clone dir under nub's store never contains the cwd.
fn should_confine_from(
    package_name: Option<&str>,
    project_root: &Path,
    cwd: Option<PathBuf>,
) -> bool {
    let Some(name) = package_name else {
        return true;
    };
    let users_own = cwd.is_some_and(|cwd| canonical(&cwd).starts_with(canonical(project_root)));
    !users_own || !opted_out(project_root, name)
}

/// Whether the CONSUMER opted `name` out of the build jail — `dependenciesMeta.<name>.
/// sandbox: false` in `project_root`'s own `package.json`.
///
/// SECURITY INVARIANT, the reason this reads exactly one file: a DEPENDENCY-authored
/// `dependenciesMeta` is IGNORED, by every route — not the building package's own
/// manifest, not a workspace member's, not a `file:`/injected dep's, and not a fetched
/// git checkout's, which aube keeps out by withholding `package_name` entirely once its
/// root is one (`SandboxScope::package_name`). A package that could switch off its own
/// confinement would be strictly worse than no jail at all, because it advertises a
/// protection that silently is not there. `name` is the installer-resolved identity for
/// the same reason, so a dependency cannot rename itself into an opt-out the consumer
/// wrote for some other package.
///
/// PRECEDENCE: this is the LAST word — a repo-wide opt-in to confinement does not seal
/// the jail shut, because the narrower per-package statement has the same author.
///
/// Only an explicit `false` opts out, and every read/parse failure confines.
///
/// `name` is aube's `registry_name()` (`alias_of` before `name`), which is what
/// `dependenciesMeta.<name>.built` in the SAME object already matches on — so the two
/// directives in one `dependenciesMeta` entry cannot mean different packages. Two
/// consequences follow, both inherited rather than introduced: an `npm:` alias does not
/// borrow an opt-out written for the aliased-as spelling (it fails closed, the direction
/// `default_trust` chose for the same reason), and an opt-out covers that registry name
/// wherever it appears in the tree, not just as a direct dependency. It does NOT carry
/// `jail_for`'s source/git keys, so it cannot distinguish a registry `esbuild` from a
/// `file:`- or git-backed package resolving under that name.
fn opted_out(project_root: &Path, name: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(project_root.join("package.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    manifest
        .get("dependenciesMeta")
        .and_then(|meta| meta.get(name))
        .and_then(|entry| entry.get("sandbox"))
        .and_then(serde_json::Value::as_bool)
        == Some(false)
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

/// A Node distribution's on-disk shape, derived from the interpreter path alone.
///
/// THE FILE NAME IS THE DISCRIMINATOR, and there are exactly two shapes: every POSIX
/// distribution is `<root>/bin/node` with the global package tree under `<root>/lib`,
/// while the Windows zip and MSI are FLAT — `<root>\node.exe` with `node_modules` beside
/// it. nub's own store extracts to whichever the platform ships
/// (`nub_core::node::discovery::store_node_binary`). Taking the grandparent
/// unconditionally, as this did before, lands a Windows install on `C:\Program Files`:
/// one level above the real root, so every path built from it is wrong. That was not
/// merely inert. `npm_config_nodedir` is derived from it, node-gyp SKIPS its header
/// download whenever nodedir is set (`configure.js`, `getNodeDir`), and the Windows build
/// jail is net deny-all — so the wrong root did not degrade to a fetch, it produced a
/// compile against a directory that does not exist.
///
/// Pure over its input, so the derivation is unit-testable without a Node on disk.
struct NodeLayout {
    /// The distribution root — what `npm_config_nodedir` names when the distribution
    /// ships its own headers.
    root: PathBuf,
    /// Where headers live IF this distribution ships them. POSIX does. The Windows
    /// distribution ships NONE (verified against `node-v22.20.0-win-x64.zip`: zero
    /// `include/`, `.h` or `.lib` entries), so there this names a path that never exists
    /// and the headers have to come from somewhere else — see the call site.
    headers: PathBuf,
    /// The globally-installed package tree.
    ///
    /// Granted as a subtree, NOT the whole root. It is what makes `npm`, `npx` and
    /// `corepack` resolvable at all: each entry beside the interpreter is a symlink (or,
    /// on Windows, a `.cmd` shim) into it, so with only the bin dir granted all three are
    /// DANGLING inside the jail and the standard `prebuild-install || npm run build`
    /// fallback dies at `npm: not found` (measured on `keytar`: rc 127 → rc 0 once the
    /// target is readable). Granting the ROOT instead would be simpler but is unbounded —
    /// `npm_node_execpath` is the user's Node, which on a Homebrew or `/usr/local`
    /// install makes the root a shared system prefix carrying unrelated `etc/`/`var/`
    /// content.
    ///
    /// Scope of what this opens: any globally installed package's SOURCE (`npm -g` lands
    /// here) — third-party code, not user data, and less sensitive than the
    /// `~/.npm/_cacache` tarballs `$tooldirs` already grants. The `.env*`/`.npmrc` deny
    /// floor is re-asserted after these grants and stays authoritative, including npm's
    /// own undotted `node_modules/npm/npmrc` (matched by its own `ENV_DENY_LEAF_GLOBS`
    /// band, not the `.npmrc` glob) — the caller additionally passes this dir's `npm/`
    /// subdir as a Linux deny-search root so the floor's recursive mask walk actually
    /// reaches it (see the call site).
    global_modules: PathBuf,
}

fn node_layout(exec: &Path) -> Option<NodeLayout> {
    let dir = exec.parent()?;
    let flat = exec
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("node.exe"));
    let (root, global_modules) = if flat {
        (dir.to_path_buf(), dir.join("node_modules"))
    } else {
        let root = dir.parent()?.to_path_buf();
        let global_modules = root.join("lib").join("node_modules");
        (root, global_modules)
    };
    Some(NodeLayout {
        headers: root.join("include").join("node"),
        root,
        global_modules,
    })
}

/// Whether `global_modules` ([`NodeLayout::global_modules`]) holds npm's own `npm/`
/// package dir — and if so, its path, to pass as the extra Linux `deny_search_roots` entry
/// so the recursive mask walk reaches `npm/npmrc` instead of stopping at the
/// `node_modules`-named ancestor (see the call site doc). Checked against the real
/// filesystem (unlike the `Speculative` read grants above): `deny_search_roots` is strict,
/// so an absent root would be a hard compile error rather than a silently-skipped grant.
fn npm_builtin_config_deny_root_for(global_modules: &Path) -> Option<PathBuf> {
    let npm_dir = global_modules.join("npm");
    npm_dir.is_dir().then_some(npm_dir)
}

/// node-gyp's one Python key that outranks `--python` / `npm_config_python`.
const NODE_GYP_FORCE_PYTHON: &str = "NODE_GYP_FORCE_PYTHON";

/// What [`python_toolchain_grant`] resolved: the interpreter to name in the child env,
/// and the read subtrees that make it runnable inside the jail.
struct PythonToolchain {
    executable: String,
    reads: Vec<PathBuf>,
}

/// Asks the interpreter where it actually lives. `sys.prefix` differs from `sys.base_prefix`
/// only inside a virtualenv, where BOTH are load-bearing (the venv holds `pyvenv.cfg` and
/// `site-packages`, the base holds the stdlib and the shared library).
///
/// The trailing lines are every shared object the interpreter ACTUALLY loaded, asked of
/// the loader rather than inferred from the install layout. A Python is not
/// self-contained: a pyenv-built one links Homebrew's `libintl` from outside every prefix
/// it reports, so a prefix-only grant leaves it unrunnable — the exact half-grant that
/// reasoning about layouts produces.
///
/// On macOS each image contributes TWO spellings: the path the loader resolved, and the
/// image's own `LC_ID_DYLIB` install name — which is the spelling dyld looks up, and for
/// Homebrew is an `opt/<formula>` alias whose link hop needs its own grant. The resolved
/// path alone is not enough (measured: `libintl.8.dylib` granted at its Cellar path,
/// still `blocked by sandbox` through the alias). Linux has no such indirection, so
/// `/proc/self/maps` is the whole answer there.
///
/// The contract is the FIRST FOUR lines and nothing else: they are flushed before the
/// introspection runs, and the caller gates on parsing rather than exit status, so an
/// interpreter whose closure is already reachable is never rejected because this block
/// was unavailable or died.
///
/// Run under `-I`, which is load-bearing for SAFETY, not tidiness. `python -c` puts the
/// CWD on `sys.path`, and the cwd here is the package being built — so a dependency
/// shipping `ctypes.py` beside its manifest would have its code imported by THIS probe,
/// which runs unconfined in nub's own process (reproduced, then blocked by `-I`). `-I`
/// also drops `PYTHONPATH`/`PYTHONHOME`; on every interpreter checked that leaves the four
/// reported values identical, and a `PYTHONHOME` fidelity gap is the right trade for
/// closing an arbitrary-code path.
const PYTHON_PROBE: &str = "import sys\n\
     print(sys.executable or '')\n\
     print(sys.prefix)\n\
     print(sys.base_prefix)\n\
     print(sys.version_info[0], sys.version_info[1])\n\
     sys.stdout.flush()\n\
     try:\n\
     \x20   import ctypes\n\
     \x20   libc = ctypes.CDLL(None)\n\
     \x20   if sys.platform == 'darwin':\n\
     \x20       libc._dyld_get_image_name.restype = ctypes.c_char_p\n\
     \x20       libc._dyld_get_image_header.restype = ctypes.c_void_p\n\
     \x20       u32 = lambda a: ctypes.c_uint32.from_address(a).value\n\
     \x20       for i in range(libc._dyld_image_count()):\n\
     \x20           print(libc._dyld_get_image_name(i).decode())\n\
     \x20           h = libc._dyld_get_image_header(i)\n\
     \x20           if not h: continue\n\
     \x20           o = 32\n\
     \x20           for _ in range(u32(h + 16)):\n\
     \x20               size = u32(h + o + 4)\n\
     \x20               if size == 0: break\n\
     \x20               if u32(h + o) == 0xd:\n\
     \x20                   print(ctypes.string_at(h + o + u32(h + o + 8)).decode())\n\
     \x20               o += size\n\
     \x20   else:\n\
     \x20       for line in open('/proc/self/maps'):\n\
     \x20           p = line.rstrip().split(maxsplit=5)[-1]\n\
     \x20           if p.startswith('/'):\n\
     \x20               print(p)\n\
     except Exception:\n\
     \x20   pass";

/// Resolve the Python node-gyp would pick, and derive the read closure that lets it RUN
/// inside the jail.
///
/// PRE-RESOLVE, DO NOT PIN. Pinning a known-granted interpreter (`/usr/bin/python3`) would
/// silently compile the user's addon with a different Python than npm/pnpm uses — a
/// guardrail, not a fix. This instead reruns node-gyp's OWN search (its key order, its
/// `>=3.6` floor, `posix_spawnp`'s first-hit PATH rule) against the effective child env and
/// the spawn's cwd, then names the winner in `npm_config_python`. node-gyp resolves any
/// candidate to that same interpreter, so only the SPELLING changes — which is what bounds
/// the grant: otherwise it would have to cover a shim's whole re-exec chain (the shim, its
/// bash, the version manager's libexec and version store) instead of one installation.
///
/// GRANTS READ ONLY — never write. The interpreter tree is user-managed and shared across
/// builds; a confined script able to modify it would be rewriting the toolchain that
/// compiles the NEXT package. Read suffices because exec is gated on read (Seatbelt allows
/// `process-exec` globally and denies the file read; Linux binds the subtree read-only).
///
/// `None` — no eligible Python, or none new enough — leaves the env and grants untouched.
///
/// KNOWN GAP: a lifecycle script that invokes `python3` DIRECTLY (not through node-gyp)
/// still reaches the ungranted shim. Granting the shim without its re-exec chain would
/// only move the failure, so it stays out until a real package needs it.
fn python_toolchain_grant(
    ambient: &BTreeMap<String, String>,
    spawn: &aube_util::LifecycleSandboxSpawn,
) -> Option<PythonToolchain> {
    // Gate on the gyp manifest: resolving costs an interpreter startup (~140ms measured),
    // and only a package node-gyp will configure can spend it usefully. `binding.gyp` is
    // what node-gyp itself keys on, and every wrapper that ends in a source build
    // (`node-gyp-build`, `prebuild-install || node-gyp rebuild`, `node-pre-gyp`) ships one.
    // A package that GENERATES its manifest at install time is missed and keeps the
    // pre-existing failure — no regression, and no evidence any real one does this.
    if !spawn.package_dir.join("binding.gyp").exists() {
        return None;
    }
    let eligible = ProbeScope::new(spawn);
    python_candidates(ambient, &eligible)
        .into_iter()
        .find_map(|candidate| probe_python(&candidate, ambient, &spawn.cwd, &eligible))
}

/// What nub is willing to EXECUTE while deciding the grant.
///
/// The probe runs UNCONFINED, in nub's own process, before any policy exists — so a
/// candidate the dependency tree can supply is arbitrary code escaping the very jail this
/// module implements. That is not hypothetical: a package declaring
/// `"bin": {"python3": …}` lands its own script FIRST on the lifecycle PATH (aube
/// prepends `node_modules/.bin`), and an early build of this feature ran it — the script
/// read `~/.ssh` and wrote `$HOME`, both of which the jail denies.
///
/// So nothing the consumer or its dependencies can author is probeable: no relative path
/// (it would resolve against nub's cwd, not the child's), nothing under the project or
/// the package being built, and nothing beneath ANY `node_modules` — which is also what
/// catches the store spellings a `.bin` shim resolves into. A real interpreter never
/// lives in those places, so the only thing this refuses is an attack.
///
/// The refusal is a SKIP, not a stop: the search continues to the next candidate, so a
/// planted `python3` costs the attacker nothing and gains them nothing.
pub(super) struct ProbeScope {
    project_root: PathBuf,
    package_dir: PathBuf,
}

impl ProbeScope {
    pub(super) fn new(spawn: &aube_util::LifecycleSandboxSpawn) -> Self {
        Self {
            project_root: canonical(&spawn.project_root),
            package_dir: canonical(&spawn.package_dir),
        }
    }

    pub(super) fn allows(&self, candidate: &Path) -> bool {
        let authored_here = |p: &Path| {
            p.components().any(|c| c.as_os_str() == "node_modules")
                || p.starts_with(&self.project_root)
                || p.starts_with(&self.package_dir)
        };
        candidate.is_absolute()
            && !authored_here(candidate)
            && !authored_here(&canonical(candidate))
    }
}

/// node-gyp's candidate order (`lib/find-python.js`): `NODE_GYP_FORCE_PYTHON` short-circuits
/// the whole search, then `--python` (which npm-style config delivers as
/// `npm_config_python`), then `PYTHON`, then a bare `python3`, then a bare `python`.
///
/// Every key here is reachable from a project-local `.npmrc`, so each resolved candidate
/// passes [`ProbeScope`] before nub will run it.
fn python_candidates(ambient: &BTreeMap<String, String>, eligible: &ProbeScope) -> Vec<PathBuf> {
    let path = ambient.get("PATH").map(String::as_str);
    let resolve = |program: &str| lookup_program(program, path, eligible);
    if let Some(forced) = ambient.get(NODE_GYP_FORCE_PYTHON) {
        return resolve(forced).into_iter().collect();
    }
    let mut out = Vec::new();
    for key in ["npm_config_python", "PYTHON"] {
        out.extend(ambient.get(key).and_then(|v| resolve(v)));
    }
    for name in ["python3", "python"] {
        out.extend(resolve(name));
    }
    out
}

/// The absolute executable a spawn of `program` would reach: itself when already
/// qualified, else the first executable hit walking `path` — `posix_spawnp`'s rule, which
/// is what node-gyp's `execFile` goes through — restricted to what [`ProbeScope`] permits.
fn lookup_program(program: &str, path: Option<&str>, eligible: &ProbeScope) -> Option<PathBuf> {
    // Eligibility first: it is the only check that rejects a RELATIVE path, and a relative
    // one would be stat'ed against nub's cwd while the exec resolves it against the child's.
    let usable = |p: &Path| eligible.allows(p) && is_executable_file(p);
    let named = Path::new(program);
    if named.components().count() > 1 {
        return usable(named).then(|| named.to_path_buf());
    }
    let candidates = |dir: PathBuf| {
        // Windows spawns resolve a bare name through PATHEXT; `.exe` is the only suffix a
        // Python installation uses, and a miss here just leaves the grant unresolved.
        let mut names = vec![dir.join(program)];
        if cfg!(windows) {
            names.push(dir.join(format!("{program}.exe")));
        }
        names
    };
    std::env::split_paths(path?)
        .flat_map(candidates)
        .find(|p| usable(p))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Run `candidate` under the effective child env and cwd, and turn what it reports about
/// itself into the read set. Rejecting a pre-3.6 interpreter here is what keeps this from
/// naming one node-gyp would go on to reject — the alternative is pinning a Python that
/// makes the build fail where it would otherwise have fallen through to the next candidate.
fn probe_python(
    candidate: &Path,
    ambient: &BTreeMap<String, String>,
    cwd: &Path,
    eligible: &ProbeScope,
) -> Option<PythonToolchain> {
    // The candidate passing [`ProbeScope`] only covers the FIRST hop. A version-manager
    // shim re-execs `python3` and searches PATH again, so leaving the dependency-authored
    // entries on it hands the planted binary right back one hop later — measured: nub
    // correctly skipped `node_modules/.bin/python3`, chose the real pyenv shim, and pyenv
    // then resolved `python3` to the planted script anyway and ran it unconfined. The
    // probe's whole process tree therefore searches only eligible directories.
    let mut env = ambient.clone();
    if let Some(path) = ambient.get("PATH") {
        let kept: Vec<_> = std::env::split_paths(path)
            .filter(|dir| eligible.allows(dir))
            .collect();
        env.insert(
            "PATH".to_string(),
            std::env::join_paths(kept)
                .ok()?
                .to_string_lossy()
                .into_owned(),
        );
    }
    // The cwd is the child's, because a version manager picks its version by walking up
    // from it for a `.python-version` — resolving anywhere else would name a different
    // interpreter than the build will use. See `PYTHON_PROBE` for why that cwd, being
    // dependency-authored, makes `-I` mandatory.
    let mut child = std::process::Command::new(candidate)
        .arg("-I")
        .arg("-c")
        .arg(PYTHON_PROBE)
        .env_clear()
        .envs(&env)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let stdout = read_bounded(&mut child, PYTHON_PROBE_TIMEOUT)?;
    // Gated on PARSING, not exit status: the four contract lines are flushed before the
    // introspection tail, so a candidate that answered them is usable even if the tail
    // died. Anything that is not a Python 3.6+ interpreter fails the parse.
    let toolchain = python_reads(&String::from_utf8(stdout).ok()?)?;
    // Re-gate what came BACK. The probe's answer decides both the interpreter nub names
    // and the tree it read-grants, so a resolution that lands inside the dependency tree
    // by any route must not become either — independent of how it got there.
    let cleared = eligible.allows(Path::new(&toolchain.executable))
        && toolchain.reads.iter().all(|p| eligible.allows(p));
    cleared.then_some(toolchain)
}

/// Unlike node-gyp's own Python spawns, this one runs OUTSIDE the jail and outside the
/// monitor that reaps it, so a candidate that never exits would wedge the install with
/// nothing to collect it. Generous for a process whose whole job is printing four lines.
const PYTHON_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Collect `child`'s stdout, killing it if it outlives `timeout`. The reader runs on its
/// own thread because a child that fills the pipe blocks until someone drains it — polling
/// `try_wait` alone would deadlock against exactly the hang this bounds.
fn read_bounded(child: &mut std::process::Child, timeout: std::time::Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut pipe = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        pipe.read_to_end(&mut buf).ok();
        buf
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
            Err(_) => return None,
        }
    }
    reader.join().ok()
}

/// Parse the probe's four lines into the grant. Split out from the spawn so the derivation
/// — the version floor, the two prefixes, the root guard — is testable without a Python.
///
/// The read set covers each reported path BOTH as the kernel stores it and as the child
/// spells it. Seatbelt matches a rule against the CANONICAL path, so a grant written on a
/// symlinked spelling silently becomes a grant on the target and leaves the link itself
/// ungranted — and resolving an ungranted link is denied, with `EPERM`, which
/// `posix_spawnp` treats as fatal. The spelling is not ours to choose: node-gyp re-derives
/// the interpreter from the candidate's own `sys.executable` and execs THAT, and a
/// Homebrew Python reports its `opt/<pkg>` alias no matter which path invoked it — so
/// there are two link hops between what node-gyp runs and the file (measured: the alias
/// spelling `EPERM`s while the identical file under its Cellar path runs).
fn python_reads(probe_stdout: &str) -> Option<PythonToolchain> {
    let mut lines = probe_stdout.lines();
    let executable = lines.next()?.trim();
    let prefix = lines.next()?.trim();
    let base_prefix = lines.next()?.trim();
    let (major, minor) = lines.next()?.trim().split_once(' ')?;
    if (major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?) < (3, 6) {
        return None;
    }
    if executable.is_empty() {
        return None;
    }
    let executable = PathBuf::from(executable);
    let mut reads = Vec::new();
    // Images are kept only if they EXIST as files. On macOS the loader reports every
    // library in the dyld shared cache by a path that has no file behind it — 352 unique
    // names on a stock Homebrew Python, of which 8 exist. Granting the other 344 would be
    // pure cost: two `FsRule`s each, ~211 KB of SBPL in an argv element that shares
    // ARG_MAX with the child's whole environment. Linux drops them anyway (a Speculative
    // mount source that is missing is skipped); this makes both backends agree.
    let images = lines
        .map(str::trim)
        .filter(|l| l.starts_with('/'))
        .map(Path::new)
        .filter(|p| p.is_file());
    for reported in [Path::new(prefix), Path::new(base_prefix), &executable]
        .into_iter()
        .chain(images)
    {
        reads.push(canonical(reported));
        reads.extend(symlink_hop_dirs(reported));
    }
    // The bin dir is NOT always under either prefix: a Homebrew Python's `bin/` sits beside
    // the `Frameworks/…/Versions/<v>` tree that `sys.prefix` names, and the siblings there
    // (`pip`, the versioned `pythonX.Y`) are what a gyp action reaches for.
    let executable = canonical(&executable);
    reads.extend(executable.parent().map(Path::to_path_buf));
    reads.retain(|p| grantable(p));
    let mut seen = std::collections::BTreeSet::new();
    reads.retain(|p| seen.insert(p.clone()));
    // Collapse anything an outer grant already covers.
    let roots = reads.clone();
    reads.retain(|p| !roots.iter().any(|r| r != p && p.starts_with(r)));
    // Outermost first, so each grant nests inside the one before it in bwrap's argv.
    reads.sort_by_key(|p| p.components().count());
    Some(PythonToolchain {
        executable: executable.to_string_lossy().into_owned(),
        reads,
    })
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether a derived path may become a read grant at all.
///
/// Refused: anything at or one level below the filesystem root (`sys.prefix` is `/usr` for
/// a distro Python, which every backend's system floor already covers), the user's HOME or
/// an ancestor of it (a version manager installed as `~/x -> ~/opt/x` would otherwise hand
/// a hop grant on the whole home directory), and any path still carrying `..`/`.` —
/// `canonical` falls back to its input for a path that does not exist, so a traversal
/// would survive the component count and only collapse later, inside the policy compiler,
/// landing on `/`.
fn grantable(path: &Path) -> bool {
    use std::path::Component;
    if !path.is_absolute() || path.components().count() <= 2 {
        return false;
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    !user_home().is_some_and(|home| home.starts_with(path))
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The directories that must be readable for a child to TRAVERSE `path` as spelled: for
/// every symlink among its ancestors, the real directory HOLDING that link. A link's own
/// canonical path is its real parent plus its name, so granting that parent is the only
/// way to make the hop legal — a rule naming the link resolves to the target and grants
/// the wrong thing. What this opens is one directory of links per hop (read-only, and
/// their targets stay ungranted unless separately allowed), which for the Homebrew case
/// that motivated it is the list of installed formulae. `grantable` is what keeps a link
/// sitting directly in `$HOME` from turning that into a grant on the whole home directory.
fn symlink_hop_dirs(path: &Path) -> Vec<PathBuf> {
    path.ancestors()
        .filter(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
        .filter_map(|a| a.parent().map(canonical))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an explicit `false` unjails. The `true` and absent cases are not symmetry
    /// for its own sake: they are what keeps the field a narrowing switch, so no manifest
    /// can pre-emptively force confinement OFF-by-default back ON, or vice versa.
    #[test]
    fn opted_out_only_on_an_explicit_false() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let write = |json: &str| std::fs::write(root.join("package.json"), json).expect("manifest");

        write(r#"{"dependenciesMeta":{"esbuild":{"sandbox":false}}}"#);
        assert!(opted_out(root, "esbuild"));
        assert!(
            !opted_out(root, "sharp"),
            "an unnamed package stays confined"
        );

        for confining in [
            r#"{"dependenciesMeta":{"esbuild":{"sandbox":true}}}"#,
            r#"{"dependenciesMeta":{"esbuild":{"built":false}}}"#,
            r#"{"dependenciesMeta":{"esbuild":{}}}"#,
            r#"{"dependenciesMeta":{}}"#,
            // A non-bool must not be coerced — `"false"` is not `false`.
            r#"{"dependenciesMeta":{"esbuild":{"sandbox":"false"}}}"#,
            r#"{"name":"app"}"#,
            "{ not json",
        ] {
            write(confining);
            assert!(!opted_out(root, "esbuild"), "must confine: {confining}");
        }

        std::fs::remove_file(root.join("package.json")).expect("remove");
        assert!(
            !opted_out(root, "esbuild"),
            "a missing root manifest confines"
        );
    }

    /// The invariant the whole feature rests on: the opt-out is read from the CONSUMER's
    /// root manifest and nowhere else, so a dependency writing the identical field into
    /// its OWN `package.json` — the manifest sitting right beside the script about to run
    /// — buys nothing. A package that could unjail itself is worse than no jail.
    #[test]
    fn a_dependency_authored_opt_out_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("package.json"), r#"{"name":"app"}"#).expect("root manifest");
        let dep = root.join("node_modules/evil");
        std::fs::create_dir_all(&dep).expect("dep dir");
        std::fs::write(
            dep.join("package.json"),
            r#"{"name":"evil","dependenciesMeta":{"evil":{"sandbox":false}}}"#,
        )
        .expect("dep manifest");

        assert!(
            !opted_out(root, "evil"),
            "a dependency's own dependenciesMeta must never disable its confinement"
        );
        // Stated the other way: the dep's manifest DOES carry the field, so this is a
        // scope test, not a parse test.
        assert!(dep.as_path() != root && opted_out(&dep, "evil"));
    }

    /// The GIT-DEP ESCALATION, which the first cut of this feature shipped and which the
    /// `opted_out` scope test above does NOT catch — there the hole was never in which
    /// file is read, but in which `project_root` the caller supplies.
    ///
    /// `run_git_dep_prepare` roots a nested install at the fetched clone dir, so a git
    /// dependency's own manifest becomes that install's "project root" and every package
    /// IT builds would consult attacker-authored `dependenciesMeta`. Both gates are
    /// pinned independently, because either one alone closes it and the point of having
    /// two is that neither is load-bearing on its own.
    #[test]
    fn a_fetched_checkout_cannot_unjail_the_packages_its_nested_install_builds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let checkout = tmp.path();
        std::fs::write(
            checkout.join("package.json"),
            r#"{"name":"x","dependenciesMeta":{"payload":{"sandbox":false}}}"#,
        )
        .expect("checkout manifest");

        // The user's real project, somewhere else entirely — the cwd every arm below runs
        // under.
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let cwd = Some(elsewhere.path().to_path_buf());

        // Gate 1 — aube withholds the identity under a fetched root, so there is nothing
        // to look up however tempting the manifest beside it looks.
        assert!(should_confine_from(None, checkout, cwd.clone()));
        // Gate 2 — even handed a name, the root must contain nub's cwd to be the user's.
        // A clone dir under nub's store never does. This is the gate that survives a
        // future aube caller re-pointing `project_dir`.
        assert!(
            should_confine_from(Some("payload"), checkout, cwd),
            "a fetched checkout's dependenciesMeta must not unjail its nested install's builds"
        );

        // Non-vacuousness: the SAME manifest unjails once the cwd is inside it, so the
        // two assertions above are the provenance gates firing — not a parse failure, a
        // missing field, or a jail that never opts out at all. The cwd must EXIST:
        // `canonical` returns its input unchanged for a path that does not, and on macOS
        // the real root canonicalizes `/var` to `/private/var`, so a made-up subpath
        // would fail containment for a reason that has nothing to do with provenance.
        let inside = checkout.join("sub");
        std::fs::create_dir(&inside).expect("cwd inside the root");
        assert!(!should_confine_from(
            Some("payload"),
            checkout,
            Some(inside)
        ));
    }

    #[test]
    fn node_layout_reads_the_posix_bin_node_shape() {
        let layout = node_layout(Path::new("/home/u/.cache/nub/node/v22.14.0/bin/node"))
            .expect("derives a layout");
        assert_eq!(
            layout.root,
            PathBuf::from("/home/u/.cache/nub/node/v22.14.0")
        );
        assert_eq!(
            layout.headers,
            PathBuf::from("/home/u/.cache/nub/node/v22.14.0/include/node")
        );
        assert_eq!(
            layout.global_modules,
            PathBuf::from("/home/u/.cache/nub/node/v22.14.0/lib/node_modules")
        );
    }

    /// The regression this whole change exists for. The Windows zip and MSI are FLAT, so
    /// taking the grandparent walks off the distribution entirely — a stock MSI install
    /// derives `C:\Program Files`, whose `include/node` and `lib/node_modules` exist under
    /// no Windows Node whatsoever. Expressed with forward slashes so it pins the
    /// derivation on every host: the discriminator is the executable's NAME, not the
    /// platform the test runs on.
    #[test]
    fn node_layout_reads_the_flat_windows_shape() {
        let layout = node_layout(Path::new("/Program Files/nodejs/node.exe")).expect("a layout");
        assert_eq!(layout.root, PathBuf::from("/Program Files/nodejs"));
        assert_eq!(
            layout.global_modules,
            PathBuf::from("/Program Files/nodejs/node_modules"),
            "the Windows global package tree sits beside node.exe, not under lib/"
        );
        assert_ne!(
            layout.root,
            PathBuf::from("/Program Files"),
            "deriving the grandparent is the mis-derivation this replaces"
        );
    }

    /// PATH lookup is case-insensitive on Windows, so `NODE.EXE` names the same file and
    /// must not fall through to the POSIX branch.
    #[test]
    fn node_layout_matches_the_executable_name_case_insensitively() {
        let layout = node_layout(Path::new("/opt/nodejs/NODE.EXE")).expect("a layout");
        assert_eq!(layout.root, PathBuf::from("/opt/nodejs"));
    }

    /// The grant stays SCOPED to toolchain subtrees. Granting the derived root itself
    /// would hand a dependency build script the whole prefix — for a `/usr/local/bin/node`
    /// or Homebrew Node that is a shared system prefix, not nub's own store.
    #[test]
    fn node_layout_subtrees_never_name_the_bare_root() {
        let layout = node_layout(Path::new("/usr/local/bin/node")).expect("derives a layout");
        assert_eq!(layout.root, PathBuf::from("/usr/local"));
        for subtree in [&layout.headers, &layout.global_modules] {
            assert!(
                subtree.starts_with("/usr/local") && subtree != Path::new("/usr/local"),
                "the shared prefix itself must never be a read grant: {subtree:?}"
            );
        }
    }

    #[test]
    fn node_layout_absent_without_a_grandparent() {
        assert!(node_layout(Path::new("/node")).is_none());
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

    /// Absence must be tolerated, not just "usually present": a from-source Node build, or
    /// a distribution that installs no global packages at all, can hand this a
    /// `global_modules` that doesn't exist. `deny_search_roots` is strict (an absent root
    /// is a hard error), so this must return `None`, never a dangling path.
    #[test]
    fn npm_builtin_config_deny_root_absent_when_npm_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let lib_node_modules = tmp.path().join("lib/node_modules");
        std::fs::create_dir_all(&lib_node_modules).unwrap();
        assert_eq!(npm_builtin_config_deny_root_for(&lib_node_modules), None);

        let nonexistent = tmp.path().join("nowhere/lib/node_modules");
        assert_eq!(npm_builtin_config_deny_root_for(&nonexistent), None);
    }

    /// The Python-grant cases are POSIX-shaped throughout — absolute paths without a
    /// drive prefix, a `symlink` that only exists under `std::os::unix`, `/bin/*`
    /// candidates. The derivation itself is platform-neutral; expressing these on Windows
    /// would mean a second set of fixtures for a jail whose Windows story diverges
    /// elsewhere anyway (deny-all net, AppContainer), so they run where they are honest.
    #[cfg(unix)]
    mod python {
        use super::super::*;
        use std::os::unix::fs::PermissionsExt;

        fn probe_output(lines: &[&str]) -> String {
            lines.join("\n")
        }

        /// The regression this whole path exists for. An interpreter reached through a
        /// symlinked directory — a Homebrew `opt/<formula>` alias, a version manager's
        /// current-version link — must have the LINK's holding directory granted, not just the
        /// target: the sandbox resolves a rule to the target, so a target-only grant leaves the
        /// hop denied and the exec fails `EPERM` with the interpreter itself readable.
        #[test]
        fn python_reads_grants_the_directory_holding_an_ancestor_symlink() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
            std::fs::create_dir_all(root.join("real/bin")).expect("prefix");
            std::fs::write(root.join("real/bin/python3"), b"").expect("interpreter");
            std::os::unix::fs::symlink(root.join("real"), root.join("alias")).expect("alias");

            let aliased = root.join("alias/bin/python3");
            let grant = python_reads(&probe_output(&[
                &aliased.to_string_lossy(),
                &root.join("real").to_string_lossy(),
                &root.join("real").to_string_lossy(),
                "3 12",
            ]))
            .expect("a 3.12 interpreter yields a grant");

            assert!(
                grant.reads.contains(&root),
                "the directory holding the `alias` symlink must be granted so the hop resolves; \
                 got {:?}",
                grant.reads
            );
            assert_eq!(
                grant.executable,
                root.join("real/bin/python3").to_string_lossy(),
                "the child is handed the resolved spelling"
            );
        }

        /// A loaded shared object outside every reported prefix is part of the closure — a
        /// pyenv-built Python links its `libintl` from the system package manager's tree —
        /// while an entry an outer grant already covers is dropped rather than repeated, and
        /// a reported image with no file behind it is dropped entirely. That last one is not
        /// an edge case: on macOS the loader names every dyld-shared-cache library, 344 of
        /// 352 on a stock host, and granting them cost ~211 KB of SBPL in an argv element
        /// that shares ARG_MAX with the child's environment.
        #[test]
        fn python_reads_covers_out_of_prefix_images_and_drops_covered_or_absent_ones() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
            let prefix = root.join("py/3.12");
            std::fs::create_dir_all(prefix.join("lib")).expect("prefix");
            std::fs::create_dir_all(prefix.join("bin")).expect("bin");
            std::fs::write(prefix.join("bin/python3"), b"").expect("interpreter");
            std::fs::write(prefix.join("lib/libpython.so"), b"").expect("nested image");
            let outside = root.join("brew/gettext/1.0/lib");
            std::fs::create_dir_all(&outside).expect("outside prefix");
            std::fs::write(outside.join("libintl.8.dylib"), b"").expect("outside image");

            let grant = python_reads(&probe_output(&[
                &prefix.join("bin/python3").to_string_lossy(),
                &prefix.to_string_lossy(),
                &prefix.to_string_lossy(),
                "3 12",
                &prefix.join("lib/libpython.so").to_string_lossy(),
                &outside.join("libintl.8.dylib").to_string_lossy(),
                "/System/Library/dyld-shared-cache/only/libNoSuchFile.dylib",
            ]))
            .expect("a 3.12 interpreter yields a grant");

            assert!(
                !grant.reads.iter().any(|p| p.starts_with("/System")),
                "an image with no file behind it must not become a grant: {:?}",
                grant.reads
            );
            assert!(
                grant.reads.contains(&outside.join("libintl.8.dylib")),
                "an image outside the prefix must be granted: {:?}",
                grant.reads
            );
            assert!(
                !grant.reads.contains(&prefix.join("lib/libpython.so")),
                "an image the prefix grant already covers must not be repeated: {:?}",
                grant.reads
            );
        }

        /// node-gyp requires `>=3.6.0` and falls through to its next candidate below it.
        /// Naming an older interpreter would turn that fall-through into a hard failure.
        #[test]
        fn python_reads_rejects_an_interpreter_older_than_node_gyps_floor() {
            let older = probe_output(&["/usr/bin/python2.7", "/usr", "/usr", "2 7"]);
            assert!(python_reads(&older).is_none());
            let unsupported_three = probe_output(&["/usr/bin/python3.5", "/usr", "/usr", "3 5"]);
            assert!(python_reads(&unsupported_three).is_none());
        }

        /// The three ways a derived path could widen the read set far past one interpreter:
        /// a root-level prefix (`/usr` for a distro Python), a traversal that only collapses
        /// later inside the policy compiler and lands on `/`, and — the one a real host hits
        /// — a hop directory that IS `$HOME`, which a version manager installed as
        /// `~/x -> …` would otherwise produce, handing every dependency build script the
        /// whole home directory.
        #[test]
        fn grantable_refuses_every_path_that_would_widen_past_one_interpreter() {
            for refused in ["/", "/usr", "/usr/../.."] {
                assert!(!grantable(Path::new(refused)), "{refused} must be refused");
            }
            assert!(grantable(Path::new("/usr/local/py")));

            let home = user_home().expect("HOME is set under test");
            assert!(!grantable(&home), "$HOME itself must be refused");
            assert!(
                !grantable(home.parent().expect("HOME has a parent")),
                "an ancestor of $HOME must be refused"
            );
            assert!(
                grantable(&home.join("miniconda3")),
                "a real interpreter tree inside $HOME stays grantable"
            );
        }

        fn ambient(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        }

        /// A scope that refuses nothing a real host would offer, for the ordering cases.
        fn any_scope() -> ProbeScope {
            ProbeScope {
                project_root: PathBuf::from("/nonexistent-project"),
                package_dir: PathBuf::from("/nonexistent-package"),
            }
        }

        /// node-gyp's search order, which this must mirror exactly or the grant covers a
        /// different interpreter than the one it goes on to run.
        #[test]
        fn python_candidates_follow_node_gyps_key_order() {
            let forced = ambient(&[
                (NODE_GYP_FORCE_PYTHON, "/bin/sh"),
                ("npm_config_python", "/bin/echo"),
                ("PYTHON", "/bin/cat"),
            ]);
            assert_eq!(
                python_candidates(&forced, &any_scope()),
                vec![PathBuf::from("/bin/sh")],
                "NODE_GYP_FORCE_PYTHON short-circuits the whole search"
            );

            let configured = ambient(&[("npm_config_python", "/bin/echo"), ("PYTHON", "/bin/cat")]);
            assert_eq!(
                python_candidates(&configured, &any_scope()),
                vec![PathBuf::from("/bin/echo"), PathBuf::from("/bin/cat")],
                "--python outranks PYTHON"
            );

            assert!(
                python_candidates(&ambient(&[("PATH", "/nonexistent")]), &any_scope()).is_empty(),
                "no interpreter on PATH yields no candidate, leaving the env untouched"
            );
        }

        /// The probe runs UNCONFINED, so a candidate the dependency tree can author would
        /// be a sandbox escape: a package declaring `"bin": {"python3": …}` puts its own
        /// script first on the lifecycle PATH, and an early build of this feature executed
        /// it — `~/.ssh` read, `$HOME` written, both denied inside the jail. The planted
        /// entry must be SKIPPED, with the search continuing past it.
        #[test]
        fn python_candidates_never_run_anything_the_dependency_tree_authored() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
            let planted = root.join("project/node_modules/.bin");
            let system = root.join("usr/bin");
            for dir in [&planted, &system] {
                std::fs::create_dir_all(dir).expect("bin dir");
                // Both spellings node-gyp tries, so this also pins that the two-name loop
                // tolerates a host where `python3` and `python` are separate executables.
                for name in ["python3", "python"] {
                    let exe = dir.join(name);
                    std::fs::write(&exe, b"#!/bin/sh\n").expect("interpreter");
                    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
                        .expect("chmod");
                }
            }
            let scope = ProbeScope {
                project_root: root.join("project"),
                package_dir: root.join("project/node_modules/evil"),
            };
            let path = format!("{}:{}", planted.display(), system.display());

            assert_eq!(
                python_candidates(&ambient(&[("PATH", &path)]), &scope),
                vec![system.join("python3"), system.join("python")],
                "the planted bin must be skipped and the search continue"
            );
            assert_eq!(
                python_candidates(
                    &ambient(&[(
                        "npm_config_python",
                        &planted.join("python3").to_string_lossy()
                    )]),
                    &scope
                ),
                Vec::<PathBuf>::new(),
                "a project-local .npmrc must not be able to name it either"
            );
            assert!(
                !scope.allows(Path::new("relative/python3")),
                "a relative candidate resolves against nub's cwd, not the child's"
            );
        }
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
