use super::util::non_empty;

/// Validate a `tokenHelper` value against the same contract pnpm
/// 10.27.0 introduced for CVE-2025-69262. The value must be a bare
/// absolute path to an executable, with no shell metacharacters,
/// no whitespace-separated arguments, no environment substitution
/// markers. `run_token_helper` spawns the path directly without a
/// shell wrapper, so a post-fix attacker who somehow gets a value
/// past this sanitizer still cannot smuggle a shell pipeline, only
/// a file name that has to exist on disk as an executable.
pub(super) fn sanitize_token_helper(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Absolute on unix starts with `/`. Absolute on Windows starts
    // with a drive letter (`C:\`, `C:/`) or a UNC prefix (`\\`). The
    // `\\` form also covers `\\?\` and `\\.\`.
    let is_unix_absolute = trimmed.starts_with('/');
    let is_windows_absolute = trimmed.starts_with("\\\\")
        || trimmed.as_bytes().get(1).is_some_and(|&b| b == b':')
            && trimmed
                .as_bytes()
                .first()
                .is_some_and(|&b| b.is_ascii_alphabetic())
            && matches!(trimmed.as_bytes().get(2), Some(b'/' | b'\\'));
    if !(is_unix_absolute || is_windows_absolute) {
        return None;
    }
    // Reject any shell metacharacter or whitespace. A legitimate
    // helper is a single executable path. Arguments go into the
    // binary's own config, not the tokenHelper value.
    if trimmed.chars().any(|c| {
        c.is_ascii_whitespace()
            || matches!(
                c,
                '"' | '\'' | '`' | '$' | '&' | '|' | ';' | '<' | '>' | '(' | ')' | '*' | '?' | '\0'
            )
    }) {
        return None;
    }
    Some(trimmed.to_string())
}
pub(crate) fn run_token_helper(command: &str) -> Option<String> {
    // Spawn the helper directly rather than through `sh -c` / `cmd /C`.
    // The value is already sanitized by `sanitize_token_helper` at
    // config-load time (must be a bare absolute path with no shell
    // metacharacters), so any new path that ever reaches this sink
    // still cannot be reinterpreted as a shell pipeline. Removing
    // the shell wrapper closes the sink even if sanitization is
    // bypassed in the future.
    let output = match std::process::Command::new(command).output() {
        Ok(o) => o,
        Err(e) => {
            // Log the spawn failure so a user with a broken
            // tokenHelper path (missing binary, wrong permissions)
            // gets a clear hint instead of a mysterious 401 from
            // the registry.
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_TOKEN_HELPER_SPAWN_FAILED,
                "tokenHelper {command:?} could not be spawned: {e}"
            );
            return None;
        }
    };
    if !output.status.success() {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_TOKEN_HELPER_NON_ZERO_EXIT,
            "tokenHelper {command:?} exited with {}",
            output.status
        );
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    non_empty(token.lines().next().unwrap_or_default().to_string())
}
