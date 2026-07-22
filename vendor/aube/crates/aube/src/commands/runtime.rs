//! `aube runtime` — manage the project's Node.js runtime (pnpm 11's
//! `pnpm runtime` surface).
//!
//! `set` pins a version in `package.json`'s `devEngines.runtime`
//! (OpenJS shape) and chains an install so the runtime is fetched and
//! the resolved version lands in the lockfile. `list` shows what the
//! current project resolves to and which versions are installed.

use clap::Args;
use miette::{Context, IntoDiagnostic, miette};

use super::install;

#[derive(Debug, Args)]
pub struct RuntimeArgs {
    #[command(subcommand)]
    pub command: RuntimeCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum RuntimeCommand {
    /// Show the resolved runtime and installed versions
    #[command(visible_alias = "ls")]
    List(RuntimeListArgs),
    /// Pin a runtime in package.json devEngines.runtime and install it
    Set(RuntimeSetArgs),
}

#[derive(Debug, Args)]
pub struct RuntimeSetArgs {
    /// Runtime name (only `node` is supported)
    pub name: String,
    /// Version request: an exact version, a range (`^24`, `22`), `lts`,
    /// `latest`, or an LTS codename (`lts/jod`)
    // Explicit id: clap reserves the `version` id for the global
    // `--version` flag, and a positional with the same id panics at
    // dispatch with a bool/String downcast mismatch.
    #[arg(id = "runtime-version", value_name = "VERSION")]
    pub version: String,
    /// Install for the user instead of the project (delegates to
    /// `mise use -g node@<version>` when mise manages installs)
    #[arg(short = 'g', long)]
    pub global: bool,
    /// `onFail` policy written to devEngines.runtime
    #[arg(long, value_name = "POLICY", default_value = "download")]
    pub on_fail: String,
    /// Pin the exact resolved version instead of a caret range
    #[arg(long)]
    pub save_exact: bool,
}

#[derive(Debug, Args)]
pub struct RuntimeListArgs {}

pub async fn run(args: RuntimeArgs) -> miette::Result<()> {
    match args.command {
        RuntimeCommand::Set(set) => run_set(set).await,
        RuntimeCommand::List(_) => run_list().await,
    }
}

async fn run_set(args: RuntimeSetArgs) -> miette::Result<()> {
    if args.name != "node" {
        return Err(miette!(
            "{} only manages the `node` runtime (got `{}`); deno/bun pins are not supported",
            aube_util::prog(),
            args.name
        ));
    }
    let on_fail: aube_manifest::OnFail = args
        .on_fail
        .parse()
        .map_err(|e: String| miette!("--on-fail: {e}"))?;
    let spec = aube_runtime::NodeSpec::parse(&args.version)
        .map_err(|e| miette!(code = e.code(), "{e}"))?;

    // Resolve the request to an exact version up front — `set lts`
    // must write a concrete range, and a typo'd version should fail
    // before the manifest is touched.
    let cwd = crate::dirs::cwd()?;
    let settings = super::with_settings_ctx(&cwd, crate::runtime::RuntimeSettings::from_ctx);
    let cfg = aube_runtime::RuntimeConfig {
        installer: settings.installer,
        mirror: settings.mirror.clone(),
        network: aube_runtime::NetworkMode::Online,
        retries: 2,
    };
    let pin = aube_runtime::NodeRuntime::new(cfg)
        .resolve_for_lockfile(&spec)
        .await
        .map_err(|e| miette!(code = e.code(), "{e}"))?;
    let resolved = pin.version.to_string();

    // The range written to devEngines (pnpm's rule): an exact input is
    // kept verbatim; anything else becomes a caret range on the
    // resolved version. `--save-exact` forces the exact form.
    let range = if args.save_exact {
        resolved.clone()
    } else {
        match &spec {
            aube_runtime::NodeSpec::Exact(v) => v.to_string(),
            _ => format!("^{resolved}"),
        }
    };

    if args.global {
        return run_set_global(&settings, &resolved).await;
    }

    let project_dir = crate::dirs::find_project_root(&cwd).ok_or_else(|| {
        miette!(
            "{}: no package.json found in {}",
            aube_util::cmd("runtime set"),
            cwd.display()
        )
    })?;
    let manifest_path = project_dir.join("package.json");
    let on_fail_str = on_fail.to_string();
    super::manifest_io::update_manifest_json_object(&manifest_path, |obj| {
        write_dev_engines_runtime(obj, &range, &on_fail_str)
    })
    .wrap_err("failed to update package.json devEngines")?;
    println!("devEngines.runtime: node {range} (onFail: {on_fail_str})");

    // Chain an install: resolves the runtime (installing it via the
    // configured installer) and records the pin in the lockfile.
    let opts =
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Prefer));
    install::run(opts).await?;

    // Install runtime state is invocation-scoped so concurrent projects cannot
    // replace one another. Re-read the manifest rather than using the process
    // cache, which may still contain the pre-edit package.json, then resolve the
    // now-installed, lockfile-pinned runtime for the follow-up status message.
    let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read updated package.json")?;
    let parse_options = super::with_settings_ctx(&project_dir, |ctx| aube_lockfile::ParseOptions {
        strict_store_integrity: aube_settings::resolved::strict_store_integrity(ctx)
            || aube_settings::resolved::paranoid(ctx),
    });
    let pin = crate::runtime::lockfile_node_pin(&project_dir, &manifest, parse_options);
    let ctx = crate::runtime::ensure(&project_dir, Some(&manifest), settings, pin.as_ref()).await?;
    if let Some(version) = &ctx.version {
        println!("node {version} ready ({})", ctx.provenance.label());
    }
    Ok(())
}

