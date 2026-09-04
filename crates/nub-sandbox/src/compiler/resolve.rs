//! `$(…)` command substitution for env values. stdout (trailing newline trimmed)
//! becomes the value, whole or embedded — `"postgres://u:$(op read …)@h/db"`.
//!
//! Resolves ONLY in a scope holding the `env_substitution` capability (approved
//! `nub.jsonc` / `scriptsMeta`) — NEVER in a `dependenciesMeta` scope. The caller
//! assigns each scope its `ScopeCapabilities`; an ungranted `$(…)` is a hard
//! [`CompileError`], never a silent exec (trust inversion is the whole reason the
//! sandbox exists).

/// Does the value contain a `$(…)` substitution?
pub fn has_substitution(value: &str) -> bool {
    find_next(value, 0).is_some()
}

/// Does the value contain a `$(` opener at all (balanced or not)? Paired with
/// [`has_substitution`] to detect an UNTERMINATED substitution: opener present but
/// no balanced close (`has_substitution` false). nub does no shell substitution
/// outside a complete `$(…)`, so such a value is named as a substitution error
/// rather than passed through literally (a footgun) or mislabeled "unknown env type".
pub fn has_open_substitution(value: &str) -> bool {
    value.contains("$(")
}

/// A `$(` opener with no balanced close: nub does no shell substitution outside a
/// complete `$(…)`, so an unterminated one is named rather than passed through as a
/// literal or mislabeled "unknown env type" (D18). Shared by every reject site.
pub(crate) const UNTERMINATED_SUBST_MSG: &str =
    "unterminated `$(…)` command substitution — expected a closing `)`";

/// Locate the next `$(` … `)` span (with paren nesting) starting at `from`.
/// Returns `(open_idx, close_idx_exclusive, inner)`.
fn find_next(value: &str, from: usize) -> Option<(usize, usize, String)> {
    let bytes = value.as_bytes();
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1] == b'(' {
            let mut depth = 1;
            let mut j = i + 2;
            while j < bytes.len() {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            let inner = value[i + 2..j].to_string();
                            return Some((i, j + 1, inner));
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            return None; // unbalanced — treated as no-substitution by callers
        }
        i += 1;
    }
    None
}

/// The command runner seam. Real runs shell out; tests inject a deterministic
/// stub. Returns the command's stdout on success.
pub trait CommandRunner {
    fn run(&self, command: &str) -> Result<String, String>;
}

/// The production runner: `sh -c <command>` (POSIX) / `cmd /C <command>`
/// (Windows), capturing stdout. A non-zero exit or spawn failure is an error.
pub struct ShellRunner;

impl CommandRunner for ShellRunner {
    fn run(&self, command: &str) -> Result<String, String> {
        let output = shell_command(command)
            .output()
            .map_err(|e| format!("failed to spawn `$( {command} )`: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "`$( {command} )` exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
}

#[cfg(windows)]
fn shell_command(command: &str) -> std::process::Command {
    let mut c = std::process::Command::new("cmd");
    c.arg("/C").arg(command);
    c
}

/// Resolve every `$(…)` in `value` using `runner`, returning the substituted
/// string. stdout is trimmed of a single trailing newline (shell convention).
pub fn resolve_with(value: &str, runner: &dyn CommandRunner) -> Result<String, String> {
    let mut out = String::new();
    let mut cursor = 0;
    while let Some((open, close, inner)) = find_next(value, cursor) {
        out.push_str(&value[cursor..open]);
        let stdout = runner.run(inner.trim())?;
        out.push_str(stdout.trim_end_matches('\n').trim_end_matches('\r'));
        cursor = close;
    }
    // A `$(` left in the trailing literal is an unterminated opener AFTER a balanced
    // span (e.g. `$(echo hi) $(oops`). `find_next` skips it (never balances), so it
    // would otherwise ship silently as literal text — reject instead (D18). Only the
    // tail can hold one: any earlier `$(` would make the FIRST find_next unbalance.
    if has_open_substitution(&value[cursor..]) {
        return Err(UNTERMINATED_SUBST_MSG.to_string());
    }
    out.push_str(&value[cursor..]);
    Ok(out)
}
