//! `aube publish` — upload the current project's tarball to a registry.
//!
//! Builds the same in-memory archive as `aube pack`, then issues an npm
//! PUT request to `{registry}/{name}`. The body shape matches what npm and
//! pnpm produce: a single-version packument containing the manifest under
//! `versions.<v>`, a `dist-tags` map, and the tarball base64-encoded under
//! `_attachments`. The registry stores the tarball at the URL named in
//! `versions.<v>.dist.tarball` and indexes it by `shasum` (SHA-1 hex) and
//! `integrity` (SHA-512 SRI) exactly like `npm publish`.
//!
//! Auth and per-registry TLS come from `.npmrc` via `RegistryClient`, so
//! `aube login` and pre-existing per-registry auth entries both work.
//! Scoped packages are routed through their scope's registry when configured.
//!
//! This cut implements the P1 subset from `CLI_SPEC.md`:
//! `--tag`, `--access`, `--dry-run`, `--registry`, `--otp`, `--no-git-checks`,
//! `--force`, `--provenance`, plus workspace fanout via the global
//! `-r` / `--filter`.
//!
//! Workspace fanout (`-r` / `-F`) discovers packages from
//! `pnpm-workspace.yaml`, skips any with `"private": true`, optionally
//! narrows by exact `--filter=<name>` matches (repeatable), and for each
//! survivor checks whether `name@version` already exists on the target
//! registry. Matches are silently skipped so `aube -r publish` is
//! re-runnable after a partial success — this matches pnpm's
//! "publish what's changed" semantics without the git-diff dance.
//! `--force` bypasses both the per-package skip and the single-package
//! "already published" error, leaving the registry itself to decide
//! whether a republish is allowed.

use crate::commands::pack::{
    BuiltArchive, build_archive, build_archive_with_package_json, tarball_filename,
};
use crate::commands::{encode_package_name, ensure_registry_auth_for_package};
use aube_manifest::PackageJson;
use aube_registry::client::RegistryClient;
use aube_registry::config::{NpmConfig, normalize_registry_url_pub};
use base64::Engine;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use reqwest::Url;
use serde::Deserialize;
use sha2::Digest as _;
use sha2::Sha512;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct PublishArgs {
    /// Publish as `public` or `restricted`.
    ///
    /// Sent as the `access` field in the publish body; scoped
    /// packages default to `restricted` on the registry side, so
    /// pass `--access=public` to make a new scoped package
    /// world-readable.
    #[arg(long, value_name = "LEVEL")]
    pub access: Option<String>,
    /// Don't upload; print what would be published.
    #[arg(long)]
    pub dry_run: bool,
    /// Republish even when the version is already on the registry.
    ///
    /// By default `aube publish` issues a GET before the PUT and
    /// refuses to proceed when the version exists, surfacing a clear
    /// error instead of relying on the registry to return 409. In
    /// `--recursive` / `--filter` mode, `--force` overrides the
    /// silent "already-published" skip so every selected workspace
    /// package is re-PUT. The registry must still accept the
    /// republish — npm's public registry rejects re-publishes
    /// outright; Verdaccio and most private mirrors allow them.
    #[arg(long)]
    pub force: bool,
    /// Skip publish lifecycle scripts.
    ///
    /// Suppresses `prepublishOnly`, `prepublish`, `prepack`, `prepare`,
    /// `postpack`, `publish`, and `postpublish` scripts for this
    /// publish.
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Emit the publish result as JSON.
    ///
    /// Output matches `npm publish --json` / `pnpm publish --json`; recursive multi-package publishes emit an array.
    #[arg(long)]
    pub json: bool,
    /// Skip the "working tree must be clean" check.
    ///
    /// When unset, aube refuses to publish from a dirty git checkout
    /// (uncommitted tracked changes) or from a detached / non-release
    /// branch.
    #[arg(long)]
    pub no_git_checks: bool,
    /// One-time password for registries that require 2FA.
    ///
    /// Sent verbatim as the `npm-otp` header.
    #[arg(long, value_name = "CODE")]
    pub otp: Option<String>,
    /// Generate a SLSA provenance attestation and attach it to the publish
    /// body.
    ///
    /// Requires an OIDC-capable CI environment (GitHub Actions with
    /// `id-token: write`, GitLab CI, Buildkite, or CircleCI) — aube
    /// signs via the Sigstore public-good instance (Fulcio + Rekor)
    /// and attaches the resulting bundle so registries that honor
    /// npm's provenance protocol light up the "provenance" badge on
    /// the published version.
    #[arg(long)]
    pub provenance: bool,
    /// Default dist-tag to publish under (default: `latest`).
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(
    args: PublishArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    let cwd = crate::dirs::project_root()?;

    if !args.no_git_checks {
        enforce_git_checks(&cwd)?;
    }

    if !filter.is_empty() {
        return run_recursive(&cwd, &args, &filter, args.network.registry.as_deref()).await;
    }

    // Single-package mode: config_root == pkg_dir == cwd.
    let config = super::load_npm_config(&cwd);
    let policy = super::resolve_fetch_policy(&cwd);
    let client = RegistryClient::from_config_with_policy(config.clone(), policy);
    let outcome = publish_one(
        &cwd,
        &config,
        &client,
        &args,
        false,
        args.network.registry.as_deref(),
    )
    .await?;
    emit_outcome(&outcome, args.json)?;
    Ok(())
}

/// pnpm-compatible git pre-flight: in a git worktree, refuse to
/// publish when there are uncommitted tracked changes or when the
/// current branch isn't one of the conventional release branches.
/// Outside a git repo — or when `git` isn't on `PATH` — this is a
/// no-op (pnpm does the same; you just can't gate something you
/// can't observe).
fn enforce_git_checks(cwd: &Path) -> miette::Result<()> {
    // `git rev-parse --is-inside-work-tree` → "true" inside a repo,
    // error otherwise. We treat any failure as "not a git repo" and
    // skip the rest of the checks.
    let inside = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    let Ok(out) = inside else {
        return Ok(());
    };
    if !out.status.success() || String::from_utf8_lossy(&out.stdout).trim() != "true" {
        return Ok(());
    }

    // `git status --porcelain` on tracked files only; `--untracked-files=no`
    // matches pnpm's logic (untracked files are fine — they just haven't
    // been added yet and won't be published).
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(cwd)
        .output()
        .map_err(|e| miette!("failed to run `git status`: {e}"))?;
    if !status.status.success() {
        return Err(miette!(
            "git status failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    let dirty = String::from_utf8_lossy(&status.stdout);
    if !dirty.trim().is_empty() {
        return Err(miette!(
            "{}: working tree has uncommitted changes:\n{}\n\
             help: commit or stash them, or pass --no-git-checks to override",
            aube_util::cmd("publish"),
            dirty.trim_end()
        ));
    }

    // pnpm also refuses to publish off non-release branches (anything
    // other than `master`, `main`, or a semver `v*` branch). We match
    // that set exactly so `--no-git-checks` remains the only escape.
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .map_err(|e| miette!("failed to run `git rev-parse`: {e}"))?;
    if !branch.status.success() {
        return Ok(());
    }
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    // Release-branch allowlist. `v1.x` / `release/1.2` pass; unrelated
    // prefixes like `vendor/` or `validation` do not. Match the pnpm
    // default (`master`, `main`) plus the semver-style variants aube
    // has historically accepted, but require the `v`/`release` prefix
    // to actually lead into a version segment.
    //
    // Detached HEAD (`git rev-parse --abbrev-ref HEAD` returns the
    // literal string `"HEAD"`) is intentionally allowed: tag-based CI
    // checkouts run in detached HEAD state, and that's the most common
    // automated publish flow. Users who want to refuse detached-HEAD
    // publishes can still require a specific branch via their own
    // git hook or CI gate — aube mirrors pnpm's default here.
    let is_version_branch = |b: &str, prefix: &str| -> bool {
        let Some(rest) = b.strip_prefix(prefix) else {
            return false;
        };
        rest.chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '/' || c == '-' || c == '.')
    };
    let ok = matches!(branch.as_str(), "master" | "main" | "HEAD")
        || is_version_branch(&branch, "v")
        || is_version_branch(&branch, "release");
    if !ok {
        return Err(miette!(
            "{}: current branch `{branch}` is not a release branch\n\
             help: switch to main/master or pass --no-git-checks to override",
            aube_util::cmd("publish")
        ));
    }
    Ok(())
}

/// Workspace fanout: discover packages, filter, and publish each one.
/// Exits non-zero if any per-package publish fails, but keeps going so
/// one bad package doesn't hide the state of the rest.
async fn run_recursive(
    source_root: &Path,
    args: &PublishArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
    registry_override: Option<&str>,
) -> miette::Result<()> {
    let workspace_pkgs = aube_workspace::find_workspace_packages(source_root)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "{}: no workspace packages found. \
             `--recursive` / `--filter` requires a workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) at {}",
            aube_util::cmd("publish"),
            source_root.display()
        ));
    }

    let selected = select_workspace_packages(source_root, &workspace_pkgs, filter)?;
    if selected.is_empty() {
        if !filter.is_empty() {
            return Err(miette!(
                "{}: --filter {:?} did not match any workspace package",
                aube_util::cmd("publish"),
                filter
            ));
        }
        return Err(miette!(
            "{}: no publishable workspace packages (all private or empty)",
            aube_util::cmd("publish")
        ));
    }

    // Load `.npmrc` once from the workspace root, not from each package
    // subdir. pnpm walks both, but in practice auth tokens and scoped
    // registry overrides live in the root `.npmrc` (or ~/.npmrc) — a
    // per-package load would silently miss them and every package in
    // the fanout would 401/403 on read or "no auth token" on write.
    let config = super::load_npm_config(source_root);
    let policy = super::resolve_fetch_policy(source_root);
    let client = RegistryClient::from_config_with_policy(config.clone(), policy);

    let mut outcomes: Vec<PublishOutcome> = Vec::new();
    let mut failures: Vec<(String, miette::Report)> = Vec::new();
    for pkg_dir in &selected {
        // Each package carries its own display label for error attribution
        // — workspace folder names are usually more stable than package
        // names under refactors, so we lean on the path.
        match publish_one(pkg_dir, &config, &client, args, true, registry_override).await {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => failures.push((pkg_dir.display().to_string(), e)),
        }
    }

    if args.json {
        emit_json_outcomes(&outcomes)?;
    } else {
        for o in &outcomes {
            emit_outcome_line(o);
        }
    }

    if !failures.is_empty() {
        let joined = failures
            .iter()
            .map(|(p, e)| format!("  {p}: {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(miette!(
            "{}: {} failed:\n{joined}",
            aube_util::cmd("publish"),
            pluralizer::pluralize("package", failures.len() as isize, true)
        ));
    }
    Ok(())
}

