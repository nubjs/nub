//! `aube login` — store a registry auth token in the user's `~/.npmrc`.
//!
//! Two flows are supported via `--auth-type`:
//!
//! - `legacy` (default): the token comes from `$AUBE_AUTH_TOKEN`, piped
//!   stdin, or a masked interactive prompt — in that order — and is
//!   written straight to `~/.npmrc` as `//host/:_authToken=<tok>`.
//! - `web`: the npm OAuth web flow. POSTs `{registry}/-/v1/login`, opens
//!   the returned `loginUrl` in the user's browser, and polls `doneUrl`
//!   until the registry returns the minted token, which is then written
//!   to `~/.npmrc` exactly like the legacy case.
//!
//! If `--scope` is given, the scope->registry mapping is written
//! alongside the token so the next `aube install` will route that
//! scope's packages to the right registry without further config.

use crate::commands::npmrc::{NpmrcEdit, registry_host_key, resolve_registry, user_npmrc_path};
use clap::Args;
use miette::{IntoDiagnostic, miette};
use std::io::{BufRead, IsTerminal};
use std::time::{Duration, Instant};

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Authentication flow: `legacy` (token paste; default) or `web`
    /// (OAuth flow against `{registry}/-/v1/login`).
    #[arg(long, value_name = "TYPE", default_value = "legacy")]
    pub auth_type: String,

    /// Scope to bind this registry to (e.g. `@myorg`).
    ///
    /// When set, the scope->registry mapping is also written to
    /// `~/.npmrc`.
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(args: LoginArgs) -> miette::Result<()> {
    args.network.install_overrides();
    if args.auth_type != "legacy" && args.auth_type != "web" {
        return Err(miette!(
            "--auth-type={} is not supported (expected `legacy` or `web`)",
            args.auth_type
        ));
    }

    if let Some(scope) = &args.scope
        && !scope.starts_with('@')
    {
        return Err(miette!("--scope must start with `@` (got `{scope}`)"));
    }

    let registry = resolve_registry(args.network.registry.as_deref(), args.scope.as_deref())?;
    let host_key = registry_host_key(&registry);
    let token = if args.auth_type == "web" {
        web_login(&registry).await?
    } else {
        read_token()?
    };

    let path = user_npmrc_path()?;
    let mut edit = NpmrcEdit::load(&path)?;
    if let Some(scope) = &args.scope {
        let scope = scope.to_ascii_lowercase();
        edit.set(&format!("{host_key}:{scope}:_authToken"), &token);
        edit.set(&format!("{scope}:registry"), &registry);
    } else {
        edit.set(&format!("{host_key}:_authToken"), &token);
    }
    edit.save(&path)?;

    eprintln!(
        "Logged in to {registry} (token saved to {})",
        path.display()
    );
    Ok(())
}

/// Read the auth token from the branded `<PREFIX>_AUTH_TOKEN` env var
/// (standalone aube → `$AUBE_AUTH_TOKEN`), then piped stdin, and finally an
/// interactive `demand` prompt — in that order. The env var is the escape
/// hatch for CI; the piped case is for `echo $TOKEN | … login`; the prompt is
/// the human path, rendered as a masked password field so the token doesn't
/// echo to the terminal or end up in shell scrollback. An embedder with no
/// `env_prefix` reads no branded var and falls straight through to stdin.
fn read_token() -> miette::Result<String> {
    if let Some(prefix) = aube_util::embedder().env_prefix
        && let Ok(tok) = std::env::var(format!("{prefix}_AUTH_TOKEN"))
    {
        let tok = tok.trim();
        if !tok.is_empty() {
            return Ok(tok.to_string());
        }
    }

    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .into_diagnostic()
            .map_err(|e| miette!("failed to read token from stdin: {e}"))?;
        let line = line.trim().to_string();
        if line.is_empty() {
            return Err(miette!("no token provided on stdin"));
        }
        return Ok(line);
    }

    let token = demand::Input::new("Token")
        .description("Paste your registry auth token")
        .password(true)
        .run()
        .into_diagnostic()
        .map_err(|e| miette!("failed to read token: {e}"))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(miette!("no token entered"));
    }
    Ok(token)
}

