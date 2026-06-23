//! `aube deprecate <pkg-spec> <message>` — mark published versions as
//! deprecated on the registry. Mirrors `npm deprecate` / `pnpm deprecate`.
//!
//! Flow: fetch the full packument fresh (no cache — we can't roll back
//! concurrent writes), set `versions.<v>.deprecated = message` on each
//! version whose semver matches the supplied range, and PUT the modified
//! document back. A blank message un-deprecates, so `aube undeprecate`
//! is a thin wrapper that calls into here with `Some("")`.

use crate::commands::{make_client, split_name_spec};
use aube_registry::config::NpmConfig;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use serde_json::Value;

#[derive(Debug, Args)]
pub struct DeprecateArgs {
    /// Package spec: `name`, `name@version`, or `name@<range>`.
    ///
    /// Omitting the version deprecates every published version.
    pub package: String,

    /// Deprecation message shown to installers.
    ///
    /// Pass an empty string to clear an existing deprecation (or use
    /// `aube undeprecate`).
    pub message: String,

    /// Don't PUT anything — print which versions would be touched and exit.
    #[arg(long)]
    pub dry_run: bool,

    /// One-time password from a 2FA authenticator; sent as `npm-otp`.
    #[arg(long, value_name = "CODE")]
    pub otp: Option<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(args: DeprecateArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let (name, spec) = split_name_spec(&args.package);
    let name = name.to_string();
    let spec = spec.unwrap_or("*").to_string();

    apply(
        &name,
        &spec,
        &args.message,
        args.dry_run,
        args.otp.as_deref(),
        args.network.registry.as_deref(),
    )
    .await
}

/// Shared execution path used by both `deprecate` and `undeprecate`. Split
/// out so the two commands agree on matching rules and user-facing output.
pub async fn apply(
    name: &str,
    range: &str,
    message: &str,
    dry_run: bool,
    otp: Option<&str>,
    registry_override: Option<&str>,
) -> miette::Result<()> {
    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = if let Some(url) = registry_override {
        // `--registry` is an explicit "talk to this URL" — clear
        // `scoped_registries` so scoped packages don't silently route back
        // to whatever `.npmrc` has pinned for their scope, and normalize
        // the URL so auth_token_for can match `//host/:_authToken` entries
        // in `.npmrc` (which are stored with a trailing slash).
        let policy = crate::commands::resolve_fetch_policy(&cwd);
        aube_registry::client::RegistryClient::from_config_with_policy(
            NpmConfig {
                registry: aube_registry::config::normalize_registry_url_pub(url),
                scoped_registries: Default::default(),
                ..NpmConfig::load(&cwd)
            },
            policy,
        )
    } else {
        make_client(&cwd)
    };

    let mut packument = client
        .fetch_packument_json_fresh(name)
        .await
        .map_err(|e| match e {
            aube_registry::Error::NotFound(n) => miette!("package not found: {n}"),
            other => miette!("failed to fetch {name}: {other}"),
        })?;

    let versions_obj = packument
        .get_mut("versions")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| miette!("registry response for {name} has no `versions` field"))?;

    let range_parsed = node_semver::Range::parse(range)
        .into_diagnostic()
        .wrap_err_with(|| format!("invalid version range {range:?}"))?;

    let mut matched: Vec<String> = Vec::new();
    for (version_str, entry) in versions_obj.iter_mut() {
        let Ok(v) = node_semver::Version::parse(version_str) else {
            continue;
        };
        if !range_parsed.satisfies(&v) {
            continue;
        }
        let Some(entry_obj) = entry.as_object_mut() else {
            // Malformed packument entry — skip it silently rather than
            // counting it in `matched`, which would inflate the reported
            // count and risk PUT-ing an unchanged document back while
            // telling the user we deprecated things.
            continue;
        };
        // npm's convention for "un-deprecate" is to set `deprecated` to the
        // empty string, not to omit the field — registries that merge PUTs
        // (verdaccio among them) won't actually drop an omitted key, so
        // writing `""` is the portable way to clear it.
        entry_obj.insert("deprecated".into(), Value::String(message.to_string()));
        matched.push(version_str.clone());
    }

    if matched.is_empty() {
        return Err(miette!("no published versions of {name} match {range:?}"));
    }

    if dry_run {
        let verb = if message.is_empty() {
            "undeprecate"
        } else {
            "deprecate"
        };
        eprintln!(
            "Would {verb} {} of {name}:",
            pluralizer::pluralize("version", matched.len() as isize, true)
        );
        for v in &matched {
            eprintln!("  {v}");
        }
        return Ok(());
    }

    client
        .put_packument(name, &packument, otp)
        .await
        .map_err(|e| miette!("failed to update {name}: {e}"))?;

    // Drop the full-packument cache entry so a subsequent `aube view` in
    // the 5-minute TTL window doesn't serve the pre-deprecation document.
    client.invalidate_full_packument_cache(name, &crate::commands::packument_full_cache_dir());

    let verb = if message.is_empty() {
        "Undeprecated"
    } else {
        "Deprecated"
    };
    eprintln!(
        "{verb} {} of {name}:",
        pluralizer::pluralize("version", matched.len() as isize, true)
    );
    for v in &matched {
        eprintln!("  {v}");
    }
    Ok(())
}
