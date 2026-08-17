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
//! The jail is GLOBAL: on by default, off only via `nub.jsonc` `install.buildJail: false`
//! ([`build_jail_enabled`]). There is no per-package opt-out.

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
    /// The confinement gate, side-effect-free. aube also asks this while PLANNING, to key
    /// the side-effects cache for packages whose cached tree it may restore without
    /// spawning anything — so the notice lives in `confines` below, not here.
    ///
    /// Both package arguments are unused: the switch is GLOBAL since c5651408f4, so nothing
    /// about the package can change the answer. They stay in the signature because the trait
    /// is aube's, and because the catalog's version-scoped GRANTS still key on them in `run`
    /// — confinement is all-or-nothing, what a confined script may DO is per package+version.
    fn would_confine(
        &self,
        package_name: Option<&str>,
        _package_version: Option<&str>,
        project_root: &Path,
    ) -> bool {
        should_confine(package_name, project_root)
    }

    /// The spawn-time call. `false` sends the script back to aube's ordinary unconfined
    /// spawn, and is announced — a script really is about to run.
    fn confines(
        &self,
        package_name: Option<&str>,
        package_version: Option<&str>,
        project_root: &Path,
    ) -> bool {
        if self.would_confine(package_name, package_version, project_root) {
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
                 (install.buildJail is false in nub.jsonc)"
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

        // Windows stamps `NODE_OPTIONS` too — below, where the interpreter's version is
        // already known.

        // Make node-gyp compile offline. It reads Node headers from `npm_config_nodedir/
        // include/node` (default devdir `~/.cache/node-gyp/<ver>`, unreadable → network
        // fallback the jail denies). Point nodedir at a directory that ACTUALLY HOLDS
        // them and grant the toolchain subtrees (the store path is outside `$tooldirs` +
        // the interpreter grant). Set-if-absent: an explicit ambient nodedir is a
        // deliberate build-against-custom-node choice; the case we fix carries none.
        let probe = ProbeScope::new(&spawn);

        // WINDOWS: redirect the interpreter to a nub-owned COPY of the same distribution,
        // BEFORE anything else reads `npm_node_execpath`. Two independent reasons the ambient
        // one is unusable — nub cannot write the read-grant ACE where the stock MSI installs,
        // and a confined caller cannot open that image even where it can — are on
        // [`super::jail_bin`]'s module doc with their measurements. Everything below then
        // derives from the copy: the interpreter grant, `node_layout`'s `node_modules` and
        // header paths, and the version the `NODE_OPTIONS` gate asks for. Declining leaves the
        // ambient interpreter, which is the behavior before this existed.
        #[cfg(windows)]
        if let Some(staged) = super::jail_bin::stage(&ambient, &probe) {
            staged.redirect_env(&mut ambient);
        }

        // NODE VERSION RESOLUTION — the same move prefetch makes below, applied to nub's
        // OWN runtime: settle it out here so the confined child never has to. A package
        // whose `engines.node` (or a pin file above it) names a version the ambient
        // interpreter does not satisfy sends the in-jail `node` shim back through nub's
        // discovery, and inside the jail that walk can reach NONE of its answers — not
        // `~/.nvm`, not nub's store, and not nodejs.org. It failed closed on all three,
        // which reads as a network break but is not one: a version this host already had
        // unpacked under `~/.nvm` was as unreachable as one it had never downloaded.
        // Resolving here reuses the ordinary order (PATH → store → nvm → download), so
        // the network is touched only when a real download is the only remaining answer,
        // and it is nub fetching its own runtime rather than a dependency opening a socket.
        //
        // Gated on a pin EXISTING, so the unpinned majority keeps the ambient interpreter
        // and pays nothing; a resolution failure leaves the spawn byte-identical, i.e. the
        // pre-existing break rather than a new one.
        if let Some(node) = pinned_interpreter(&spawn.cwd) {
            // Both spellings, because they are read by different consumers and a split
            // between them is its own silent wrong answer: `NODE_EXECUTABLE` is the one
            // key nub's discovery honours BEFORE reading any pin file, so it is what stops
            // the in-jail re-resolution; `npm_node_execpath` is what npm, node-gyp and the
            // shim's re-exec follow, and it is the input `node_layout` derives the headers
            // node-gyp compiles against from — leaving it on the ambient interpreter would
            // build a pinned package's addon against the wrong Node's headers.
            ambient.insert("NODE_EXECUTABLE".to_string(), node.clone());
            ambient.insert("npm_node_execpath".to_string(), node);
        }

        // The interpreter closure to grant READ. nub provisions its own Node under its
        // store (not `/usr`), so the tight-read base can't reach it. Under nub a bare
        // `node` resolves via the PATH-prepended shim (`NODE`) which re-execs the real
        // binary (`npm_node_execpath`), so BOTH must be readable/executable — grant each
        // (compile_build_jail dedups and adds each one's bin dir). On Windows both spellings
        // already name the staged copy, so this resolves to one directory.
        // ⛔ ABSOLUTE, EXISTING PATHS ONLY. These become `FsOrigin::Authored` read grants, and
        // an authored grant whose source is MISSING is a hard refusal in the Linux backend
        // (`linux_grants.rs`: speculative absences are skipped, authored ones abort) — so one bad
        // entry does not degrade the policy, it makes the whole jail UNCOMPILABLE.
        //
        // MEASURED on Linux before this filter: `PolicyNotExpressible("filesystem mount source
        // does not exist: node")`, surfaced to the user as "requires Landlock (Linux 5.13+), which
        // this kernel does not provide" ON A 6.17 KERNEL. Worse, the probe's search read that as a
        // capability need and escalated until `write.disk` produced an expressible policy, so nine
        // corpus packages recorded a FABRICATED `write.disk` grant.
        //
        // The bare name came from this list's own premise: before the node-shim removal, `NODE`
        // named the PATH-prepended shim and a bare `node` resolved through it. With the shim gone
        // `NODE` is the real execpath, and any bare spelling that survives is a relative path with
        // nothing behind it.
        let interpreter: Vec<PathBuf> = ["npm_node_execpath", "NODE"]
            .iter()
            .filter_map(|k| ambient.get(*k))
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.exists())
            .collect();

        // WINDOWS: deliver the `child_process` stdio shim. A piped spawn under the
        // AppContainer does not fail, it SPINS — libuv retries the refused named pipe
        // forever inside `uv_spawn`, before any timeout can arm — and every `node-gyp`
        // configure pipes. Rationale, and the residuals it still leaves (handle passing and
        // `serialization: 'advanced'`, which no userland stream can carry), are on
        // `nub_sandbox::windows_build_jail_node_options`.
        //
        // UNCONDITIONAL, like `NODE_COMPAT` above: it OVERWRITES any ambient value, which is
        // also the ONLY thing that keeps the env allowlist's `NODE_OPTIONS` entry from
        // becoming an ambient code-injection channel into every lifecycle script. Gated on
        // the interpreter supporting `--import` (20.6+) because an unrecognised option in
        // `NODE_OPTIONS` aborts Node at startup — that would turn a missing repair into a
        // broken install. An interpreter that cannot be asked gets no stamp, and so neither
        // shim: the same piped-spawn hang as before, never a worse failure.
        //
        // The gate reads the SAME identity the curated filesystem table is keyed by — aube's
        // installer-resolved `registry_name()`, which a dependency cannot rename itself into.
        //
        // THE THIRD TERM is the realpath repair. The backend's ancestor repair
        // (`ancestor_chain`) is best-effort — a refused ACE write is skipped — so this
        // preload is what keeps module resolution working when it did not land, and is inert when
        // it did. Its roots are the anchors the jail actually grants, which is what scopes the
        // tolerance rule; the interpreter is among them so a `require()` of npm's own modules out
        // of the Node install tree resolves too.
        #[cfg(windows)]
        if super::build_prefetch::node_version(&ambient, &probe).is_some_and(supports_import) {
            let mut realpath_roots = vec![spawn.project_root.clone(), spawn.package_dir.clone()];
            realpath_roots.extend(interpreter.iter().cloned());
            let realpath = nub_sandbox::realpath_shim_node_options(&realpath_roots);
            ambient.insert(
                "NODE_OPTIONS".to_string(),
                format!(
                    "{} {realpath}",
                    nub_sandbox::windows_build_jail_node_options(
                        spawn.package_name.as_deref(),
                        spawn.package_version.as_deref(),
                    )
                )
                .trim_end()
                .to_string(),
            );
        } else {
            ambient.remove("NODE_OPTIONS");
        }

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
            //
            // …EXCEPT that `npm_config_python` is read by node-gyp and NOTHING ELSE, which is
            // what the KNOWN GAP on `python_toolchain_grant` records. node-gyp's GENERATED
            // MAKEFILE shells a bare `python3` for its `LIBTOOL-STATIC` rule, bash resolves
            // that on PATH, and on a machine whose `python3` is a version-manager shim under
            // `$HOME` the jail correctly denies EXEC of an ungranted path. MEASURED on
            // `hiredis@0.5.0`, era Node 10, one variable (`install.buildJail`):
            //   jail ON  -> `bash: ~/.pyenv/shims/python3: Operation not permitted`,
            //               `make: *** [Release/hiredis-c.a] Error 126`, no addon
            //   jail OFF -> rc 0, `hiredis.node` built
            // So the package is not broken; the gap is. The grant is already correct — the
            // interpreter and its whole closure are read-granted — the bare name just does
            // not resolve TO it.
            //
            // FIXED BY WINNING THE PATH RACE, NOT BY GRANTING THE SHIM. Granting a shim would
            // pull in its entire re-exec chain (the shim, its bash, the version manager's
            // libexec and version store), which is the objection that kept this gap open. A
            // nub-owned directory holding one symlink to the ALREADY-GRANTED interpreter
            // costs one path and closes it.
            //
            // ⛔ IT MUST GO AT THE FRONT, and `ambient` is why that is safe here. `ambient` is
            // `reconstruct_child_env`, i.e. the ALREADY-COMPOSED child PATH — dependency
            // `.bin` chain included — so prepending lands ahead of a dependency-planted
            // `python3`. That attack is real and measured (see `probe_python`: nub skipped a
            // planted `node_modules/.bin/python3`, chose the shim, and pyenv then resolved
            // `python3` back to the planted script). Contributing this through aube's
            // `tool_bin_dirs` instead would sit BEHIND the dep chain and reopen it.
            let front = python_path_front_dir(&python.executable, &spawn.project_root);
            ambient.insert("npm_config_python".to_string(), python.executable);
            extra_reads.extend(python.reads);
            if let Some(dir) = front {
                if let Some(path) = ambient.get("PATH") {
                    let joined = std::env::join_paths(
                        std::iter::once(dir.clone()).chain(std::env::split_paths(path)),
                    );
                    if let Ok(joined) = joined {
                        ambient.insert("PATH".to_string(), joined.to_string_lossy().into_owned());
                    }
                }
                extra_reads.push(dir);
            }
        }

        let jail_cache = sandbox_homes(&spawn.project_root).cache;
        redirect_npm_prefix(&mut ambient, &jail_cache);
        redirect_electron_cache(&mut ambient, &jail_cache);
        redirect_playwright_browsers(&mut ambient, &jail_cache);

        // WINDOWS: node-gyp's THIRD out-of-jail toolchain dependency, and the only one whose
        // discovery an AppContainer cannot perform at all — it activates a COM server, which
        // no filesystem grant reaches and no unprivileged permission opens. Pre-resolved out
        // here and handed over as the env trio `findVisualStudio` short-circuits on; see
        // [`super::jail_msvc`] for why the trio is stamped as a unit.
        #[cfg(windows)]
        if let Some(msvc) = super::jail_msvc::resolve(&ambient, &spawn, &probe) {
            msvc.stamp(&mut ambient);
            extra_reads.extend(msvc.reads);
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
        // The name is aube's `registry_name()` — the identity the catalog keys the
        // per-package opt-out on, and for the same reason: a dependency cannot rename
        // itself into it, and aube withholds it entirely once its root is a checkout it
        // fetched. It selects a curated exception only; it can never widen the baseline.
        let policy = nub_sandbox::compile_build_jail(
            homes,
            &spawn.package_dir,
            spawn.package_name.as_deref(),
            spawn.package_version.as_deref(),
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
            let status = child.wait();
            persist_declared_home_writes(&spawn);
            status
        }
        // Windows owns spawn+wait inside its launch plan and refuses the asynchronous
        // `spawn` seam, so the uniform `status()` verb stays the entry point off unix.
        #[cfg(not(unix))]
        {
            prepared.status()
        }
    }
}