/// Narrow a discovered workspace-package list to the ones we should
/// try to publish. Drops packages without a `name`/`version`, drops
/// private packages, and (if `filters` is non-empty) keeps only those
/// matching at least one selector.
fn select_workspace_packages(
    workspace_root: &Path,
    workspace_pkgs: &[PathBuf],
    filters: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Vec<PathBuf>> {
    let selected = aube_workspace::selector::select_workspace_packages(
        workspace_root,
        workspace_pkgs,
        filters,
    )
    .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    let seen_names: Vec<String> = selected.iter().filter_map(|p| p.name.clone()).collect();
    let out: Vec<PathBuf> = selected
        .into_iter()
        .filter(|p| p.name.is_some() && p.version.is_some() && !p.private)
        .map(|p| p.dir)
        .collect();
    if !filters.is_empty() && out.is_empty() && !seen_names.is_empty() {
        tracing::debug!("aube publish: known workspace packages: {seen_names:?}");
    }
    Ok(out)
}

/// Result of a single-package publish attempt. Carries the resolved
/// name/version unconditionally so the `AlreadyPublished` skip path
/// can report without having built a tarball. The `archive` is only
/// present for outcomes that actually built one (published + dry-run);
/// skipping a package that's already on the registry deliberately
/// avoids the expensive `build_archive` / body-hash work, which is
/// the whole point of the pre-PUT existence check.
struct PublishOutcome {
    name: String,
    version: String,
    registry_url: String,
    archive: Option<BuiltArchive>,
    status: PublishStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishStatus {
    Published,
    DryRun,
    AlreadyPublished,
}

/// Publish a single package rooted at `pkg_dir`. All registry work
/// lives here so `run` and `run_recursive` share one code path. `config`
/// is loaded once at the workspace root by the caller so every package
/// in a fanout sees the same auth/scoped-registry view.
async fn publish_one(
    pkg_dir: &Path,
    config: &NpmConfig,
    client: &RegistryClient,
    args: &PublishArgs,
    fanout: bool,
    registry_override: Option<&str>,
) -> miette::Result<PublishOutcome> {
    // Read the manifest *first* so the name/version needed for the
    // existence check are available without touching the filesystem
    // for file collection or the CPU for gzip/SHA hashing. This is the
    // whole reason re-running `aube publish -r` on a mostly-published
    // workspace is cheap — the happy-path skip must not pay the cost
    // of a packed tarball.
    let manifest = PackageJson::from_path(&pkg_dir.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err_with(|| format!("failed to read {}/package.json", pkg_dir.display()))?;
    let name = manifest
        .name
        .as_deref()
        .ok_or_else(|| miette!("publish: {}/package.json has no `name`", pkg_dir.display()))?
        .to_string();
    let version = normalize_publish_version(manifest.version.as_deref().ok_or_else(|| {
        miette!(
            "publish: {}/package.json has no `version`",
            pkg_dir.display()
        )
    })?);

    // publishConfig in package.json overrides both registry and tag
    // if the user has not passed CLI flags. pnpm and npm both honor
    // this field, so without it migrating users would silently
    // publish to the wrong place. Most common case: scoped private
    // registries like `{"publishConfig": {"registry": "https://npm.pkg.github.com"}}`
    // and `{"publishConfig": {"access": "public"}}` for first-time
    // scoped-public publishes. CLI override still wins over the
    // manifest setting, matching pnpm precedence.
    let publish_config = manifest
        .extra
        .get("publishConfig")
        .and_then(|v| v.as_object());
    let pc_registry = publish_config
        .and_then(|p| p.get("registry"))
        .and_then(|v| v.as_str());
    let pc_tag = publish_config
        .and_then(|p| p.get("tag"))
        .and_then(|v| v.as_str());

    let registry_url = registry_override
        .map(normalize_registry_url_pub)
        .or_else(|| pc_registry.map(normalize_registry_url_pub))
        .unwrap_or_else(|| config.registry_for(&name).to_string());

    let tag = args
        .tag
        .as_deref()
        .or(pc_tag)
        .unwrap_or("latest")
        .to_string();

    if args.dry_run {
        // Dry-run still runs the pre-publish chain so users can smoke-test
        // their `prepublishOnly` / `prepack` / `prepare` scripts without
        // hitting the registry, matching pnpm. `publish` / `postpublish`
        // are skipped — nothing was actually uploaded.
        run_publish_lifecycle_pre(pkg_dir, &manifest, args.ignore_scripts).await?;
        let archive = build_archive_for_publish(pkg_dir)?;
        super::pack::run_pack_lifecycle_post(pkg_dir, args.ignore_scripts).await?;
        // `--dry-run --provenance` is a common "does my CI actually have
        // OIDC wired up?" smoke test. Silently skipping the OIDC probe
        // here would give a false green light — so we run the ambient
        // detection even in dry-run mode. We stop short of the Fulcio /
        // Rekor round-trip because (a) we don't want to spam the public
        // tlog with throwaway entries and (b) dry-run should be cheap.
        if args.provenance {
            crate::commands::publish_provenance::probe_oidc_available()
                .await
                .wrap_err("--dry-run --provenance: OIDC probe failed")?;
        }
        return Ok(PublishOutcome {
            name: archive.name.clone(),
            version: archive.version.clone(),
            registry_url,
            archive: Some(archive),
            status: PublishStatus::DryRun,
        });
    }

    // Pre-flight: ask the registry whether `name@version` is already
    // there. In fanout mode a hit is a silent skip (so `-r publish` is
    // idempotent on partial success) and in single-package mode it is
    // a hard error with a clear message — pnpm's behavior. `--force`
    // opts out of both: it turns the skip into a PUT and suppresses
    // the single-package error, leaving the registry to decide whether
    // a republish is allowed (npm refuses, Verdaccio usually accepts).
    if !args.force && version_on_registry(client, &registry_url, &name, &version).await {
        if fanout {
            return Ok(PublishOutcome {
                name,
                version,
                registry_url,
                archive: None,
                status: PublishStatus::AlreadyPublished,
            });
        }
        return Err(miette!(
            "{}: {name}@{version} is already on {registry_url}\n\
             help: pass --force to republish (the registry must allow it; npm's public registry does not)",
            aube_util::cmd("publish")
        ));
    }

    // Lifecycle hooks + tarball build only happen now that we know
    // we're actually going to PUT. For a re-run of `-r publish` where
    // every package is already on the registry, the loop never reaches
    // this point and the whole fanout is script-free and gzip-free.
    run_publish_lifecycle_pre(pkg_dir, &manifest, args.ignore_scripts).await?;
    let archive = build_archive_for_publish(pkg_dir)?;
    super::pack::run_pack_lifecycle_post(pkg_dir, args.ignore_scripts).await?;

    // Re-read the manifest *after* the pre-pack chain. Publish-time
    // hooks often rewrite `package.json` on the fly — `clean-package`
    // strips `devDependencies`, build tools inject `exports`, a
    // `prepublishOnly` might stamp a git SHA into the version. The
    // tarball always reflects the on-disk state (`build_archive`
    // reads it fresh), so the registry-visible metadata at
    // `versions.<v>.*` and the env seen by `publish` / `postpublish`
    // must agree with it or consumers get a mismatch between
    // `npm info` output and the tarball they actually download.
    let manifest = super::pack::read_root_manifest(pkg_dir)?;

    // Sigstore signing is the one step here that can take seconds
    // (Fulcio + Rekor + optional TSA round-trips), so we do it *before*
    // serializing the publish body rather than after — a signing
    // failure should never leave us with a half-built request.
    let provenance_bundle = if args.provenance {
        Some(
            crate::commands::publish_provenance::generate(
                &archive.tarball,
                &archive.name,
                &archive.version,
            )
            .await
            .wrap_err("failed to generate SLSA provenance attestation")?,
        )
    } else {
        None
    };

    // Same publishConfig precedence story for `access`. CLI flag
    // wins, then manifest.publishConfig.access, then default.
    // Without this, a first-time `@scope/pkg` publish with
    // `publishConfig.access=public` in package.json would fail with
    // 402 unless the user also passed `--access public` on every
    // publish invocation. Re-derive from the post-hook manifest so
    // `prepublishOnly`-injected publishConfig entries are honored.
    let pc_access = manifest
        .extra
        .get("publishConfig")
        .and_then(|v| v.as_object())
        .and_then(|p| p.get("access"))
        .and_then(|v| v.as_str());
    let effective_access = args.access.as_deref().or(pc_access);

    let body = build_publish_body(
        &archive,
        &manifest,
        &registry_url,
        &tag,
        effective_access,
        provenance_bundle.as_deref(),
    )?;

    let url = put_url(&registry_url, &archive.name);
    let trusted_publish_token = trusted_publish_token(client, &registry_url, &archive.name).await?;
    if trusted_publish_token.is_none() {
        ensure_registry_auth_for_package(client, &registry_url, &archive.name)?;
    }
    let body_bytes = serde_json::to_vec(&body).into_diagnostic()?;
    match send_publish_put(
        client,
        &url,
        &registry_url,
        &archive.name,
        body_bytes.clone(),
        trusted_publish_token.as_deref(),
        args.otp.as_deref(),
    )
    .await?
    {
        Ok(()) => {}
        Err(first) if args.otp.is_none() && publish_failure_needs_otp(&first) => {
            let otp = read_publish_otp(&archive.name, &archive.version)?;
            if let Err(second) = send_publish_put(
                client,
                &url,
                &registry_url,
                &archive.name,
                body_bytes,
                trusted_publish_token.as_deref(),
                Some(&otp),
            )
            .await?
            {
                return Err(publish_failure_report(second));
            }
        }
        Err(failure) => return Err(publish_failure_report(failure)),
    }

    run_publish_lifecycle_post(pkg_dir, &manifest, args.ignore_scripts).await?;

    Ok(PublishOutcome {
        name: archive.name.clone(),
        version: archive.version.clone(),
        registry_url,
        archive: Some(archive),
        status: PublishStatus::Published,
    })
}

fn normalize_archive_for_publish(archive: &mut BuiltArchive) {
    let version = normalize_publish_version(&archive.version);
    if version != archive.version {
        archive.version = version;
        archive.filename = tarball_filename(&archive.name, &archive.version);
    }
}

fn build_archive_for_publish(pkg_dir: &Path) -> miette::Result<BuiltArchive> {
    let manifest_path = pkg_dir.join("package.json");
    let manifest_bytes = std::fs::read(&manifest_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?;
    let mut manifest_json: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).into_diagnostic()?;
    let Some(raw_version) = manifest_json.get("version").and_then(|v| v.as_str()) else {
        return build_archive(pkg_dir);
    };
    let version = normalize_publish_version(raw_version);
    if version == raw_version {
        return build_archive(pkg_dir);
    }

    if let Some(obj) = manifest_json.as_object_mut() {
        obj.insert("version".into(), version.into());
    }
    let mut package_json = serde_json::to_vec_pretty(&manifest_json).into_diagnostic()?;
    package_json.push(b'\n');

    let mut archive = build_archive_with_package_json(pkg_dir, Some(package_json))?;
    normalize_archive_for_publish(&mut archive);
    Ok(archive)
}

fn normalize_publish_version(version: &str) -> String {
    node_semver::Version::parse(version)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| version.to_string())
}

#[derive(Debug, Deserialize)]
struct GitHubOidcResponse {
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NpmOidcExchangeResponse {
    token: Option<String>,
}

/// Try npm Trusted Publishing before falling back to traditional `.npmrc`
/// auth. npm's OIDC exchange is publish-specific: the CI-issued ID token must
/// have audience `npm:<registry-host>`, then the registry returns a short-lived
/// package-scoped token used as the PUT bearer token.
async fn trusted_publish_token(
    client: &RegistryClient,
    registry_url: &str,
    package_name: &str,
) -> miette::Result<Option<String>> {
    let Some(id_token) = npm_oidc_id_token(client, registry_url).await? else {
        return Ok(None);
    };
    exchange_npm_oidc_token(client, registry_url, package_name, &id_token).await
}

async fn npm_oidc_id_token(
    client: &RegistryClient,
    registry_url: &str,
) -> miette::Result<Option<String>> {
    if let Ok(token) = std::env::var("NPM_ID_TOKEN")
        && !token.trim().is_empty()
    {
        return Ok(Some(token));
    }

    if std::env::var("GITHUB_ACTIONS").is_err() {
        return Ok(None);
    }
    let Ok(request_url) = std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL") else {
        return Ok(None);
    };
    let Ok(request_token) = std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN") else {
        return Ok(None);
    };
    if request_url.trim().is_empty() || request_token.trim().is_empty() {
        return Ok(None);
    }

    let registry = Url::parse(registry_url)
        .into_diagnostic()
        .wrap_err_with(|| format!("invalid registry URL for npm OIDC: {registry_url}"))?;
    let host = registry
        .host_str()
        .ok_or_else(|| miette!("invalid registry URL for npm OIDC: missing host"))?;
    let audience = format!("npm:{host}");

    let mut url = Url::parse(&request_url)
        .into_diagnostic()
        .wrap_err("invalid ACTIONS_ID_TOKEN_REQUEST_URL")?;
    url.query_pairs_mut().append_pair("audience", &audience);

    let resp = match client
        .request(reqwest::Method::GET, url.as_str(), registry_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .bearer_auth(request_token)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "GitHub Actions OIDC token request failed; falling back to configured registry auth"
            );
            return Ok(None);
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::debug!(
            %status,
            body = body.trim(),
            "GitHub Actions OIDC token request failed; falling back to configured registry auth"
        );
        return Ok(None);
    }
    let Ok(body) = resp.json::<GitHubOidcResponse>().await else {
        tracing::debug!(
            "failed to parse GitHub Actions OIDC token response; falling back to configured registry auth"
        );
        return Ok(None);
    };
    Ok(body.value.filter(|token| !token.trim().is_empty()))
}

