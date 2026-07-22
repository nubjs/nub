//! `aube access` — manage package visibility and team access on an npm registry.

use crate::commands::{make_client, split_name_spec};
use clap::{Args, Subcommand};
use miette::{IntoDiagnostic, miette};

#[derive(Debug, Args)]
pub struct AccessArgs {
    #[command(subcommand)]
    pub command: AccessCommand,
    /// Emit registry responses as JSON when the subcommand has a result.
    #[arg(long)]
    pub json: bool,
    /// One-time password from a 2FA authenticator; sent as `npm-otp`.
    #[arg(long)]
    pub otp: Option<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Subcommand)]
pub enum AccessCommand {
    /// Get package visibility status.
    Get {
        #[command(subcommand)]
        command: AccessGetCommand,
    },
    /// Grant a team read-only or read-write access to a package.
    #[command(override_usage = "access grant <PERMISSIONS> <TEAM> <PACKAGE>")]
    Grant {
        /// `read-only` or `read-write`.
        permissions: String,
        /// Team in `@scope:team` form.
        team: String,
        /// Package name.
        package: String,
    },
    /// List packages visible to a user, organization, or team.
    List {
        #[command(subcommand)]
        command: AccessListCommand,
    },
    /// Alias for `list packages`.
    #[command(override_usage = "access ls [ENTITY]\n       access ls packages [ENTITY]")]
    Ls {
        /// User, `@organization`, or `@scope:team`. Also accepts pnpm's
        /// `packages [ENTITY]` compatibility form. Accepted forms are
        /// `aube access ls [ENTITY]` and `aube access ls packages [ENTITY]`.
        #[arg(num_args = 0..=2)]
        entities: Vec<String>,
    },
    /// Revoke a team's access to a package.
    Revoke {
        /// Team in `@scope:team` form.
        team: String,
        /// Package name.
        package: String,
    },
    /// Set package visibility or a publish MFA requirement.
    Set {
        /// `status=public|private|restricted` or `mfa=none|publish|automation`.
        setting: String,
        /// Package name.
        package: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AccessListCommand {
    /// List collaborators for a package, optionally filtering to one user.
    Collaborators {
        /// Package name.
        package: String,
        /// Optional user name.
        user: Option<String>,
    },
    /// List packages visible to the current user or an optional entity.
    Packages {
        /// User, `@organization`, or `@scope:team`.
        entity: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AccessGetCommand {
    /// Get a package's public or restricted status.
    Status {
        /// Package name.
        package: String,
    },
}

pub async fn run(args: AccessArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);

    match args.command {
        AccessCommand::List { command } => match command {
            AccessListCommand::Packages { entity } => {
                let value = client
                    .access_list_packages(entity.as_deref())
                    .await
                    .map_err(access_registry_error)?;
                emit_list_packages(&value, args.json)
            }
            AccessListCommand::Collaborators { package, user } => {
                let package = bare_package(&package)?;
                let value = client
                    .access_list_collaborators(&package, user.as_deref())
                    .await
                    .map_err(access_registry_error)?;
                emit_collaborators(&value, args.json)
            }
        },
        AccessCommand::Ls { entities } => {
            let entity = match entities.as_slice() {
                [] => None,
                [entity] if entity == "packages" => None,
                [entity] => Some(entity.as_str()),
                [marker, entity] if marker == "packages" => Some(entity.as_str()),
                [marker, _] => {
                    return Err(miette!(
                        code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
                        "expected `packages` before the entity in `access ls`, got {marker:?}"
                    ));
                }
                _ => {
                    return Err(miette!(
                        code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
                        "`access ls` accepts at most an optional `packages` marker and one entity"
                    ));
                }
            };
            let value = client
                .access_list_packages(entity)
                .await
                .map_err(access_registry_error)?;
            emit_list_packages(&value, args.json)
        }
        AccessCommand::Get { command } => match command {
            AccessGetCommand::Status { package } => {
                let package = bare_package(&package)?;
                let value = client
                    .access_get_status(&package)
                    .await
                    .map_err(access_registry_error)?;
                emit_status(&package, &value, args.json)
            }
        },
        AccessCommand::Set { setting, package } => {
            let package = bare_package(&package)?;
            let (kind, value) = setting.split_once('=').ok_or_else(|| {
                miette!(
                    code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
                    "expected `status=…` or `mfa=…`, got {setting:?}"
                )
            })?;
            match kind {
                "status" => {
                    if !package.starts_with('@') {
                        return Err(miette!(
                            code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
                            "access status can only be changed for scoped packages"
                        ));
                    }
                    let access = match value {
                        "public" => "public",
                        "private" | "restricted" => "restricted",
                        _ => {
                            return Err(miette!(
                                code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
                                "invalid access status {value:?}; expected `public`, `private`, or `restricted`"
                            ));
                        }
                    };
                    client
                        .access_set_status(&package, access, args.otp.as_deref())
                        .await
                        .map_err(access_registry_error)?;
                    emit_mutation(
                        args.json,
                        serde_json::json!({ "package": package, "access": access }),
                        format!("{package}: {access}"),
                    )
                }
                "mfa" => {
                    let publish_requires_tfa = match value {
                        "none" => false,
                        "publish" | "automation" => true,
                        _ => {
                            return Err(miette!(
                                code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
                                "invalid MFA level {value:?}; expected `none`, `publish`, or `automation`"
                            ));
                        }
                    };
                    client
                        .access_set_mfa(&package, publish_requires_tfa, args.otp.as_deref())
                        .await
                        .map_err(access_registry_error)?;
                    emit_mutation(
                        args.json,
                        serde_json::json!({ "package": package, "mfa": value }),
                        format!("{package}: mfa={value}"),
                    )
                }
                _ => Err(miette!(
                    code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
                    "unknown access setting {kind:?}; expected `status` or `mfa`"
                )),
            }
        }
        AccessCommand::Grant {
            permissions,
            team,
            package,
        } => {
            let package = bare_package(&package)?;
            if !matches!(permissions.as_str(), "read-only" | "read-write") {
                return Err(miette!(
                    code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
                    "invalid permissions {permissions:?}; expected `read-only` or `read-write`"
                ));
            }
            let (scope, team_name) = split_team(&team)?;
            client
                .access_grant(
                    &package,
                    scope,
                    team_name,
                    &permissions,
                    args.otp.as_deref(),
                )
                .await
                .map_err(access_registry_error)?;
            emit_mutation(
                args.json,
                serde_json::json!({ "package": package, "team": team, "permissions": permissions }),
                format!("+{team} ({permissions}): {package}"),
            )
        }
        AccessCommand::Revoke { team, package } => {
            let package = bare_package(&package)?;
            let (scope, team_name) = split_team(&team)?;
            client
                .access_revoke(&package, scope, team_name, args.otp.as_deref())
                .await
                .map_err(access_registry_error)?;
            emit_mutation(
                args.json,
                serde_json::json!({ "package": package, "team": team }),
                format!("-{team}: {package}"),
            )
        }
    }
}

fn bare_package(raw: &str) -> miette::Result<String> {
    let (name, version) = split_name_spec(raw);
    if version.is_some() || aube_store::validate_and_encode_name(name).is_none() {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_INVALID_PACKAGE_NAME,
            "expected a valid bare package name, got {raw:?}"
        ));
    }
    Ok(name.to_string())
}

fn split_team(raw: &str) -> miette::Result<(&str, &str)> {
    let Some((scope, team)) = raw.split_once(':') else {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
            "expected team in `@scope:team` form, got {raw:?}"
        ));
    };
    if scope
        .strip_prefix('@')
        .is_none_or(|scope| scope.is_empty() || scope.contains('@'))
        || team.is_empty()
        || team.contains(':')
    {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_ACCESS_INVALID_ARGUMENT,
            "expected team in `@scope:team` form, got {raw:?}"
        ));
    }
    Ok((scope, team))
}

