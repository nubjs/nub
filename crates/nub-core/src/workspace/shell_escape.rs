//! Shell-argument escaping for appended `nub run` / dlx args, ported faithfully
//! from npm's `@npmcli/promise-spawn` (`lib/escape.js`).
//!
//! npm runs a package script as `sh -c "<body> <arg1> <arg2> …"` (or
//! `cmd /d /s /c …` on Windows), where `<body>` is the unescaped `package.json`
//! script — so the body's own globs / expansions still run — and each appended
//! argument is escaped so it reaches the script as a single literal token. nub
//! used to join the args unquoted, so `nub run s -- 'a b' '$X' 'x;y'` split,
//! expanded, and re-parsed them. Matching npm's escape gives byte-for-byte arg
//! fidelity without quoting the body. This is a compatibility fix, not a
//! security boundary: the args are the user's own argv (see A42).
//!
//! The npm algorithm is stable (identical across the npm in `.repos/node` and
//! npm 11.9.0). Do not "improve" it — divergence is the bug.

/// POSIX `sh -c` single-argument escape. An argument with no shell-special
/// character is returned unchanged (so plain words and `--flags` are untouched);
/// otherwise it is wrapped in single quotes with embedded `'` rendered as
/// `'\''`, then npm's two cosmetic cleanups are applied (they shorten the string
/// without changing how `sh` tokenizes it).
pub fn sh(input: &str) -> String {
    if input.is_empty() {
        return "''".to_string();
    }

    // No shell-special chars → pass through verbatim. Mirrors npm's
    // /[\t\n\r "#$&'()*;<>?\\`|~]/ test exactly.
    let special = |c: char| {
        matches!(
            c,
            '\t' | '\n'
                | '\r'
                | ' '
                | '"'
                | '#'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | ';'
                | '<'
                | '>'
                | '?'
                | '\\'
                | '`'
                | '|'
                | '~'
        )
    };
    if !input.contains(special) {
        return input.to_string();
    }

    // Wrap in single quotes, escaping embedded single quotes as '\''.
    let quoted = format!("'{}'", input.replace('\'', r"'\''"));
    // npm: .replace(/^(?:'')+(?!$)/, '') then .replace(/\\'''/g, "\\'").
    let cleaned = strip_leading_empty_quote_pairs(&quoted);
    cleaned.replace(r"\'''", r"\'")
}

/// Drop leading `''` pairs (npm's `/^(?:'')+(?!$)/` → ``). The negative
/// lookahead means: never consume the entire string — if the run of pairs is the
/// whole string, leave the last pair in place (so `sh("")`'s `''` survives, and
/// `''''` collapses to `''`).
fn strip_leading_empty_quote_pairs(s: &str) -> String {
    let b = s.as_bytes();
    let mut pairs = 0;
    while 2 * (pairs + 1) <= b.len() && b[2 * pairs] == b'\'' && b[2 * pairs + 1] == b'\'' {
        pairs += 1;
    }
    if pairs == 0 {
        return s.to_string();
    }
    // (?!$): if the pairs reach end-of-string, backtrack one so the match ends
    // before EOS; otherwise strip them all.
    let strip = if 2 * pairs == b.len() {
        pairs - 1
    } else {
        pairs
    };
    s[2 * strip..].to_string()
}

/// `cmd.exe` single-argument escape, ported from npm's `escape.cmd`. Quotes
/// whitespace/`"` per the MS command-line rules, then prefixes cmd metacharacters
/// with `^`. `double_escape` repeats the `^` pass — npm does this when the script
/// target is a `.cmd`/`.bat` file, which re-parses its arguments once more.
pub fn cmd(input: &str, double_escape: bool) -> String {
    if input.is_empty() {
        return "\"\"".to_string();
    }

    let chars: Vec<char> = input.chars().collect();
    let needs_quotes = chars
        .iter()
        .any(|&c| matches!(c, ' ' | '\t' | '\n' | '\u{0B}' | '"'));

    let mut result = if !needs_quotes {
        input.to_string()
    } else {
        // Backslash/quote handling per
        // blogs.msdn.microsoft.com/.../everyone-quotes-command-line-arguments-the-wrong-way.
        let mut r = String::from("\"");
        let mut i = 0;
        loop {
            let mut slash_count = 0;
            while i < chars.len() && chars[i] == '\\' {
                i += 1;
                slash_count += 1;
            }
            if i == chars.len() {
                r.push_str(&"\\".repeat(slash_count * 2));
                break;
            }
            if chars[i] == '"' {
                r.push_str(&"\\".repeat(slash_count * 2 + 1));
                r.push('"');
            } else {
                r.push_str(&"\\".repeat(slash_count));
                r.push(chars[i]);
            }
            i += 1;
        }
        r.push('"');
        r
    };

    result = caret_escape(&result);
    if double_escape {
        result = caret_escape(&result);
    }
    result
}