/// Move the directories a package's catalog entry DECLARES out of its throwaway `$HOME` and
/// into the user's real one, once its lifecycle scripts have finished.
///
/// WHY THIS EXISTS. The jail redirects `$HOME` to a per-package directory that is discarded, so
/// a package caching under `~/.cache/<vendor>` installs cleanly and its artefact is thrown
/// away — measured: puppeteer's browser was 355 of the 359 paths it wrote, with none under the
/// real `~/.cache/puppeteer`. At run time `HOME` is the real home and the package finds nothing.
///
/// WHY A MOVE RATHER THAN GRANTING WRITE ON THE REAL HOME. Granting hands a dependency script a
/// live handle on `$HOME` for the whole run. This never does: the script writes to the
/// throwaway and nub relocates only what the catalog names. The AUTHORITY is the same — nub
/// copies whatever landed there — which is why the field is called `writePaths` and not
/// something softer. The mechanism is tighter; the trust is not.
///
/// FAILURES ARE NON-FATAL. A lifecycle script that already succeeded must not be turned into a
/// failed install because a cache could not be relocated; the package degrades to the
/// pre-existing behaviour, which is the artefact being discarded.
#[cfg(unix)]
fn persist_declared_home_writes(spawn: &aube_util::LifecycleSandboxSpawn) {
    #[cfg(feature = "build-jail-catalog-override")]
    {
        let Some(name) = spawn.package_name.as_deref() else {
            return;
        };
        // THE VERSION IS PART OF THE LOOKUP. The grant an old pin resolves to is not the one
        // `latest` resolves to, and moving the wrong entry's directories would either strand a
        // cache in the throwaway or promote one the resolved grant never declared.
        // ⛔ AN UNCATALOGUED PACKAGE PROMOTES THE BASELINE'S PATHS, NOT NOTHING — the THIRD site that
        // must agree about the baseline, after the filesystem grant in `compile_build_jail` and egress
        // in `build_jail_net`. This used to `return` on a missing entry, which would have left
        // `baseline_caps().write_paths` INERT: the script writes its cache into the throwaway home,
        // nothing copies it out, and the baseline's whole compatibility benefit evaporates silently
        // while every grant still reads correctly. Promotion is what makes a cache allowlist mean
        // anything.
        let here = nub_sandbox::catalog_v2::Platform::current();
        let caps =
            match nub_sandbox::catalog_override_v2_grant(name, spawn.package_version.as_deref()) {
                // THE OS IS PART OF THE LOOKUP TOO: a per-OS block may withdraw `writePaths` where
                // the outer grant declares them, and promoting a directory this OS was never granted
                // would move a cache the resolved grant does not authorise.
                Some(grant) => grant.on(here),
                None => std::borrow::Cow::Owned(nub_sandbox::catalog_v2::baseline_caps()),
            };
        if caps.write_paths.is_empty() {
            return;
        }
        let homes = sandbox_homes(&spawn.project_root);
        let Some(private) = nub_sandbox::jail_private_home(&homes, &spawn.package_dir) else {
            return;
        };
        for rel in &caps.write_paths {
            let from = private.join(rel);
            if !from.exists() {
                continue;
            }
            let to = homes.home.join(rel);
            // ALREADY THERE. A package's scripts run more than once per install (the approve
            // window re-runs them), and a re-download lands in a FRESH private home while the
            // first copy is already in place. `rename` onto a populated directory fails
            // ENOTEMPTY, so treat an existing destination as done rather than warning about a
            // cache that is present and correct. Measured: the real home was populated at
            // 09:10:50 by the install and the second copy appeared 16s later.
            if to.exists() {
                // ⛔ A PRE-EXISTING DESTINATION IS NOT ALWAYS "ALREADY PROMOTED" — it is also every
                // PREFIX path, and treating the two the same made the baseline's allowlist inert.
                //
                // The case this branch was written for is a LEAF (`.cache/prisma`, `.electron`): the
                // scripts re-run inside the approve window, a re-download lands in a fresh private
                // home, and the first copy is already in place — so skip the move and drop the
                // duplicate source. Measured on puppeteer: `nub install` then
                // `nub approve-builds --all` otherwise left 350 files in the real cache and 351 in the
                // throwaway, a complete duplicate, forever, per package.
                //
                // But `baseline_caps()` names PREFIXES (`.cache`, `.config`) so that a package
                // published tomorrow which caches conventionally works with no catalog entry — and
                // `$HOME/.cache` ALWAYS exists, because nub's own cache and the jail's private homes
                // live under it. Every prefix therefore hit this branch and promoted NOTHING. Verified
                // with `NUB_DEBUG_PROMOTE`: `rel=.cache from=…/.cache exists=true` and the marker
                // still never reached the real home.
                //
                // So descend ONE level and promote each child individually. That is the same rename
                // semantics applied at the granularity the destination actually collides at: a child
                // that is already there is the genuine "already promoted" case and its source goes;
                // a child that is not gets moved. One level is deliberate — it is exactly enough to
                // turn a prefix into the leaves the mechanism was built for, without becoming a
                // recursive merge whose conflict rules are a separate design.
                if from.is_dir() {
                    if let Ok(children) = std::fs::read_dir(&from) {
                        for child in children.flatten() {
                            let child_to = to.join(child.file_name());
                            if child_to.exists() {
                                let _ = std::fs::remove_dir_all(child.path());
                                continue;
                            }
                            if std::fs::rename(child.path(), &child_to).is_err() {
                                tracing::warn!(
                                    "build-jail: could not relocate {rel:?}/{:?} out of the \
                                     package's private home; the artefact stays in the throwaway",
                                    child.file_name()
                                );
                            }
                        }
                    }
                }
                // Whatever is left is either a duplicate or unmovable; either way it must not be
                // stranded in a home that persists across runs.
                let _ = std::fs::remove_dir_all(&from);
                continue;
            }
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Rename first: the throwaway home and the real cache are on one filesystem, so a
            // 300 MB browser costs nothing. Fall back to nothing rather than a deep copy — a
            // cross-device case wants deliberate handling, not a silent multi-hundred-MB copy
            // inside an install.
            if std::fs::rename(&from, &to).is_err() {
                tracing::warn!(
                    "build-jail: could not relocate {rel:?} out of the package's private home; \
                     the artefact stays in the throwaway and the package may not find it later"
                );
            }
        }
    }
    #[cfg(not(feature = "build-jail-catalog-override"))]
    let _ = spawn;
}

