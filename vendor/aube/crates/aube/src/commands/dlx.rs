use super::install::{FrozenMode, InstallOptions};
use crate::commands::add::build_flags::parse_allow_build_value;
use aube_manifest::AllowBuildRaw;
use clap::{Args, CommandFactory};
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Args)]
// dlx forwards everything after `<command>` to the bin it runs, including
// `--help` and `--version`. Let clap auto-inject its own `-h`/`--help` and
// `--version` handlers and they'd silently swallow those flags before they
// reach the binary — users would see aube's help screen instead of the
// tool's. Disable clap's built-in flags on this subcommand.
//
// `aube dlx --help` on its own (no command) still prints aube's dlx help:
// `params` is optional and the handler intercepts a leading `--help` /
// `-h` before treating anything as a command.
#[command(disable_help_flag = true)]
pub struct DlxArgs {
    /// Command (binary) to run, followed by arguments to pass through to
    /// it.
    ///
    /// The first positional is the command; the rest are forwarded
    /// verbatim to the binary. Without `--package`, a local
    /// `node_modules/.bin/<command>` wins when present; otherwise dlx
    /// installs into a throwaway project. Under `--shell-mode`/`-c` the
    /// positionals are joined and evaluated by `sh -c` instead of
    /// looked up directly.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub params: Vec<String>,
    /// Run the assembled command line through `sh -c`.
    ///
    /// `<scratch>/node_modules/.bin` is prepended to `PATH`. Use this
    /// for pipelines, redirects, or env expansion (`aube dlx -p cowsay
    /// -c 'cowsay hello | tr a-z A-Z'`). Mirrors `pnpm dlx --shell-mode`.
    #[arg(short = 'c', long)]
    pub shell_mode: bool,
    /// Install a specific package (repeatable).
    ///
    /// Overrides inferring from the command.
    #[arg(short = 'p', long = "package")]
    pub package: Vec<String>,
    /// Allow named packages to run lifecycle scripts during the
    /// transient install. Use `--allow-build=<pkg>`.
    ///
    /// Repeatable — pass once per package. The named package's
    /// `preinstall` / `install` / `postinstall` scripts execute during
    /// the transient install. Requires the equals form
    /// (`--allow-build=<pkg>`); space-separated forms are rejected.
    /// Mirrors pnpm's `pnpm dlx --allow-build=<pkg>` compatibility
    /// surface while keeping dlx scripts skipped unless explicitly
    /// approved.
    #[arg(
        long = "allow-build",
        value_name = "PKG",
        require_equals = true,
        value_parser = parse_allow_build_value,
    )]
    pub allow_build: Vec<String>,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

