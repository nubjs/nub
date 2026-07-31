use super::ensure_installed_in;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::path::Path;

#[derive(Debug, Default, Args)]
pub struct ExecArgs {
    /// Binary name
    pub bin: String,
    /// Arguments to pass to the binary
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Continue recursive execution after a command fails.
    ///
    /// Parsed for pnpm compatibility; aube currently stops on the
    /// first failure.
    #[arg(long)]
    pub no_bail: bool,
    /// Skip auto-install check
    #[arg(long)]
    pub no_install: bool,
    /// Disable topological sorting (default is on).
    ///
    /// Without this, recursive execs visit packages in a deps-first
    /// order. Pass this to fall back to raw workspace-listing order.
    #[arg(long, overrides_with = "sort")]
    pub no_sort: bool,
    /// Run recursive workspace executions concurrently.
    #[arg(long)]
    pub parallel: bool,
    /// Write a recursive exec summary file.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long)]
    pub report_summary: bool,
    /// Hide the `<package>: ` label on parallel-exec output lines.
    ///
    /// Lines are still piped (clean line breaks even with concurrent
    /// children) but the source package isn't named on each line.
    /// Sequential execs ignore this flag.
    #[arg(long)]
    pub reporter_hide_prefix: bool,
    /// Resume recursive execution starting at this package name.
    ///
    /// Packages before the named one in the post-sort, post-reverse
    /// order are skipped. Errors if the name isn't in the matched set.
    #[arg(long, value_name = "PACKAGE")]
    pub resume_from: Option<String>,
    /// Reverse the recursive execution order (after topo sort).
    #[arg(long)]
    pub reverse: bool,
    /// Run the command through `sh -c`.
    #[arg(short = 'c', long)]
    pub shell_mode: bool,
    /// Sort recursive packages topologically (this is the default).
    ///
    /// Pass to override an earlier `--no-sort` on the same invocation.
    #[arg(long, overrides_with = "no_sort")]
    pub sort: bool,
    /// Cap the number of recursive packages running at once.
    ///
    /// Setting this implicitly enables parallel mode at width `N`.
    /// `0` means "use the available CPU count". Without this flag,
    /// `--parallel` stays unbounded.
    #[arg(long, value_name = "N")]
    pub workspace_concurrency: Option<usize>,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    exec_args: ExecArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<i32>> {
    run_in(exec_args, filter, None).await
}

/// `exec` rooted at an explicit `base_dir` instead of the process cwd.
/// `None` reproduces the CLI behavior (resolve from the process cwd);
/// `Some(dir)` is the in-process embedding entry — the project is
/// resolved from `dir`, so concurrent embed calls in different projects
/// don't race on process-global cwd.
pub async fn run_in(
    exec_args: ExecArgs,
    filter: aube_workspace::selector::EffectiveFilter,
    base_dir: Option<std::path::PathBuf>,
) -> miette::Result<Option<i32>> {
    exec_args.network.install_overrides();
    exec_args.lockfile.install_overrides();
    exec_args.virtual_store.install_overrides();
    let effective_cwd = match base_dir {
        Some(dir) => dir,
        None => crate::dirs::cwd()?,
    };
    // Resolve the project's Node runtime before anything spawns (see
    // run.rs for the warm-path rationale).
    crate::runtime::ensure_for_cwd(&effective_cwd).await?;
    let ExecArgs {
        bin,
        args,
        no_install,
        parallel,
        no_bail: _,
        no_sort,
        report_summary: _,
        reporter_hide_prefix,
        resume_from,
        reverse,
        shell_mode,
        sort: _,
        workspace_concurrency,
        lockfile: _,
        network: _,
        virtual_store: _,
    } = exec_args;
    let cwd = crate::dirs::find_project_root(&effective_cwd).ok_or_else(|| {
        miette!(
            "no package.json found in {} or any parent directory",
            effective_cwd.display()
        )
    })?;

    ensure_installed_in(no_install, Some(&cwd)).await?;

    if !filter.is_empty() {
        // Same defaulting rule as `aube run`: sort=on unless `--no-sort`
        // was explicitly passed.
        let recursive = super::run::RecursiveOpts {
            sort: !no_sort,
            reverse,
            resume_from,
            workspace_concurrency,
            reporter_hide_prefix,
            no_bail: false,
        };
        return run_filtered(&cwd, &bin, &args, shell_mode, parallel, &filter, recursive).await;
    }

    let bin_path = super::project_modules_dir(&cwd).join(".bin").join(&bin);
    // Non-recursive `aube exec` is a terminal single-tool run with no
    // post-run work, so the standalone binary hands off via image
    // replacement; embedded hosts / Windows fall back to a supervised child.
    exec_bin_terminal(&cwd, &bin_path, &bin, &args, shell_mode).await
}

async fn run_filtered(
    cwd: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
    parallel: bool,
    filter: &aube_workspace::selector::EffectiveFilter,
    recursive: super::run::RecursiveOpts,
) -> miette::Result<Option<i32>> {
    let (_root, matched) = super::select_workspace_packages(cwd, filter, "exec")?;
    let matched = super::run::order_matched_packages(matched, &recursive)?;

    if let Some(concurrency) =
        super::run::effective_concurrency(parallel, recursive.workspace_concurrency)
    {
        return run_filtered_parallel(
            bin,
            args,
            shell_mode,
            matched,
            concurrency,
            recursive.reporter_hide_prefix,
            recursive.reverse,
        )
        .await;
    }

    for pkg in matched {
        let bin_path = super::project_modules_dir(&pkg.dir).join(".bin").join(bin);
        // Sequential fanout bails on the first non-zero exit, matching the
        // previous behavior where the inner `exec` terminated the process.
        if let Some(code) = exec_bin(&pkg.dir, &bin_path, bin, args, shell_mode).await? {
            return Ok(Some(code));
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn run_filtered_parallel(
    bin: &str,
    args: &[String],
    shell_mode: bool,
    matched: Vec<aube_workspace::selector::SelectedPackage>,
    concurrency: usize,
    reporter_hide_prefix: bool,
    reverse: bool,
) -> miette::Result<Option<i32>> {
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    if !shell_mode {
        for pkg in &matched {
            let bin_path = super::project_modules_dir(&pkg.dir).join(".bin").join(bin);
            if !bin_path.exists() {
                let name = pkg
                    .name
                    .as_deref()
                    .unwrap_or_else(|| pkg.dir.to_str().unwrap_or("<unknown>"));
                return Err(miette!(
                    "binary not found in {name}: {bin}\nTry running `{}` first, or check that the package providing '{bin}' is in its dependencies.",
                    aube_util::cmd("install")
                ));
            }
        }
    }

    // Topo barrier: same dep-before-dependent contract as
    // `run_filtered_parallel` in run.rs — see that function's doc for
    // the watch-channel rationale, cycle handling, and reverse-mode
    // transposition.
    let prereqs = aube_workspace::topo::compute_prereq_indices(&matched);
    let prereqs = if reverse {
        aube_workspace::topo::transpose_prereqs(&prereqs)
    } else {
        prereqs
    };
    let senders: Vec<tokio::sync::watch::Sender<bool>> = (0..matched.len())
        .map(|_| tokio::sync::watch::channel(false).0)
        .collect();
    let prereq_rxs_per_task: Vec<Vec<tokio::sync::watch::Receiver<bool>>> = (0..matched.len())
        .map(|i| prereqs[i].iter().map(|&j| senders[j].subscribe()).collect())
        .collect();

    let sem = Arc::new(Semaphore::new(concurrency));
    let mut tasks: Vec<tokio::task::JoinHandle<miette::Result<std::process::ExitStatus>>> =
        Vec::with_capacity(matched.len());
    let mut task_names = Vec::with_capacity(matched.len());
    let mut senders_iter = senders.into_iter();
    let mut prereq_rxs_iter = prereq_rxs_per_task.into_iter();
    for (index, pkg) in matched.into_iter().enumerate() {
        let name = pkg
            .name
            .clone()
            .unwrap_or_else(|| pkg.dir.display().to_string());
        let output_mode = if reporter_hide_prefix {
            super::run_output::OutputMode::NoPrefix
        } else {
            super::run_output::OutputMode::prefix(pkg.name.as_deref(), index)
        };
        let prereq_rxs = prereq_rxs_iter.next().expect("one rx vec per package");
        let done_tx = senders_iter.next().expect("one sender per package");
        let bin_path = super::project_modules_dir(&pkg.dir).join(".bin").join(bin);
        let dir = pkg.dir.clone();
        let bin = bin.to_string();
        let args = args.to_vec();
        let sem = Arc::clone(&sem);
        task_names.push(name);
        tasks.push(tokio::spawn(async move {
            for mut rx in prereq_rxs {
                while !*rx.borrow_and_update() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            }
            let _permit = sem
                .acquire_owned()
                .await
                .map_err(|e| miette!("workspace concurrency semaphore closed: {e}"))?;
            let result =
                exec_bin_status(&dir, &bin_path, &bin, &args, shell_mode, &output_mode).await;
            let _ = done_tx.send(true);
            result
        }));
    }

    let mut first_err: Option<miette::Report> = None;
    let mut first_exit: Option<i32> = None;
    for (task, _name) in tasks.into_iter().zip(task_names) {
        match task.await {
            Ok(Ok(status)) => {
                if !status.success() && first_exit.is_none() {
                    // Record the first non-zero child status; the code travels
                    // up via `Ok(Some(code))` below. Deliberately no `miette!`
                    // here: the exit-code return path (not an error) carries
                    // the failure, matching the sequential path, and creating
                    // an error would clobber any earlier task's real error in
                    // `first_err` (which the fallback path still needs).
                    first_exit = Some(aube_scripts::exit_code_from_status(status));
                }
            }
            Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
            Ok(Err(_)) => {}
            Err(e) if first_err.is_none() => first_err = Some(miette!("task panicked: {e}")),
            Err(_) => {}
        }
    }
    if let Some(code) = first_exit {
        // Propagate the first non-zero child exit up to the binary's single
        // `std::process::exit` rather than terminating here, so an embedder
        // driving aube in-process isn't hard-killed by a failing task.
        return Ok(Some(code));
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(None)
}

pub(crate) async fn exec_bin(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
) -> miette::Result<Option<i32>> {
    exec_bin_with_node_args(cwd, bin_path, bin, args, &[], shell_mode).await
}

pub(crate) async fn exec_bin_with_env(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
    child_env: &std::collections::BTreeMap<String, String>,
) -> miette::Result<Option<i32>> {
    exec_bin_with_node_args_and_env(cwd, bin_path, bin, args, &[], shell_mode, child_env).await
}

/// Run a project-local binary. On a non-zero child exit, returns
/// `Ok(Some(code))` so the caller can propagate the code up to the binary's
/// single `std::process::exit` instead of terminating in place — keeping the
/// command layer embed-safe for a host driving aube as a library.
/// `Ok(None)` means the binary succeeded.
pub(crate) async fn exec_bin_with_node_args(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    node_args: &[String],
    shell_mode: bool,
) -> miette::Result<Option<i32>> {
    exec_bin_with_node_args_and_env(
        cwd,
        bin_path,
        bin,
        args,
        node_args,
        shell_mode,
        &Default::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn exec_bin_with_node_args_and_env(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    node_args: &[String],
    shell_mode: bool,
    child_env: &std::collections::BTreeMap<String, String>,
) -> miette::Result<Option<i32>> {
    if !shell_mode && !bin_path.exists() {
        return Err(bin_not_found_error(bin));
    }

    let command = build_bin_command(cwd, bin_path, bin, args, node_args, shell_mode, child_env);
    let status = crate::process_guard::spawn_and_wait(command)
        .await
        .into_diagnostic()
        .wrap_err("failed to execute binary")?;

    if !status.success() {
        return Ok(Some(aube_scripts::exit_code_from_status(status)));
    }

    Ok(None)
}

/// Run a single terminal tool for the *standalone* binary by replacing the
/// process image with it (`execvp`), so the tool inherits aube's pid. With
/// no separate aube process left to outlive, any signal — including an
/// uncatchable `SIGKILL` — reaches the tool directly, so no supervisor or
/// `PR_SET_PDEATHSIG` is needed and the macOS `SIGKILL` gap doesn't apply.
///
/// Only sound where nothing runs after the tool and aube owns the whole
/// process: an embedded host must not have its image blown away, and Windows
/// has no `execvp`. Both fall back to the supervised spawn in `exec_bin`.
/// Use only from terminal call sites with no post-run cleanup (no dlx scratch
/// dir, not the recursive workspace loop).
pub(crate) async fn exec_bin_terminal(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
) -> miette::Result<Option<i32>> {
    exec_bin_terminal_with_env(cwd, bin_path, bin, args, shell_mode, &Default::default()).await
}

pub(crate) async fn exec_bin_terminal_with_env(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
    child_env: &std::collections::BTreeMap<String, String>,
) -> miette::Result<Option<i32>> {
    #[cfg(unix)]
    if aube_util::embedder().name == aube_util::AUBE.name {
        use std::os::unix::process::CommandExt;
        if !shell_mode && !bin_path.exists() {
            return Err(bin_not_found_error(bin));
        }
        let mut command = build_bin_command(cwd, bin_path, bin, args, &[], shell_mode, child_env);
        // `exec` only returns on failure; on success the tool has replaced us.
        let err = command.as_std_mut().exec();
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_EXEC_FAILED,
            "failed to exec `{bin}`: {err}"
        ));
    }
    // Embedded host or Windows: replacing the image is unsafe or unsupported,
    // so run the tool as a supervised child instead.
    exec_bin_with_env(cwd, bin_path, bin, args, shell_mode, child_env).await
}

fn bin_not_found_error(bin: &str) -> miette::Report {
    miette!(
        "binary not found: {bin}\nTry running `{}` first, or check that the package providing '{bin}' is in your dependencies.",
        aube_util::cmd("install")
    )
}

/// Assemble the `tokio::process::Command` that runs `bin`, shared by the
/// supervised (`spawn_and_wait`) and image-replacing (`exec_bin_terminal`)
/// paths. Callers own the bin-exists check. `child_env` carries the embedder's
/// added environment; standalone callers pass an empty map.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_bin_command(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    node_args: &[String],
    shell_mode: bool,
    child_env: &std::collections::BTreeMap<String, String>,
) -> tokio::process::Command {
    let mut command =
        if !shell_mode && let Some(cmd) = resolved_bin_command(bin_path, args, node_args) {
            cmd
        } else if shell_mode {
            let line = std::iter::once(aube_scripts::shell_quote_arg(bin))
                .chain(args.iter().map(|arg| aube_scripts::shell_quote_arg(arg)))
                .collect::<Vec<_>>()
                .join(" ");
            let bin_dir = super::project_modules_dir(cwd).join(".bin");
            let mut path_dirs = vec![bin_dir];
            path_dirs.extend(crate::runtime::path_entries());
            let new_path = aube_scripts::prepend_paths(&path_dirs);
            let mut cmd = aube_scripts::spawn_shell(&line);
            cmd.env("PATH", &new_path);
            cmd
        } else {
            let exec_path = resolve_exec_shim(bin_path);
            let mut cmd = tokio::process::Command::new(exec_path);
            cmd.args(args);
            // `#!/usr/bin/env node` shebangs resolve through the child's PATH
            // so the generated shim must see the project's switched runtime.
            let runtime_dirs = crate::runtime::path_entries();
            if !runtime_dirs.is_empty() {
                cmd.env("PATH", aube_scripts::prepend_paths(&runtime_dirs));
            }
            cmd
        };
    crate::runtime::apply_child_env(&mut command);
    command
        .envs(child_env)
        .current_dir(cwd)
        .stderr(aube_scripts::child_stderr());
    command
}

pub(crate) async fn exec_bin_status(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
    output_mode: &super::run_output::OutputMode,
) -> miette::Result<std::process::ExitStatus> {
    exec_bin_status_with_node_args(cwd, bin_path, bin, args, &[], shell_mode, output_mode).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn exec_bin_status_with_node_args(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    node_args: &[String],
    shell_mode: bool,
    output_mode: &super::run_output::OutputMode,
) -> miette::Result<std::process::ExitStatus> {
    if !shell_mode && !bin_path.exists() {
        return Err(miette!(
            "binary not found: {bin}\nTry running `{}` first, or check that the package providing '{bin}' is in your dependencies.",
            aube_util::cmd("install")
        ));
    }

    let command = build_bin_command(
        cwd,
        bin_path,
        bin,
        args,
        node_args,
        shell_mode,
        &std::collections::BTreeMap::new(),
    );
    super::run_output::run_command(command, output_mode).await
}

/// Resolve an aube-generated shim (or symlink) to its package target and
/// choose the launcher that understands that file. Native executable formats
/// run directly. JavaScript targets are unwrapped only when `node_args` must
/// be injected; otherwise they and other interpreters retain the established
/// generated-shim behavior.
fn resolved_bin_command(
    bin_path: &Path,
    args: &[String],
    node_args: &[String],
) -> Option<tokio::process::Command> {
    let target = resolve_bin_target(bin_path);
    if is_native_executable(&target.path) {
        let mut cmd = tokio::process::Command::new(&target.path);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().arg0(bin_path);
        }
        if let Some(node_path) = &target.node_path {
            cmd.env("NODE_PATH", node_path);
        }
        // Preserve the generated shim's switched-runtime behavior for native
        // launchers that spawn Node themselves.
        let runtime_dirs = crate::runtime::path_entries();
        if !runtime_dirs.is_empty() {
            cmd.env("PATH", aube_scripts::prepend_paths(&runtime_dirs));
        }
        cmd.args(args);
        return Some(cmd);
    }
    if node_args.is_empty() || !is_node_backed_bin(&target.path) {
        return None;
    }
    let mut cmd =
        tokio::process::Command::new(target.node.unwrap_or_else(crate::runtime::node_program));
    if let Some(node_path) = target.node_path {
        cmd.env("NODE_PATH", node_path);
    }
    cmd.args(node_args).arg(target.path).args(args);
    Some(cmd)
}

struct BinTarget {
    path: std::path::PathBuf,
    node: Option<std::path::PathBuf>,
    node_path: Option<std::ffi::OsString>,
}

fn resolve_bin_target(bin_path: &Path) -> BinTarget {
    let path = resolve_exec_shim(bin_path);
    if let Ok(target) = std::fs::read_link(&path) {
        let target = if target.is_absolute() {
            target
        } else if let Some(parent) = path.parent() {
            // Keep `..` components intact. Resolving them lexically can change
            // meaning when an earlier component is itself a symlink.
            parent.join(target)
        } else {
            target
        };
        return BinTarget {
            path: target,
            node: None,
            node_path: None,
        };
    }
    if let Ok(Some(shim)) = aube_linker::sys::resolve_bin_shim(&path) {
        return BinTarget {
            path: shim.target,
            node: path.parent().and_then(local_node_program),
            node_path: shim.node_path,
        };
    }
    BinTarget {
        path,
        node: None,
        node_path: None,
    }
}

fn local_node_program(parent: &Path) -> Option<std::path::PathBuf> {
    let node = parent.join(if cfg!(windows) { "node.exe" } else { "node" });
    node.exists().then_some(node)
}

fn is_native_executable(target: &Path) -> bool {
    use std::io::{Read, Seek};

    let mut header = [0u8; 64];
    let (mut file, n) = match std::fs::File::open(target) {
        Ok(mut file) => {
            let n = file.read(&mut header).unwrap_or(0);
            (Some(file), n)
        }
        Err(_) => (None, 0),
    };
    if is_native_magic(&header[..n.min(4)]) {
        return true;
    }
    if n >= 64 && header.starts_with(b"MZ") {
        let pe_offset =
            u32::from_le_bytes([header[0x3c], header[0x3d], header[0x3e], header[0x3f]]);
        let mut signature = [0u8; 4];
        if let Some(file) = &mut file
            && file
                .seek(std::io::SeekFrom::Start(u64::from(pe_offset)))
                .is_ok()
            && file.read_exact(&mut signature).is_ok()
            && signature == *b"PE\0\0"
        {
            return true;
        }
    }

    #[cfg(windows)]
    {
        // A shebang is stronger evidence than a Windows-looking suffix. This
        // keeps intentionally interpreter-backed polyglot bins on their shim.
        if header[..n].starts_with(b"#!") {
            return false;
        }
        return target
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                ["exe", "cmd", "bat", "com"]
                    .iter()
                    .any(|candidate| ext.eq_ignore_ascii_case(candidate))
            });
    }

    #[cfg(not(windows))]
    false
}

fn is_native_magic(magic: &[u8]) -> bool {
    magic.starts_with(b"\x7fELF")
        || matches!(
            magic,
            [0xcf, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xce]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
                | [0xbf, 0xba, 0xfe, 0xca]
        )
}

fn is_node_backed_bin(target: &Path) -> bool {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(target) else {
        return false;
    };
    let mut buf = [0u8; 256];
    let n = file.read(&mut buf).unwrap_or(0);
    let first_line = buf[..n]
        .split(|b| *b == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .unwrap_or("")
        .trim_end_matches('\r');
    if let Some(interpreter) = first_line.strip_prefix("#!") {
        return is_node_interpreter(interpreter);
    }
    matches!(
        target.extension().and_then(|ext| ext.to_str()),
        Some("js" | "cjs" | "mjs")
    )
}

fn is_node_interpreter(raw: &str) -> bool {
    let interpreter = raw.trim();
    let name = if let Some(rest) = interpreter.strip_prefix("/usr/bin/env") {
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("-S").map_or(rest, |r| r.trim_start());
        rest.split_whitespace()
            .find(|part| !part.contains('='))
            .unwrap_or("")
    } else {
        interpreter.split_whitespace().next().unwrap_or("")
    };
    let basename = std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    matches!(basename, "node" | "nodejs")
}

/// Pick the executable variant of a `node_modules/.bin/<name>` shim.
///
/// On Unix the bare path is a sh shebang script and is what we want.
/// On Windows the linker writes `<name>.cmd`, `<name>.ps1`, and a bare
/// `<name>` sh shim. `Command::new` can launch the `.cmd` shim, but the
/// bare sh shim fails with OS error 193.
pub(crate) fn resolve_exec_shim(bin_path: &Path) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let mut cmd_path = bin_path.as_os_str().to_os_string();
        cmd_path.push(".cmd");
        let cmd_path = std::path::PathBuf::from(cmd_path);
        if cmd_path.exists() {
            return cmd_path;
        }
    }
    bin_path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::resolve_bin_target;
    use super::{
        build_bin_command, is_native_executable, is_native_magic, is_node_backed_bin,
        resolve_exec_shim, resolved_bin_command,
    };

    #[test]
    fn resolve_exec_shim_returns_bare_path_when_no_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("loner");
        std::fs::write(&bare, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_exec_shim(&bare), bare);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_exec_shim_prefers_cmd_sibling_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("cowsay");
        let cmd_shim = tmp.path().join("cowsay.cmd");
        std::fs::write(&bare, b"#!/bin/sh\n").unwrap();
        std::fs::write(&cmd_shim, b"@echo off\n").unwrap();
        assert_eq!(resolve_exec_shim(&bare), cmd_shim);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_exec_shim_appends_cmd_to_dotted_bin_name() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("tool.exe");
        let cmd_shim = tmp.path().join("tool.exe.cmd");
        std::fs::write(&bare, b"#!/bin/sh\n").unwrap();
        std::fs::write(&cmd_shim, b"@echo off\n").unwrap();
        assert_eq!(resolve_exec_shim(&bare), cmd_shim);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_exec_shim_keeps_bare_path_on_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("cowsay");
        let cmd_shim = tmp.path().join("cowsay.cmd");
        std::fs::write(&bare, b"#!/bin/sh\n").unwrap();
        std::fs::write(&cmd_shim, b"@echo off\n").unwrap();
        assert_eq!(resolve_exec_shim(&bare), bare);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_bin_target_follows_symlink_without_canonicalizing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("bin.js");
        let shim = tmp.path().join("shim");
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();
        std::os::unix::fs::symlink("bin.js", &shim).unwrap();
        let resolved = resolve_bin_target(&shim);
        assert_eq!(resolved.path, target);
        assert_eq!(resolved.node, None);
        assert_eq!(resolved.node_path, None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_bin_target_preserves_parent_components_after_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let shim = tmp.path().join("shim");
        std::os::unix::fs::symlink("pivot/../bin", &shim).unwrap();

        let resolved = resolve_bin_target(&shim);
        assert_eq!(resolved.path, tmp.path().join("pivot/../bin"));
    }

    #[test]
    fn resolve_bin_target_preserves_posix_shim_env() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("pkg").join("bin.js");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();

        let shim = tmp.path().join("mycli");
        let local_node = tmp
            .path()
            .join(if cfg!(windows) { "node.exe" } else { "node" });
        let node_path = tmp.path().join("node_modules");
        std::fs::write(&local_node, b"").unwrap();
        std::fs::write(
            &shim,
            "#!/bin/sh\n\
             # aube-bin-shim v1 target=pkg/bin.js\n\
             basedir=$(dirname \"$0\")\n\
             export NODE_PATH=\"$basedir/node_modules\"\n\
             exec node \"$basedir/pkg/bin.js\" \"$@\"\n",
        )
        .unwrap();

        let resolved = resolve_bin_target(&shim);
        assert_eq!(resolved.path, target);
        assert_eq!(resolved.node, Some(local_node));
        assert_eq!(
            resolved.node_path,
            Some(std::env::join_paths([node_path]).unwrap())
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_bin_target_reads_cmd_shim_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("pkg").join("bin.js");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();
        let local_node = tmp.path().join("node.exe");
        let node_path = tmp.path().join("node_modules");
        std::fs::write(&local_node, b"").unwrap();

        let bare = tmp.path().join("mycli");
        std::fs::write(
            &bare,
            b"#!/bin/sh\nexec node \"$basedir/pkg/bin.js\" \"$@\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("mycli.cmd"),
            b"@SETLOCAL\r\n\
              @SET NODE_PATH=%~dp0node_modules\r\n\
              @IF EXIST \"%~dp0\\node.exe\" (\r\n\
              \x20 \"%~dp0\\node.exe\" \"%~dp0\\pkg\\bin.js\" %*\r\n\
              ) ELSE (\r\n\
              \x20 @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n\
              \x20 node \"%~dp0\\pkg\\bin.js\" %*\r\n\
              )\r\n",
        )
        .unwrap();

        let resolved = resolve_bin_target(&bare);
        assert_eq!(resolved.path, target);
        assert_eq!(resolved.node, Some(local_node));
        assert_eq!(
            resolved.node_path,
            Some(std::env::join_paths([node_path]).unwrap())
        );
    }

    #[test]
    fn is_node_backed_bin_detects_node_shebang() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("bin");
        std::fs::write(&target, b"#!/usr/bin/env node\nconsole.log(1)\n").unwrap();
        assert!(is_node_backed_bin(&target));
    }

