//! `aube logout` — remove a registry auth token from the user's `~/.npmrc`.
//!
//! Inverse of `aube login`. Strips `//host/:_authToken` or
//! `//host/:@scope:_authToken` (and, when a `--scope` is passed, the
//! matching `@scope:registry` mapping) from the user-level `.npmrc` in
//! place, leaving every other entry untouched.

use crate::commands::npmrc::{NpmrcEdit, registry_host_key, resolve_registry, user_npmrc_path};
use clap::Args;
use miette::miette;

#[derive(Debug, Args)]
pub struct LogoutArgs {
    /// Scope whose registry mapping should also be removed (e.g. `@myorg`).
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(args: LogoutArgs) -> miette::Result<()> {
    args.network.install_overrides();
    if let Some(scope) = &args.scope
        && !scope.starts_with('@')
    {
        return Err(miette!("--scope must start with `@` (got `{scope}`)"));
    }

    let registry = resolve_registry(args.network.registry.as_deref(), args.scope.as_deref())?;
    let host_key = registry_host_key(&registry);

    let path = user_npmrc_path()?;
    if !path.exists() {
        eprintln!("No ~/.npmrc to edit; nothing to do");
        return Ok(());
    }

    let mut edit = NpmrcEdit::load(&path)?;
    let removed_token = match &args.scope {
        Some(scope) => {
            let scoped_token_prefix = format!("{host_key}:");
            edit.remove_matching(|key| {
                key.strip_prefix(&scoped_token_prefix)
                    .and_then(|rest| rest.strip_suffix(":_authToken"))
                    .is_some_and(|stored_scope| stored_scope.eq_ignore_ascii_case(scope))
            })
        }
        None => {
            let unscoped_token_key = format!("{host_key}:_authToken");
            let scoped_token_prefix = format!("{host_key}:@");
            edit.remove_matching(|key| {
                key == unscoped_token_key.as_str()
                    || (key.starts_with(&scoped_token_prefix) && key.ends_with(":_authToken"))
            })
        }
    };
    let removed_scope = match &args.scope {
        Some(scope) => edit.remove_matching(|key| {
            key.strip_suffix(":registry")
                .is_some_and(|stored_scope| stored_scope.eq_ignore_ascii_case(scope))
        }),
        None => false,
    };

    if removed_token || removed_scope {
        edit.save(&path)?;
        eprintln!("Logged out of {registry}");
    } else {
        eprintln!("No credentials found for {registry}; nothing to do");
    }
    Ok(())
}