#[cfg(not(unix))]
fn persist_declared_home_writes(_spawn: &aube_util::LifecycleSandboxSpawn) {}

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
/// The refusal headline.
///
/// This used to branch on whether the project had opted IN via `install.sandbox`, so a reader
/// could tell "my team asked for this" from "nub decided it". That distinction is gone with the
/// setting: the jail is ON BY DEFAULT and `install.buildJail: false` is the only opt-out, so a
/// project that did nothing is in the same position as everyone else and there is nothing to
/// attribute.
fn headline() -> String {
    String::from("nub install: the build jail could not confine a dependency's install script\n\n")
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

/// The `allowBuilds` value that runs one package's scripts UNCONFINED.
///
/// A constant because the same placeholder is compared by string equality wherever it is honoured,
/// and three independent `== "no-jail"` literals is how one of them ends up quietly accepting
/// `"nojail"`.
pub(crate) const NO_JAIL: &str = "no-jail";

/// Whether this script stays confined. `package_name` is `None` when aube's root is a
/// checkout it fetched rather than the consumer's project; that case stays confined.
fn should_confine(package_name: Option<&str>, project_root: &Path) -> bool {
    if !build_jail_enabled() {
        return false;
    }
    // A ROOT-AUTHORED `allowBuilds` value of `"no-jail"` runs THIS package's scripts unconfined.
    //
    // ⛔ WHY THIS IS NOT THE PER-PACKAGE OPT-OUT THAT WAS REMOVED. That one was
    // `dependenciesMeta.<name>.sandbox`, and its hazard was that `dependenciesMeta` is
    // DEPENDENCY-authored: a package could ship a manifest switching off its own confinement, which is
    // strictly worse than no jail because it advertises a protection that silently is not there. This
    // reads ONLY the root project's `package.json` — which the user owns and a dependency cannot
    // reach. That distinction is the entire safety argument, and it is why the path below is
    // `project_root` and never `spawn.package_dir`.
    //
    // WHY `allowBuilds` RATHER THAN A NEW FIELD. Build approval already lives there; it is already a
    // MAP whose value type admits strings (`AllowBuildRaw::{Bool, Other}`, whose `from_json` stores a
    // string verbatim precisely so a "known placeholder" comparison works); and it sits in
    // `package.json`, where package-name lists belong rather than in `nub.jsonc`. `true` keeps meaning
    // "run it, confined"; `"no-jail"` means "run it, unconfined". No new config surface, and the trust
    // decision lands next to the decision to run the script at all.
    let Some(name) = package_name else {
        return true;
    };
    !root_opted_out_of_jail(project_root, name)
}

/// Did the ROOT project ask for this one package to run unconfined, via `allowBuilds: "no-jail"`?
///
/// Split out from [`should_confine`] so the manifest half is testable without faking the global
/// project config that `build_jail_enabled` reads.
fn root_opted_out_of_jail(project_root: &Path, package_name: &str) -> bool {
    let Ok(manifest) =
        aube_manifest::PackageJson::from_path_cached(&project_root.join("package.json"))
    else {
        return false;
    };
    // ⛔ EXACT NAME MATCH ONLY, deliberately narrower than the RUN decision. `allowBuilds` keys may be
    // patterns for deciding WHETHER a script runs; honouring a glob here would let a single entry
    // silently unconfine a whole scope, and "may run" is a much weaker statement than "may run with no
    // confinement at all".
    manifest.pnpm_allow_builds().iter().any(|(pattern, allow)| {
        pattern == package_name
            && matches!(allow, aube_manifest::AllowBuildRaw::Other(v) if v == NO_JAIL)
    })
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
/// Is the build jail on? It is GLOBAL — `nub.jsonc` `install.buildJail: false` is the only way
/// off, and absence means on.
///
/// A per-package opt-out (`dependenciesMeta.<name>.sandbox: false`) was removed. It carried a
/// real invariant — a DEPENDENCY-authored `dependenciesMeta` had to be ignored by every route,
/// because a package that could switch off its own confinement is strictly worse than no jail,
/// advertising a protection that silently is not there. A single global switch deletes that
/// whole question rather than defending it.
///
/// Orthogonal to `approveBuilds`/`allowBuilds`, which decide WHETHER a script runs; this
/// decides whether a script that runs is CONFINED.
fn build_jail_enabled() -> bool {
    crate::project_config::effective_config()
        .and_then(|config| config.values.install.build_jail)
        .unwrap_or(true)
}

/// The effective child env: the current (aube) process env with the command's explicit
/// operations applied (`Some` = set/override, `None` = removed). Non-UTF-8 keys/values
/// are skipped.
///
/// WINDOWS FOLDS THE KEYS; POSIX DOES NOT. Windows env names are case-INSENSITIVE while
/// this map is exact-case, so on that platform the raw block is the wrong shape twice over
/// and both failures are silent — see [`canonical_env_key`].
fn reconstruct_child_env(
    delta: &[(std::ffi::OsString, Option<std::ffi::OsString>)],
) -> BTreeMap<String, String> {
    let mut env = EffectiveEnv::for_host();
    for (key, value) in std::env::vars_os() {
        if let (Ok(key), Ok(value)) = (key.into_string(), value.into_string()) {
            env.set(key, value);
        }
    }
    for (key, value) in delta {
        let Ok(key) = key.clone().into_string() else {
            continue;
        };
        match value {
            Some(value) => {
                if let Ok(value) = value.clone().into_string() {
                    env.set(key, value);
                }
            }
            None => env.unset(&key),
        }
    }
    env.into_map()
}

/// Accumulates the effective child env under the SPAWNING platform's name-equality rule.
///
/// The rule is the whole point: on POSIX two spellings are two variables, on Windows they
/// are one. Folding at the single point the map is built is what makes every downstream
/// `get`/`contains_key`/`insert` in this module correct without each becoming case-aware.
///
/// `case_insensitive` is a FIELD rather than a `cfg` so the Windows rule — ordinary string
/// logic, and where any bug in this will be — is exercised by the tests on the dev host and
/// not only on a Windows runner. Same reason `jail_msvc`/`jail_bin` compile everywhere.
struct EffectiveEnv {
    /// Folded name -> (the spelling to emit, value). Under the identity fold the key IS
    /// the spelling.
    inner: BTreeMap<String, (String, String)>,
    case_insensitive: bool,
}

impl EffectiveEnv {
    fn for_host() -> Self {
        Self {
            inner: BTreeMap::new(),
            case_insensitive: cfg!(windows),
        }
    }

    /// Last write wins, spelling included — the same rule the child block's own dedupe
    /// (`dedupe_windows_env_pairs`) already documents, applied early enough that nub's
    /// lookups see the folded map rather than racing it.
    fn set(&mut self, key: String, value: String) {
        let key = self.canonical(&key).into_owned();
        self.inner.insert(self.fold(&key), (key, value));
    }

    fn unset(&mut self, key: &str) {
        let folded = self.fold(&self.canonical(key));
        self.inner.remove(&folded);
    }

    fn canonical<'a>(&self, key: &'a str) -> std::borrow::Cow<'a, str> {
        if self.case_insensitive {
            canonical_env_key(key)
        } else {
            std::borrow::Cow::Borrowed(key)
        }
    }

    fn fold(&self, key: &str) -> String {
        if self.case_insensitive {
            key.to_ascii_uppercase()
        } else {
            key.to_string()
        }
    }

    fn into_map(self) -> BTreeMap<String, String> {
        self.inner.into_values().collect()
    }
}

/// npm's own env spelling for a config key, and the prefix nub reads a dozen of them under.
const NPM_CONFIG_PREFIX: &str = "npm_config_";

/// The spelling nub's own code uses for the env names it reads or writes, given an ambient
/// key that names the same variable in some other case.
///
/// TWO REAL WINDOWS SPELLINGS MAKE THIS LOAD-BEARING, not a hypothetical: Windows' OWN
/// spelling of the search path is `Path`, and npm's documented env form for a config key is
/// UPPERCASE `NPM_CONFIG_<KEY>` (it lowercases them itself before anything reads them). An
/// exact-case `ambient.get("PATH")` / `get("npm_config_python")` misses both, and the miss
/// is invisible: the Python grant returns no candidates at all, and the `npm_config_nodedir`
/// set-if-absent reads "absent" for a value the user deliberately set. nub then inserts its
/// own spelling beside the ambient one, and which of the two reaches the child is decided by
/// exact-case `BTreeMap` order — nobody chose that.
///
/// A name outside the table keeps its ambient spelling; the fold above still collapses it to
/// one entry. Applied only under the case-insensitive rule, i.e. never on POSIX, where the
/// two spellings are genuinely two variables.
fn canonical_env_key(key: &str) -> std::borrow::Cow<'_, str> {
    if key
        .get(..NPM_CONFIG_PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(NPM_CONFIG_PREFIX))
    {
        return std::borrow::Cow::Owned(key.to_ascii_lowercase());
    }
    CANONICAL_ENV_KEYS
        .iter()
        .find(|canonical| canonical.eq_ignore_ascii_case(key))
        .map_or(std::borrow::Cow::Borrowed(key), |canonical| {
            std::borrow::Cow::Borrowed(*canonical)
        })
}

/// Every env name this crate reads from or writes into the reconstructed child env with a
/// fixed spelling. The `npm_config_*` family is a prefix rule above rather than entries here
/// because it is open-ended — `build_prefetch` alone reads a dozen of them.
const CANONICAL_ENV_KEYS: &[&str] = &[
    "INIT_CWD",
    "LIBC",
    "NODE",
    "NODE_COMPAT",
    "NODE_EXECUTABLE",
    NODE_GYP_FORCE_PYTHON,
    "NODE_OPTIONS",
    "PATH",
    "PYTHON",
    "npm_node_execpath",
    // node-gyp's MSVC short-circuit trio, whose stamp is all-three-or-nothing
    // (`jail_msvc`): an ambient spelling surviving beside one of nub's would be exactly the
    // half-nub/half-ambient trio that module refuses to produce.
    "VCINSTALLDIR",
    "VSCMD_VER",
    "WindowsSDKVersion",
];

