//! Supply-chain gates that run at the top of `aube add`.
//!
//! Two checks, layered by signal strength:
//!
//! 1. **OSV `MAL-*` advisory check** — hard block via
//!    `ERR_AUBE_MALICIOUS_PACKAGE`. Confirmed malicious advisories
//!    aren't a judgement call. Default fails open on a fetch error
//!    (so offline workflows still install); `advisoryCheck=required`
//!    flips that to fail closed for hardened CI.
//!
//! 2. **Weekly-downloads floor** — interactive confirm prompt below
//!    the threshold, hard refusal in non-interactive contexts unless
//!    `--allow-low-downloads` is passed. Catches typosquats and
//!    impersonations that haven't been reported to OSV yet. The
//!    `allowedUnpopularPackages` setting (glob patterns) bypasses
//!    this gate for opted-in names, leaving the OSV check intact.
//!
//! The gate fires only on the names the user typed for *registry*
//! packages — git/local/workspace/jsr/aliased specs all skip both
//! checks because the public-registry signal doesn't apply. Names
//! whose resolved registry isn't `registry.npmjs.org` (per
//! `NpmConfig::is_public_npmjs`) are filtered out upstream in
//! `registry_bound_inputs_for_supply_chain`.

use aube_codes::errors::{
    ERR_AUBE_ADVISORY_CHECK_FAILED, ERR_AUBE_LOW_DOWNLOAD_PACKAGE, ERR_AUBE_MALICIOUS_PACKAGE,
};
use aube_codes::warnings::{
    WARN_AUBE_ADVISORY_CHECK_FAILED, WARN_AUBE_LOW_DOWNLOAD_PACKAGE,
    WARN_AUBE_OSV_BLOOM_REFRESH_FAILED, WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
};
use aube_registry::osv_bloom_client::OsvBloomClient;
use aube_registry::osv_mirror::OsvMirror;
use aube_registry::supply_chain::{
    DownloadCount, MaliciousAdvisory, advisory_url, fetch_malicious_advisories,
    fetch_malicious_advisories_versioned, fetch_weekly_downloads_with,
};
use aube_settings::resolved::{AdvisoryBloomCheck, AdvisoryCheck, AdvisoryCheckOnInstall};
use miette::miette;
use std::io::{BufRead, IsTerminal, Write};