/// Prefix cmd.exe metacharacters with `^` (npm's `/[ !%^&()<>|"]/g → "^$&"`).
fn caret_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            ' ' | '!' | '%' | '^' | '&' | '(' | ')' | '<' | '>' | '|' | '"'
        ) {
            out.push('^');
        }
        out.push(c);
    }
    out
}

/// Does `shell` invoke `cmd.exe`? Mirrors npm's `/(?:^|\\)cmd(?:\.exe)?$/i`, so a
/// custom `script-shell` of `bash`/`zsh` selects POSIX escaping while the Windows
/// default (`cmd`) selects cmd escaping.
pub fn is_cmd(shell: &str) -> bool {
    let lower = shell.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    stem == "cmd" || stem.ends_with("\\cmd")
}

/// Best-effort detection of whether a script body's command is a `.cmd`/`.bat`
/// batch file, used to choose `double_escape` for the cmd path. Checks the first
/// whitespace-delimited token's literal extension.
///
/// LIMITATION: unlike npm — which resolves the token through `PATH`/`PATHEXT`
/// (`which.sync`) — this does not resolve PATHEXT, so a body like `eslint .`
/// whose `eslint` resolves to `eslint.cmd` is treated as non-batch and
/// single-escaped. Closing that needs Windows PATHEXT resolution and Windows
/// validation; tracked as the residual Windows gap on A42.
pub fn body_targets_batch_file(script_body: &str) -> bool {
    let first = script_body.split_whitespace().next().unwrap_or("");
    let lower = first.to_ascii_lowercase();
    lower.ends_with(".cmd") || lower.ends_with(".bat")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── POSIX sh (the verified-against-npm path) ──────────────────────────

    #[test]
    fn sh_passes_plain_words_and_flags_through() {
        // No special chars → byte-identical to the input, so the common case
        // (script names, --flags, simple values) is unchanged from a raw join.
        for s in ["build", "--watch", "src/index.ts", "key=value", "a-b_c.d"] {
            assert_eq!(sh(s), s, "{s:?} should pass through unquoted");
        }
    }

    #[test]
    fn sh_quotes_metacharacters_literally() {
        // The args from the A42 empirical comparison against npm 11.9.0.
        assert_eq!(sh("hello world"), "'hello world'");
        assert_eq!(sh("a*b"), "'a*b'");
        assert_eq!(sh("$HOME"), "'$HOME'");
        assert_eq!(sh("x;y"), "'x;y'");
    }

    #[test]
    fn sh_escapes_embedded_single_quotes() {
        // The classic POSIX close-escape-reopen, matching npm byte-for-byte.
        assert_eq!(sh("it's"), r"'it'\''s'");
        // Leading quote triggers npm's leading-''-pair cleanup.
        assert_eq!(sh("'foo"), r"\''foo'");
    }

    #[test]
    fn sh_empty_string_is_a_quoted_empty() {
        assert_eq!(sh(""), "''");
    }

    // ── Windows cmd (algorithm verified by unit test; runtime is Windows-CI) ──

    #[test]
    fn cmd_passes_plain_words_through() {
        assert_eq!(cmd("build", false), "build");
        assert_eq!(cmd("src\\index.ts", false), "src\\index.ts");
    }

    #[test]
    fn cmd_quotes_and_carets_metacharacters() {
        // Space forces quoting; the quotes themselves are caret-escaped.
        assert_eq!(cmd("hello world", false), "^\"hello^ world^\"");
        // A bare metachar with no whitespace/quote is caret-escaped, not quoted.
        assert_eq!(cmd("a&b", false), "a^&b");
    }

    #[test]
    fn cmd_double_escape_repeats_the_caret_pass() {
        // .cmd/.bat targets re-parse args, so npm carets twice.
        assert_eq!(cmd("a&b", true), "a^^^&b");
    }

    #[test]
    fn cmd_empty_string_is_quoted_empty() {
        assert_eq!(cmd("", false), "\"\"");
    }

    #[test]
    fn is_cmd_matches_npm_regex() {
        assert!(is_cmd("cmd"));
        assert!(is_cmd("cmd.exe"));
        assert!(is_cmd("C:\\Windows\\System32\\cmd.exe"));
        assert!(is_cmd("\\cmd"));
        assert!(!is_cmd("bash"));
        assert!(!is_cmd("/bin/sh"));
        assert!(!is_cmd("mycmd")); // no boundary before "cmd"
    }

    #[test]
    fn body_targets_batch_file_checks_first_token_extension() {
        assert!(body_targets_batch_file("foo.cmd --flag"));
        assert!(body_targets_batch_file("build.bat"));
        assert!(!body_targets_batch_file("eslint .")); // PATHEXT gap (documented)
        assert!(!body_targets_batch_file("node script.js"));
    }
}