/// `-g`: hand the install to mise when it manages runtimes (one Node
/// store on disk, and mise's activation puts it on PATH globally).
/// Under `runtimeInstaller=aube` the install lands in aube's runtime
/// dir; shell activation is the opt-in path for command shims.
async fn run_set_global(
    settings: &crate::runtime::RuntimeSettings,
    resolved: &str,
) -> miette::Result<()> {
    let use_mise = match settings.installer {
        aube_runtime::InstallerMode::Aube => false,
        aube_runtime::InstallerMode::Mise => true,
        aube_runtime::InstallerMode::Auto => aube_runtime::mise_on_path().is_some(),
    };
    if use_mise {
        let Some(mise) = aube_runtime::mise_on_path() else {
            return Err(miette!(
                code = aube_codes::errors::ERR_AUBE_RUNTIME_MISE_INSTALL_FAILED,
                "runtimeInstaller=mise but mise is not on PATH"
            ));
        };
        let status = tokio::process::Command::new(&mise)
            .args(["use", "-g", &format!("node@{resolved}")])
            .status()
            .await
            .into_diagnostic()
            .wrap_err("failed to spawn mise")?;
        if !status.success() {
            return Err(miette!(
                code = aube_codes::errors::ERR_AUBE_RUNTIME_MISE_INSTALL_FAILED,
                "mise use -g node@{resolved} failed (exit {:?})",
                status.code()
            ));
        }
        println!("node {resolved} set globally via mise");
        return Ok(());
    }

    // aube-managed global install: fetch into the runtime dir and tell
    // the user how to reach it directly or through opt-in activation.
    let version: node_semver::Version = resolved.parse().into_diagnostic()?;
    let cfg = aube_runtime::RuntimeConfig {
        installer: aube_runtime::InstallerMode::Aube,
        mirror: settings.mirror.clone(),
        network: aube_runtime::NetworkMode::Online,
        retries: 2,
    };
    let request = aube_runtime::NodeRequest {
        spec: aube_runtime::NodeSpec::Exact(version),
        raw: resolved.to_string(),
        on_fail: aube_manifest::OnFail::Download,
        source: aube_runtime::RequestSource::DevEngines,
        origin: std::path::PathBuf::from("aube runtime set -g"),
    };
    let resolution = aube_runtime::NodeRuntime::new(cfg)
        .resolve(&request, None, &crate::runtime::CliProgress::node())
        .await
        .map_err(|e| miette!(code = e.code(), "{e}"))?
        .ok_or_else(|| miette!("runtime resolution returned no install"))?;
    // Resolution follows the normal precedence (PATH, then installed,
    // then download), so say what actually happened instead of
    // claiming an install when the version was already available.
    match resolution.from {
        aube_runtime::ResolvedFrom::PathEnv => {
            println!(
                "node {resolved} already on PATH at {}; nothing to install",
                resolution.node_bin.display()
            );
            return Ok(());
        }
        aube_runtime::ResolvedFrom::Installed(origin) => {
            println!(
                "node {resolved} already installed via {} at {}",
                origin.label(),
                resolution.node_bin.display()
            );
        }
        aube_runtime::ResolvedFrom::FreshInstall(_) => {
            println!(
                "node {resolved} installed to {}",
                resolution.node_bin.display()
            );
        }
    }
    if let Some(bin_dir) = &resolution.bin_dir {
        println!(
            "projects running through {} pick it up automatically;\n\
             to use shims, run `{} <shell>` for your shell;\n\
             to use this install directly, add it to PATH: export PATH=\"{}:$PATH\"",
            aube_util::prog(),
            aube_util::cmd("activate"),
            bin_dir.display()
        );
    }
    Ok(())
}

