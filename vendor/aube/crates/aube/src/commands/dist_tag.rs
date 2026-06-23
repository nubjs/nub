//! `aube dist-tag` — manage package distribution tags on the registry.
//!
//! Mirrors `npm dist-tag` / `pnpm dist-tag`:
//!
//! - `aube dist-tag add <pkg>@<version> [tag]` — create or update a tag
//!   (default `latest`). Sends `PUT /-/package/<pkg>/dist-tags/<tag>`
//!   with a JSON-string body of the version. Requires a registry auth
//!   token (use `aube login` first).
//! - `aube dist-tag rm <pkg> <tag>` — remove a tag. `DELETE` against the
//!   same endpoint. Requires auth.
//! - `aube dist-tag ls [<pkg>]` — list every tag and the version it
//!   points at. `GET` against the dist-tags endpoint — no auth required
//!   for public packages. Reads the package name from `./package.json`
//!   when no argument is given, matching npm.
//!
//! Visible alias: `dist-tags` (npm supports both forms).
//!
//! This is a read/write command that talks to the registry but never
//! touches the lockfile, `node_modules`, or the local store, so it
//! deliberately bypasses the project lock.

use crate::commands::{make_client, split_name_spec};
use clap::{Args, Subcommand};
use miette::{Context, miette};

#[derive(Debug, Args)]
pub struct DistTagArgs {
    #[command(subcommand)]
    pub command: DistTagCommand,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Subcommand)]
pub enum DistTagCommand {
    /// Add or update a dist-tag on a package.
    ///
    /// Spec must include a concrete version: `aube dist-tag add
    /// react@18.2.0 stable`. The tag argument defaults to `latest`.
    Add {
        /// Package spec in `name@version` form (exact version required,
        /// ranges and tags aren't resolved here).
        spec: String,
        /// Tag to create or update. Defaults to `latest`.
        tag: Option<String>,
        /// One-time password from a 2FA authenticator; sent as `npm-otp`.
        #[arg(long)]
        otp: Option<String>,
    },
    /// List every dist-tag for a package.
    ///
    /// Reads the package name from `./package.json` when no argument
    /// is given.
    Ls {
        /// Package name (no version).
        ///
        /// Defaults to the current project's `package.json` `name`
        /// field.
        package: Option<String>,
    },
    /// Remove a dist-tag from a package.
    #[command(visible_alias = "remove")]
    Rm {
        /// Package name (no version).
        package: String,
        /// Tag to remove.
        tag: String,
        /// One-time password from a 2FA authenticator; sent as `npm-otp`.
        #[arg(long)]
        otp: Option<String>,
    },
}

/// Render a registry 403 into a user-facing report. A 403 means the
/// request authenticated but the credentials aren't permitted, so —
/// unlike a 401 — `aube login` won't help. We surface the registry's
/// own message (Artifactory/GitHub Packages put the actionable reason
/// there: blocked by policy, missing token scope, read-only mirror) and
/// give a permissions-oriented hint instead of an auth one. When the
/// registry sent no body we still name the right remediation class.
fn forbidden_report(name: &str, body: String) -> miette::Report {
    let detail = if body.is_empty() {
        String::new()
    } else {
        format!("\nregistry: {body}")
    };
    miette!(
        "forbidden by registry for {name}{detail}\nhelp: your token authenticated but isn\'t allowed to do this — check its permissions/scope (e.g. `read:packages`/`write:packages`) or whether the package is blocked by an org or registry policy. `{}` won\'t fix a 403.",
        aube_util::cmd("login")
    )
}

pub async fn run(args: DistTagArgs) -> miette::Result<()> {
    args.network.install_overrides();
    match args.command {
        DistTagCommand::Add { spec, tag, otp } => add(&spec, tag.as_deref(), otp.as_deref()).await,
        DistTagCommand::Rm { package, tag, otp } => rm(&package, &tag, otp.as_deref()).await,
        DistTagCommand::Ls { package } => ls(package.as_deref()).await,
    }
}

