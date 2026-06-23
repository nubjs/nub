//! Per-package output prefixing for parallel recursive runs.
//!
//! `aube run -r --parallel` and `aube exec -r --parallel` (or any mode
//! with `--workspace-concurrency`) need to keep the stdout/stderr from
//! N concurrent child processes legible. The default mode here pipes
//! both streams from each child, reads them line-by-line, and emits
//! every line as `<pkg>: <line>` with a per-package ANSI color so the
//! source of each line is unambiguous in a mixed-up scrollback.
//!
//! ## Trade-offs
//!
//! Piping child stdio loses TTY autodetection in the child, so most
//! tools (tsc, vite, next, vitest, etc.) stop emitting their own ANSI
//! colors. We deliberately do **not** set `FORCE_COLOR=1` to compensate:
//! that env var is a sledgehammer that overrides legitimate
//! `NO_COLOR` / CI heuristics in the child and produces broken output
//! for tools whose `FORCE_COLOR` handling is incomplete. Users who want
//! the child's native colors should run the recursive script
//! sequentially (no `--parallel`, no `--workspace-concurrency`), where
//! we keep stdio inherited and pnpm-style topo ordering still applies.
//!
//! ## Modes
//!
//! - [`OutputMode::Prefix`] — pipe + per-line `<name>: ` prefix in a
//!   per-package color. The default for parallel runs.
//! - [`OutputMode::NoPrefix`] — pipe but emit lines without the label.
//!   Triggered by `--reporter-hide-prefix` for pnpm parity. Useful when
//!   the user wants line-buffered (so it can be redirected without
//!   torn writes) but doesn't care which package each line came from.
//!
//! Sequential (`aube run -r`, no `--parallel` / `--workspace-concurrency`)
//! does not go through this module — those paths inherit stdio
//! directly via `Command::status` so child color autodetection still
//! works. Multiplexing only kicks in once the user opts into parallel
//! and is paying for the loss of color anyway.

use miette::{IntoDiagnostic, WrapErr, miette};
use std::io::IsTerminal;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

/// 6 ANSI foreground colors rotated round-robin per package. SGR
/// reset (`\x1b[0m`) is emitted after every prefix so a child line
/// that itself contains escape codes can't bleed the package color
/// onto subsequent lines.
const PALETTE: &[u8] = &[31, 32, 33, 34, 35, 36];

#[derive(Debug, Clone)]
pub(crate) enum OutputMode {
    /// Pipe child stdio and prepend `<name>: ` to every line, colored
    /// from [`PALETTE`] when the parent stdout is a TTY.
    Prefix { name: String, color_index: usize },
    /// Pipe child stdio but emit lines as-is (no name prefix). Matches
    /// pnpm's `--reporter-hide-prefix` semantics.
    NoPrefix,
}

impl OutputMode {
    /// Build a `Prefix` mode for the given package name and index in
    /// the matched-package list. Falls back to `NoPrefix` if the
    /// package has no name (degenerate manifest with `name` unset),
    /// since a colored empty prefix is just confusing noise.
    pub(crate) fn prefix(name: Option<&str>, index: usize) -> Self {
        match name {
            Some(n) => Self::Prefix {
                name: n.to_string(),
                color_index: index % PALETTE.len(),
            },
            None => Self::NoPrefix,
        }
    }
}

/// Run `cmd` to completion, piping its stdout/stderr through line
/// pumps that prepend `mode`'s prefix to every line. Both `Prefix` and
/// `NoPrefix` modes pipe; the only difference is the formatted prefix
/// string. `wait()` runs concurrently with the pump tasks so we don't
/// drop output if the child exits before the pipes drain.
pub(crate) async fn run_command(
    mut cmd: Command,
    mode: &OutputMode,
) -> miette::Result<std::process::ExitStatus> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .into_diagnostic()
        .wrap_err("failed to spawn child process")?;
    // `.expect` is sound here: we just set both streams to `piped()`
    // above, so `take()` on either is guaranteed to return `Some`.
    // Surfacing this as `unwrap_or_else` -> miette would only ever
    // fire on a future stdio refactor that bypassed the lines above,
    // and that's exactly the sort of bug we want to crash loudly on.
    let stdout = child
        .stdout
        .take()
        .expect("stdout was piped above; take() must succeed");
    let stderr = child
        .stderr
        .take()
        .expect("stderr was piped above; take() must succeed");

    // Compute color gating per stream — stdout and stderr can have
    // different TTY/redirection state (e.g. `aube run -r --parallel
    // build 2>errors.log` keeps stdout on a TTY while stderr is a
    // file). Using a single prefix would either color a non-TTY file
    // or skip color on a real TTY for one of the streams.
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let stdout_prefix = format_line_prefix(mode, std::io::stdout().is_terminal() && !no_color);
    let stderr_prefix = format_line_prefix(mode, std::io::stderr().is_terminal() && !no_color);
    let stdout_pump = tokio::spawn(pump_lines(stdout, stdout_prefix, false));
    let stderr_pump = tokio::spawn(pump_lines(stderr, stderr_prefix, true));

    let status = child
        .wait()
        .await
        .into_diagnostic()
        .wrap_err("failed to wait on child process")?;

    // Pumps will see EOF once the child closes its pipe ends on exit
    // and finish on their own. JoinHandle errors here can only be a
    // panic in the pump (genuinely unexpected) or a cancellation
    // (we never cancel them). Surface so a future refactor can't
    // silently swallow a pump panic.
    stdout_pump
        .await
        .map_err(|e| miette!("stdout pump task failed: {e}"))?
        .map_err(|e| miette!("stdout pump io error: {e}"))?;
    stderr_pump
        .await
        .map_err(|e| miette!("stderr pump task failed: {e}"))?
        .map_err(|e| miette!("stderr pump io error: {e}"))?;
    Ok(status)
}