async fn exchange_npm_oidc_token(
    client: &RegistryClient,
    registry_url: &str,
    package_name: &str,
    id_token: &str,
) -> miette::Result<Option<String>> {
    let endpoint = format!(
        "{}/-/npm/v1/oidc/token/exchange/package/{}",
        registry_url.trim_end_matches('/'),
        encode_package_name(package_name)
    );
    let resp = client
        .request(reqwest::Method::POST, &endpoint, registry_url)
        .bearer_auth(id_token)
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to exchange npm OIDC token at {endpoint}"))?;
    if !resp.status().is_success() {
        tracing::debug!(
            status = %resp.status(),
            "npm OIDC token exchange failed; falling back to configured registry auth"
        );
        return Ok(None);
    }
    let body = resp
        .json::<NpmOidcExchangeResponse>()
        .await
        .into_diagnostic()
        .wrap_err("failed to parse npm OIDC token exchange response")?;
    Ok(body.token.filter(|token| !token.trim().is_empty()))
}

struct PublishHttpFailure {
    status: reqwest::StatusCode,
    body: String,
}

async fn send_publish_put(
    client: &RegistryClient,
    url: &str,
    registry_url: &str,
    name: &str,
    body: Vec<u8>,
    trusted_publish_token: Option<&str>,
    otp: Option<&str>,
) -> miette::Result<Result<(), PublishHttpFailure>> {
    let mut req = if let Some(token) = trusted_publish_token {
        client
            .request(reqwest::Method::PUT, url, registry_url)
            .bearer_auth(token)
    } else {
        client.authed_request_for_package(reqwest::Method::PUT, url, registry_url, name)
    }
    .header("content-type", "application/json")
    .body(body);
    if let Some(otp) = otp {
        req = req.header("npm-otp", otp);
    }

    let resp = req
        .send()
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to PUT {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Ok(Err(PublishHttpFailure { status, body }));
    }

    Ok(Ok(()))
}