/// Run both supply-chain gates against the registry-bound specs the
/// user passed to `aube add`. Inputs should already be filtered to
/// packages that resolve via the public npm registry — workspace, git,
/// and local specs are not in scope. Exact pins can use OSV's
/// version-aware query; ranges and dist-tags stay on the name-only
/// query until the resolver picks a concrete version.
///
/// `allow_low_downloads` is the per-invocation `--allow-low-downloads`
/// override; when `true` the download gate is skipped entirely (the
/// advisory check still runs).
///
/// `allowed_unpopular_globs` are the `allowedUnpopularPackages`
/// setting entries: full-name globs that exempt matching names from
/// the downloads gate only. The advisory check still runs against
/// every package regardless — exempting confirmed-malicious advisories
/// is not what this list is for.
pub async fn run_gates(
    name_only_advisory_names: &[String],
    exact_advisory_pairs: &[(String, String)],
    download_names: &[String],
    advisory_check: AdvisoryCheck,
    low_download_threshold: u64,
    allow_low_downloads: bool,
    allowed_unpopular_globs: &[String],
) -> miette::Result<()> {
    if name_only_advisory_names.is_empty()
        && exact_advisory_pairs.is_empty()
        && download_names.is_empty()
    {
        return Ok(());
    }
    // One client shared across both gates and every per-package
    // probe so the OSV POST and the (potentially parallel) downloads
    // GETs all reuse the same connection pool + TLS session.
    //
    // Builder failure (TLS init, no root certs, etc.) routes through
    // the same `advisoryCheck` policy `osv_gate` applies to HTTP
    // failures: under `Required` it's a hard fail with
    // `ERR_AUBE_ADVISORY_CHECK_FAILED`, otherwise it warns and skips
    // both gates. `Off` short-circuits before even surfacing the
    // warning — the user opted out of OSV entirely, so a probe-
    // client init failure is no longer their concern.
    let client = match aube_registry::supply_chain::build_probe_client() {
        Ok(c) => c,
        Err(e) => {
            if matches!(advisory_check, AdvisoryCheck::Off) {
                tracing::debug!(
                    "supply-chain probe client init failed; OSV is off, skipping all gates: {e}"
                );
                return Ok(());
            }
            tracing::warn!(
                code = WARN_AUBE_ADVISORY_CHECK_FAILED,
                "supply-chain probe client init failed: {e}"
            );
            if matches!(advisory_check, AdvisoryCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "supply-chain probe client could not be initialised and `advisoryCheck = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    osv_gate(&client, name_only_advisory_names, advisory_check).await?;
    osv_gate_versioned(
        &client,
        exact_advisory_pairs,
        advisory_check,
        "refusing to add malicious package(s):",
    )
    .await?;
    if !allow_low_downloads && low_download_threshold > 0 {
        let patterns = compile_allowed_unpopular(allowed_unpopular_globs);
        let gated: Vec<String> = download_names
            .iter()
            .filter(|n| !patterns.iter().any(|p| p.matches(n)))
            .cloned()
            .collect();
        if !gated.is_empty() {
            downloads_gate(&client, &gated, low_download_threshold).await?;
        }
    }
    Ok(())
}

/// Single entry point for the post-resolve OSV `MAL-*` routing
/// `install::run` uses from both lockfile branches.
///
/// Pre-PR the routing block was inline in the no-lockfile (Err)
/// match arm only, so `aube ci` and any `aube install` that
/// matched the lockfile cleanly silently skipped OSV entirely —
/// `advisoryCheckEveryInstall = true` and `advisoryCheckOnInstall`
/// were designed for exactly that path. Extracted here so both
/// arms call the same helper and the routing table actually
/// applies to every install entry point.
///
/// Decision table:
/// - `fresh_resolution || osv_transitive_check || advisory_check_every_install`
///   → live OSV API (`run_transitive_osv_gate`)
/// - otherwise, when `advisory_check_on_install != Off`
///   → local mirror (`run_transitive_osv_gate_via_mirror`)
/// - otherwise → no OSV check
///
/// `advisory_check` is the caller's already-upgraded policy
/// (paranoid → `Required`). Both gates internally short-circuit
/// on `Off`, but skip the call entirely so an empty graph
/// doesn't get a useless `transitive_registry_pairs` walk.
///
/// Returns whether an OSV gate actually covered this install (`true`
/// = one of the three backends ran against the graph). The
/// `defaultTrust` build-policy floor consults this: it only trusts a
/// listed package when the advisory machinery vouched for the graph,
/// so turning every advisory check off turns the floor off with it.
#[allow(clippy::too_many_arguments)]
pub async fn run_post_resolve_osv_routing(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    fresh_resolution: bool,
    osv_transitive_check: bool,
    advisory_check: AdvisoryCheck,
    advisory_check_on_install: AdvisoryCheckOnInstall,
    advisory_bloom_check: AdvisoryBloomCheck,
    advisory_check_every_install: bool,
) -> miette::Result<bool> {
    let needs_live_api = osv_transitive_check || advisory_check_every_install || fresh_resolution;
    if needs_live_api {
        if !matches!(advisory_check, AdvisoryCheck::Off) {
            run_transitive_osv_gate(cwd, graph, advisory_check).await?;
            return Ok(true);
        }
    } else if !matches!(advisory_bloom_check, AdvisoryBloomCheck::Off) {
        // Bloom preferred over the local-mirror fallback when both
        // are configured: it's <1 MB on the wire vs the mirror's
        // 200 MB zip, and bloom hits escalate to the same live-API
        // oracle the fresh-resolution path uses, so a confirmed
        // hit produces the identical `ERR_AUBE_MALICIOUS_PACKAGE`
        // either way.
        run_transitive_osv_gate_via_bloom(cwd, graph, advisory_bloom_check).await?;
        return Ok(true);
    } else if !matches!(advisory_check_on_install, AdvisoryCheckOnInstall::Off) {
        run_transitive_osv_gate_via_mirror(cwd, graph, advisory_check_on_install).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Live-API transitive OSV `MAL-*` check.
///
/// Runs against the full post-resolve transitive set, batch-querying
/// `api.osv.dev`. Used by the fresh-resolution install paths —
/// `aube add`, `aube update`, missing-lockfile installs, and any
/// install where the resolver picked a `(name, version)` the
/// lockfile didn't already pin. The companion
/// [`run_transitive_osv_gate_via_mirror`] is the local-mirror
/// fallback for plain reinstalls where the lockfile was authoritative.
///
/// Policy mapping (same shape as the existing CLI-name gate):
/// - `Off` → no-op.
/// - `On` → OSV fetch failures degrade to
///   `WARN_AUBE_ADVISORY_CHECK_FAILED` and install proceeds.
/// - `Required` → fetch failures map to
///   `ERR_AUBE_ADVISORY_CHECK_FAILED`. Hits map to
///   `ERR_AUBE_MALICIOUS_PACKAGE` under both `On` and `Required`.
pub async fn run_transitive_osv_gate(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: AdvisoryCheck,
) -> miette::Result<()> {
    if matches!(policy, AdvisoryCheck::Off) {
        return Ok(());
    }
    let pairs = transitive_registry_pairs(cwd, graph);
    if pairs.is_empty() {
        return Ok(());
    }
    let client = match aube_registry::supply_chain::build_probe_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_ADVISORY_CHECK_FAILED,
                "supply-chain probe client init failed: {e}"
            );
            if matches!(policy, AdvisoryCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "supply-chain probe client could not be initialised and `advisoryCheck = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    osv_gate_versioned(
        &client,
        &pairs,
        policy,
        "refusing to install malicious package(s):",
    )
    .await
}

/// Mirror-backed transitive OSV `MAL-*` check for plain reinstalls.
///
/// Counterpart to [`run_transitive_osv_gate`]. Fires when none of
/// the fresh-resolution triggers apply (no `aube add` / `aube
/// update`, no `advisoryCheckEveryInstall`, no lockfile drift) — so
/// the live-API gate is dormant and the mirror picks up the slack
/// against the post-resolve graph without an `api.osv.dev`
/// round-trip on every reinstall. Off by default.
///
/// Policy mapping (mirrors the live-API gate's shape so CI configs
/// that have `advisoryCheck = required` can mirror that bit onto
/// `advisoryCheckOnInstall = required` without surprise):
/// - `Off` → no-op.
/// - `On` → mirror refresh failures degrade to `WARN_AUBE_OSV_MIRROR_REFRESH_FAILED`
///   and a `tracing::warn!`; install continues against the prior
///   (possibly empty) on-disk index.
/// - `Required` → mirror refresh failures map to
///   `ERR_AUBE_ADVISORY_CHECK_FAILED`. Hits map to
///   `ERR_AUBE_MALICIOUS_PACKAGE` under both `On` and `Required`,
///   same as the live-API gate.
pub async fn run_transitive_osv_gate_via_mirror(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: AdvisoryCheckOnInstall,
) -> miette::Result<()> {
    if matches!(policy, AdvisoryCheckOnInstall::Off) {
        return Ok(());
    }
    let pairs = transitive_registry_pairs(cwd, graph);
    if pairs.is_empty() {
        return Ok(());
    }
    let Some(cache_dir) = aube_store::dirs::cache_dir() else {
        // `$HOME` (or platform equivalent) is unset, so we can't
        // open the mirror. Same policy split as a refresh failure
        // — `Required` is a hard stop, `On` is a warning.
        tracing::warn!(
            code = WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
            "OSV mirror cache dir unavailable (HOME/XDG_CACHE_HOME unset); skipping install-time advisory check"
        );
        if matches!(policy, AdvisoryCheckOnInstall::Required) {
            return Err(miette!(
                code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                "OSV mirror cache dir unavailable and `advisoryCheckOnInstall = required` is set"
            ));
        }
        return Ok(());
    };
    let mirror = OsvMirror::open(&cache_dir);
    let client = match OsvMirror::build_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
                "OSV mirror probe client init failed: {e}"
            );
            if matches!(policy, AdvisoryCheckOnInstall::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV mirror probe client could not be initialised and `advisoryCheckOnInstall = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    if let Err(e) = mirror.refresh_if_stale_default(&client).await {
        tracing::warn!(
            code = WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
            "OSV mirror refresh failed: {e}"
        );
        if matches!(policy, AdvisoryCheckOnInstall::Required) {
            return Err(miette!(
                code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                "OSV mirror refresh failed and `advisoryCheckOnInstall = required` is set: {e}"
            ));
        }
        // Fall through under `On`: `refresh_if_stale` already
        // seeded the in-memory cache with whatever the on-disk
        // index held going in, so `lookup_advisories` below
        // checks against the previously cached data. When the
        // mirror has never been synced successfully the prior
        // data is empty and lookup is a no-op — the warning is
        // the only user-visible signal in that case.
    }
    let hits = match mirror.lookup_advisories_versioned(&pairs) {
        Ok(hits) => hits,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
                "OSV mirror lookup failed: {e}"
            );
            if matches!(policy, AdvisoryCheckOnInstall::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV mirror lookup failed and `advisoryCheckOnInstall = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    if hits.is_empty() {
        return Ok(());
    }
    Err(miette!(
        code = ERR_AUBE_MALICIOUS_PACKAGE,
        "{}",
        format_malicious_message(
            "refusing to install malicious package(s):",
            &hits,
            "Set `advisoryCheckOnInstall = off` to bypass (not recommended).",
        ),
    ))
}

/// Bloom-prefilter transitive OSV `MAL-*` check for lockfile-driven
/// installs. Probes each `(registry_name, semver-major)` against the
/// upstream-published bloom (sub-MB, regenerated every 10 minutes
/// by `endevco/osv-bloom`); only bloom hits escalate to the live
/// OSV API for exact-version confirmation. Cheap enough on the wire
/// that a future PR can flip the default to `on` once we've watched
/// the FPR in real installs.
///
/// Policy mapping mirrors the live-API gate so settings can be
/// composed sensibly:
/// - `Off` → no-op.
/// - `On` → bloom refresh / live-API failures degrade to
///   `WARN_AUBE_OSV_BLOOM_REFRESH_FAILED` or
///   `WARN_AUBE_ADVISORY_CHECK_FAILED` and install proceeds.
/// - `Required` → refresh / live-API failures map to
///   `ERR_AUBE_ADVISORY_CHECK_FAILED`. Confirmed hits map to
///   `ERR_AUBE_MALICIOUS_PACKAGE` under both `On` and `Required`,
///   identical to every other OSV gate.
pub async fn run_transitive_osv_gate_via_bloom(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: AdvisoryBloomCheck,
) -> miette::Result<()> {
    if matches!(policy, AdvisoryBloomCheck::Off) {
        return Ok(());
    }
    let pkgs = transitive_registry_pairs(cwd, graph);
    if pkgs.is_empty() {
        return Ok(());
    }
    let Some(cache_dir) = aube_store::dirs::cache_dir() else {
        tracing::warn!(
            code = WARN_AUBE_OSV_BLOOM_REFRESH_FAILED,
            "OSV bloom cache dir unavailable (HOME/XDG_CACHE_HOME unset); skipping bloom check"
        );
        if matches!(policy, AdvisoryBloomCheck::Required) {
            return Err(miette!(
                code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                "OSV bloom cache dir unavailable and `advisoryBloomCheck = required` is set"
            ));
        }
        return Ok(());
    };
    let bloom_client = OsvBloomClient::open(&cache_dir);
    let http = match OsvBloomClient::build_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_OSV_BLOOM_REFRESH_FAILED,
                "OSV bloom probe client init failed: {e}"
            );
            if matches!(policy, AdvisoryBloomCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV bloom probe client could not be initialised and `advisoryBloomCheck = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    if let Err(e) = bloom_client.refresh_if_stale_default(&http).await {
        tracing::warn!(
            code = WARN_AUBE_OSV_BLOOM_REFRESH_FAILED,
            "OSV bloom refresh failed: {e}"
        );
        if matches!(policy, AdvisoryBloomCheck::Required) {
            return Err(miette!(
                code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                "OSV bloom refresh failed and `advisoryBloomCheck = required` is set: {e}"
            ));
        }
        return Ok(());
    }
    let bloom_hits = match bloom_client.probe_lockfile(&pkgs) {
        Ok(hits) => hits,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_OSV_BLOOM_REFRESH_FAILED,
                "OSV bloom probe failed: {e}"
            );
            if matches!(policy, AdvisoryBloomCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV bloom probe failed and `advisoryBloomCheck = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    if bloom_hits.is_empty() {
        return Ok(());
    }
    // Escalate to the live OSV API for exact-version confirmation.
    // The bloom is a prefilter: a hit is a *probable* malicious
    // package, not a confirmed one. False positives turn into one
    // extra `/querybatch` request per FP, which the existing
    // chunking in `fetch_malicious_advisories` already handles.
    let live_policy = match policy {
        AdvisoryBloomCheck::Required => AdvisoryCheck::Required,
        _ => AdvisoryCheck::On,
    };
    let live_client = match aube_registry::supply_chain::build_probe_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_ADVISORY_CHECK_FAILED,
                "live-OSV probe client init failed during bloom escalation: {e}"
            );
            if matches!(policy, AdvisoryBloomCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "live-OSV probe client could not be initialised and `advisoryBloomCheck = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    // Escalate through the versioned gate: the bloom probe returns
    // the `(name, version)` pairs that *might* be malicious; the
    // live API confirms or clears each pair against its exact
    // pinned version. Name-only escalation here would collapse a
    // version-specific compromise (e.g. `ansi-regex@6.2.1`) into a
    // permanent name-level block of every release. The bloom path's
    // `On`/`Required` policy is `advisoryBloomCheck`, not
    // `advisoryCheck` — `osv_gate_versioned_with_bypass` threads
    // that setting name through to the `ERR_AUBE_MALICIOUS_PACKAGE`
    // footer and the required-failure message.
    osv_gate_versioned_with_bypass(
        &live_client,
        &bloom_hits,
        live_policy,
        "refusing to install malicious package(s):",
        "advisoryBloomCheck",
    )
    .await
}

/// True when the resolved graph contains at least one
/// `(registry_name, version)` pair the pre-existing lockfile
/// didn't already pin — meaning the resolver did fresh work and
/// the result deserves a live-API OSV pass rather than the
/// mirror-backed fallback. A missing pre-existing lockfile
/// (`None`) is treated as drift by definition: nothing on disk
/// vouched for what just got resolved.
///
/// Filtered to public-npmjs registry names so private / workspace
/// / git / file deps don't get classified as "new" just because
/// they aren't in a public-npmjs comparison set.
pub fn lockfile_has_new_picks(
    cwd: &std::path::Path,
    prior: Option<&aube_lockfile::LockfileGraph>,
    resolved: &aube_lockfile::LockfileGraph,
) -> bool {
    use std::collections::HashSet;
    let npm_config = aube_registry::config::NpmConfig::load(cwd);
    // Both the prior-pairs set and the resolved walk filter by
    // `local_source.is_none()` + `is_public_npmjs`. Building the
    // prior set as empty when `prior` is `None` means a
    // workspace-only project (all `link:` / `file:` / workspace
    // deps, no public npm) doesn't get classified as drift just
    // because there's no lockfile — the resolved walk also drops
    // those entries, so no fresh pair survives and the function
    // returns `false`. Public-npmjs entries against `None` prior
    // are real fresh picks and surface correctly.
    let prior_pairs: HashSet<(&str, &str)> = prior
        .map(|g| {
            g.packages
                .values()
                .filter(|p| p.local_source.is_none())
                .map(|p| (p.registry_name(), p.version.as_str()))
                .collect()
        })
        .unwrap_or_default();
    resolved
        .packages
        .values()
        .filter(|p| p.local_source.is_none())
        .filter(|p| npm_config.is_public_npmjs(p.registry_name()))
        .any(|p| !prior_pairs.contains(&(p.registry_name(), p.version.as_str())))
}

/// Distinct public-npmjs `(registry_name, version)` pairs in
/// `graph`, filtered to match the CLI-name gate's
/// `registry_bound_inputs_for_supply_chain` shape so a scoped
/// registry override (`@myorg:registry=...`) or a swapped default
/// registry doesn't ship internal package names to OSV. Workspace /
/// `link:` / `file:` entries drop out via
/// `LockedPackage::local_source.is_none()`. Sorted + deduped so
/// aliased entries (`{"my-alias": "npm:lodash@^4"}`) collapse onto
/// their real registry name.
///
/// Pairs (not just names) because the post-resolve OSV check
/// needs to ask "is *this specific version* malicious?" — a
/// name-only query collapses version-specific compromises (e.g.
/// the Sep 2025 `ansi-regex@6.2.1` worm) into a permanent
/// name-level block of every published release.
fn transitive_registry_pairs(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
) -> Vec<(String, String)> {
    let npm_config = aube_registry::config::NpmConfig::load(cwd);
    let mut pairs: Vec<(String, String)> = graph
        .packages
        .values()
        .filter(|pkg| pkg.local_source.is_none())
        .filter(|pkg| npm_config.is_public_npmjs(pkg.registry_name()))
        .map(|pkg| (pkg.registry_name().to_string(), pkg.version.clone()))
        .collect();
    pairs.sort();
    pairs.dedup();
    pairs
}

/// Parse `allowedUnpopularPackages` entries into compiled
/// `glob::Pattern`s. Invalid entries are logged and dropped — we'd
/// rather miss an exemption (and prompt the user) than fail the
/// whole `aube add` over a typo in a user-defined glob.
fn compile_allowed_unpopular(raw: &[String]) -> Vec<glob::Pattern> {
    raw.iter()
        .filter_map(|p| match glob::Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(e) => {
                tracing::warn!("ignoring malformed allowedUnpopularPackages entry `{p}`: {e}");
                None
            }
        })
        .collect()
}

async fn osv_gate(
    client: &reqwest::Client,
    names: &[String],
    policy: AdvisoryCheck,
) -> miette::Result<()> {
    if matches!(policy, AdvisoryCheck::Off) {
        return Ok(());
    }
    handle_osv_result(
        fetch_malicious_advisories(client, names).await,
        policy,
        "refusing to add malicious package(s):",
        "advisoryCheck",
    )
}

async fn osv_gate_versioned(
    client: &reqwest::Client,
    pairs: &[(String, String)],
    policy: AdvisoryCheck,
    refusal_header: &str,
) -> miette::Result<()> {
    osv_gate_versioned_with_bypass(client, pairs, policy, refusal_header, "advisoryCheck").await
}

/// Versioned-OSV gate with an overridable bypass-setting name.
/// Used by the bloom-prefilter escalation path so the
/// `ERR_AUBE_MALICIOUS_PACKAGE` footer and
/// `ERR_AUBE_ADVISORY_CHECK_FAILED` message point the user at
/// `advisoryBloomCheck` — the setting that actually controls that
/// install gate — rather than `advisoryCheck`, which doesn't.
async fn osv_gate_versioned_with_bypass(
    client: &reqwest::Client,
    pairs: &[(String, String)],
    policy: AdvisoryCheck,
    refusal_header: &str,
    bypass_setting: &str,
) -> miette::Result<()> {
    if matches!(policy, AdvisoryCheck::Off) {
        return Ok(());
    }
    handle_osv_result(
        fetch_malicious_advisories_versioned(client, pairs).await,
        policy,
        refusal_header,
        bypass_setting,
    )
}

fn handle_osv_result(
    result: Result<Vec<MaliciousAdvisory>, aube_registry::supply_chain::SupplyChainError>,
    policy: AdvisoryCheck,
    refusal_header: &str,
    bypass_setting: &str,
) -> miette::Result<()> {
    match result {
        Ok(hits) if hits.is_empty() => Ok(()),
        Ok(hits) => Err(miette!(
            code = ERR_AUBE_MALICIOUS_PACKAGE,
            "{}",
            format_malicious_message(
                refusal_header,
                &hits,
                &format!("Set `{bypass_setting} = off` to bypass (not recommended)."),
            ),
        )),
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_ADVISORY_CHECK_FAILED,
                "OSV advisory check failed: {e}"
            );
            // `AdvisoryCheck::Off` short-circuits at the top of the
            // caller and never reaches this branch — only the
            // `On` / `Required` split needs handling here.
            if matches!(policy, AdvisoryCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV advisory check failed and `{bypass_setting} = required` is set: {e}"
                ));
            }
            Ok(())
        }
    }
}