/// Merge a node entry into `devEngines.runtime`, preserving the
/// existing shape: a non-node single object becomes a two-entry
/// array, an array gets its node entry replaced (or appended), and
/// unrelated devEngines slots (os/cpu/packageManager) are untouched.
fn write_dev_engines_runtime(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    range: &str,
    on_fail: &str,
) -> miette::Result<()> {
    use serde_json::{Value, json};
    let node_entry = json!({
        "name": "node",
        "version": range,
        "onFail": on_fail,
    });
    let dev_engines = obj
        .entry("devEngines")
        .or_insert_with(|| Value::Object(Default::default()));
    let Value::Object(dev_engines) = dev_engines else {
        return Err(miette!(
            "package.json devEngines must be an object to update it"
        ));
    };
    match dev_engines.get_mut("runtime") {
        None => {
            dev_engines.insert("runtime".to_string(), node_entry);
        }
        Some(Value::Object(existing)) => {
            if existing.get("name").and_then(Value::as_str) == Some("node") {
                dev_engines.insert("runtime".to_string(), node_entry);
            } else {
                let prior = Value::Object(existing.clone());
                dev_engines.insert("runtime".to_string(), Value::Array(vec![prior, node_entry]));
            }
        }
        Some(Value::Array(entries)) => {
            if let Some(slot) = entries
                .iter_mut()
                .find(|e| e.get("name").and_then(Value::as_str) == Some("node"))
            {
                *slot = node_entry;
            } else {
                entries.push(node_entry);
            }
        }
        Some(other) => {
            return Err(miette!(
                "package.json devEngines.runtime has an unsupported shape ({other}); fix it manually"
            ));
        }
    }
    Ok(())
}

async fn run_list() -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let ctx = crate::runtime::ensure_for_cwd(&cwd).await?;

    match (&ctx.requested, &ctx.version) {
        (Some(requested), Some(version)) => {
            println!(
                "node {version} (requested {requested} via {}, provided by {})",
                ctx.source.label(),
                ctx.provenance.label()
            );
        }
        (Some(requested), None) => {
            println!(
                "node: requested {requested} via {} but unsatisfied (running on PATH node)",
                ctx.source.label()
            );
        }
        _ => match aube_runtime::probe_path_node() {
            Some((version, path)) => {
                println!("node {version} (no project pin; PATH: {})", path.display());
            }
            None => println!("node: none found on PATH and no project pin"),
        },
    }
    if let Some(bin) = &ctx.node_bin {
        println!("  bin: {}", bin.display());
    }

    let installed = aube_runtime::list_installed();
    if installed.is_empty() {
        println!("\nno managed node installs found (aube or mise)");
    } else {
        println!("\ninstalled:");
        for node in installed {
            println!(
                "  {} ({}) {}",
                node.version,
                node.origin.label(),
                node.install_dir.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn apply(initial: Value, range: &str) -> Value {
        let Value::Object(mut obj) = initial else {
            panic!("test input must be an object")
        };
        write_dev_engines_runtime(&mut obj, range, "download").unwrap();
        Value::Object(obj)
    }

    #[test]
    fn creates_dev_engines_from_scratch() {
        let out = apply(json!({"name": "x"}), "^24.4.1");
        assert_eq!(
            out["devEngines"]["runtime"],
            json!({"name": "node", "version": "^24.4.1", "onFail": "download"})
        );
    }

    #[test]
    fn replaces_existing_node_object() {
        let out = apply(
            json!({"devEngines": {"runtime": {"name": "node", "version": "^20"}}}),
            "^24.4.1",
        );
        assert_eq!(out["devEngines"]["runtime"]["version"], "^24.4.1");
    }

    #[test]
    fn non_node_object_becomes_array() {
        let out = apply(
            json!({"devEngines": {"runtime": {"name": "bun", "version": "^1.2"}}}),
            "^24.4.1",
        );
        let arr = out["devEngines"]["runtime"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "bun");
        assert_eq!(arr[1]["name"], "node");
    }

    #[test]
    fn array_node_entry_is_replaced_in_place() {
        let out = apply(
            json!({"devEngines": {"runtime": [
                {"name": "deno", "version": "^2"},
                {"name": "node", "version": "^20"}
            ]}}),
            "^24.4.1",
        );
        let arr = out["devEngines"]["runtime"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1]["version"], "^24.4.1");
    }

    #[test]
    fn sibling_dev_engines_slots_survive() {
        let out = apply(
            json!({"devEngines": {
                "packageManager": {"name": "pnpm", "version": "^10"},
                "runtime": {"name": "node", "version": "^20"}
            }}),
            "^24.4.1",
        );
        assert_eq!(out["devEngines"]["packageManager"]["name"], "pnpm");
    }
}