fn publish_failure_report(failure: PublishHttpFailure) -> miette::Report {
    miette!(
        "publish failed: {}: {}",
        failure.status,
        failure.body.trim()
    )
}

fn publish_failure_needs_otp(failure: &PublishHttpFailure) -> bool {
    if failure.status != reqwest::StatusCode::UNAUTHORIZED
        && failure.status != reqwest::StatusCode::FORBIDDEN
    {
        return false;
    }
    let body = failure.body.to_ascii_lowercase();
    let requires = body.contains("required") || body.contains("requires");
    let two_factor =
        body.contains("two-factor") || body.contains("two factor") || body.contains("2fa");
    body.contains("eotp")
        || body.contains("npm-otp")
        || body.contains("one-time password")
        || body.contains("one-time pass")
        || body.contains("one time password")
        || body.contains("one time pass")
        || (body.contains("otp") && requires)
        || (two_factor && requires)
}

fn read_publish_otp(name: &str, version: &str) -> miette::Result<String> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(miette!(
            "publish requires a one-time password (OTP) for {name}@{version}\n\
             help: pass --otp <CODE> when running non-interactively"
        ));
    }

    let description = format!("Enter one-time password for {name}@{version}");
    let otp = demand::Input::new("OTP")
        .description(&description)
        .mask_on_submit(true)
        .run()
        .into_diagnostic()
        .wrap_err("failed to read OTP")?;
    let otp = otp.trim().to_string();
    if otp.is_empty() {
        return Err(miette!("no OTP entered"));
    }
    Ok(otp)
}