/// Format the user-facing refusal message. Versioned hits surface
/// as `name@version (MAL-…)`; name-only hits (the pre-resolve `aube
/// add` gate) surface as `name (MAL-…)` since no version was
/// queried.
fn format_malicious_message(header: &str, hits: &[MaliciousAdvisory], footer: &str) -> String {
    let mut lines = vec![header.to_string()];
    for hit in hits {
        let display_name = match &hit.version {
            Some(v) => format!("{}@{}", hit.package, v),
            None => hit.package.clone(),
        };
        lines.push(format!(
            "  - {} ({}: {})",
            display_name,
            hit.advisory_id,
            advisory_url(&hit.advisory_id),
        ));
    }
    lines.push(String::new());
    lines.push(footer.to_string());
    lines.join("\n")
}

async fn downloads_gate(
    client: &reqwest::Client,
    names: &[String],
    threshold: u64,
) -> miette::Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut set: tokio::task::JoinSet<(String, Result<DownloadCount, _>)> =
        tokio::task::JoinSet::new();
    for name in names {
        let client = client.clone();
        let name = name.clone();
        set.spawn(async move {
            let result = fetch_weekly_downloads_with(&client, &name).await;
            (name, result)
        });
    }
    // Preserve input order so the warning / prompt sequence is
    // deterministic regardless of which probe returns first.
    let mut by_name: std::collections::HashMap<String, _> =
        std::collections::HashMap::with_capacity(names.len());
    while let Some(joined) = set.join_next().await {
        // `join_next` only errors on panic / cancellation — those are
        // bugs in this call site rather than expected probe failures,
        // so propagate via tracing and skip the slot. The OSV gate
        // above is still the harder line.
        let (name, result) = match joined {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!("downloads probe task join failed: {e}");
                continue;
            }
        };
        by_name.insert(name, result);
    }
    for name in names {
        let Some(result) = by_name.remove(name) else {
            continue;
        };
        let count = match result {
            Ok(c) => c,
            Err(e) => {
                // Treat a downloads-API fetch error as "no signal" —
                // we'd rather let a sketchy install through than break
                // every add when api.npmjs.org has a hiccup.
                tracing::debug!("downloads probe failed for {name}: {e}");
                continue;
            }
        };
        let DownloadCount::Known(weekly) = count else {
            // Scoped packages, brand-new names with no published
            // history, or registry mirrors that don't proxy
            // `api.npmjs.org` all fall here. No signal → no gate.
            continue;
        };
        if weekly >= threshold {
            continue;
        }
        tracing::warn!(
            code = WARN_AUBE_LOW_DOWNLOAD_PACKAGE,
            "{name}: {weekly} weekly downloads (threshold: {threshold})"
        );
        if !interactive {
            return Err(miette!(
                code = ERR_AUBE_LOW_DOWNLOAD_PACKAGE,
                "refusing to add {name}: only {weekly} weekly downloads (threshold: {threshold}). Pass --allow-low-downloads to bypass, or set `lowDownloadThreshold = 0`."
            ));
        }
        if !prompt_continue(name, weekly, threshold)? {
            return Err(miette!(
                code = ERR_AUBE_LOW_DOWNLOAD_PACKAGE,
                "user aborted `{} {name}`",
                aube_util::cmd("add")
            ));
        }
    }
    Ok(())
}