/// Point npm's global PREFIX at a path INSIDE the jail.
///
/// `npm-conf` — reached at MODULE LOAD by `get-proxy`, which every `bin-wrapper`-style
/// installer requires — stats the prefix only to read `stats.uid`, and its handling is
/// asymmetric (`lib/conf.js:165`):
///
/// ```js
/// catch (err) { if (err.code === 'ENOENT') return; throw err; }
/// ```
///
/// ABSENT is fine; DENIED throws. So a confined child dies at require time on any prefix it
/// cannot stat, taking the hugo-extended / gifsicle / saucectl family with it. Measured on
/// Windows CI: 52 of one run's 56 cell logs carry `EPERM … stat 'C:\npm\prefix'`, ~4x the next
/// most common failure. Windows-only because `%ProgramFiles%` carries
/// `ALL APPLICATION PACKAGES: ReadAndExecute` inheritably while the runner image's drive-root
/// `C:\npm` carries nothing; on POSIX the path is simply absent, hence ENOENT.
///
/// REDIRECTING rather than GRANTING adds NO read surface: the host's prefix is unreachable from
/// inside the jail by construction, so replacing it takes nothing away. The target must be a
/// BASELINE grant — `$cache/nub/pm/tools` is granted at every rung
/// (`preset.rs::NUB_PM_CACHE_PATTERNS`), where the project root is not (readable only from the
/// `read.project` rung up). The leaf need not exist: granted-and-absent yields the handled
/// ENOENT.
///
/// ⛔ THE CASE-VARIANT PURGE IS LOAD-BEARING, NOT TIDINESS. `NPM_CONFIG_PREFIX` is npm's
/// DOCUMENTED env spelling and npm-conf lowercases before merging, so a host-inherited uppercase
/// copy lands on the same config key and WINS over a lowercase insert. Measured against real
/// npm-conf@1.1.3: lowercase alone resolves to ours, uppercase alone to the host's, and with BOTH
/// present the host's uppercase value wins. Inserting only the lowercase spelling would be
/// silently INERT wherever the runner exports the documented one — the exact failure shape this
/// redirect exists to remove.
/// ⛔ AND THE DIRECTORY IS CREATED HERE, BECAUSE npm WILL NOT CREATE IT AND ERRORS ON ITS ABSENCE.
/// The write grant in `preset.rs` lets a confined script mkdir this path, and that is necessary but
/// NOT sufficient: npm's own config machinery `lstat`s the prefix and fails rather than creating
/// it. MEASURED on `use-mask-input@3.3.2` (install script `./scripts.sh postinstall`, which shells
/// npm) under Node 20, jailed:
///
/// ```text
/// npm error code ENOENT
/// npm error syscall lstat
/// npm error path …/pm/tools/npm-prefix
/// ```
///
/// Confirmed the grant itself is fine, so this is not a confinement failure: a canary postinstall
/// on the same path reported `EXISTS=false`, then `MKDIR-OK` and `WRITE-OK`. nub names the path and
/// hands it over, so nub owes its existence — the same obligation `preset.rs` states as "a redirect
/// that hands a package a path is incomplete until that path is writable", one step further.
///
/// It went unseen on POSIX for the reason that comment already records: a measuring host that has
/// run an unjailed install already HAS the directory, so the defect only appears on a genuinely
/// fresh `$HOME` — which is exactly what a per-cell spot-check fixture gives you.
///
/// Failure to create is deliberately IGNORED. It is a best-effort improvement on a path nub owns;
/// if it cannot be made, the pre-existing behaviour is what happens, and refusing the whole
/// lifecycle spawn over it would trade a package-specific break for a total one.
fn redirect_npm_prefix(ambient: &mut BTreeMap<String, String>, cache: &std::path::Path) {
    ambient.retain(|k, _| !k.eq_ignore_ascii_case("npm_config_prefix"));
    let prefix = cache
        .join("nub")
        .join("pm")
        .join("tools")
        .join("npm-prefix");
    // THE STANDARD GLOBAL LAYOUT, not just the root. npm walks into `<prefix>/lib/node_modules`
    // and `<prefix>/bin` and `lstat`s each, so creating only the root moves the error one level
    // down rather than removing it — observed directly: the reported path went from
    // `…/npm-prefix` to `…/npm-prefix/lib` once the root existed. `create_dir_all` makes parents,
    // so these two leaves cover the whole layout.
    for leaf in [prefix.join("lib").join("node_modules"), prefix.join("bin")] {
        let _ = std::fs::create_dir_all(&leaf);
    }
    ambient.insert(
        "npm_config_prefix".to_string(),
        prefix.to_string_lossy().into_owned(),
    );
}

/// Point `@electron/get`'s artifact cache at a path INSIDE the jail.
///
/// `@electron/get` computes its default with `envPaths('electron', {suffix:''}).cache`
/// (`Cache.js`), which resolves to `~/.cache/electron` on Linux, `~/Library/Caches/electron` on
/// macOS, and `%LOCALAPPDATA%\electron\Cache` on WINDOWS. That difference is the whole story:
///
///   POSIX   — the path hangs off HOME, which the jail REDIRECTS, so the download lands inside and
///             the writePaths mover promotes it back. `.cache/electron` and
///             `Library/Caches/electron` appear on 70 corpus records, i.e. the machinery works.
///   WINDOWS — `LOCALAPPDATA` is an AppContainer ESSENTIAL and is passed through UNREDIRECTED
///             (`defaults.rs`: the profile itself lives at `%LOCALAPPDATA%\Packages\…`, so a block
///             missing it fails to start). Only `$cache/nub/pm/tools` is granted beneath it, so the
///             electron cache is outside every rung and the package walks to `write:"disk"`.
///
/// MEASURED on the corpus, electron-chromedriver@43.2.0 [win32], nub 8a49b39413:
///     home/AppData/Local/electron/Cache/<sha>/chromedriver-v43.2.0-win32-x64.zip
///     home/AppData/Local/electron/Cache/<sha>/SHASUMS256.txt
/// and the same package measures `{network}` on macOS and Linux, which is the divergence this
/// removes.
///
/// ⛔ THIS IS A CALLER-SIDE OVERRIDE, NOT A LIBRARY ONE, and that bounds what it fixes.
/// `@electron/get` takes `cacheRoot` as an OPTION; it reads no env var of its own. The env name
/// below is what CONSUMERS forward — electron-chromedriver's `download-chromedriver.js` does
/// exactly `cacheRoot: process.env.electron_config_cache`. A consumer that forwards nothing keeps
/// the default, so this narrows the family rather than closing it. `ELECTRON_CACHE` is set beside
/// it because it is the spelling most other electron-download consumers read; neither is read by
/// `@electron/get` itself — verified by enumerating every `process.env` read in versions 1.14.1,
/// 2.0.3, 3.1.0 and 5.1.0, which between them read only `ELECTRON_GET_NO_PROGRESS`,
/// `ELECTRON_GET_USE_PROXY` and (1.x only) `ELECTRON_CUSTOM_VERSION`. The default when a consumer
/// forwards nothing is `envPaths('electron').cache`, i.e. `%LOCALAPPDATA%\electron\Cache` — which
/// is why this is Windows-shaped: on POSIX the same expression hangs off `HOME`, which the jail
/// already redirects.
///
/// ★ `electron` ITSELF FORWARDS IT — `install.js:46` is `cacheRoot: process.env.electron_config_cache`.
/// That is what sizes this fix: `electron` is ~5.6M weekly downloads against
/// `electron-chromedriver`'s ~33K, so the witness that surfaced the bug is 170× smaller than the
/// package the fix reaches.
///
/// ⛔ REDIRECTING ADDS NO READ SURFACE — the host's `%LOCALAPPDATA%\electron` is unreachable from
/// inside the jail by construction, so replacing the value takes nothing away. The target is under
/// `$cache/nub/pm/tools`, which `preset.rs::NUB_PM_CACHE_PATTERNS` grants at EVERY rung; the
/// project root would not do, being readable only from `read.project` up.
///
/// ⛔ NOT A FIX FOR THE WHOLE AC WITNESS SET, and it should not be described as one. The playwright
/// family needs its own redirect (`redirect_playwright_browsers`, below) — the SAME asymmetry in a
/// different package. (This comment previously called playwright "a different cause"; reading
/// playwright's own registry resolver disproved that, and the correction is recorded there.)
fn redirect_electron_cache(ambient: &mut BTreeMap<String, String>, cache: &std::path::Path) {
    let target = cache
        .join("nub")
        .join("pm")
        .join("tools")
        .join("electron-cache")
        .to_string_lossy()
        .into_owned();
    // Same case-variant purge as the npm prefix: a host-inherited spelling that differs only in
    // case would otherwise sit beside ours, and which one a consumer reads is not ours to predict.
    ambient.retain(|k, _| {
        !k.eq_ignore_ascii_case("electron_config_cache")
            && !k.eq_ignore_ascii_case("ELECTRON_CACHE")
    });
    ambient.insert("electron_config_cache".to_string(), target.clone());
    ambient.insert("ELECTRON_CACHE".to_string(), target);
}