/// Drive the npm OAuth web login flow against `registry`.
///
/// 1. POST `{registry}-/v1/login` with `{hostname}`. The registry replies
///    with `{loginUrl, doneUrl}`.
/// 2. Print `loginUrl` and — if we're on an interactive TTY and the user
///    hasn't opted out via the branded `<PREFIX>_NO_BROWSER` (standalone
///    aube → `AUBE_NO_BROWSER`) — try to open it in the default browser.
/// 3. Poll `doneUrl`. The registry returns 202 (with optional
///    `Retry-After`) while the user hasn't finished, and 200 with
///    `{token}` once login succeeds. Give up after five minutes so a
///    stuck flow can't wedge a script forever.
async fn web_login(registry: &str) -> miette::Result<String> {
    let base = if registry.ends_with('/') {
        registry.to_string()
    } else {
        format!("{registry}/")
    };
    let login_endpoint = format!("{base}-/v1/login");

    let client = aube_util::http::with_webpki_root_fallback(reqwest::Client::builder())
        .user_agent(aube_util::embedder().user_agent)
        .build()
        .into_diagnostic()
        .map_err(|e| miette!("failed to build http client: {e}"))?;

    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| aube_util::embedder().name.to_string());

    let resp = client
        .post(&login_endpoint)
        .json(&serde_json::json!({ "hostname": hostname }))
        .send()
        .await
        .into_diagnostic()
        .map_err(|e| miette!("failed to POST {login_endpoint}: {e}"))?;

    if !resp.status().is_success() {
        return Err(miette!(
            "web login failed: {login_endpoint} returned {}",
            resp.status()
        ));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .into_diagnostic()
        .map_err(|e| miette!("failed to parse /-/v1/login response: {e}"))?;

    let login_url = body
        .get("loginUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| miette!("missing `loginUrl` in /-/v1/login response"))?
        .to_string();
    let done_url = body
        .get("doneUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| miette!("missing `doneUrl` in /-/v1/login response"))?
        .to_string();

    eprintln!("Open this URL in your browser to sign in:");
    eprintln!("  {login_url}");
    // Branded opt-out `<PREFIX>_NO_BROWSER` (standalone aube → AUBE_NO_BROWSER);
    // an embedder with no env_prefix has no such knob, so it never opts out.
    let no_browser = aube_util::embedder()
        .env_prefix
        .is_some_and(|prefix| std::env::var_os(format!("{prefix}_NO_BROWSER")).is_some());
    if std::io::stderr().is_terminal() && !no_browser {
        let _ = open_browser(&login_url);
    }
    eprintln!("Waiting for authentication...");

    poll_done(&client, &done_url).await
}

/// Poll `done_url` until it returns 200 with a token, 202 keeps waiting.
async fn poll_done(client: &reqwest::Client, done_url: &str) -> miette::Result<String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut delay = Duration::from_millis(500);

    loop {
        if Instant::now() >= deadline {
            return Err(miette!("timed out waiting for web login to complete"));
        }
        let resp = client
            .get(done_url)
            .send()
            .await
            .into_diagnostic()
            .map_err(|e| miette!("failed to GET {done_url}: {e}"))?;

        match resp.status().as_u16() {
            202 => {
                if let Some(retry) = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    delay = Duration::from_secs(retry.clamp(1, 10));
                }
                tokio::time::sleep(delay).await;
            }
            200 => {
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .into_diagnostic()
                    .map_err(|e| miette!("failed to parse doneUrl response: {e}"))?;
                return body
                    .get("token")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| miette!("missing `token` in doneUrl response"));
            }
            status => {
                return Err(miette!("web login failed: doneUrl returned {status}"));
            }
        }
    }
}

/// Best-effort launch the OS's default browser. Failures are intentionally
/// swallowed by the caller — the URL is always printed first, so the user
/// can copy it manually if we can't spawn a browser (headless env, missing
/// `xdg-open`, etc).
///
/// The URL is validated against a strict `http(s)://` shape before being
/// passed to the platform launcher. On Windows the launcher goes through
/// `cmd /c start`, and `cmd.exe` re-parses its argument after
/// stdlib quoting so a URL containing `&`, `|`, `^`, or `%VAR%` from a
/// hostile registry login response would otherwise become a command
/// injection primitive (same class as CVE-2024-24576 / BatBadBut).
fn open_browser(url: &str) -> std::io::Result<()> {
    if !is_safe_browser_url(url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to open non-http(s) or unsafe URL: {url:?}"),
        ));
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).status()?;
    }
    #[cfg(target_os = "windows")]
    {
        // `cmd.exe` expands `%VAR%` inside double-quoted args, so the
        // stdlib arg quoting alone cannot neutralize a hostile URL
        // containing `%SYSTEMROOT%` or similar. Double every `%` to
        // suppress expansion, matching the same escape the lifecycle
        // script runner already applies in `aube-scripts`.
        let escaped = url.replace('%', "%%");
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &escaped])
            .status()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).status()?;
    }
    Ok(())
}

/// Reject anything that isn't a plain `http(s)://` URL, or that contains
/// a character `cmd.exe` re-parses after stdlib arg quoting. Accepts the
/// set of characters needed for a realistic OAuth / device-code flow
/// (query strings, percent-encoding, fragments).
fn is_safe_browser_url(url: &str) -> bool {
    let rest = match url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        Some(r) => r,
        None => return false,
    };
    if rest.is_empty() || rest.len() > 2048 {
        return false;
    }
    // RFC 3986 unreserved + reserved chars. Excludes whitespace, any
    // control character, and `"` / `\\` / `|` / `^` / `<` / `>` / `` ` ``
    // so a hostile URL can neither close the stdlib's double-quoted arg
    // nor pivot into a shell metachar. `%` stays in the allow list for
    // legitimate percent-encoding; the Windows `start` path separately
    // doubles every `%` to suppress `cmd.exe` variable expansion.
    rest.chars().all(|c| {
        matches!(c,
            'a'..='z' | 'A'..='Z' | '0'..='9'
            | '-' | '_' | '.' | '~'
            | ':' | '/' | '?' | '#' | '[' | ']' | '@'
            | '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ','
            | ';' | '=' | '%'
        )
    })
}