fn prompt_continue(name: &str, weekly: u64, threshold: u64) -> miette::Result<bool> {
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "  ⚠ {name} looks suspicious:").ok();
    writeln!(
        stderr,
        "    • {weekly} downloads last week (threshold: {threshold})"
    )
    .ok();
    write!(stderr, "  Continue adding {name}? [y/N] ").ok();
    stderr.flush().ok();
    drop(stderr);

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| {
        miette!(
            code = ERR_AUBE_LOW_DOWNLOAD_PACKAGE,
            "failed to read confirmation: {e}"
        )
    })?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn osv_gate_off_skips_network() {
        // `Off` short-circuits before any HTTP — important so users
        // who set `advisoryCheck = off` for an air-gapped registry
        // don't see spurious timeouts on add. The dummy client is
        // never touched on this code path; we still have to
        // construct one to satisfy the type signature.
        let client = aube_registry::supply_chain::build_probe_client()
            .expect("probe client builder shouldn't fail in tests");
        let names = vec!["lodash".to_string()];
        assert!(osv_gate(&client, &names, AdvisoryCheck::Off).await.is_ok());
    }

    #[tokio::test]
    async fn run_gates_no_op_on_empty() {
        // Workspace/git/local-only invocations end up with an empty
        // registry-name list. The function must be a no-op in that
        // case (no network, no error) so those code paths stay free.
        assert!(
            run_gates(&[], &[], &[], AdvisoryCheck::Required, 1000, false, &[])
                .await
                .is_ok()
        );
    }

    #[test]
    fn compile_allowed_unpopular_drops_invalid_patterns() {
        // `[` is a malformed range — we keep the well-formed entries
        // and drop the broken one so a single typo doesn't disable
        // every exemption.
        let pats = compile_allowed_unpopular(&[
            "@myorg/*".to_string(),
            "[unterminated".to_string(),
            "internal-*".to_string(),
        ]);
        assert_eq!(pats.len(), 2);
        assert!(pats.iter().any(|p| p.matches("@myorg/foo")));
        assert!(pats.iter().any(|p| p.matches("internal-thing")));
        assert!(!pats.iter().any(|p| p.matches("public-pkg")));
    }

    #[test]
    fn compile_allowed_unpopular_scope_glob_matches_only_in_scope() {
        // `@myorg/*` should match every name in the `@myorg` scope
        // but not a same-named unscoped package, and not a different
        // scope. Catches the regression where a too-greedy pattern
        // (e.g. plain `myorg*`) would skip arbitrary names.
        let pats = compile_allowed_unpopular(&["@myorg/*".to_string()]);
        assert!(pats[0].matches("@myorg/utils"));
        assert!(pats[0].matches("@myorg/nested-name"));
        assert!(!pats[0].matches("@otherorg/utils"));
        assert!(!pats[0].matches("myorg-utils"));
    }

    fn registry_pkg(name: &str, version: &str) -> aube_lockfile::LockedPackage {
        aube_lockfile::LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn transitive_registry_pairs_skips_local_source_entries() {
        // `file:` / `link:` / workspace edges resolve outside the
        // public registry — OSV has nothing to say about them, and
        // forwarding the workspace package name to OSV could leak
        // an internal name to a public API.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let mut linked = registry_pkg("@workspace/util", "1.0.0");
        linked.local_source = Some(aube_lockfile::LocalSource::Link("../util".into()));
        packages.insert("@workspace/util@1.0.0".to_string(), linked);
        let graph = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let pairs = transitive_registry_pairs(tmp.path(), &graph);
        assert_eq!(pairs, vec![("lodash".to_string(), "4.17.21".to_string())],);
    }

    #[test]
    fn transitive_registry_pairs_dedups_by_registry_name_and_version() {
        // Alias entries (`{"my-alias": "npm:lodash@^4"}`) and the
        // real package both report under `registry_name() = "lodash"`.
        // The mirror lookup shouldn't see duplicates — and shouldn't
        // surface the alias name to the public API either.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let mut aliased = registry_pkg("my-alias", "4.17.21");
        aliased.alias_of = Some("lodash".to_string());
        packages.insert("my-alias@4.17.21".to_string(), aliased);
        let graph = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let pairs = transitive_registry_pairs(tmp.path(), &graph);
        assert_eq!(pairs, vec![("lodash".to_string(), "4.17.21".to_string())],);
    }

    #[test]
    fn transitive_registry_pairs_keeps_distinct_versions_of_one_name() {
        // Two pinned versions of the same name (common when a peer
        // pulls in an older copy) must both reach OSV — checking
        // only one would leave the other's per-version compromise
        // status unanswered.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "ansi-regex@3.0.1".to_string(),
            registry_pkg("ansi-regex", "3.0.1"),
        );
        packages.insert(
            "ansi-regex@6.2.1".to_string(),
            registry_pkg("ansi-regex", "6.2.1"),
        );
        let graph = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let pairs = transitive_registry_pairs(tmp.path(), &graph);
        assert_eq!(
            pairs,
            vec![
                ("ansi-regex".to_string(), "3.0.1".to_string()),
                ("ansi-regex".to_string(), "6.2.1".to_string()),
            ],
        );
    }

    #[test]
    fn format_malicious_message_includes_version_when_present() {
        // Versioned hits should surface as `name@version` so the
        // user can see which pinned version OSV flagged. The
        // pre-resolve gate (no version) keeps the bare name shape
        // it had before.
        let hits = vec![
            MaliciousAdvisory {
                package: "ansi-regex".to_string(),
                advisory_id: "MAL-2025-46966".to_string(),
                version: Some("6.2.1".to_string()),
            },
            MaliciousAdvisory {
                package: "evil".to_string(),
                advisory_id: "MAL-9999".to_string(),
                version: None,
            },
        ];
        let msg = format_malicious_message("header:", &hits, "footer.");
        assert!(msg.contains("ansi-regex@6.2.1"));
        assert!(msg.contains("MAL-2025-46966"));
        assert!(msg.contains("- evil ("), "name-only hit keeps bare name");
        assert!(msg.starts_with("header:"));
        assert!(msg.ends_with("footer."));
    }

    #[test]
    fn add_exact_pin_refusal_uses_add_header() {
        let err = handle_osv_result(
            Ok(vec![MaliciousAdvisory {
                package: "nx".to_string(),
                advisory_id: "MAL-2025-41443".to_string(),
                version: Some("20.9.0".to_string()),
            }]),
            AdvisoryCheck::On,
            "refusing to add malicious package(s):",
            "advisoryCheck",
        )
        .expect_err("malicious hit should fail");
        let rendered = err.to_string();
        assert!(rendered.contains("refusing to add malicious package(s):"));
        assert!(!rendered.contains("refusing to install malicious package(s):"));
        assert!(rendered.contains("nx@20.9.0"));
    }

    #[test]
    fn lockfile_drift_no_prior_lockfile_is_drift_when_resolved_has_entries() {
        // No on-disk lockfile + non-empty resolve = fresh
        // resolution by definition. The router needs this to flip
        // to the live API even though the user typed plain
        // `aube install`.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let resolved = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(lockfile_has_new_picks(tmp.path(), None, &resolved));
    }

    #[test]
    fn lockfile_drift_no_prior_with_only_workspace_entries_is_not_drift() {
        // Workspace-only project (all `link:` / `file:` / workspace
        // deps, no public npm) with no lockfile must NOT be classified
        // as fresh-resolution drift — the live-API OSV gate has
        // nothing to check against a graph of internal-only entries.
        // Regression: the `None`-prior short-circuit used to ignore
        // the local-source / public-npmjs filter and surfaced workspace
        // graphs as drift, forcing an unnecessary live-API hit.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        let mut linked = registry_pkg("@workspace/util", "1.0.0");
        linked.local_source = Some(aube_lockfile::LocalSource::Link("../util".into()));
        packages.insert("@workspace/util@1.0.0".to_string(), linked);
        let resolved = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!lockfile_has_new_picks(tmp.path(), None, &resolved));
    }

    #[test]
    fn lockfile_drift_empty_resolve_and_no_prior_is_not_drift() {
        // No lockfile + nothing resolved (e.g. a workspace with no
        // deps) shouldn't flip the live-API gate on — there's
        // nothing to check anyway.
        let resolved = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!lockfile_has_new_picks(tmp.path(), None, &resolved));
    }

    #[test]
    fn lockfile_drift_fully_pinned_is_not_drift() {
        // Prior and resolved hold the same (registry_name, version)
        // pair: the lockfile was authoritative, fall through to the
        // mirror path.
        use std::collections::BTreeMap;
        let mut prior_packages = BTreeMap::new();
        prior_packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let prior = aube_lockfile::LockfileGraph {
            packages: prior_packages.clone(),
            ..Default::default()
        };
        let resolved = aube_lockfile::LockfileGraph {
            packages: prior_packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!lockfile_has_new_picks(tmp.path(), Some(&prior), &resolved));
    }

    #[test]
    fn lockfile_drift_new_version_is_drift() {
        // Resolver picked a version the lockfile didn't pin — the
        // canonical fresh-resolution signal. Same name, different
        // version, both public-npmjs.
        use std::collections::BTreeMap;
        let mut prior_packages = BTreeMap::new();
        prior_packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let prior = aube_lockfile::LockfileGraph {
            packages: prior_packages,
            ..Default::default()
        };
        let mut resolved_packages = BTreeMap::new();
        resolved_packages.insert(
            "lodash@4.17.22".to_string(),
            registry_pkg("lodash", "4.17.22"),
        );
        let resolved = aube_lockfile::LockfileGraph {
            packages: resolved_packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(lockfile_has_new_picks(tmp.path(), Some(&prior), &resolved));
    }

    #[test]
    fn lockfile_drift_ignores_local_source_entries() {
        // Workspace / link: / file: entries shouldn't trigger
        // drift detection even when they aren't in the prior
        // lockfile — they don't resolve through the public
        // registry, so OSV has no signal on them.
        use std::collections::BTreeMap;
        let mut resolved_packages = BTreeMap::new();
        let mut linked = registry_pkg("@workspace/util", "1.0.0");
        linked.local_source = Some(aube_lockfile::LocalSource::Link("../util".into()));
        resolved_packages.insert("@workspace/util@1.0.0".to_string(), linked);
        let resolved = aube_lockfile::LockfileGraph {
            packages: resolved_packages,
            ..Default::default()
        };
        let prior = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!lockfile_has_new_picks(tmp.path(), Some(&prior), &resolved));
    }

    #[tokio::test]
    async fn run_transitive_osv_gate_off_skips_network() {
        // Mirror of the live-API gate's off-policy test: `Off`
        // must short-circuit before any client construction or
        // network access.
        let graph = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            run_transitive_osv_gate(tmp.path(), &graph, AdvisoryCheck::Off)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_transitive_osv_gate_via_mirror_off_short_circuits() {
        // `advisoryCheckOnInstall = off` is the default for every
        // user that hasn't opted in. A `LockfileGraph` with real
        // entries must not refresh the on-disk mirror or hit the
        // network — that would defeat the "no per-install network
        // cost" promise of the install-time gate.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let graph = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            run_transitive_osv_gate_via_mirror(tmp.path(), &graph, AdvisoryCheckOnInstall::Off,)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_transitive_osv_gate_via_mirror_empty_graph_is_noop() {
        // No public-npmjs entries → nothing to check. The mirror
        // should not even be opened, much less refreshed.
        let graph = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            run_transitive_osv_gate_via_mirror(tmp.path(), &graph, AdvisoryCheckOnInstall::On,)
                .await
                .is_ok()
        );
    }
}
