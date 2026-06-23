use super::ensure_installed;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::path::Path;

#[derive(Debug, Args)]
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
    exec_args.network.install_overrides();
    exec_args.lockfile.install_overrides();
    exec_args.virtual_store.install_overrides();
    // Resolve the project's Node runtime before anything spawns (see
    // run.rs for the warm-path rationale).
    crate::runtime::ensure_for_cwd(&crate::dirs::cwd()?).await?;
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
    let cwd = crate::dirs::project_root()?;

    ensure_installed(no_install).await?;

    if !filter.is_empty() {
        // Same defaulting rule as `aube run`: sort=on unless `--no-sort`
        // was explicitly passed.
        let recursive = super::run::RecursiveOpts {
            sort: !no_sort,
            reverse,
            resume_from,
            workspace_concurrency,
            reporter_hide_prefix,
        };
        return run_filtered(&cwd, &bin, &args, shell_mode, parallel, &filter, recursive).await;
    }

    let bin_path = super::project_modules_dir(&cwd).join(".bin").join(&bin);
    exec_bin(&cwd, &bin_path, &bin, &args, shell_mode).await
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
    if !shell_mode && !bin_path.exists() {
        return Err(miette!(
            "binary not found: {bin}\nTry running `{}` first, or check that the package providing '{bin}' is in your dependencies.",
            aube_util::cmd("install")
        ));
    }

    let mut command = if let Some(cmd) = node_bin_command(bin_path, args, node_args, shell_mode) {
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
        cmd
    };
    let status = command
        .current_dir(cwd)
        .stderr(aube_scripts::child_stderr())
        .status()
        .await
        .into_diagnostic()
        .wrap_err("failed to execute binary")?;

    if !status.success() {
        return Ok(Some(aube_scripts::exit_code_from_status(status)));
    }

    Ok(None)
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

    let mut command = if let Some(cmd) = node_bin_command(bin_path, args, node_args, shell_mode) {
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
        // `#!/usr/bin/env node` shebangs resolve through the child's
        // PATH — put the switched runtime ahead of the inherited one.
        let runtime_dirs = crate::runtime::path_entries();
        if !runtime_dirs.is_empty() {
            cmd.env("PATH", aube_scripts::prepend_paths(&runtime_dirs));
        }
        cmd
    };
    crate::runtime::apply_child_env(&mut command);
    command.current_dir(cwd);
    super::run_output::run_command(command, output_mode).await
}