/// Pre-pack chain for publish: `prepublishOnly` → `prepublish` →
/// `prepack` → `prepare`. Runs only for packages that are actually
/// being uploaded — the "already on registry" skip path avoids all of
/// this so `aube -r publish` remains idempotent. `prepublish` is
/// deprecated by npm but pnpm still runs it on publish, so we match
/// pnpm for the common case the discussion in #253 flagged. The
/// manifest is threaded through so the whole chain shares a single
/// parse of `package.json`.
async fn run_publish_lifecycle_pre(
    pkg_dir: &Path,
    manifest: &PackageJson,
    ignore_scripts: bool,
) -> miette::Result<()> {
    if ignore_scripts {
        return Ok(());
    }
    super::pack::run_root_lifecycle_script(pkg_dir, manifest, "prepublishOnly").await?;
    super::pack::run_root_lifecycle_script(pkg_dir, manifest, "prepublish").await?;
    super::pack::run_root_lifecycle_script(pkg_dir, manifest, "prepack").await?;
    super::pack::run_root_lifecycle_script(pkg_dir, manifest, "prepare").await?;
    Ok(())
}

/// Post-upload chain for publish: `publish` → `postpublish`. These
/// run only after a successful PUT — a non-2xx response short-circuits
/// out before we get here, matching pnpm.
async fn run_publish_lifecycle_post(
    pkg_dir: &Path,
    manifest: &PackageJson,
    ignore_scripts: bool,
) -> miette::Result<()> {
    if ignore_scripts {
        return Ok(());
    }
    super::pack::run_root_lifecycle_script(pkg_dir, manifest, "publish").await?;
    super::pack::run_root_lifecycle_script(pkg_dir, manifest, "postpublish").await?;
    Ok(())
}

/// GET `{registry}/{name}` and check whether `versions[version]` is
/// present. Any transport/parse failure returns `false` so we fall
/// through to the PUT and let the registry itself reject duplicates —
/// being *wrong* about "already published" is worse than a harmless
/// extra PUT attempt. The GET is sent through the same registry client
/// we'd use for the PUT so private registries (Verdaccio auth, GitHub
/// Packages, Artifactory) can actually answer it.
async fn version_on_registry(
    client: &RegistryClient,
    registry_url: &str,
    name: &str,
    version: &str,
) -> bool {
    let url = put_url(registry_url, name);
    let Ok(resp) = client
        .authed_request_for_package(reqwest::Method::GET, &url, registry_url, name)
        .send()
        .await
    else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    let Ok(doc) = resp.json::<serde_json::Value>().await else {
        return false;
    };
    doc.get("versions").and_then(|v| v.get(version)).is_some()
}

fn emit_outcome(outcome: &PublishOutcome, as_json: bool) -> miette::Result<()> {
    if as_json {
        emit_json_single(outcome)
    } else {
        emit_outcome_line(outcome);
        Ok(())
    }
}

fn emit_outcome_line(outcome: &PublishOutcome) {
    match outcome.status {
        PublishStatus::DryRun => {
            println!(
                "+ {}@{} (dry run, would PUT to {})",
                outcome.name,
                outcome.version,
                put_url(&outcome.registry_url, &outcome.name)
            );
            if let Some(archive) = &outcome.archive {
                for f in &archive.files {
                    println!("  {f}");
                }
            }
        }
        PublishStatus::Published => {
            println!("+ {}@{}", outcome.name, outcome.version);
        }
        PublishStatus::AlreadyPublished => {
            println!(
                "= {}@{} (already on registry, skipping)",
                outcome.name, outcome.version
            );
        }
    }
}

fn emit_json_outcomes(outcomes: &[PublishOutcome]) -> miette::Result<()> {
    let out = serde_json::to_string_pretty(&publish_outcomes_json(outcomes)).into_diagnostic()?;
    println!("{out}");
    Ok(())
}

fn emit_json_single(outcome: &PublishOutcome) -> miette::Result<()> {
    let out = serde_json::to_string_pretty(&publish_outcome_json(outcome)).into_diagnostic()?;
    println!("{out}");
    Ok(())
}

fn publish_outcomes_json(outcomes: &[PublishOutcome]) -> serde_json::Value {
    match outcomes {
        [outcome] => publish_outcome_json(outcome),
        _ => publish_outcomes_json_array(outcomes),
    }
}

fn publish_outcomes_json_array(outcomes: &[PublishOutcome]) -> serde_json::Value {
    serde_json::Value::Array(outcomes.iter().map(publish_outcome_json).collect())
}