async fn add(spec: &str, tag: Option<&str>, otp: Option<&str>) -> miette::Result<()> {
    let (name, version) = split_name_spec(spec);
    let version = version.ok_or_else(|| {
        miette!(
            "expected `name@version`, got `{spec}`\nhelp: `{}` needs an exact version, e.g. `react@18.2.0`",
            aube_util::cmd("dist-tag add")
        )
    })?;
    if version.is_empty() {
        return Err(miette!(
            "version is empty in `{spec}`\nhelp: specify an exact version like `react@18.2.0`"
        ));
    }
    let tag = tag.unwrap_or("latest");

    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);

    client
        .put_dist_tag(name, tag, version, otp)
        .await
        .map_err(|e| match e {
            aube_registry::Error::NotFound(n) => miette!("package not found: {n}"),
            aube_registry::Error::Unauthorized => miette!(
                "authentication required for {name}\nhelp: run `{}` first, then retry",
                aube_util::cmd("login")
            ),
            aube_registry::Error::Forbidden { body } => forbidden_report(name, body),
            other => miette!("failed to set {name}@{tag} -> {version}: {other}"),
        })?;

    println!("+{tag}: {name}@{version}");
    Ok(())
}

async fn rm(package: &str, tag: &str, otp: Option<&str>) -> miette::Result<()> {
    let (name, version_spec) = split_name_spec(package);
    if version_spec.is_some() {
        return Err(miette!(
            "expected a bare package name, got `{package}`\nhelp: `{}` takes just the package name — the tag to remove is a separate argument",
            aube_util::cmd("dist-tag rm")
        ));
    }

    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);

    client
        .delete_dist_tag(name, tag, otp)
        .await
        .map_err(|e| match e {
            aube_registry::Error::NotFound(_) => {
                miette!("no such tag: {name}@{tag}")
            }
            aube_registry::Error::Unauthorized => miette!(
                "authentication required for {name}\nhelp: run `{}` first, then retry",
                aube_util::cmd("login")
            ),
            aube_registry::Error::Forbidden { body } => forbidden_report(name, body),
            other => miette!("failed to remove {name}@{tag}: {other}"),
        })?;

    println!("-{tag}: {name}");
    Ok(())
}

async fn ls(package: Option<&str>) -> miette::Result<()> {
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Resolve the package name: explicit arg wins, otherwise read the
    // name field from `./package.json`. We intentionally don't walk up
    // the directory tree — `npm dist-tag ls` is a cwd-only lookup.
    let name: String = match package {
        Some(pkg) => {
            let (n, version_spec) = split_name_spec(pkg);
            if version_spec.is_some() {
                return Err(miette!(
                    "expected a bare package name, got `{pkg}`\nhelp: `{}` takes just the package name",
                    aube_util::cmd("dist-tag ls")
                ));
            }
            n.to_string()
        }
        None => {
            let manifest_path = cwd.join("package.json");
            let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
                .map_err(miette::Report::new)
                .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?;
            manifest.name.ok_or_else(|| {
                miette!(
                    "package.json has no `name` field\nhelp: pass a package name explicitly (`{} <name>`)",
                    aube_util::cmd("dist-tag ls")
                )
            })?
        }
    };

    let client = make_client(&cwd);
    let tags = client.fetch_dist_tags(&name).await.map_err(|e| match e {
        aube_registry::Error::NotFound(n) => miette!("package not found: {n}"),
        aube_registry::Error::Unauthorized => {
            miette!(
                "authentication required for {name}\nhelp: run `{}` first, then retry",
                aube_util::cmd("login")
            )
        }
        aube_registry::Error::Forbidden { body } => forbidden_report(&name, body),
        other => miette!("failed to fetch dist-tags for {name}: {other}"),
    })?;

    if tags.is_empty() {
        return Err(miette!(
            "no dist-tags found for {name}\nhelp: this package has never been published"
        ));
    }

    // Render one tag per line, aligned on the colon. `BTreeMap` iteration
    // is already sorted alphabetically, matching npm's output order.
    let width = tags.keys().map(|k| k.len()).max().unwrap_or(0);
    for (tag, version) in &tags {
        println!("{tag:<width$}: {version}");
    }

    Ok(())
}