/// `aube dlx [-p <pkg>]... <command> [args...]`
///
/// Install one or more packages into a throwaway project and run a binary
/// from them. Matches pnpm's `pnpm dlx` / npm's `npx` surface.
///
/// Flow:
///   1. Create a fresh tempfile::TempDir project with a minimal package.json.
///   2. Run the normal install pipeline there under a CwdGuard that restores
///      the original cwd on drop — including the panic path — so a crash
///      inside install::run can't leave the process with its cwd pointed at
///      an already-removed scratch dir.
///   3. Exec `<tmp>/node_modules/.bin/<command>` from the user's original cwd.
///   4. tempfile removes the scratch dir on drop.
pub async fn run(args: DlxArgs) -> miette::Result<Option<i32>> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let DlxArgs {
        params,
        package,
        allow_build,
        shell_mode,
        lockfile: _,
        network: _,
        virtual_store: _,
    } = args;

    // Bare `aube dlx` or `aube dlx --help` / `-h` prints aube's dlx help.
    // Once a command is present, any further flags (including `--help`)
    // belong to the installed binary.
    let first = params.first().map(String::as_str);
    if matches!(first, None | Some("--help" | "-h")) && package.is_empty() {
        crate::Cli::command()
            .find_subcommand_mut("dlx")
            .expect("dlx is a registered subcommand")
            .print_help()
            .map_err(|e| miette!("failed to render help: {e}"))?;
        println!();
        return Ok(None);
    }

    // When only `-p` is given, dlx needs at least one arg to serve as the
    // bin name; without it, we don't know which binary to exec.
    let command = params
        .first()
        .cloned()
        .ok_or_else(|| miette!("dlx: missing command to run"))?;
    let bin_args: Vec<String> = params.iter().skip(1).cloned().collect();

    // Remember whether `-p` was given. With `-p` the user has named the
    // bin explicitly (`aube dlx -p which node-which`), so we run their
    // command verbatim. Without `-p` the command doubles as the package
    // name and we may need to cross-reference the installed package's
    // `bin` map — e.g. `@tanstack/cli` ships its bin under the name
    // `tanstack`, not `cli`.
    let explicit_package = !package.is_empty();

    // Derive the packages to install. `-p` wins; otherwise the command name
    // is the package name (the common `pnpm dlx <pkg>` case). Under
    // `--shell-mode` the first positional is a shell line, not a bin name,
    // so we fall back to the first whitespace-separated word for inference
    // when `-p` wasn't given — same as pnpm.
    let install_specs: Vec<String> = if package.is_empty() {
        if shell_mode {
            let first_word = command
                .split_whitespace()
                .next()
                .ok_or_else(|| miette!("dlx --shell-mode: missing command line to run"))?;
            vec![first_word.to_string()]
        } else {
            vec![command.clone()]
        }
    } else {
        package
    };

    // Bin name is only used in the non-shell path. Under shell-mode the
    // user assembles their own line and we run it through `sh -c`, so any
    // bin lookup is the shell's job.
    let bin_name = bin_name_for(&command);
    if !explicit_package && !shell_mode && can_use_local_bin(&command) {
        let initial_cwd = crate::dirs::cwd()?;
        if let Some(project_dir) = crate::dirs::find_project_root(&initial_cwd) {
            let bin_path = super::project_modules_dir(&project_dir)
                .join(".bin")
                .join(&bin_name);
            if bin_path.exists() {
                return super::exec::exec_bin(&initial_cwd, &bin_path, &bin_name, &bin_args, false)
                    .await;
            }
        }
    }

    // The user's real working directory, captured before we switch into the
    // scratch dir below. Project-scope config (`.nvmrc`, `.npmrc`) is read
    // from here, not from the throwaway scratch project.
    let user_cwd = crate::dirs::cwd()?;

    // Resolve the runtime from the *user's* project before switching
    // into the scratch dir — a dlx scratch project has no version
    // config, and the OnceCell context is process-global, so resolving
    // here (original cwd) is what makes `aubx` honor the project's
    // .nvmrc / devEngines.
    crate::runtime::ensure_for_cwd(&user_cwd).await?;

    let tmp = tempfile::Builder::new()
        .prefix("aube-dlx-")
        .tempdir()
        .into_diagnostic()
        .wrap_err("failed to create dlx scratch dir")?;
    let project_dir = tmp.path().to_path_buf();

    let manifest = dlx_manifest(&install_specs, &allow_build);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).into_diagnostic()?;
    aube_util::fs_atomic::atomic_write(&project_dir.join("package.json"), &manifest_bytes)
        .into_diagnostic()
        .wrap_err("failed to write dlx package.json")?;

    // Carry the user's *project-scope* `.npmrc` into the scratch project, so
    // the transient install reads the same project config npm's `npx` does:
    // the project registry, scoped registries + auth, and supply-chain knobs
    // like `minimumReleaseAge` all apply to a dlx'd tool, not only the
    // user-scope `~/.npmrc` + built-in defaults. Without this the scratch dir
    // (under TMPDIR) has no project config and a per-project override would
    // silently not apply to `aube dlx` / `aubx`. User- and global-scope npmrc
    // are read by `install::run` regardless of cwd (they are not walked up
    // from the project dir), so only the project scope needs forwarding —
    // mirroring how the runtime resolution above is taken from the user's
    // project rather than the scratch dir.
    write_project_npmrc_into_scratch(&user_cwd, &project_dir)?;

    // install::run pulls its project dir from std::env::current_dir(), which
    // is process-global state. The CwdGuard below captures the current dir,
    // switches into the scratch project for the duration of the install, and
    // restores the original on drop — so the exec path below, any error
    // diagnostic rendering, and even a panic unwinding past this frame all
    // observe the user's real cwd instead of a dir that's about to vanish.
    // Pin `project_dir` to the scratch path explicitly so install::run
    // does not walk upward looking for a workspace root — the scratch
    // dir lives under TMPDIR, which inside a parent workspace would
    // otherwise resolve to that workspace and install the wrong tree.
    let prev_cwd = {
        let _cwd_guard = CwdGuard::switch_to(&project_dir)?;
        let mut opts = dlx_install_options(&allow_build);
        opts.project_dir = Some(project_dir.clone());
        let install_result = super::install::run(opts).await;
        let prev = _cwd_guard.original.clone();
        install_result.wrap_err("dlx install failed")?;
        prev
        // _cwd_guard drops here, restoring cwd.
    };

    // Run from the user's original cwd so the invoked tool sees their
    // project, not the scratch dir — this matches pnpm dlx.
    //
    // Under `--shell-mode` we evaluate the joined positionals via `sh -c`
    // with the scratch project's `node_modules/.bin` prepended to PATH,
    // so pipelines/redirects work and the freshly installed bin
    // resolves first. Otherwise we exec the bin directly so its argv
    // round-trips bit-for-bit.
    let status = if shell_mode {
        // In shell-mode the positionals are literal shell-line fragments
        // that the caller explicitly wants sh to interpret (pipes,
        // redirects, subshells). Matches pnpm dlx --shell-mode and is
        // why this call does not shell-quote like aube exec --shell-mode
        // does. Join with space preserves that contract.
        let line = std::iter::once(command.as_str())
            .chain(bin_args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        // `dlx` installs into a scratch tempdir, which honors `modulesDir`
        // from the user's `~/.npmrc` / `aube-workspace.yaml` if it's set
        // globally. Read the same setting here so the scratch bin dir
        // matches where the install actually wrote the bins.
        let bin_dir = super::project_modules_dir(&project_dir).join(".bin");
        let mut path_dirs = vec![bin_dir];
        path_dirs.extend(crate::runtime::path_entries());
        let new_path = aube_scripts::prepend_paths(&path_dirs);
        let mut cmd = aube_scripts::spawn_shell(&line);
        crate::runtime::apply_child_env(&mut cmd);
        cmd.env("PATH", &new_path)
            .current_dir(&prev_cwd)
            .stderr(aube_scripts::child_stderr())
            .status()
            .await
            .into_diagnostic()
            .wrap_err("failed to execute dlx shell command")?
    } else {
        let modules_dir = super::project_modules_dir(&project_dir);
        let bin_dir = modules_dir.join(".bin");
        // If `-p` wasn't given, the command doubles as the package name
        // and the bin is a best-guess derivation from it. Check the
        // installed package's `bin` field and prefer the actual bin name
        // it ships — e.g. `@tanstack/cli` ships `tanstack`, not `cli`.
        let resolved_bin_name = if !explicit_package && !bin_dir.join(&bin_name).exists() {
            let (pkg_name, _) = synthesize_dlx_dep(&install_specs[0]);
            resolve_bin_from_package(&modules_dir, &pkg_name).unwrap_or_else(|| bin_name.clone())
        } else {
            bin_name.clone()
        };
        let bin_path = bin_dir.join(&resolved_bin_name);
        if !bin_path.exists() {
            return Err(miette!(
                "dlx: binary not found after install: {resolved_bin_name}\n\
                 help: the package may ship the binary under a different name — try `{} -p <package> <bin>`",
                aube_util::cmd("dlx")
            ));
        }
        // The linker writes three shims for every bin on Windows:
        // `<name>.cmd`, `<name>.ps1`, and a bare extensionless sh shim
        // (for use under bash / git-bash). CreateProcess can only
        // launch real PE executables and `.cmd`/`.bat` files — handing
        // it the sh shim fails with `%1 is not a valid Win32 application`
        // (os error 193). Prefer the `.cmd` shim on Windows; on Unix
        // the bare shim is the executable.
        let exec_path = super::exec::resolve_exec_shim(&bin_path);
        let mut cmd = tokio::process::Command::new(&exec_path);
        cmd.args(&bin_args)
            .current_dir(&prev_cwd)
            .stderr(aube_scripts::child_stderr());
        // Shebang shims resolve `node` through PATH — give them the
        // switched runtime.
        let runtime_dirs = crate::runtime::path_entries();
        if !runtime_dirs.is_empty() {
            cmd.env("PATH", aube_scripts::prepend_paths(&runtime_dirs));
        }
        crate::runtime::apply_child_env(&mut cmd);
        cmd.status()
            .await
            .into_diagnostic()
            .wrap_err("failed to execute dlx binary")?
    };

    // tmp drops here, removing the scratch project.
    drop(tmp);

    if !status.success() {
        // Propagate the dlx binary's non-zero exit up to the binary's single
        // `std::process::exit` rather than terminating here, keeping the
        // command embed-safe. The scratch project (`tmp`) is already dropped.
        return Ok(Some(aube_scripts::exit_code_from_status(status)));
    }
    Ok(None)
}