/// Build the per-line prefix string up front so we don't re-format it
/// on every line. Empty for [`OutputMode::NoPrefix`]. `color` is taken
/// as a parameter rather than queried inside so callers can gate it
/// per stream (stdout and stderr can have independent TTY state).
fn format_line_prefix(mode: &OutputMode, color: bool) -> String {
    match mode {
        OutputMode::NoPrefix => String::new(),
        OutputMode::Prefix { name, color_index } => {
            if color {
                let code = PALETTE[*color_index];
                format!("\x1b[{code}m{name}\x1b[0m: ")
            } else {
                format!("{name}: ")
            }
        }
    }
}

/// Drain `reader` line-by-line, emitting each line with `prefix`
/// prepended. `is_stderr` selects the destination stream so child
/// stderr stays distinguishable for callers piping aube's own stderr
/// separately. Stderr lines route through `aube_scripts` so the
/// `SilentStderrGuard`'s saved real-stderr fd is honored — under
/// `--silent` aube redirects fd 2 to `/dev/null`, and `eprintln!` would
/// silently drop child stderr in `--silent --parallel` mode.
///
/// Uses `read_line` (UTF-8) rather than `read_until(b'\n')` because
/// npm-style script output is always text. A child that emits invalid
/// UTF-8 surfaces an io error here, which propagates as a pump
/// failure — visible noise rather than silently dropped output.
async fn pump_lines<R>(reader: R, prefix: String, is_stderr: bool) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if is_stderr {
            aube_scripts::write_line_to_real_stderr(&format!("{prefix}{trimmed}"));
        } else {
            println!("{prefix}{trimmed}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise the full pipe + pump path: a child that writes a known
    /// sequence of lines to both stdout and stderr should produce a
    /// successful `ExitStatus` with the pumps draining cleanly.
    #[tokio::test]
    async fn run_command_with_prefix_drains_both_streams() {
        // `printf` over `sh -c` is portable enough across the unix
        // CI matrices we care about; aube already tests this way in
        // bats. Skip on Windows — the script invocation differs and
        // the multiplexer is unix-tested via real run/exec usage.
        if cfg!(windows) {
            return;
        }
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("echo out1; echo err1 1>&2; echo out2");
        let mode = OutputMode::Prefix {
            name: "demo".to_string(),
            color_index: 0,
        };
        let status = run_command(cmd, &mode).await.unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn run_command_no_prefix_mode_still_pipes() {
        // `NoPrefix` mode pipes the same way `Prefix` does — only the
        // formatted prefix is empty. A child that exits cleanly with
        // no output should still produce a successful status.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("true");
        let status = run_command(cmd, &OutputMode::NoPrefix).await.unwrap();
        assert!(status.success());
    }

    #[test]
    fn prefix_color_rotates_modulo_palette() {
        // Same package name at indices 0 and PALETTE.len() should
        // pick the same color slot — round-robin without overflow.
        let a = OutputMode::prefix(Some("foo"), 0);
        let b = OutputMode::prefix(Some("foo"), PALETTE.len());
        match (a, b) {
            (
                OutputMode::Prefix { color_index: i, .. },
                OutputMode::Prefix { color_index: j, .. },
            ) => assert_eq!(i, j),
            _ => panic!("named pkg should produce Prefix mode"),
        }
    }

    #[test]
    fn unnamed_pkg_falls_back_to_noprefix() {
        assert!(matches!(OutputMode::prefix(None, 3), OutputMode::NoPrefix));
    }

    #[test]
    fn format_line_prefix_skips_color_when_disabled() {
        // The TTY/NO_COLOR resolution lives in the caller (per-stream),
        // so the formatter only needs to react to a `color` flag. Pass
        // `false` directly — no need to mutate process-global env state
        // and race other tests on the same harness.
        let p = format_line_prefix(
            &OutputMode::Prefix {
                name: "demo".to_string(),
                color_index: 0,
            },
            false,
        );
        assert_eq!(p, "demo: ");
    }

    #[test]
    fn format_line_prefix_emits_ansi_when_color_enabled() {
        let p = format_line_prefix(
            &OutputMode::Prefix {
                name: "demo".to_string(),
                color_index: 0,
            },
            true,
        );
        assert_eq!(p, "\x1b[31mdemo\x1b[0m: ");
    }
}