fn node_bin_command(
    bin_path: &Path,
    args: &[String],
    node_args: &[String],
    shell_mode: bool,
) -> Option<tokio::process::Command> {
    if shell_mode || node_args.is_empty() {
        return None;
    }
    let target = resolve_node_bin_target(bin_path)?;
    if !is_node_backed_bin(&target.path) {
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

struct NodeBinTarget {
    path: std::path::PathBuf,
    node: Option<std::path::PathBuf>,
    node_path: Option<std::path::PathBuf>,
}

fn resolve_node_bin_target(bin_path: &Path) -> Option<NodeBinTarget> {
    let path = resolve_exec_shim(bin_path);
    resolve_node_bin_target_path(&path).or(Some(NodeBinTarget {
        path,
        node: None,
        node_path: None,
    }))
}

fn resolve_node_bin_target_path(path: &Path) -> Option<NodeBinTarget> {
    if let Ok(target) = std::fs::read_link(path) {
        let path = if target.is_absolute() {
            target
        } else {
            aube_linker::normalize_path(&path.parent()?.join(target))
        };
        return Some(NodeBinTarget {
            path,
            node: None,
            node_path: None,
        });
    }
    let content = std::fs::read_to_string(path).ok()?;
    let parent = path.parent()?;
    if let Some(rel) = aube_linker::parse_posix_shim_target(&content) {
        return Some(NodeBinTarget {
            path: aube_linker::normalize_path(&parent.join(rel)),
            node: local_node_program(parent),
            node_path: parse_posix_node_path(&content)
                .map(|rel| aube_linker::normalize_path(&parent.join(rel))),
        });
    }
    let rel = parse_cmd_shim_target(&content)?;
    Some(NodeBinTarget {
        path: aube_linker::normalize_path(&parent.join(rel)),
        node: local_node_program(parent),
        node_path: parse_cmd_node_path(&content)
            .map(|rel| aube_linker::normalize_path(&parent.join(rel))),
    })
}

fn parse_cmd_shim_target(content: &str) -> Option<&str> {
    let marker = "\"%~dp0\\";
    let mut rest = content;
    while let Some(start) = rest.find(marker) {
        let after_marker = &rest[start + marker.len()..];
        let Some(end) = after_marker.find('"') else {
            break;
        };
        let candidate = &after_marker[..end];
        if !candidate.ends_with(".exe") {
            return Some(candidate);
        }
        rest = &after_marker[end + 1..];
    }
    None
}

fn parse_cmd_node_path(content: &str) -> Option<&str> {
    let rest = content
        .lines()
        .find_map(|line| line.strip_prefix("@SET NODE_PATH=%~dp0"))?;
    Some(rest.trim_end_matches('\r'))
}

fn parse_posix_node_path(content: &str) -> Option<&str> {
    content.lines().find_map(|line| {
        line.strip_prefix("export NODE_PATH=\"$basedir/")
            .and_then(|rest| rest.strip_suffix('"'))
    })
}

fn local_node_program(parent: &Path) -> Option<std::path::PathBuf> {
    let node = parent.join(if cfg!(windows) { "node.exe" } else { "node" });
    node.exists().then_some(node)
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
        if bin_path.extension().is_none() {
            let cmd_path = bin_path.with_extension("cmd");
            if cmd_path.exists() {
                return cmd_path;
            }
        }
    }
    bin_path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::resolve_node_bin_target;
    use super::{
        is_node_backed_bin, parse_cmd_node_path, parse_cmd_shim_target, parse_posix_node_path,
        resolve_exec_shim,
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
    fn resolve_node_bin_target_follows_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("bin.js");
        let shim = tmp.path().join("shim");
        std::fs::write(&target, b"#!/usr/bin/env node\n").unwrap();
        std::os::unix::fs::symlink("bin.js", &shim).unwrap();
        let resolved = resolve_node_bin_target(&shim).unwrap();
        assert_eq!(resolved.path, target);
        assert_eq!(resolved.node, None);
        assert_eq!(resolved.node_path, None);
    }

    #[test]
    fn parse_cmd_shim_target_skips_program_exe() {
        let content = "@SETLOCAL\r\n\
             @IF EXIST \"%~dp0\\node.exe\" (\r\n\
             \x20 \"%~dp0\\node.exe\" \"%~dp0\\pkg\\bin.js\" %*\r\n\
             ) ELSE (\r\n\
             \x20 node \"%~dp0\\pkg\\bin.js\" %*\r\n\
             )\r\n";

        assert_eq!(parse_cmd_shim_target(content), Some("pkg\\bin.js"));
    }

    #[test]
    fn parse_cmd_shim_target_stops_on_truncated_marker() {
        assert_eq!(parse_cmd_shim_target("\"%~dp0\\node.exe"), None);
    }

    #[test]
    fn parse_cmd_node_path_reads_generated_env() {
        let content = "@SETLOCAL\r\n@SET NODE_PATH=%~dp0..\\..\r\n";

        assert_eq!(parse_cmd_node_path(content), Some("..\\.."));
    }

    #[test]
    fn parse_posix_node_path_reads_generated_env() {
        let content = "#!/bin/sh\nexport NODE_PATH=\"$basedir/../..\"\n";

        assert_eq!(parse_posix_node_path(content), Some("../.."));
    }

    #[test]
    fn resolve_node_bin_target_preserves_posix_shim_env() {
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

        let resolved = resolve_node_bin_target(&shim).unwrap();
        assert_eq!(resolved.path, target);
        assert_eq!(resolved.node, Some(local_node));
        assert_eq!(resolved.node_path, Some(node_path));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_node_bin_target_reads_cmd_shim_on_windows() {
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
              \x20 node \"%~dp0\\pkg\\bin.js\" %*\r\n\
              )\r\n",
        )
        .unwrap();

        let resolved = resolve_node_bin_target(&bare).unwrap();
        assert_eq!(resolved.path, target);
        assert_eq!(resolved.node, Some(local_node));
        assert_eq!(resolved.node_path, Some(node_path));
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
}