fn dlx_manifest(install_specs: &[String], allow_build: &[String]) -> serde_json::Value {
    // Minimal package.json. Version specs and dist-tags pass through as-is
    // — the resolver handles them exactly as it would from a real manifest.
    let mut deps = serde_json::Map::new();
    for spec in install_specs {
        let (mut name, value) = synthesize_dlx_dep(spec);
        if deps.contains_key(&name) {
            let mut suffix = 2usize;
            while deps.contains_key(&format!("{name}-{suffix}")) {
                suffix += 1;
            }
            name = format!("{name}-{suffix}");
        }
        deps.insert(name, serde_json::Value::String(value));
    }
    let mut manifest = serde_json::Map::from_iter([
        (
            "name".to_string(),
            serde_json::Value::String("aube-dlx".to_string()),
        ),
        (
            "version".to_string(),
            serde_json::Value::String("0.0.0".to_string()),
        ),
        ("private".to_string(), serde_json::Value::Bool(true)),
        ("dependencies".to_string(), serde_json::Value::Object(deps)),
    ]);
    if !allow_build.is_empty() {
        let allow_builds = serde_json::Map::from_iter(
            allow_build
                .iter()
                .map(|name| (name.clone(), serde_json::Value::Bool(true))),
        );
        manifest.insert(
            "pnpm".to_string(),
            serde_json::json!({ "allowBuilds": allow_builds }),
        );
    }
    serde_json::Value::Object(manifest)
}