/// Point Playwright's browser registry at a path INSIDE the jail.
///
/// THE SAME PER-OS ASYMMETRY AS `@electron/get`, in a different package — which is the reason this
/// exists as its own function rather than as a line in that one. From playwright's own resolver
/// (`packages/playwright-core/src/server/registry/index.ts`, checkout `287ad47`):
///
///     defaultCacheDirectory = win32 ? process.env.LOCALAPPDATA : (XDG_CACHE_HOME || ~/.cache)
///     defaultRegistryDirectory = path.join(defaultCacheDirectory, 'ms-playwright')
///
/// POSIX hangs off HOME, which the jail REDIRECTS, so the download lands inside and the writePaths
/// mover promotes it back. Windows hangs off `LOCALAPPDATA`, which is an AppContainer ESSENTIAL and
/// is passed through UNREDIRECTED — only `$cache/nub/pm/tools` is granted beneath it, so the
/// browser download is outside every rung and the package walks to `write:"disk"`.
///
/// MEASURED on the corpus: `@playwright/browser-chromium` at latest contributes 199 blocked paths
/// under `%LOCALAPPDATA%\ms-playwright\chromium-1228` — by a wide margin the largest single-package
/// blocked set in the whole Windows tail.
///
/// ⛔ THE `= "0"` BRANCH IS NOT COVERED AND MUST NOT BE CLAIMED AS FIXED. The resolver reads
/// `PLAYWRIGHT_BROWSERS_PATH` and treats the exact string `"0"` as "put browsers in
/// `<packageRoot>/.local-browsers`" — a THIRD location, inside the package's own directory. A
/// package that sets `0` in its own script env overrides this ambient value and keeps that
/// behaviour; that path lives under `node_modules`, which is granted from `write.deps` up, so it
/// should not need this redirect. That reasoning is UNVERIFIED against a measurement, so it is
/// written here as the open question it is rather than as a covered case.
///
/// ⛔ REDIRECTING ADDS NO READ SURFACE — the host's `%LOCALAPPDATA%\ms-playwright` is unreachable
/// from inside the jail by construction, so replacing the value takes nothing away. The target sits
/// under `$cache/nub/pm/tools`, which `preset.rs::NUB_PM_CACHE_PATTERNS` grants at EVERY rung.
fn redirect_playwright_browsers(ambient: &mut BTreeMap<String, String>, cache: &std::path::Path) {
    let target = cache
        .join("nub")
        .join("pm")
        .join("tools")
        .join("ms-playwright")
        .to_string_lossy()
        .into_owned();
    // Same case-variant purge as the npm prefix and the electron cache: a host-inherited spelling
    // differing only in case would otherwise sit beside ours. Windows env lookup is case-insensitive
    // and Node normalizes `process.env` there, so which of two spellings a consumer reads is not ours
    // to predict. (A Windows/Node fact, not a playwright one.)
    //
    // ⛔ THE PURGE ALSO OVERRIDES AN AMBIENT `PLAYWRIGHT_BROWSERS_PATH=0`, a DELIBERATE SENTINEL
    // rather than a stray value: `0` means "put browsers in `<packageRoot>/.local-browsers`". A CI
    // image or a project exporting it tree-wide gets its browsers RELOCATED into
    // `$cache/nub/pm/tools/ms-playwright`. SAFE ON THE GRANT AXIS, which is why it is accepted — that
    // cache path is granted at EVERY rung (`preset.rs::NUB_PM_CACHE_PATTERNS`) whereas
    // `.local-browsers` sits under `node_modules` and needs `write.deps`.
    //
    // ⛔⛔ "SO THIS CAN ONLY WIDEN WHAT THE INSTALL REACHES, NEVER NARROW IT" — THAT CLAIM WAS HERE AND
    // IT IS REFUTED BY MEASUREMENT. `playwright-chromium@0.13.0` [win32] measured **6 cells
    // `{network}` before this redirect went live and 31 cells `{"write":{"userHome":true},"network":
    // true}` after** (corpus, `9c73c07337`). Making the redirect effective made that package need a
    // STRICTLY WIDER grant. The reasoning failed because `$cache` resolves under `LOCALAPPDATA` on
    // Windows — i.e. inside the redirected HOME — so reaching the tools cache costs a `userHome`
    // write, while the old default sat somewhere the package could already reach for free.
    //
    // KEPT ANYWAY, and the trade is deliberate rather than an oversight: the same redirect moved
    // `@playwright/browser-chromium@1.61.1` (~1.3M weekly downloads) from `write:"disk"` — filesystem
    // confinement OFF — to that same `write.userHome`, which is the per-package THROWAWAY home whose
    // contents are discarded. Widening one old, low-traffic version from `network` to a throwaway-home
    // write buys turning confinement back ON for the version people actually install. Revisit if a
    // package with real traffic shows the same regression.
    //
    // What it also changes is WHERE artifacts land, so the writePaths mover promotes them from
    // somewhere the project did not choose. Recorded because "we silently moved your browsers" is a
    // real user-visible consequence.
    ambient.retain(|k, _| !k.eq_ignore_ascii_case("PLAYWRIGHT_BROWSERS_PATH"));
    ambient.insert("PLAYWRIGHT_BROWSERS_PATH".to_string(), target);
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

/// Whether a `process.versions.node` string names a Node that accepts `--import`, the flag
/// the Windows stdio shim is delivered on (landed in 20.6.0). Unparseable ⇒ `false`: the
/// stamp is a repair, and guessing wrong costs a startup abort on every lifecycle script.
#[cfg_attr(not(windows), allow(dead_code))]
fn supports_import(version: &str) -> bool {
    let mut parts = version.split('.').map(str::parse::<u32>);
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => major > 20 || (major == 20 && minor >= 6),
        _ => false,
    }
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

/// The Node the confined script's pin chain asks for, resolved OUT of the jail —
/// `None` when nothing is pinned from `cwd` (leave the ambient interpreter alone) or
/// when resolution fails.
///
/// The gate is deliberately "is there a pin at all", not "does the ambient interpreter
/// satisfy it": answering the second question requires the same resolution as answering
/// the first, and the ambient interpreter satisfying the pin makes this a no-op anyway
/// (discovery returns that same binary, which is already granted).
///
/// FAILURE IS SILENT AND LEAVES THE SPAWN UNCHANGED. This runs on every lifecycle spawn
/// of a pinned package, including offline; surfacing an error here would convert a
/// package that installs fine today — one whose script never invokes `node` — into a hard
/// install failure over a version it was never going to use.
fn pinned_interpreter(cwd: &Path) -> Option<String> {
    let chain = nub_core::node::discovery::resolve_pin_chain(cwd).ok()?;
    // The pin's VALUE is unused — `discover_or_provision_node` re-derives it. Only its
    // existence is the gate, and bailing here is what keeps the unpinned majority free.
    chain.pin.as_ref()?;
    let node = nub_core::node::discovery::discover_or_provision_node(cwd).ok()?;
    Some(node.path.into_string())
}

/// node-gyp's one Python key that outranks `--python` / `npm_config_python`.
const NODE_GYP_FORCE_PYTHON: &str = "NODE_GYP_FORCE_PYTHON";

/// What [`python_toolchain_grant`] resolved: the interpreter to name in the child env,
/// and the read subtrees that make it runnable inside the jail.
struct PythonToolchain {
    executable: String,
    reads: Vec<PathBuf>,
    /// `(major, minor)` as the interpreter itself reported it. Carried because the FLOOR is
    /// not the only bound that matters: an old Node selects an old node-gyp, and an old
    /// node-gyp cannot use a new Python. See [`gyp_python_max_minor`].
    version: (u32, u32),
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
    if !has_gyp_manifest(&spawn.package_dir, GYP_MANIFEST_SEARCH_DEPTH) {
        python_grant_diag(spawn, ambient, &[], &[], None);
        return None;
    }
    let eligible = ProbeScope::new(spawn);
    // An old Node selects an old node-gyp, which cannot use a new Python. See
    // `gyp_python_max_minor`. `None` when the Node major is unknowable, which keeps the
    // pre-existing floor-only behaviour rather than guessing a cap.
    let max_minor = lifecycle_node_major(ambient).and_then(gyp_python_max_minor);
    let candidates = python_candidates(ambient, &eligible, max_minor);
    let mut rejected = Vec::new();
    let mut chosen = candidates.iter().find_map(|candidate| {
        match probe_python(candidate, ambient, &spawn.cwd, &eligible, max_minor) {
            Ok(toolchain) => Some(toolchain),
            Err(stage) => {
                rejected.push(format!("{}->{stage}", candidate.display()));
                None
            }
        }
    });
    // ⛔ NEVER WORSE THAN NO CEILING. If the cap rejected everything, take the best candidate
    // that only failed ON THE CAP: naming a too-new Python reproduces the pre-ceiling behaviour
    // (node-gyp fails inside gyp on `distutils`), whereas naming NOTHING is strictly worse — it
    // sends node-gyp back to its own `python3` search, which on the host that motivated the cap
    // reaches only an ungranted shim and fails earlier with a more confusing message. Measured;
    // see `python_candidates`. Over-granting is the safe direction and so is over-naming.
    if chosen.is_none() && max_minor.is_some() {
        chosen = candidates.iter().find_map(|candidate| {
            probe_python(candidate, ambient, &spawn.cwd, &eligible, None).ok()
        });
        if chosen.is_some() {
            rejected.push("cap-unsatisfiable->fell-back-to-newest".to_string());
        }
    }
    python_grant_diag(spawn, ambient, &candidates, &rejected, chosen.as_ref());
    chosen
}

/// A nub-owned directory holding `python3`/`python` symlinks to the interpreter
/// [`python_toolchain_grant`] already resolved AND already read-granted, for prepending to the
/// child's PATH.
///
/// WHY A DIRECTORY OF SYMLINKS RATHER THAN A WIDER GRANT. The failure this closes is an EXEC
/// denial on a bare `python3` that resolves to a version-manager shim under `$HOME` (see the
/// call site for the measured `hiredis@0.5.0` case). Granting the shim would require its whole
/// re-exec chain; naming the real interpreter under a name bash will find costs one symlink and
/// grants nothing new, because the target is already in the read closure.
///
/// KEYED ON THE INTERPRETER, not the package: the same resolved Python serves every spawn, so
/// the directory is shared and its creation is idempotent. Recreated rather than trusted if it
/// already exists, so a stale symlink from a previously-resolved interpreter cannot survive a
/// toolchain change.
///
/// ⛔ ANCHORED IN NUB'S OWN CACHE, WHICH IS READ-GRANTED AND NEVER WRITE-GRANTED. It is granted
/// through `extra_reads`, and every toolchain grant here is read-only for the reason
/// [`python_toolchain_grant`] states: "a confined script able to modify it would be rewriting the
/// toolchain that compiles the NEXT package". Siting this anywhere a confined script can WRITE
/// would hand it the ability to repoint `python3` at its own binary and poison the next package —
/// a worse hole than the exec denial being fixed.
///
/// POSIX ONLY, DELIBERATELY. A Windows symlink needs `SeCreateSymbolicLinkPrivilege`, which the
/// build jail must never require (zero privilege, no setup command). Windows also does not hit
/// the defect: node-gyp drives MSBuild there, not a make recipe shelling `python3`. Returning
/// `None` leaves the pre-existing behaviour exactly as it was.
#[cfg(unix)]
fn python_path_front_dir(executable: &str, project_root: &std::path::Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let exe = std::path::Path::new(executable);
    if !exe.is_absolute() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    executable.hash(&mut hasher);
    let key = format!("{:016x}", hasher.finish());
    let dir = sandbox_homes(project_root)
        .cache
        .join("nub")
        .join("pm")
        .join("jail-python")
        .join(key);
    std::fs::create_dir_all(&dir).ok()?;
    for name in ["python3", "python"] {
        let link = dir.join(name);
        // A stale link from a previously-resolved interpreter must not win; replace it.
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(exe, &link).ok()?;
    }
    Some(dir)
}

#[cfg(not(unix))]
fn python_path_front_dir(_executable: &str, _project_root: &std::path::Path) -> Option<PathBuf> {
    None
}

/// One `NUB_DIAG_*` line per lifecycle spawn saying why the grant did or did not resolve.
///
/// This is the one step of the build jail whose failure is INVISIBLE from the outside: an
/// unresolved grant simply leaves `npm_config_python` unset, and the break then surfaces as
/// node-gyp reporting its OWN three-route search failing — a symptom that looks identical
/// whether the gate bailed, nothing resolved, or the probe spawn died. Windows is where
/// that distinction has to be made and is the one platform where it cannot be reproduced
/// locally, so the answer has to be readable off a CI log.
///
/// `path_keys` is spelled out rather than assumed because `ambient` is an exact-case map
/// while Windows spells the variable `Path`: a lookup miss there would empty the candidate
/// list without any other trace.
fn python_grant_diag(
    spawn: &aube_util::LifecycleSandboxSpawn,
    ambient: &BTreeMap<String, String>,
    candidates: &[PathBuf],
    rejected: &[String],
    chosen: Option<&PythonToolchain>,
) {
    aube_util::diag::instant_lazy(
        aube_util::diag::Category::Script,
        "build_jail.python_grant",
        || {
            let list = |v: &[String]| {
                if v.is_empty() {
                    "-".into()
                } else {
                    v.join(" ")
                }
            };
            format!(
                "pkg={} manifest={} path_keys={} candidates={} rejected={} chosen={}",
                spawn.package_name.as_deref().unwrap_or("<root>"),
                has_gyp_manifest(&spawn.package_dir, GYP_MANIFEST_SEARCH_DEPTH),
                list(
                    &ambient
                        .keys()
                        .filter(|k| k.eq_ignore_ascii_case("path"))
                        .cloned()
                        .collect::<Vec<_>>()
                ),
                list(
                    &candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                ),
                list(rejected),
                chosen.map_or("<none>", |t| t.executable.as_str()),
            )
        },
    );
}

/// How far below the package root a gyp manifest is looked for. `ssh2` needs 3
/// (`lib/protocol/crypto/binding.gyp`); the extra level is headroom for the same idiom one
/// directory deeper, not a claim any package uses it.
pub(super) const GYP_MANIFEST_SEARCH_DEPTH: usize = 4;

/// Does this package ship a gyp manifest node-gyp could be pointed at?
///
/// NOT ALWAYS AT THE PACKAGE ROOT, which is what a root-only check got wrong: `ssh2` keeps
/// its optional crypto binding in `lib/protocol/crypto/` and its `install.js` runs node-gyp
/// with that as the cwd. The root check therefore read "no native build here", the
/// pre-resolve above never ran, `npm_config_python` was never set, and node-gyp fell through
/// to its bare `python3` PATH walk — which libuv aborts at the first ungranted symlink,
/// because Seatbelt answers a refused symlink read with `EPERM` and that is not in libuv's
/// `ENOENT`/`ENOTDIR`/`EACCES` continue set. So Python resolution failed under the jail and
/// ONLY under the jail, for the one corpus package whose manifest is not at its root.
///
/// BOUNDED, because this is a cost gate in front of an interpreter startup rather than a
/// correctness boundary — a miss costs the pre-resolve, not safety, and [`ProbeScope`] is
/// what bounds what may be executed. `node_modules` is skipped: a dependency's manifest
/// describes that dependency's build, not this one's. Symlinked directories are not
/// descended (`DirEntry::file_type` does not follow them), so the walk cannot loop.
pub(super) fn has_gyp_manifest(dir: &Path, depth: usize) -> bool {
    if dir.join("binding.gyp").exists() {
        return true;
    }
    let Some(depth) = depth.checked_sub(1) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|e| {
        e.file_name() != "node_modules"
            && e.file_type().is_ok_and(|t| t.is_dir())
            && has_gyp_manifest(&e.path(), depth)
    })
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
fn python_candidates(
    ambient: &BTreeMap<String, String>,
    eligible: &ProbeScope,
    max_minor: Option<u32>,
) -> Vec<PathBuf> {
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
    // VERSIONED NAMES, after node-gyp's own two, and only when a ceiling is in force.
    //
    // ⛔ WITHOUT THESE THE CEILING MAKES BREAKAGE WORSE, MEASURED. node-gyp searches `python3`
    // then `python`; on a host whose `python3` is a 3.12+ version-manager shim BOTH resolve to
    // the same too-new interpreter. The ceiling then rejects every candidate, the grant goes
    // unresolved, `npm_config_python` is left unset, and node-gyp's own search finds nothing at
    // all — the shim it would have fallen back to being ungranted and exec-denied. Measured on
    // `hiredis@0.5.0` / Node 10: a `distutils` failure became `Could not find any Python
    // installation to use`. Trading one break for another is not a fix (constraint: the failure
    // mode to avoid is packages breaking).
    //
    // A versioned name is the only way to reach a SECOND interpreter on such a host, and it is
    // something nub can do that node-gyp will not. Newest-first within the cap, so the pick is
    // the newest USABLE Python rather than the oldest present. These names go through the same
    // `resolve` as every other candidate, so `ProbeScope` still refuses anything a dependency
    // could have authored — a package declaring `"bin": {"python3.11": …}` is filtered exactly
    // as the bare spelling already is.
    if let Some(cap) = max_minor {
        for minor in (6..=cap).rev() {
            out.extend(resolve(&format!("python3.{minor}")));
        }
    }
    out.dedup();
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
///
/// `Err` names the STAGE that refused, which the search itself does not need — it exists
/// only so [`python_grant_diag`] can tell an unresolvable candidate apart from one nub
/// could not execute at all. Every variant is equally a skip to the next candidate.
fn probe_python(
    candidate: &Path,
    ambient: &BTreeMap<String, String>,
    cwd: &Path,
    eligible: &ProbeScope,
    max_minor: Option<u32>,
) -> Result<PythonToolchain, &'static str> {
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
                .map_err(|_| "path-join")?
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
        .map_err(|_| "spawn")?;
    let stdout = read_bounded(&mut child, PYTHON_PROBE_TIMEOUT).ok_or("no-output")?;
    // Gated on PARSING, not exit status: the four contract lines are flushed before the
    // introspection tail, so a candidate that answered them is usable even if the tail
    // died. Anything that is not a Python 3.6+ interpreter fails the parse.
    let text = String::from_utf8(stdout).map_err(|_| "non-utf8")?;
    let toolchain = python_reads(&text).ok_or("unparseable")?;
    // The CEILING, applied to what came back rather than to the candidate's spelling — the
    // version is only knowable from the interpreter's own answer. Rejecting here falls through
    // to the next candidate in node-gyp's own order, which is what lets a host holding both
    // 3.14 and 3.11 resolve the one the selected node-gyp can actually use.
    if let Some(max_minor) = max_minor
        && toolchain.version.0 == 3
        && toolchain.version.1 > max_minor
    {
        return Err("too-new-for-gyp");
    }
    // Re-gate what came BACK. The probe's answer decides both the interpreter nub names
    // and the tree it read-grants, so a resolution that lands inside the dependency tree
    // by any route must not become either — independent of how it got there.
    let cleared = eligible.allows(Path::new(&toolchain.executable))
        && toolchain.reads.iter().all(|p| eligible.allows(p));
    cleared.then_some(toolchain).ok_or("scope-rejected")
}

/// Unlike node-gyp's own Python spawns, this one runs OUTSIDE the jail and outside the
/// monitor that reaps it, so a candidate that never exits would wedge the install with
/// nothing to collect it. Generous for a process whose whole job is printing four lines.
const PYTHON_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Collect `child`'s stdout, killing it if it outlives `timeout`. The reader runs on its
/// own thread because a child that fills the pipe blocks until someone drains it — polling
/// `try_wait` alone would deadlock against exactly the hang this bounds.
pub(super) fn read_bounded(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
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
    let version = (major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?);
    if version < (3, 6) {
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
        version,
    })
}

