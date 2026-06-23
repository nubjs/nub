//! `aube token list|create|revoke` — manage the registry auth tokens of
//! the authenticated account. Mirrors `npm token` (pnpm does not implement
//! this verb; nub implements it for npm parity).
//!
//! - `list` — list the account's tokens (key, masked value, scope).
//! - `create` — create a classic auth token. The account password is read
//!   from the `--password`/`-p` flag or, if absent, from stdin (so it
//!   isn't captured in shell history). `--read-only` and `--cidr` map to
//!   the create-token request.
//! - `revoke <key>` — revoke a token by its key (or token-value prefix).
//!
//! All operations require an existing auth token in `.npmrc` (you must be
//! logged in to manage tokens).

use clap::{Args, Subcommand};
use miette::miette;

use crate::commands::make_client;

#[derive(Debug, Args)]
pub struct TokenArgs {
    #[command(subcommand)]
    pub command: TokenCommand,

    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// List the account's auth tokens.
    #[command(visible_alias = "ls")]
    List,
    /// Create a new auth token.
    Create {
        /// Account password. If omitted, read from stdin.
        #[arg(short = 'p', long)]
        password: Option<String>,
        /// Create a read-only token.
        #[arg(long)]
        read_only: bool,
        /// Restrict the token to these CIDR ranges. Repeatable.
        #[arg(long, value_name = "CIDR")]
        cidr: Vec<String>,
    },
    /// Revoke a token by its key (or token-value prefix).
    #[command(visible_alias = "rm")]
    Revoke { key: String },
}

pub async fn run(args: TokenArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);

    match args.command {
        TokenCommand::List => {
            let tokens = client.list_tokens().await.map_err(map_err)?;
            if tokens.is_empty() {
                eprintln!("no tokens found");
                return Ok(());
            }
            for t in tokens {
                let scope = if t.readonly {
                    "read-only"
                } else {
                    "read-write"
                };
                let created = t.created.as_deref().unwrap_or("");
                println!("{}\t{}\t{scope}\t{created}", t.key, t.token);
            }
        }
        TokenCommand::Create {
            password,
            read_only,
            cidr,
        } => {
            let password = match password {
                Some(p) => p,
                None => read_password_from_stdin()?,
            };
            let created = client
                .create_token(&password, read_only, &cidr)
                .await
                .map_err(map_err)?;
            // The full token is shown exactly once, here.
            if let Some(token) = created.get("token").and_then(|t| t.as_str()) {
                println!("{token}");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&created).unwrap_or_default()
                );
            }
        }
        TokenCommand::Revoke { key } => {
            client.revoke_token(&key).await.map_err(map_err)?;
            println!("revoked {key}");
        }
    }
    Ok(())
}

fn read_password_from_stdin() -> miette::Result<String> {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| miette!("failed to read password from stdin: {e}"))?;
    let pw = line.trim_end_matches(['\r', '\n']).to_string();
    if pw.is_empty() {
        return Err(miette!(
            "a password is required (pass --password or pipe it on stdin)"
        ));
    }
    Ok(pw)
}

fn map_err(e: aube_registry::Error) -> miette::Report {
    match e {
        aube_registry::Error::Unauthorized => {
            miette!(
                "not authenticated — run `{}` first",
                aube_util::cmd("login")
            )
        }
        aube_registry::Error::NotFound(n) => miette!("not found: {n}"),
        aube_registry::Error::RegistryWrite { status, body } => {
            miette!("registry rejected the request (HTTP {status}): {body}")
        }
        other => miette!("{other}"),
    }
}