fn dlx_install_options(allow_build: &[String]) -> InstallOptions {
    let mut opts = InstallOptions::with_mode(FrozenMode::No);
    // `dlx` executes bins from a throwaway project and deletes that project
    // immediately. Keeping package materialization inside the scratch tree is
    // what lets Node walk through `node_modules/.aube/node_modules`, the
    // hidden hoist fallback used by CLIs with undeclared runtime imports.
    let gvs = super::global_virtual_store_flags();
    if gvs.is_set() {
        opts.cli_flags.extend(gvs.to_cli_flag_bag());
    } else {
        opts.cli_flags.push((
            "disable-global-virtual-store".to_string(),
            "false".to_string(),
        ));
    }
    if allow_build.is_empty() {
        // Force ignore-scripts on transient dlx installs. User asked to
        // run one bin. They did not ask postinstall scripts on fresh
        // downloaded pkg to run. Without this, a user with
        // `allowedBuildDependencies=["*"]` in ~/.npmrc (common for
        // convenience) gets every dlx'd package running arbitrary
        // postinstall code. That is how supply chain attacks land.
        opts.ignore_scripts = true;
        opts.cli_flags
            .push(("ignore-scripts".to_string(), "true".to_string()));
    } else {
        // The isolated dlx allowlist should skip unapproved transitive
        // builds, not fail the one-off command because the user's project
        // has strictDepBuilds/paranoid enabled.
        opts.cli_flags
            .push(("strictDepBuilds".to_string(), "false".to_string()));
        opts.cli_flags
            .push(("paranoid".to_string(), "false".to_string()));
        opts.build_policy_override = Some(Arc::new(dlx_build_policy(allow_build)));
    }
    opts
}

/// Forward the user's project-scope `.npmrc` into the dlx scratch project so
/// the transient install reads the same per-project config `npx` would: the
/// project registry, scoped registries + auth, and supply-chain settings like
/// `minimumReleaseAge`. Only the *project* scope is forwarded — user/global
/// npmrc are read by `install::run` independent of cwd, so re-emitting them
/// would double-apply.
///
/// Entries come back already normalized to `key=value` pairs from the same
/// loader `install::run` consumes, so re-parsing the synthesized file yields
/// identical values. Values are written verbatim (npm/aube parse the whole
/// remainder after the first `=` as the value); a stray newline in a value —
/// which `.npmrc` cannot represent and the loader could never have produced —
/// is dropped defensively so a single entry can't inject extra lines.
///
/// Best-effort: a missing project `.npmrc` writes nothing, and the install
/// still runs with user-scope config + built-in defaults (the prior behavior).
fn write_project_npmrc_into_scratch(
    user_cwd: &std::path::Path,
    scratch_dir: &std::path::Path,
) -> miette::Result<()> {
    let entries = aube_registry::config::load_project_npmrc_entries(user_cwd);
    if entries.is_empty() {
        return Ok(());
    }
    let mut body = String::new();
    for (key, value) in entries {
        let value = value.replace(['\n', '\r'], "");
        body.push_str(&key);
        body.push('=');
        body.push_str(&value);
        body.push('\n');
    }
    aube_util::fs_atomic::atomic_write(&scratch_dir.join(".npmrc"), body.as_bytes())
        .into_diagnostic()
        .wrap_err("failed to write dlx scratch .npmrc")?;
    Ok(())
}