/// The highest Python MINOR the node-gyp that will actually run can use, or `None` for no bound.
///
/// ⛔ THE FLOOR WAS NEVER THE ONLY BOUND, AND THE MISSING CEILING BREAKS REAL PACKAGES.
/// [`python_reads`] enforces node-gyp's own `>=3.6` floor and stopped there, so on a host whose
/// newest Python is 3.12+ the first eligible candidate wins even when the selected node-gyp
/// cannot use it. Python 3.12 REMOVED `distutils`, which the gyp vendored in node-gyp <=9
/// imports.
///
/// MEASURED on `hiredis@0.5.0` with era Node 10 (which selects the node-gyp v8 bucket): nub
/// resolved Python 3.14.6 and the build died `ModuleNotFoundError: No module named 'distutils'`.
/// Re-run with `npm_config_python` pointed at 3.11.9 by hand, it compiled and linked. So the
/// interpreter choice, not the package, was the failure.
///
/// This is the SAME two-axis constraint `node_gyp_bootstrap` already documents from the other
/// side — it picks a node-gyp old enough to RUN on the ambient Node, and that older node-gyp then
/// needs an older Python. So the bands mirror `bucket_for`'s exactly, and there are TWO of them:
///
/// | ambient Node | node-gyp | ceiling | what breaks above it |
/// |---|---|---|---|
/// | `<=9` | 5 | **3.10** | gyp 5 opens `binding.gyp` with mode `'rU'`, REMOVED in Python 3.11 |
/// | `10..=15` | 8 or 9 | **3.11** | gyp `<=9` imports `distutils`, REMOVED in Python 3.12 |
/// | `>=16` | `>=10` | none | gyp-next dropped both dependencies |
///
/// BOTH boundaries are measured, not read off a changelog, and the second was found only because
/// the first was set too high. `hiredis@0.5.0` on Node 10 (bucket v8) died `ModuleNotFoundError:
/// No module named 'distutils'` on Python 3.14 and built on 3.11.9. `contextify@1.0.0` on Node 8
/// (bucket v5) then died `ValueError: invalid mode: 'rU'` on that same 3.11.9. Confirmed
/// directly: `python3.11 -c "open(f,'rU')"` raises, `python3.10` accepts.
///
/// Deliberately expressed as a CAP ON CANDIDATES rather than a pinned interpreter: rejecting a
/// too-new Python lets the search fall through to the next candidate in node-gyp's own order, so
/// a host with a suitable Python resolves normally. It never pins an interpreter npm would not
/// have used, and [`python_candidates`] widens to versioned names so the fall-through has
/// somewhere to land.
fn gyp_python_max_minor(node_major: u64) -> Option<u32> {
    match node_major {
        0..=9 => Some(10),
        10..=15 => Some(11),
        _ => None,
    }
}

