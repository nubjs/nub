//! Shared `clap::Args` groups flattened into per-command argument structs.
//!
//! Flags here used to live as `global = true` on the top-level `Cli`,
//! which made every command's `--help` display the union of every flag
//! the binary accepted. Bucketing them into per-command groups keeps
//! `--help` focused on what each command actually consumes.
//!
//! Pre-subcommand placement (`aube --frozen-lockfile install`,
//! `aube --registry=URL install`, …) keeps working through the
//! argv-rewriting pass in `main::lift_per_subcommand_flags`, which
//! shifts these flags past the subcommand before clap parses argv.

use crate::commands;
use clap::Args;

#[derive(Args, Debug, Default, Clone, Copy)]
#[command(next_help_heading = "Lockfile")]
pub struct LockfileArgs {
    /// Error if the lockfile drifts from package.json.
    #[arg(long, conflicts_with_all = ["no_frozen_lockfile", "prefer_frozen_lockfile"])]
    pub frozen_lockfile: bool,

    /// Always re-resolve, even if the lockfile is up to date.
    #[arg(long, conflicts_with_all = ["frozen_lockfile", "prefer_frozen_lockfile"])]
    pub no_frozen_lockfile: bool,

    /// Use the lockfile when fresh, re-resolve when stale.
    #[arg(long, conflicts_with_all = ["frozen_lockfile", "no_frozen_lockfile"])]
    pub prefer_frozen_lockfile: bool,
}

impl LockfileArgs {
    pub fn frozen_override(&self) -> Option<commands::install::FrozenOverride> {
        if self.frozen_lockfile {
            Some(commands::install::FrozenOverride::Frozen)
        } else if self.no_frozen_lockfile {
            Some(commands::install::FrozenOverride::No)
        } else if self.prefer_frozen_lockfile {
            Some(commands::install::FrozenOverride::Prefer)
        } else {
            None
        }
    }

    /// Wire the parsed lockfile flags into the process-global slot so
    /// install entry points (direct, chained, or auto-install) honor
    /// them. Call once at the start of run().
    pub fn install_overrides(&self) {
        commands::set_global_frozen_override(self.frozen_override());
    }
}

#[derive(Args, Debug, Default, Clone)]
#[command(next_help_heading = "Network")]
pub struct NetworkArgs {
    /// Number of retry attempts for failed registry fetches.
    ///
    /// Overrides `fetchRetries` / `fetch-retries` from `.npmrc` /
    /// `aube-workspace.yaml` when set. Pair with `--fetch-timeout` to
    /// fail fast in scripted test runs.
    #[arg(long, value_name = "N")]
    pub fetch_retries: Option<u64>,

    /// Exponential backoff factor between retry attempts.
    ///
    /// Overrides `fetchRetryFactor` / `fetch-retry-factor` from
    /// `.npmrc` / `aube-workspace.yaml` when set. Integer-only — the
    /// underlying `FetchPolicy.retry_factor` is `u32`. Fractional
    /// values like `1.5` are rejected by clap.
    #[arg(long, value_name = "N")]
    pub fetch_retry_factor: Option<u64>,

    /// Upper bound (ms) on the computed retry backoff.
    ///
    /// Overrides `fetchRetryMaxtimeout` / `fetch-retry-maxtimeout` from
    /// `.npmrc` / `aube-workspace.yaml` when set.
    #[arg(long, value_name = "MS")]
    pub fetch_retry_maxtimeout: Option<u64>,

    /// Lower bound (ms) on the computed retry backoff.
    ///
    /// Overrides `fetchRetryMintimeout` / `fetch-retry-mintimeout` from
    /// `.npmrc` / `aube-workspace.yaml` when set.
    #[arg(long, value_name = "MS")]
    pub fetch_retry_mintimeout: Option<u64>,

    /// Per-request HTTP timeout in milliseconds.
    ///
    /// Overrides `fetchTimeout` / `fetch-timeout` from `.npmrc` /
    /// `aube-workspace.yaml` when set. Applied via `reqwest`'s
    /// `.timeout()` so it covers headers + body together.
    #[arg(long, value_name = "MS")]
    pub fetch_timeout: Option<u64>,

    /// Override the default registry URL for this invocation.
    ///
    /// Use this npm registry URL for package metadata, tarballs,
    /// audit requests, dist-tags, and registry writes.
    #[arg(long, value_name = "URL")]
    pub registry: Option<String>,
}

impl NetworkArgs {
    /// Extract `--fetch-*` flags into the `(name, value)` shape expected
    /// by `ResolveCtx::cli`. Keys match the `sources.cli` aliases declared
    /// for each setting in `settings.toml` (kebab-case).
    pub fn fetch_cli_flag_bag(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(v) = self.fetch_timeout {
            out.push(("fetch-timeout".to_string(), v.to_string()));
        }
        if let Some(v) = self.fetch_retries {
            out.push(("fetch-retries".to_string(), v.to_string()));
        }
        if let Some(v) = self.fetch_retry_factor {
            out.push(("fetch-retry-factor".to_string(), v.to_string()));
        }
        if let Some(v) = self.fetch_retry_mintimeout {
            out.push(("fetch-retry-mintimeout".to_string(), v.to_string()));
        }
        if let Some(v) = self.fetch_retry_maxtimeout {
            out.push(("fetch-retry-maxtimeout".to_string(), v.to_string()));
        }
        out
    }

    /// Wire registry + fetch overrides into the process-global slots so
    /// downstream registry-client construction picks them up. Call once
    /// at the start of any command's `run()` that hits the network.
    pub fn install_overrides(&self) {
        commands::set_registry_override(self.registry.clone());
        commands::set_fetch_cli_overrides(self.fetch_cli_flag_bag());
    }
}

#[derive(Args, Debug, Default, Clone, Copy)]
#[command(next_help_heading = "Virtual store")]
pub struct VirtualStoreArgs {
    /// Force the shared global virtual store off for this invocation.
    ///
    /// Packages are materialized inside the project's virtual store
    /// instead of symlinked from `~/.cache/aube/virtual-store/`.
    #[arg(
        long,
        visible_alias = "disable-gvs",
        conflicts_with = "enable_global_virtual_store"
    )]
    pub disable_global_virtual_store: bool,

    /// Force the shared global virtual store on for this invocation.
    ///
    /// Overrides CI's default per-project materialization and the
    /// `disableGlobalVirtualStoreForPackages` auto-disable heuristic.
    #[arg(
        long,
        visible_alias = "enable-gvs",
        conflicts_with = "disable_global_virtual_store"
    )]
    pub enable_global_virtual_store: bool,
}

impl VirtualStoreArgs {
    pub fn flags(&self) -> commands::install::GlobalVirtualStoreFlags {
        commands::install::GlobalVirtualStoreFlags {
            enable: self.enable_global_virtual_store,
            disable: self.disable_global_virtual_store,
        }
    }

    pub fn install_overrides(&self) {
        commands::set_global_virtual_store_flags(self.flags());
    }
}