fn dlx_build_policy(allow_build: &[String]) -> aube_scripts::BuildPolicy {
    let allow_builds = BTreeMap::from_iter(
        allow_build
            .iter()
            .map(|name| (name.clone(), AllowBuildRaw::Bool(true))),
    );
    let (policy, warnings) = aube_scripts::BuildPolicy::from_config(&allow_builds, &[], &[], false);
    for warning in warnings {
        crate::progress::safe_eprintln(&format!("warn: --allow-build: {warning}"));
    }
    policy
}

/// RAII guard that swaps the process cwd on construction and restores it
/// on drop — including when the enclosing scope unwinds due to a panic.
struct CwdGuard {
    original: std::path::PathBuf,
}

impl CwdGuard {
    fn switch_to(new_dir: &std::path::Path) -> miette::Result<Self> {
        let original = std::env::current_dir().into_diagnostic()?;
        std::env::set_current_dir(new_dir)
            .into_diagnostic()
            .wrap_err("failed to switch into dlx scratch dir")?;
        Ok(Self { original })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        // Best-effort: if restoring the cwd fails we can't meaningfully
        // recover, but we also don't want to double-panic from Drop.
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Strip any `@version` suffix from a package spec, preserving `@scope/`
/// prefixes. Dlx defaults to the `latest` dist-tag when no version is given,
/// so the spec arm of `split_name_spec` becomes `"latest"` here.
fn split_spec(spec: &str) -> (&str, &str) {
    let (name, version) = super::split_name_spec(spec);
    (name, version.unwrap_or("latest"))
}

fn is_non_registry_spec(s: &str) -> bool {
    if s.starts_with("github:")
        || s.starts_with("gitlab:")
        || s.starts_with("bitbucket:")
        || s.starts_with("gist:")
        || s.starts_with("git+")
        || s.starts_with("git://")
        || s.starts_with("https://")
        || s.starts_with("http://")
        || s.starts_with("ssh://")
        || s.starts_with("file:")
        || s.starts_with("link:")
    {
        return true;
    }
    is_scp_form(s) || is_owner_repo_shorthand(s)
}

/// SCP-form `user@host:path` is only treated as Git for the three known
/// providers, matching pnpm 11. Unknown hosts fall through (pnpm treats
/// them as local paths).
fn is_scp_form(s: &str) -> bool {
    if s.contains("://") {
        return false;
    }
    let Some(colon) = s.find(':') else {
        return false;
    };
    let before = &s[..colon];
    let Some(at) = before.find('@') else {
        return false;
    };
    let user = &before[..at];
    let host = &before[at + 1..];
    if user.is_empty() || host.is_empty() {
        return false;
    }
    matches!(host, "github.com" | "gitlab.com" | "bitbucket.org")
}

/// Pnpm 11: a bare `owner/repo[#ref]` with no provider prefix defaults
/// to GitHub. Distinguished from registry specs (`name`, `@scope/name`,
/// `name@version`) by: no leading `@`, no `@` anywhere (registry version
/// separator), no `:` (URL/SCP), exactly one `/`, both halves non-empty.
fn is_owner_repo_shorthand(s: &str) -> bool {
    let body = s.split('#').next().unwrap_or(s);
    if body.starts_with('@') || body.contains('@') || body.contains(':') {
        return false;
    }
    let mut parts = body.splitn(2, '/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repo) = parts.next() else {
        return false;
    };
    !owner.is_empty() && !repo.is_empty() && !repo.contains('/')
}

fn derive_dlx_pkg_name(spec: &str) -> Option<String> {
    let body = spec.split('#').next().unwrap_or(spec);
    let after_colon = body.rsplit(':').next().unwrap_or(body);
    let last = after_colon.rsplit('/').next().unwrap_or(after_colon);
    let trimmed = last.strip_suffix(".git").unwrap_or(last);
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn synthesize_dlx_dep(spec: &str) -> (String, String) {
    if is_owner_repo_shorthand(spec) {
        // Rewrite to the explicit `github:` form so the resolver picks
        // it up via the same code path as `aube dlx github:owner/repo`.
        let name = derive_dlx_pkg_name(spec).unwrap_or_else(|| "aube-dlx-pkg".to_string());
        return (name, format!("github:{spec}"));
    }
    if is_non_registry_spec(spec) {
        let name = derive_dlx_pkg_name(spec).unwrap_or_else(|| "aube-dlx-pkg".to_string());
        return (name, spec.to_string());
    }
    let (name, version) = split_spec(spec);
    (name.to_string(), version.to_string())
}

/// The binary name `aube dlx <cmd>` should resolve to — strip any version
/// suffix and any `@scope/` prefix, since `node_modules/.bin/` is flat and
/// scoped packages still land under their unscoped bin name.
fn bin_name_for(command: &str) -> String {
    if is_non_registry_spec(command) {
        return derive_dlx_pkg_name(command).unwrap_or_else(|| "aube-dlx-pkg".to_string());
    }
    let (name, _) = split_spec(command);
    name.rsplit('/').next().unwrap_or(name).to_string()
}

fn can_use_local_bin(command: &str) -> bool {
    !is_non_registry_spec(command) && super::split_name_spec(command).1.is_none()
}

/// When the bin derived from the package name doesn't match the installed
/// package's actual bin, fall back to reading the package's `bin` field to
/// find the right name. Matches `npx`/`pnpm dlx` behavior so e.g.
/// `aube dlx @tanstack/cli create` works (ships its bin as `tanstack`, not
/// `cli`) and `aube dlx which` works (ships `node-which`).
///
/// `modules_dir` is the project's resolved virtual-modules directory — the
/// same one we derive the `.bin` path from, so a user with a custom
/// `modulesDir` still sees the fallback work. Returns `None` when we can't
/// make a confident pick; caller keeps the original inference and lets the
/// bin-missing error fire.
fn resolve_bin_from_package(modules_dir: &std::path::Path, pkg_name: &str) -> Option<String> {
    let pkg_json_path = modules_dir.join(pkg_name).join("package.json");
    let content = std::fs::read_to_string(&pkg_json_path).ok()?;
    let pkg_json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let bin = pkg_json.get("bin")?;
    let inferred = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
    match bin {
        // String bin: npm always names it after the unscoped package name,
        // so this matches what `bin_name_for` already derived. Returning
        // it explicitly keeps the lookup path symmetric.
        serde_json::Value::String(_) => Some(inferred.to_string()),
        serde_json::Value::Object(bins) => {
            if bins.contains_key(inferred) {
                Some(inferred.to_string())
            } else if bins.len() == 1 {
                // Single bin under a different name — unambiguous pick.
                bins.keys().next().cloned()
            } else {
                // Multiple bins, none matching the package name. We don't
                // know which one the user wants; let them pick via `-p`.
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_spec_plain() {
        assert_eq!(split_spec("cowsay"), ("cowsay", "latest"));
    }

    #[test]
    fn split_spec_versioned() {
        assert_eq!(split_spec("cowsay@1.5.0"), ("cowsay", "1.5.0"));
    }

    #[test]
    fn split_spec_scoped() {
        assert_eq!(split_spec("@scope/foo"), ("@scope/foo", "latest"));
    }

    #[test]
    fn split_spec_scoped_versioned() {
        assert_eq!(split_spec("@scope/foo@2.0.0"), ("@scope/foo", "2.0.0"));
    }

    #[test]
    fn bin_name_strips_scope_and_version() {
        assert_eq!(bin_name_for("cowsay@1.5.0"), "cowsay");
        assert_eq!(bin_name_for("@scope/foo@2"), "foo");
        assert_eq!(bin_name_for("@scope/foo"), "foo");
    }

    #[test]
    fn local_bin_shortcut_only_applies_to_bare_registry_names() {
        assert!(can_use_local_bin("cowsay"));
        assert!(can_use_local_bin("@scope/foo"));
        assert!(!can_use_local_bin("cowsay@1.5.0"));
        assert!(!can_use_local_bin("cowsay@next"));
        assert!(!can_use_local_bin("@scope/foo@2"));
        assert!(!can_use_local_bin("github:owner/repo"));
    }

    #[test]
    fn dlx_install_disables_global_virtual_store() {
        let opts = dlx_install_options(&[]);
        let empty_workspace = std::collections::BTreeMap::new();
        let empty_env = Vec::new();
        let ctx = aube_settings::ResolveCtx {
            project_aube_config: &[],
            project_npmrc: &[],
            user_aube_config: &[],
            user_npmrc: &[],
            workspace_yaml: &empty_workspace,
            global_config_yaml: &empty_workspace,
            env: &empty_env,
            cli: &opts.cli_flags,
            embedder_defaults: &[],
        };
        assert_eq!(
            aube_settings::resolved::enable_global_virtual_store(&ctx),
            Some(false)
        );
    }

    #[test]
    fn dlx_install_skips_scripts_by_default() {
        let opts = dlx_install_options(&[]);
        assert!(opts.ignore_scripts);
        assert!(
            opts.cli_flags
                .iter()
                .any(|(key, value)| { key == "ignore-scripts" && value == "true" })
        );
    }

    #[test]
    fn dlx_install_allows_scripts_when_builds_are_approved() {
        let allow_build = vec!["esbuild".to_string()];
        let opts = dlx_install_options(&allow_build);
        assert!(!opts.ignore_scripts);
        assert!(opts.build_policy_override.is_some());
        assert!(
            opts.cli_flags
                .iter()
                .any(|(key, value)| { key == "strictDepBuilds" && value == "false" })
        );
        assert!(
            opts.cli_flags
                .iter()
                .any(|(key, value)| { key == "paranoid" && value == "false" })
        );
        assert!(
            !opts
                .cli_flags
                .iter()
                .any(|(key, _)| key == "ignore-scripts")
        );
    }

    #[test]
    fn write_project_npmrc_forwards_project_scope_into_scratch() {
        // A project `.npmrc` in the user's cwd is forwarded into the scratch
        // project's `.npmrc`, so a dlx install reads the same project config
        // `npx` would (registry / minimumReleaseAge / scoped auth).
        let user = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join(".npmrc"),
            "minimumReleaseAge=4321\nregistry=https://registry.example.test/\n",
        )
        .unwrap();

        write_project_npmrc_into_scratch(user.path(), scratch.path()).unwrap();

        let written = std::fs::read_to_string(scratch.path().join(".npmrc")).unwrap();
        assert!(
            written.contains("minimumReleaseAge=4321"),
            "project minimumReleaseAge must be forwarded, got: {written:?}"
        );
        assert!(
            written.contains("registry=https://registry.example.test/"),
            "project registry must be forwarded, got: {written:?}"
        );
    }

    #[test]
    fn write_project_npmrc_no_project_config_writes_nothing() {
        // No project `.npmrc` → no scratch `.npmrc` written; the install then
        // runs on user-scope config + built-in defaults (the prior behavior).
        let user = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();

        write_project_npmrc_into_scratch(user.path(), scratch.path()).unwrap();

        assert!(
            !scratch.path().join(".npmrc").exists(),
            "no project .npmrc should leave the scratch dir without one"
        );
    }

    #[test]
    fn dlx_build_policy_only_allows_explicit_approvals() {
        let allow_build = vec!["esbuild".to_string()];
        let policy = dlx_build_policy(&allow_build);
        assert_eq!(
            policy.decide("esbuild", "0.25.0"),
            aube_scripts::AllowDecision::Allow
        );
        assert_eq!(
            policy.decide("sharp", "0.33.0"),
            aube_scripts::AllowDecision::Unspecified
        );
    }

    #[test]
    fn dlx_manifest_records_allow_build_approvals() {
        let install_specs = vec!["vite".to_string()];
        let allow_build = vec!["esbuild".to_string()];
        let manifest = dlx_manifest(&install_specs, &allow_build);
        assert_eq!(manifest["dependencies"]["vite"], "latest");
        assert_eq!(manifest["pnpm"]["allowBuilds"]["esbuild"], true);
    }

    #[test]
    fn dlx_manifest_omits_policy_when_no_builds_are_approved() {
        let install_specs = vec!["vite".to_string()];
        let manifest = dlx_manifest(&install_specs, &[]);
        assert!(manifest.get("pnpm").is_none());
    }

    fn write_pkg_json(modules_dir: &std::path::Path, pkg_name: &str, pkg_json: serde_json::Value) {
        let dir = modules_dir.join(pkg_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&pkg_json).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn resolve_bin_single_object_bin_picks_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(
            tmp.path(),
            "@tanstack/cli",
            serde_json::json!({
                "name": "@tanstack/cli",
                "bin": {"tanstack": "dist/bin.js"},
            }),
        );
        assert_eq!(
            resolve_bin_from_package(tmp.path(), "@tanstack/cli"),
            Some("tanstack".to_string())
        );
    }

    #[test]
    fn resolve_bin_object_with_matching_key_prefers_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(
            tmp.path(),
            "foo",
            serde_json::json!({
                "name": "foo",
                "bin": {"foo": "x.js", "foo-helper": "y.js"},
            }),
        );
        assert_eq!(
            resolve_bin_from_package(tmp.path(), "foo"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn resolve_bin_object_multiple_no_match_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(
            tmp.path(),
            "foo",
            serde_json::json!({
                "name": "foo",
                "bin": {"a": "a.js", "b": "b.js"},
            }),
        );
        assert_eq!(resolve_bin_from_package(tmp.path(), "foo"), None);
    }

    #[test]
    fn resolve_bin_string_bin_returns_package_tail() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(
            tmp.path(),
            "@scope/foo",
            serde_json::json!({
                "name": "@scope/foo",
                "bin": "./x.js",
            }),
        );
        assert_eq!(
            resolve_bin_from_package(tmp.path(), "@scope/foo"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn resolve_bin_no_bin_field_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(tmp.path(), "foo", serde_json::json!({"name": "foo"}));
        assert_eq!(resolve_bin_from_package(tmp.path(), "foo"), None);
    }

    #[test]
    fn synthesize_dlx_dep_handles_github_shorthand() {
        let (name, value) = synthesize_dlx_dep("github:user/repo");
        assert_eq!(name, "repo");
        assert_eq!(value, "github:user/repo");
    }

    #[test]
    fn synthesize_dlx_dep_handles_github_shorthand_with_ref() {
        let (name, value) = synthesize_dlx_dep("github:user/repo#v1.2.3");
        assert_eq!(name, "repo");
        assert_eq!(value, "github:user/repo#v1.2.3");
    }

    #[test]
    fn synthesize_dlx_dep_handles_scp_url() {
        let (name, value) = synthesize_dlx_dep("git@github.com:user/repo.git");
        assert_eq!(name, "repo");
        assert_eq!(value, "git@github.com:user/repo.git");
    }

    #[test]
    fn synthesize_dlx_dep_handles_scp_url_bitbucket() {
        let (name, value) = synthesize_dlx_dep("git@bitbucket.org:pnpmjs/git-resolver.git");
        assert_eq!(name, "git-resolver");
        assert_eq!(value, "git@bitbucket.org:pnpmjs/git-resolver.git");
    }

    #[test]
    fn synthesize_dlx_dep_rejects_unknown_host_scp() {
        // pnpm 11 treats `user@unknown-host:path` as a local path, not Git.
        // Aube falls through to registry handling — the install will fail
        // later with a clearer error than silently cloning an arbitrary host.
        let (name, _) = synthesize_dlx_dep("alice@host.example.com:org/repo.git");
        assert_ne!(name, "repo");
    }

    #[test]
    fn synthesize_dlx_dep_handles_owner_repo_shorthand() {
        let (name, value) = synthesize_dlx_dep("zkochan/is-negative");
        assert_eq!(name, "is-negative");
        assert_eq!(value, "github:zkochan/is-negative");
    }

    #[test]
    fn synthesize_dlx_dep_handles_owner_repo_shorthand_with_ref() {
        let (name, value) = synthesize_dlx_dep("zkochan/is-negative#2.0.1");
        assert_eq!(name, "is-negative");
        assert_eq!(value, "github:zkochan/is-negative#2.0.1");
    }

    #[test]
    fn synthesize_dlx_dep_handles_git_plus_url() {
        let (name, value) = synthesize_dlx_dep("git+https://host/u/r.git#v1");
        assert_eq!(name, "r");
        assert_eq!(value, "git+https://host/u/r.git#v1");
    }

    #[test]
    fn synthesize_dlx_dep_registry_spec_unchanged() {
        let (name, value) = synthesize_dlx_dep("lodash@4.17.0");
        assert_eq!(name, "lodash");
        assert_eq!(value, "4.17.0");
    }

    #[test]
    fn synthesize_dlx_dep_scoped_registry_spec_unchanged() {
        let (name, value) = synthesize_dlx_dep("@babel/core@7.0.0");
        assert_eq!(name, "@babel/core");
        assert_eq!(value, "7.0.0");
    }

    #[test]
    fn bin_name_for_non_registry_spec_uses_repo_name() {
        assert_eq!(bin_name_for("github:user/repo"), "repo");
        assert_eq!(bin_name_for("git@github.com:user/repo.git"), "repo");
    }
}