/// The Node major the lifecycle scripts will run under, cached for the process.
///
/// Asked of the `node` the SCRIPTS resolve (`$NODE`, which the jail sets to the real provisioned
/// binary before anything reads `npm_node_execpath`), not of nub's own process — under an
/// embedder those differ, and `node_gyp_bootstrap::ambient_node_major` makes the same
/// distinction for the same reason. Cached because it cannot change within one install and the
/// spawn would otherwise repeat per lifecycle script.
fn lifecycle_node_major(ambient: &BTreeMap<String, String>) -> Option<u64> {
    static CACHED: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let node = ambient
            .get("NODE")
            .or_else(|| ambient.get("npm_node_execpath"))?;
        let out = std::process::Command::new(node)
            .arg("--version")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .trim_start_matches('v')
            .split('.')
            .next()?
            .parse()
            .ok()
    })
}

/// The matcher's canonicalizer, NOT `std::fs::canonicalize`. On Windows the latter is
/// `GetFinalPathNameByHandleW(VOLUME_NAME_DOS)`, which always returns the `\\?\` verbatim
/// form — a spelling nothing downstream expects. Everything derived here is consumed as a
/// PLAIN path: `npm_config_python` is parsed by node-gyp and the tools it spawns, the read
/// grants are slash-normalized by the compiler where the `?` would re-read as a glob
/// metachar and get the grant DROPPED, and [`ProbeScope`] compares it against raw
/// (never-verbatim) candidate spellings. `canonicalize_including_nonexistent` strips the
/// prefix (see its doc for the volume-GUID case it deliberately leaves alone) and resolves
/// a path whose tail does not exist yet instead of returning it untouched.
fn canonical(path: &Path) -> PathBuf {
    nub_sandbox::matcher::path::canonicalize_including_nonexistent(path)
}