fn publish_outcome_json(outcome: &PublishOutcome) -> serde_json::Value {
    let status = match outcome.status {
        PublishStatus::Published => "published",
        PublishStatus::AlreadyPublished => "skipped",
        PublishStatus::DryRun => "dry-run",
    };
    let mut obj = serde_json::json!({
        "id": format!("{}@{}", outcome.name, outcome.version),
        "name": outcome.name,
        "version": outcome.version,
        "status": status,
    });
    if let Some(archive) = &outcome.archive {
        let (shasum, integrity) = archive_hashes(archive);
        let m = obj.as_object_mut().expect("json object");
        m.insert("size".into(), archive.tarball.len().into());
        m.insert("unpackedSize".into(), archive.unpacked_size.into());
        m.insert("shasum".into(), shasum.into());
        m.insert("integrity".into(), integrity.into());
        m.insert("filename".into(), archive.filename.clone().into());
        m.insert(
            "files".into(),
            serde_json::Value::Array(
                archive
                    .files
                    .iter()
                    .map(|p| serde_json::json!({"path": p}))
                    .collect(),
            ),
        );
        m.insert("entryCount".into(), archive.files.len().into());
        m.insert("bundled".into(), serde_json::Value::Array(Vec::new()));
    }
    obj
}

/// `{registry}/{name}`. Uses the shared `encode_package_name` helper
/// from `commands/mod.rs` so `publish` and `unpublish` can't drift on
/// URL shape.
fn put_url(registry: &str, name: &str) -> String {
    let base = registry.trim_end_matches('/');
    format!("{base}/{}", encode_package_name(name))
}

/// Assemble the JSON body npm/pnpm send for `PUT /<name>`. The tarball
/// URL we hand the registry is where *we think* the file will live; real
/// registries rewrite it on ingest, so its exact form only needs to be
/// parseable — we use `{registry}/{name}/-/{filename}` to match pnpm.
fn build_publish_body(
    archive: &BuiltArchive,
    manifest: &PackageJson,
    registry_url: &str,
    tag: &str,
    access: Option<&str>,
    provenance_bundle_json: Option<&str>,
) -> miette::Result<serde_json::Value> {
    let (shasum, integrity) = archive_hashes(archive);
    let b64_tarball = base64::engine::general_purpose::STANDARD.encode(&archive.tarball);

    let tarball_url = format!(
        "{}/{}/-/{}",
        registry_url.trim_end_matches('/'),
        archive.name,
        archive.filename
    );

    // Start from the manifest JSON so every field the user set (scripts,
    // keywords, repository, ...) reaches the registry, then bolt on the
    // `_id` and `dist` block that the publish protocol requires.
    let mut version_doc = serde_json::to_value(manifest).into_diagnostic()?;
    normalize_publish_manifest(&mut version_doc);
    let obj = version_doc
        .as_object_mut()
        .ok_or_else(|| miette!("manifest did not serialize to a JSON object"))?;
    obj.insert("version".into(), archive.version.clone().into());
    obj.insert(
        "_id".into(),
        format!("{}@{}", archive.name, archive.version).into(),
    );
    obj.insert(
        "dist".into(),
        serde_json::json!({
            "shasum": shasum,
            "integrity": integrity,
            "tarball": tarball_url,
        }),
    );

    let mut body = serde_json::json!({
        "_id": archive.name,
        "name": archive.name,
        "dist-tags": { tag: archive.version },
        "versions": { archive.version.clone(): version_doc },
        "_attachments": {
            archive.filename.clone(): {
                "content_type": "application/octet-stream",
                "data": b64_tarball,
                "length": archive.tarball.len(),
            }
        }
    });
    if let Some(access) = access {
        body.as_object_mut()
            .unwrap()
            .insert("access".into(), access.into());
    }

    // Provenance: npm's publish protocol expects the sigstore bundle to
    // ride along as an extra `_attachments` entry keyed by
    // `<name>-<version>.sigstore` with the DSSE bundle v0.3 media type.
    // The registry re-exposes it through the `/-/npm/v1/attestations/<pkg>`
    // endpoint, which is what lights up the "provenance" badge on npmjs.
    //
    // Unlike the tarball attachment, `data` here is the *raw* JSON
    // string, not base64 — that's what `libnpmpublish` sends and what
    // the registry parses. Sending base64 instead would leave `data`
    // and `length` out of sync (length is the raw byte count) and the
    // registry would fail to decode the bundle.
    if let Some(bundle_json) = provenance_bundle_json {
        let attachment_name = format!("{}-{}.sigstore", archive.name, archive.version);
        let length = bundle_json.len();
        body.as_object_mut()
            .unwrap()
            .get_mut("_attachments")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| miette!("publish body missing _attachments object"))?
            .insert(
                attachment_name,
                serde_json::json!({
                    "content_type": "application/vnd.dev.sigstore.bundle+json;version=0.3",
                    "data": bundle_json,
                    "length": length,
                }),
            );
    }

    Ok(body)
}

fn normalize_publish_manifest(manifest: &mut serde_json::Value) {
    let Some(obj) = manifest.as_object_mut() else {
        return;
    };
    let Some(repository) = obj.get_mut("repository") else {
        return;
    };
    let Some(url) = repository.as_str().and_then(normalize_repository_url) else {
        return;
    };
    *repository = serde_json::json!({
        "type": "git",
        "url": url,
    });
}

fn normalize_repository_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.contains("://") || raw.starts_with("git@") {
        return Some(raw.to_string());
    }
    for (prefix, host) in [
        ("github:", "github.com"),
        ("gitlab:", "gitlab.com"),
        ("bitbucket:", "bitbucket.org"),
    ] {
        if let Some(path) = raw.strip_prefix(prefix) {
            return hosted_repository_url(host, path);
        }
    }
    if raw.split('/').count() == 2 && !raw.contains(':') {
        return hosted_repository_url("github.com", raw);
    }
    Some(raw.to_string())
}

fn hosted_repository_url(host: &str, path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return None;
    }
    let (path, suffix) = path.split_once('#').unwrap_or((path, ""));
    let fragment = if suffix.is_empty() {
        String::new()
    } else {
        format!("#{suffix}")
    };
    let git_path = if path.ends_with(".git") {
        path.to_string()
    } else {
        format!("{path}.git")
    };
    Some(format!("https://{host}/{git_path}{fragment}"))
}