fn access_registry_error(error: aube_registry::Error) -> miette::Report {
    match error {
        aube_registry::Error::NotFound(name) => miette!(
            code = aube_codes::errors::ERR_AUBE_PACKAGE_NOT_FOUND,
            "package not found: {name}"
        ),
        aube_registry::Error::AccessEntityNotFound(entity) => miette!(
            code = aube_codes::errors::ERR_AUBE_ACCESS_ENTITY_NOT_FOUND,
            "access entity not found: {entity}"
        ),
        aube_registry::Error::Unauthorized => miette!(
            code = aube_codes::errors::ERR_AUBE_UNAUTHORIZED,
            "authentication required\nhelp: run `{}` first, then retry",
            aube_util::cmd("login")
        ),
        aube_registry::Error::RegistryWrite { status, body } => miette!(
            code = aube_codes::errors::ERR_AUBE_REGISTRY_WRITE_REJECTED,
            "registry rejected access request: HTTP {status}: {body}"
        ),
        other => miette!(
            code = aube_codes::errors::ERR_AUBE_REGISTRY_ERROR,
            "{other}"
        ),
    }
}

fn emit_list_packages(value: &serde_json::Value, json: bool) -> miette::Result<()> {
    if json {
        return emit_json(value);
    }
    let object = value.as_object().ok_or_else(|| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_REGISTRY_ERROR,
            "registry returned an invalid package access list"
        )
    })?;
    let mut lines: Vec<String> = object
        .iter()
        .map(|(name, access)| match access.as_str() {
            Some(access) => format!("{name}: {access}"),
            None => name.clone(),
        })
        .collect();
    lines.sort();
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