/// Whether a derived path may become a read grant at all.
///
/// Refused: anything at or one level below the filesystem root (`sys.prefix` is `/usr` for
/// a distro Python, which every backend's system floor already covers), the user's HOME or
/// an ancestor of it (a version manager installed as `~/x -> ~/opt/x` would otherwise hand
/// a hop grant on the whole home directory), and any path still carrying `..`/`.`. That
/// last one is belt-and-braces since `canonical` collapses a traversal even through a
/// non-existent tail, and it stays because the cost of a surviving `..` is a grant that
/// collapses later, inside the policy compiler, landing on `/`.
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

    /// The gate on the Windows stdio-shim stamp. Getting it wrong in the permissive
    /// direction does not degrade the repair, it aborts Node at startup for every lifecycle
    /// script — so the boundary and the unparseable case are both pinned.
    #[test]
    fn only_a_node_that_accepts_import_gets_the_shim_stamp() {
        assert!(!supports_import("18.19.0"));
        assert!(!supports_import("20.5.1"));
        assert!(supports_import("20.6.0"));
        assert!(supports_import("22.23.1"));
        assert!(!supports_import(""));
        assert!(!supports_import("v20.6.0"));
    }

    fn effective_env(case_insensitive: bool, pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut env = EffectiveEnv {
            inner: BTreeMap::new(),
            case_insensitive,
        };
        for (k, v) in pairs {
            env.set((*k).to_string(), (*v).to_string());
        }
        env.into_map()
    }

    /// ⛔ THE FAILURE MODE IS SILENT INERTNESS, not an error. `@electron/get` takes `cacheRoot` as
    /// an OPTION and reads no env var itself, so the value only bites through a consumer that
    /// forwards it — leaving a host-inherited spelling in place would simply keep the old cache
    /// root with nothing to show for it. So assert that NO case-variant of the host's value
    /// survives, and that the target is a BASELINE-granted `$cache/nub/pm/tools` path: merely
    /// checking ours is present passes while the redirect does nothing.
    /// The playwright redirect must leave exactly ONE spelling behind and must not preserve the
    /// host's registry root: `getFromENV` is case-insensitive on Windows, so a surviving
    /// host-inherited variant could be the one playwright actually reads.
    #[test]
    fn the_playwright_redirect_removes_every_inherited_case_variant() {
        let host = r"C:\Users\dev\AppData\Local\ms-playwright";
        let mut ambient = BTreeMap::from([
            ("PLAYWRIGHT_BROWSERS_PATH".to_string(), host.to_string()),
            ("playwright_browsers_path".to_string(), host.to_string()),
            ("Playwright_Browsers_Path".to_string(), host.to_string()),
            (
                "npm_config_python".to_string(),
                "/usr/bin/python3".to_string(),
            ),
        ]);

        redirect_playwright_browsers(&mut ambient, std::path::Path::new("/cache"));

        let survivors: Vec<&String> = ambient
            .keys()
            .filter(|k| k.eq_ignore_ascii_case("PLAYWRIGHT_BROWSERS_PATH"))
            .collect();
        assert_eq!(
            survivors.len(),
            1,
            "exactly one spelling may survive or the host's may win; got {survivors:?}"
        );

        let v = ambient
            .get("PLAYWRIGHT_BROWSERS_PATH")
            .expect("redirect must set PLAYWRIGHT_BROWSERS_PATH");
        assert_ne!(v, host, "still carries the host's registry root");
        assert!(
            v.contains("nub") && v.contains("pm") && v.contains("tools"),
            "target must sit under the always-granted $cache/nub/pm/tools; got {v}"
        );
        // The redirect owns its own variable and nothing else.
        assert_eq!(
            ambient.get("npm_config_python").map(String::as_str),
            Some("/usr/bin/python3"),
            "an unrelated npm_config_* must survive untouched"
        );
    }

    #[test]
    fn the_electron_cache_redirect_removes_every_inherited_case_variant() {
        let host = r"C:\Users\dev\AppData\Local\electron\Cache";
        let mut ambient = BTreeMap::from([
            ("ELECTRON_CACHE".to_string(), host.to_string()),
            ("electron_config_cache".to_string(), host.to_string()),
            ("Electron_Config_Cache".to_string(), host.to_string()),
            (
                "npm_config_python".to_string(),
                "/usr/bin/python3".to_string(),
            ),
        ]);

        redirect_electron_cache(&mut ambient, std::path::Path::new("/cache"));

        for name in ["electron_config_cache", "ELECTRON_CACHE"] {
            let survivors: Vec<&String> = ambient
                .keys()
                .filter(|k| k.eq_ignore_ascii_case(name))
                .collect();
            assert_eq!(
                survivors.len(),
                1,
                "exactly one spelling of {name} may survive or the host's may win; got {survivors:?}"
            );
        }
        for name in ["electron_config_cache", "ELECTRON_CACHE"] {
            let v = ambient.get(name).expect("redirect must set {name}");
            assert_ne!(v, host, "{name} still carries the host's cache root");
            assert!(
                v.contains("nub") && v.contains("tools"),
                "{name} must land under the baseline-granted $cache/nub/pm/tools, got {v}"
            );
        }
        assert!(
            ambient.contains_key("npm_config_python"),
            "the purge must not disturb unrelated vars"
        );
    }

    /// The redirect's failure mode is SILENT INERTNESS rather than an error: npm documents the
    /// UPPERCASE spelling, npm-conf lowercases before merging, and with both present the
    /// INHERITED one wins. So what has to be asserted is that no case-variant of the host's
    /// value SURVIVES — merely checking that ours is present passes while the fix does nothing.
    #[test]
    fn the_npm_prefix_redirect_removes_every_inherited_case_variant() {
        let mut ambient = BTreeMap::from([
            (
                "NPM_CONFIG_PREFIX".to_string(),
                r"C:\npm\prefix".to_string(),
            ),
            (
                "npm_config_prefix".to_string(),
                r"C:\npm\prefix".to_string(),
            ),
            (
                "npm_config_python".to_string(),
                "/usr/bin/python3".to_string(),
            ),
        ]);

        redirect_npm_prefix(&mut ambient, std::path::Path::new("/cache"));

        let survivors: Vec<&String> = ambient
            .keys()
            .filter(|k| k.eq_ignore_ascii_case("npm_config_prefix"))
            .collect();
        assert_eq!(
            survivors.len(),
            1,
            "exactly one spelling may survive or the host's overrides ours; got {survivors:?}"
        );
        let expected = std::path::Path::new("/cache")
            .join("nub")
            .join("pm")
            .join("tools")
            .join("npm-prefix");
        assert_eq!(
            ambient.get("npm_config_prefix").map(String::as_str),
            Some(expected.to_string_lossy().as_ref()),
            "the redirect must land under $cache/nub/pm/tools, a baseline grant at every rung"
        );
        assert_eq!(
            ambient.get("npm_config_python").map(String::as_str),
            Some("/usr/bin/python3"),
            "an unrelated npm_config_* key must be left alone by the purge"
        );
    }

    /// The two spellings that are not hypothetical: `Path` is Windows' OWN spelling of the
    /// search path, and `NPM_CONFIG_<KEY>` is npm's documented env form. Both were invisible
    /// to the exact-case lookups this module makes — `python_candidates` reads `PATH` and
    /// `npm_config_python`, and a miss on either silently empties the interpreter search
    /// rather than failing.
    #[test]
    fn the_windows_rule_folds_the_two_spellings_windows_and_npm_actually_use() {
        let raw = &[
            ("Path", r"C:\Windows"),
            ("NPM_CONFIG_PYTHON", r"C:\Python312\python.exe"),
            ("NPM_CONFIG_NODEDIR", r"C:\hdrs"),
        ];

        let win = effective_env(true, raw);
        assert_eq!(win.get("PATH").map(String::as_str), Some(r"C:\Windows"));
        assert_eq!(
            win.get("npm_config_python").map(String::as_str),
            Some(r"C:\Python312\python.exe"),
            "an uppercase npm config key must reach the lookup that reads it"
        );
        assert!(
            win.contains_key("npm_config_nodedir"),
            "the set-if-absent gate must see a user-set nodedir, not insert over it"
        );

        // POSIX spellings are genuinely distinct variables, so nothing is rewritten there.
        let posix = effective_env(false, raw);
        assert!(posix.contains_key("Path") && !posix.contains_key("PATH"));
        assert!(posix.contains_key("NPM_CONFIG_PYTHON"));
    }

    /// Under the Windows rule a name has ONE entry however it was spelled. Two surviving
    /// spellings is the shape that reaches a .NET consumer down the chain as
    /// `ArgumentException: Item has already been added` (dotnet/msbuild#5726), and — before
    /// that — leaves which value the child gets to exact-case map order.
    #[test]
    fn the_windows_rule_keeps_one_entry_per_name_and_the_last_write_wins() {
        let folded = effective_env(
            true,
            &[
                ("HTTP_PROXY", "http://first"),
                ("http_proxy", "http://second"),
            ],
        );
        assert_eq!(folded.len(), 1);
        // Not in CANONICAL_ENV_KEYS, so the spelling is the last writer's, not a rewrite.
        assert_eq!(
            folded.get("http_proxy").map(String::as_str),
            Some("http://second")
        );
    }

    /// A removal in the spawn's env delta has to honour the same name-equality rule as a
    /// set, or a variable aube meant to clear survives under another case.
    #[test]
    fn the_windows_rule_unsets_a_variable_however_it_was_spelled() {
        let mut env = EffectiveEnv {
            inner: BTreeMap::new(),
            case_insensitive: true,
        };
        env.set("Path".to_string(), r"C:\Windows".to_string());
        env.set("SomeVar".to_string(), "v".to_string());
        env.unset("path");
        env.unset("SOMEVAR");
        assert!(env.into_map().is_empty());
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
                python_candidates(&forced, &any_scope(), None),
                vec![PathBuf::from("/bin/sh")],
                "NODE_GYP_FORCE_PYTHON short-circuits the whole search"
            );

            let configured = ambient(&[("npm_config_python", "/bin/echo"), ("PYTHON", "/bin/cat")]);
            assert_eq!(
                python_candidates(&configured, &any_scope(), None),
                vec![PathBuf::from("/bin/echo"), PathBuf::from("/bin/cat")],
                "--python outranks PYTHON"
            );

            assert!(
                python_candidates(&ambient(&[("PATH", "/nonexistent")]), &any_scope(), None)
                    .is_empty(),
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
                python_candidates(&ambient(&[("PATH", &path)]), &scope, None),
                vec![system.join("python3"), system.join("python")],
                "the planted bin must be skipped and the search continue"
            );
            assert_eq!(
                python_candidates(
                    &ambient(&[(
                        "npm_config_python",
                        &planted.join("python3").to_string_lossy()
                    )]),
                    &scope,
                    None
                ),
                Vec::<PathBuf>::new(),
                "a project-local .npmrc must not be able to name it either"
            );
            assert!(
                !scope.allows(Path::new("relative/python3")),
                "a relative candidate resolves against nub's cwd, not the child's"
            );
        }

        /// An old Node selects an old node-gyp, and node-gyp <=9 imports the `distutils` that
        /// Python 3.12 removed — so the interpreter search needs a CEILING as well as its
        /// `>=3.6` floor. Measured on `hiredis@0.5.0` under era Node 10: nub resolved Python
        /// 3.14.6 and the build died `ModuleNotFoundError: No module named 'distutils'`.
        ///
        /// The band boundary is `bucket_for`'s: Node <=15 takes node-gyp <=9 and needs the cap;
        /// Node >=16 takes node-gyp >=10, whose gyp-next dropped `distutils` and is uncapped.
        #[test]
        fn an_old_node_caps_the_python_it_will_accept() {
            // node-gyp 5 needs 3.10: its gyp opens binding.gyp with mode 'rU', gone in 3.11.
            for node_major in [0, 8, 9] {
                assert_eq!(
                    gyp_python_max_minor(node_major),
                    Some(10),
                    "Node {node_major} selects node-gyp 5, whose gyp uses mode 'rU'"
                );
            }
            // node-gyp 8/9 need 3.11: they import distutils, gone in 3.12.
            for node_major in [10, 12, 15] {
                assert_eq!(
                    gyp_python_max_minor(node_major),
                    Some(11),
                    "Node {node_major} selects node-gyp 8 or 9, which import distutils"
                );
            }
            // Uncapped, because these select node-gyp >=10.
            for node_major in [16, 18, 20, 22, 26] {
                assert_eq!(
                    gyp_python_max_minor(node_major),
                    None,
                    "Node {node_major} selects node-gyp >=10, which has no distutils dependency"
                );
            }
        }

        /// `ssh2`'s layout, which a root-only gate missed: the manifest sits three levels
        /// down (`lib/protocol/crypto/binding.gyp`) because `install.js` runs node-gyp with
        /// that directory as the cwd. Missing it skipped the Python pre-resolve, so node-gyp
        /// walked PATH by bare name and died `EPERM` on the first ungranted symlink.
        #[test]
        fn a_gyp_manifest_below_the_package_root_still_gates_open() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path();

            assert!(
                !has_gyp_manifest(root, GYP_MANIFEST_SEARCH_DEPTH),
                "a package with no manifest anywhere must not pay the interpreter startup"
            );

            let nested = root.join("lib/protocol/crypto");
            std::fs::create_dir_all(&nested).expect("nested dir");
            std::fs::write(nested.join("binding.gyp"), b"{}").expect("manifest");
            assert!(
                has_gyp_manifest(root, GYP_MANIFEST_SEARCH_DEPTH),
                "ssh2 keeps binding.gyp at lib/protocol/crypto, not the package root"
            );

            // A dependency's manifest describes ITS build, not this package's, so finding
            // one under node_modules must not open the gate.
            let dep = tmp.path().join("dep");
            let buried = dep.join("node_modules/other");
            std::fs::create_dir_all(&buried).expect("dep dir");
            std::fs::write(buried.join("binding.gyp"), b"{}").expect("dep manifest");
            assert!(
                !has_gyp_manifest(&dep, GYP_MANIFEST_SEARCH_DEPTH),
                "node_modules is skipped"
            );

            // The bound is real: a manifest deeper than the cap is not found, which costs
            // the pre-resolve rather than correctness.
            let deep = tmp.path().join("deep");
            let far = deep.join("a/b/c/d/e");
            std::fs::create_dir_all(&far).expect("deep dir");
            std::fs::write(far.join("binding.gyp"), b"{}").expect("deep manifest");
            assert!(
                !has_gyp_manifest(&deep, GYP_MANIFEST_SEARCH_DEPTH),
                "the walk is bounded"
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

    /// `allowBuilds: "no-jail"` is a ROOT-authored, per-package request to run unconfined — and only
    /// that exact spelling, only on an exact name.
    ///
    /// ⛔ WHY THE SCOPE MATTERS MORE THAN THE SPELLING. The per-package opt-out that was REMOVED was
    /// `dependenciesMeta.<name>.sandbox`, whose hazard is that `dependenciesMeta` is DEPENDENCY-authored:
    /// a package could ship a manifest disabling its own confinement, which is strictly worse than no
    /// jail because it advertises a protection that is not there. This reads the ROOT project's
    /// `package.json` only. The assertion that a dependency's own manifest is never consulted is the
    /// security property, so it is tested explicitly rather than left to the reader.
    #[test]
    fn no_jail_is_root_authored_exact_and_only_that_spelling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"r","version":"1.0.0","pnpm":{"allowBuilds":{
                 "wants-out":"no-jail","ordinary":true,"denied":false,"typo":"nojail"}}}"#,
        )
        .expect("write manifest");

        assert!(root_opted_out_of_jail(root, "wants-out"), "an exact `no-jail` entry must opt out");
        assert!(!root_opted_out_of_jail(root, "ordinary"), "`true` means run CONFINED, not unconfined");
        assert!(!root_opted_out_of_jail(root, "denied"), "`false` must never be read as an opt-out");
        // A near-miss must not work: the placeholder is compared exactly, so a typo fails CLOSED
        // (confined) rather than silently unconfining.
        assert!(!root_opted_out_of_jail(root, "typo"), "`nojail` is not `no-jail` and must fail closed");
        assert!(!root_opted_out_of_jail(root, "absent"), "a package with no entry stays confined");

        // A DEPENDENCY's own manifest must be irrelevant, even when it names itself.
        let dep = root.join("node_modules").join("selfish");
        std::fs::create_dir_all(&dep).expect("mkdir dep");
        std::fs::write(
            dep.join("package.json"),
            r#"{"name":"selfish","version":"1.0.0","pnpm":{"allowBuilds":{"selfish":"no-jail"}}}"#,
        )
        .expect("write dep manifest");
        assert!(
            !root_opted_out_of_jail(root, "selfish"),
            "a dependency-authored no-jail must be ignored — it is read from the ROOT manifest only"
        );
    }

}