fn archive_hashes(archive: &BuiltArchive) -> (String, String) {
    let shasum = hex::encode(sha1::Sha1::digest(&archive.tarball));
    let digest = Sha512::digest(&archive.tarball);
    let integrity = format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    );
    (shasum, integrity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_registry::config::registry_uri_key_pub;

    #[test]
    fn put_url_encodes_scoped_slash() {
        assert_eq!(
            put_url("https://registry.npmjs.org/", "@scope/pkg"),
            "https://registry.npmjs.org/@scope%2Fpkg"
        );
    }

    #[test]
    fn put_url_plain_name() {
        assert_eq!(
            put_url("https://registry.npmjs.org", "lodash"),
            "https://registry.npmjs.org/lodash"
        );
    }

    #[test]
    fn publish_failure_detects_npm_otp_challenge() {
        let failure = PublishHttpFailure {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: r#"{"error":"EOTP","reason":"This operation requires a one-time password."}"#
                .into(),
        };
        assert!(publish_failure_needs_otp(&failure));
    }

    #[test]
    fn publish_failure_detects_npm_one_time_pass_challenge() {
        let failure = PublishHttpFailure {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: r#"{"error":"You must provide a one-time pass. Upgrade your client to npm@latest in order to use 2FA."}"#
                .into(),
        };
        assert!(publish_failure_needs_otp(&failure));
    }

    #[test]
    fn publish_failure_detects_npm_otp_header_hint() {
        let failure = PublishHttpFailure {
            status: reqwest::StatusCode::FORBIDDEN,
            body: "missing npm-otp header".into(),
        };
        assert!(publish_failure_needs_otp(&failure));
    }

    #[test]
    fn publish_failure_detects_two_factor_required_challenge() {
        let failure = PublishHttpFailure {
            status: reqwest::StatusCode::FORBIDDEN,
            body: "Package requires two-factor authentication for publishing".into(),
        };
        assert!(publish_failure_needs_otp(&failure));
    }

    #[test]
    fn publish_failure_detects_2fa_required_challenge() {
        let failure = PublishHttpFailure {
            status: reqwest::StatusCode::FORBIDDEN,
            body: "2FA is required for this operation".into(),
        };
        assert!(publish_failure_needs_otp(&failure));
    }

    #[test]
    fn publish_failure_does_not_prompt_for_plain_auth_failure() {
        let failure = PublishHttpFailure {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "invalid npm token".into(),
        };
        assert!(!publish_failure_needs_otp(&failure));
    }

    #[test]
    fn publish_failure_does_not_prompt_for_unrelated_two_factor_text() {
        let failure = PublishHttpFailure {
            status: reqwest::StatusCode::FORBIDDEN,
            body: "two-factor authentication is disabled for this account".into(),
        };
        assert!(!publish_failure_needs_otp(&failure));
    }

    #[test]
    fn publish_failure_does_not_prompt_for_server_error() {
        let failure = PublishHttpFailure {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: "EOTP".into(),
        };
        assert!(!publish_failure_needs_otp(&failure));
    }

    fn write_manifest(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("package.json");
        std::fs::write(&p, body).unwrap();
        dir.to_path_buf()
    }

    #[test]
    fn select_skips_private_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write_manifest(&tmp.path().join("a"), r#"{"name":"a","version":"1.0.0"}"#);
        let b = write_manifest(
            &tmp.path().join("b"),
            r#"{"name":"b","version":"1.0.0","private":true}"#,
        );
        let out = select_workspace_packages(
            tmp.path(),
            &[a.clone(), b],
            &aube_workspace::selector::EffectiveFilter::default(),
        )
        .unwrap();
        assert_eq!(out, vec![a]);
    }

    #[test]
    fn select_respects_filter_exact_name() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write_manifest(
            &tmp.path().join("a"),
            r#"{"name":"@scope/a","version":"1.0.0"}"#,
        );
        let b = write_manifest(&tmp.path().join("b"), r#"{"name":"b","version":"1.0.0"}"#);
        let out = select_workspace_packages(
            tmp.path(),
            &[a, b.clone()],
            &aube_workspace::selector::EffectiveFilter::from_filters(["b"]),
        )
        .unwrap();
        assert_eq!(out, vec![b]);
    }

    #[test]
    fn select_respects_filter_glob() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write_manifest(
            &tmp.path().join("a"),
            r#"{"name":"@scope/a","version":"1.0.0"}"#,
        );
        let b = write_manifest(
            &tmp.path().join("b"),
            r#"{"name":"@scope/b","version":"1.0.0"}"#,
        );
        let c = write_manifest(
            &tmp.path().join("c"),
            r#"{"name":"other","version":"1.0.0"}"#,
        );
        let out = select_workspace_packages(
            tmp.path(),
            &[a.clone(), b.clone(), c],
            &aube_workspace::selector::EffectiveFilter::from_filters(["@scope/*"]),
        )
        .unwrap();
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn select_skips_manifest_without_version() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write_manifest(&tmp.path().join("a"), r#"{"name":"a"}"#);
        assert!(
            select_workspace_packages(
                tmp.path(),
                &[a],
                &aube_workspace::selector::EffectiveFilter::default(),
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn publish_json_single_uses_npm_compatible_object_shape() {
        let archive = BuiltArchive {
            name: "demo".to_string(),
            version: "1.2.3".to_string(),
            filename: "demo-1.2.3.tgz".to_string(),
            files: vec!["package.json".to_string(), "index.js".to_string()],
            unpacked_size: 42,
            tarball: b"archive bytes".to_vec(),
        };
        let outcome = PublishOutcome {
            name: "demo".to_string(),
            version: "1.2.3".to_string(),
            registry_url: "https://registry.npmjs.org/".to_string(),
            archive: Some(archive),
            status: PublishStatus::Published,
        };
        let json = publish_outcome_json(&outcome);
        assert_eq!(json.get("id").and_then(|v| v.as_str()), Some("demo@1.2.3"));
        assert_eq!(json.get("name").and_then(|v| v.as_str()), Some("demo"));
        assert_eq!(json.get("version").and_then(|v| v.as_str()), Some("1.2.3"));
        assert_eq!(
            json.get("filename").and_then(|v| v.as_str()),
            Some("demo-1.2.3.tgz")
        );
        assert_eq!(json.get("entryCount").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(json.get("unpackedSize").and_then(|v| v.as_u64()), Some(42));
        assert!(json.get("shasum").and_then(|v| v.as_str()).is_some());
        assert!(
            json.get("integrity")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.starts_with("sha512-"))
        );
    }

    #[test]
    fn publish_normalizes_v_prefixed_semver_like_npm() {
        assert_eq!(normalize_publish_version("v2026.5.16"), "2026.5.16");
        assert_eq!(normalize_publish_version("1.2.3-beta.1"), "1.2.3-beta.1");
        assert_eq!(normalize_publish_version("not-semver"), "not-semver");
    }

    #[test]
    fn publish_body_uses_normalized_version_for_registry_metadata() {
        let mut archive = BuiltArchive {
            name: "@jdxcode/mise-linux-x64".to_string(),
            version: "v2026.5.16".to_string(),
            filename: "jdxcode-mise-linux-x64-v2026.5.16.tgz".to_string(),
            files: vec!["package.json".to_string()],
            unpacked_size: 42,
            tarball: b"archive bytes".to_vec(),
        };
        normalize_archive_for_publish(&mut archive);

        let manifest: PackageJson = serde_json::from_value(serde_json::json!({
            "name": "@jdxcode/mise-linux-x64",
            "version": "v2026.5.16"
        }))
        .unwrap();
        let body = build_publish_body(
            &archive,
            &manifest,
            "https://registry.npmjs.org/",
            "latest",
            Some("public"),
            None,
        )
        .unwrap();

        assert_eq!(archive.version, "2026.5.16");
        assert_eq!(archive.filename, "jdxcode-mise-linux-x64-2026.5.16.tgz");
        assert_eq!(body["dist-tags"]["latest"], "2026.5.16");
        assert!(body["versions"].get("2026.5.16").is_some());
        assert!(body["versions"].get("v2026.5.16").is_none());
        assert_eq!(body["versions"]["2026.5.16"]["version"], "2026.5.16");
        assert_eq!(
            body["versions"]["2026.5.16"]["_id"],
            "@jdxcode/mise-linux-x64@2026.5.16"
        );
    }

    #[test]
    fn publish_body_normalizes_string_repository() {
        let archive = BuiltArchive {
            name: "pkg".to_string(),
            version: "1.0.0".to_string(),
            filename: "pkg-1.0.0.tgz".to_string(),
            files: vec!["package.json".to_string()],
            unpacked_size: 42,
            tarball: b"archive bytes".to_vec(),
        };
        let manifest: PackageJson = serde_json::from_value(serde_json::json!({
            "name": "pkg",
            "version": "1.0.0",
            "repository": "https://codeberg.org/acme/pkg.git"
        }))
        .unwrap();

        let body = build_publish_body(
            &archive,
            &manifest,
            "https://registry.example/",
            "latest",
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            body["versions"]["1.0.0"]["repository"],
            serde_json::json!({
                "type": "git",
                "url": "https://codeberg.org/acme/pkg.git"
            })
        );
    }

    #[test]
    fn publish_body_preserves_repository_object() {
        let archive = BuiltArchive {
            name: "pkg".to_string(),
            version: "1.0.0".to_string(),
            filename: "pkg-1.0.0.tgz".to_string(),
            files: vec!["package.json".to_string()],
            unpacked_size: 42,
            tarball: b"archive bytes".to_vec(),
        };
        let manifest: PackageJson = serde_json::from_value(serde_json::json!({
            "name": "pkg",
            "version": "1.0.0",
            "repository": {
                "type": "hg",
                "url": "https://example.com/acme/pkg"
            }
        }))
        .unwrap();

        let body = build_publish_body(
            &archive,
            &manifest,
            "https://registry.example/",
            "latest",
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            body["versions"]["1.0.0"]["repository"],
            serde_json::json!({
                "type": "hg",
                "url": "https://example.com/acme/pkg"
            })
        );
    }

    #[test]
    fn repository_url_normalizer_expands_npm_shorthands() {
        assert_eq!(
            normalize_repository_url("acme/pkg").as_deref(),
            Some("https://github.com/acme/pkg.git")
        );
        assert_eq!(
            normalize_repository_url("github:acme/pkg").as_deref(),
            Some("https://github.com/acme/pkg.git")
        );
        assert_eq!(
            normalize_repository_url("gitlab:platform/tools/pkg#main").as_deref(),
            Some("https://gitlab.com/platform/tools/pkg.git#main")
        );
        assert_eq!(
            normalize_repository_url("bitbucket:acme/pkg.git").as_deref(),
            Some("https://bitbucket.org/acme/pkg.git")
        );
    }

    #[test]
    fn repository_url_normalizer_preserves_explicit_urls() {
        assert_eq!(
            normalize_repository_url("https://codeberg.org/acme/pkg.git").as_deref(),
            Some("https://codeberg.org/acme/pkg.git")
        );
        assert_eq!(
            normalize_repository_url("git@github.com:acme/pkg.git").as_deref(),
            Some("git@github.com:acme/pkg.git")
        );
    }

    #[test]
    fn publish_archive_embeds_normalized_package_json_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"@jdxcode/mise-linux-x64","version":"v2026.5.16"}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("README.md"), "mise").unwrap();

        let archive = build_archive_for_publish(tmp.path()).unwrap();
        let gz = flate2::read::GzDecoder::new(archive.tarball.as_slice());
        let mut tar = tar::Archive::new(gz);
        let mut package_json = None;
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap() == std::path::Path::new("package/package.json") {
                let mut contents = String::new();
                std::io::Read::read_to_string(&mut entry, &mut contents).unwrap();
                package_json = Some(contents);
                break;
            }
        }
        let package_json: serde_json::Value =
            serde_json::from_str(&package_json.expect("package.json in tarball")).unwrap();

        assert_eq!(archive.version, "2026.5.16");
        assert_eq!(archive.filename, "jdxcode-mise-linux-x64-2026.5.16.tgz");
        assert_eq!(package_json["version"], "2026.5.16");
    }

    #[test]
    fn publish_json_outcomes_uses_array_only_for_multiple_packages() {
        let first = PublishOutcome {
            name: "one".to_string(),
            version: "1.0.0".to_string(),
            registry_url: "https://registry.npmjs.org/".to_string(),
            archive: None,
            status: PublishStatus::DryRun,
        };
        let second = PublishOutcome {
            name: "two".to_string(),
            version: "2.0.0".to_string(),
            registry_url: "https://registry.npmjs.org/".to_string(),
            archive: None,
            status: PublishStatus::DryRun,
        };

        let single = publish_outcomes_json(std::slice::from_ref(&first));
        assert_eq!(single.get("name").and_then(|v| v.as_str()), Some("one"));
        assert!(!single.is_array());

        let multiple = publish_outcomes_json(&[first, second]);
        assert_eq!(multiple.as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn uri_key_matches_registry_helper() {
        // sanity: registry_uri_key_pub must produce the same shape
        // login/logout use, so tokens written by login are findable here.
        assert_eq!(
            registry_uri_key_pub("https://registry.npmjs.org/"),
            "//registry.npmjs.org/"
        );
    }
}