    #[test]
    fn is_node_backed_bin_rejects_node_substring_interpreters() {
        let tmp = tempfile::tempdir().unwrap();
        for interpreter in ["nodemon", "nodeenv", "node-gyp", "node-18"] {
            let target = tmp.path().join(interpreter);
            std::fs::write(
                &target,
                format!("#!/usr/bin/env {interpreter}\n").as_bytes(),
            )
            .unwrap();
            assert!(
                !is_node_backed_bin(&target),
                "{interpreter} should not be treated as node"
            );
        }
    }

    #[test]
    fn is_node_backed_bin_accepts_nodejs_shebang() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("bin");
        std::fs::write(&target, b"#!/usr/bin/nodejs\nconsole.log(1)\n").unwrap();
        assert!(is_node_backed_bin(&target));
    }

    #[test]
    fn resolved_bin_command_keeps_js_shim_without_node_args() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("pkg/bin.js");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();
        let shim = tmp.path().join("tool");
        std::fs::write(&shim, "#!/bin/sh\n# aube-bin-shim v1 target=pkg/bin.js\n").unwrap();

        assert!(resolved_bin_command(&shim, &[], &[]).is_none());
    }

    #[test]
    fn native_magic_recognizes_supported_executable_formats() {
        for magic in [
            b"\x7fELF".as_slice(),
            b"\xcf\xfa\xed\xfe".as_slice(),
            b"\xfe\xed\xfa\xcf".as_slice(),
            b"\xce\xfa\xed\xfe".as_slice(),
            b"\xfe\xed\xfa\xce".as_slice(),
            b"\xca\xfe\xba\xbe".as_slice(),
            b"\xbe\xba\xfe\xca".as_slice(),
            b"\xca\xfe\xba\xbf".as_slice(),
            b"\xbf\xba\xfe\xca".as_slice(),
        ] {
            assert!(is_native_magic(magic), "unrecognized magic: {magic:x?}");
        }
        assert!(!is_native_magic(b"#!/u"));
    }

    #[test]
    fn pe_detection_requires_the_pe_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("tool");
        let mut pe = vec![0u8; 68];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        pe[64..68].copy_from_slice(b"PE\0\0");
        std::fs::write(&target, pe).unwrap();
        assert!(is_native_executable(&target));

        std::fs::write(&target, b"MZ = 1; console.log(MZ);\n").unwrap();
        assert!(!is_native_executable(&target));
    }

    #[test]
    fn native_executable_detection_reads_magic_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("tool");
        std::fs::write(&target, b"\x7fELFpayload").unwrap();

        assert!(is_native_executable(&target));
    }

    #[cfg(windows)]
    #[test]
    fn native_executable_detection_accepts_windows_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["tool.EXE", "tool.cmd", "tool.Bat", "tool.com"] {
            let target = tmp.path().join(name);
            std::fs::write(&target, b"not magic").unwrap();
            assert!(is_native_executable(&target), "rejected {name}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_executable_detection_preserves_shebang_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("tool.exe");
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();

        assert!(!is_native_executable(&target));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn bin_command_executes_windows_target_from_dotted_generated_cmd_shim() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("node_modules/.bin");
        let target = std::env::var_os("COMSPEC")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"));
        aube_linker::create_bin_shim(
            &bin_dir,
            "native-tool.exe",
            &target,
            aube_linker::BinShimOptions::default(),
        )
        .unwrap();

        let mut command = build_bin_command(
            tmp.path(),
            &bin_dir.join("native-tool.exe"),
            "native-tool.exe",
            &[
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                "echo launched-directly".to_string(),
            ],
            &[],
            false,
            &std::collections::BTreeMap::new(),
        );
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "launched-directly"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bin_command_executes_native_target_behind_generated_shim() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let hidden_modules = tmp.path().join("node_modules/.aube/node_modules");
        std::fs::create_dir_all(&hidden_modules).unwrap();
        let target = std::path::Path::new("/bin/echo");
        aube_linker::create_bin_shim(
            &bin_dir,
            "native-echo",
            target,
            aube_linker::BinShimOptions {
                extend_node_path: true,
                prefer_symlinked_executables: Some(false),
                hidden_modules_dir: Some(&hidden_modules),
            },
        )
        .unwrap();

        let shim = bin_dir.join("native-echo");
        let mut command = build_bin_command(
            tmp.path(),
            &shim,
            "native-echo",
            &["launched-directly".to_string()],
            &[],
            false,
            &std::collections::BTreeMap::new(),
        );
        let node_path = command
            .as_std()
            .get_envs()
            .find_map(|(key, value)| (key == "NODE_PATH").then_some(value).flatten())
            .unwrap();
        assert_eq!(
            node_path,
            std::env::join_paths([tmp.path().join("node_modules"), hidden_modules.clone()])
                .unwrap()
        );
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "launched-directly\n"
        );
    }
}