fn emit_collaborators(value: &serde_json::Value, json: bool) -> miette::Result<()> {
    if json {
        return emit_json(value);
    }
    for line in format_collaborators(value)? {
        println!("{line}");
    }
    Ok(())
}

fn format_collaborators(value: &serde_json::Value) -> miette::Result<Vec<String>> {
    let collaborators = value.as_object().ok_or_else(|| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_REGISTRY_ERROR,
            "registry returned an invalid collaborator list"
        )
    })?;
    let mut lines: Vec<String> = collaborators
        .iter()
        .map(|(user, permissions)| match permissions.as_str() {
            Some(permissions) => format!("{user}: {permissions}"),
            None => user.clone(),
        })
        .collect();
    lines.sort();
    Ok(lines)
}

fn emit_status(package: &str, value: &serde_json::Value, json: bool) -> miette::Result<()> {
    if json {
        return emit_json(value);
    }
    let access = value
        .get("access")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("public");
    println!("package: {package}");
    println!("access: {access}");
    Ok(())
}

fn emit_mutation(json: bool, value: serde_json::Value, text: String) -> miette::Result<()> {
    if json {
        emit_json(&value)
    } else {
        println!("{text}");
        Ok(())
    }
}

fn emit_json(value: &serde_json::Value) -> miette::Result<()> {
    println!("{}", serde_json::to_string_pretty(value).into_diagnostic()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AccessArgs, AccessCommand, bare_package, format_collaborators, split_team};
    use clap::Parser;

    #[test]
    fn bare_package_rejects_specifiers() {
        assert!(bare_package("@scope/pkg").is_ok());
        assert!(bare_package("@scope/pkg@1.0.0").is_err());
        assert!(bare_package("../../tmp/pkg").is_err());
    }

    #[test]
    fn split_team_requires_one_scope_separator() {
        assert_eq!(
            split_team("@scope:developers").unwrap(),
            ("@scope", "developers")
        );
        assert!(split_team("@scope").is_err());
        assert!(split_team("@scope:dev:ops").is_err());
        assert!(split_team("scope:developers").is_err());
        assert!(split_team("@@scope:developers").is_err());
    }

    #[test]
    fn ls_accepts_the_pnpm_packages_marker() {
        let cli = crate::Cli::try_parse_from(["aube", "access", "ls", "packages", "@scope"])
            .expect("access ls packages should parse");
        let Some(crate::Commands::Access(AccessArgs {
            command: AccessCommand::Ls { entities },
            ..
        })) = cli.command
        else {
            panic!("expected access ls command");
        };
        assert_eq!(entities, ["packages", "@scope"]);
    }

    #[test]
    fn collaborator_map_formats_sorted_permissions() {
        let collaborators = serde_json::json!({
            "zoe": "read-only",
            "amy": "read-write",
        });
        assert_eq!(
            format_collaborators(&collaborators).unwrap(),
            ["amy: read-write", "zoe: read-only"]
        );
    }
}
