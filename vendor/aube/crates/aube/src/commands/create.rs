use super::dlx::{self, DlxArgs};
use clap::{Args, CommandFactory};
use miette::miette;

#[derive(Debug, Args)]
// Same contract as dlx: every positional after <TEMPLATE> is forwarded
// verbatim to the scaffold binary, including `--help` / `--version`.
// Without `disable_help_flag`, clap intercepts `-h` / `--help` before
// positional parsing and `aube create vite --help` would show aube's
// help for `create` instead of `create-vite`'s help — diverging from
// pnpm, where the flag reaches the template binary.
//
// We still want `aube create --help` on its own (no template) to print
// aube's help for this subcommand, so the template is collapsed into
// `params` and the handler intercepts a leading `--help` / `-h` before
// doing any name mapping.
#[command(disable_help_flag = true)]
pub struct CreateArgs {
    /// Template package name followed by any args to pass through to
    /// the scaffold binary.
    ///
    /// The first positional is the template; the rest are forwarded
    /// verbatim to `create-<template>`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub params: Vec<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

/// `aube create <template> [args...]`
///
/// Scaffold a project from a `create-*` starter kit. Matches pnpm/npm
/// semantics: `aube create foo` runs the `create-foo` package via dlx,
/// `aube create @scope/foo` runs `@scope/create-foo`, etc.
pub async fn run(args: CreateArgs) -> miette::Result<Option<i32>> {
    args.network.install_overrides();
    let CreateArgs { params, network } = args;

    // Bare `aube create` or `aube create --help` / `-h` prints aube's
    // help for the subcommand. Once a template is present, any further
    // flags (including `--help`) belong to the scaffold binary.
    let first = params.first().map(String::as_str);
    if matches!(first, None | Some("--help" | "-h")) {
        crate::Cli::command()
            .find_subcommand_mut("create")
            .expect("create is a registered subcommand")
            .print_help()
            .map_err(|e| miette!("failed to render help: {e}"))?;
        println!();
        return Ok(None);
    }

    let (template, rest) = params.split_first().expect("checked non-empty above");
    let create_package = convert_to_create_name(template);
    let mut dlx_params = Vec::with_capacity(rest.len() + 1);
    dlx_params.push(create_package);
    dlx_params.extend(rest.iter().cloned());
    dlx::run(DlxArgs {
        params: dlx_params,
        package: Vec::new(),
        allow_build: Vec::new(),
        shell_mode: false,
        lockfile: Default::default(),
        network,
        virtual_store: Default::default(),
    })
    .await
}

/// npm's algorithm for mapping a `create <name>` argument to the actual
/// package name to install. Mirrors pnpm's `convertToCreateName`.
///
/// Examples:
///   - `foo`            -> `create-foo`
///   - `@usr/foo`       -> `@usr/create-foo`
///   - `@usr`           -> `@usr/create`
///   - `@usr@2.0.0`     -> `@usr/create@2.0.0`
///   - `@usr/foo@2.0.0` -> `@usr/create-foo@2.0.0`
fn convert_to_create_name(input: &str) -> String {
    const CREATE_PREFIX: &str = "create-";

    if let Some(rest) = input.strip_prefix('@') {
        // Split off any trailing `@version` on the scoped form.
        let (scoped, version) = match rest.find('@') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        let (scope, pkg) = match scoped.split_once('/') {
            Some((s, p)) => (s, p),
            None => (scoped, ""),
        };
        if pkg.is_empty() {
            format!("@{scope}/create{version}")
        } else if pkg.starts_with(CREATE_PREFIX) {
            format!("@{scope}/{pkg}{version}")
        } else {
            format!("@{scope}/{CREATE_PREFIX}{pkg}{version}")
        }
    } else if input.starts_with(CREATE_PREFIX) {
        input.to_string()
    } else {
        format!("{CREATE_PREFIX}{input}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unscoped_plain() {
        assert_eq!(convert_to_create_name("foo"), "create-foo");
    }

    #[test]
    fn unscoped_already_prefixed() {
        assert_eq!(convert_to_create_name("create-foo"), "create-foo");
    }

    #[test]
    fn unscoped_versioned() {
        assert_eq!(convert_to_create_name("foo@1.2.3"), "create-foo@1.2.3");
    }

    #[test]
    fn scoped_name_only() {
        assert_eq!(convert_to_create_name("@usr/foo"), "@usr/create-foo");
    }

    #[test]
    fn scoped_bare() {
        assert_eq!(convert_to_create_name("@usr"), "@usr/create");
    }

    #[test]
    fn scoped_bare_versioned() {
        assert_eq!(convert_to_create_name("@usr@2.0.0"), "@usr/create@2.0.0");
    }

    #[test]
    fn scoped_versioned() {
        assert_eq!(
            convert_to_create_name("@usr/foo@2.0.0"),
            "@usr/create-foo@2.0.0"
        );
    }

    #[test]
    fn scoped_already_prefixed() {
        assert_eq!(convert_to_create_name("@usr/create-foo"), "@usr/create-foo");
    }
}
