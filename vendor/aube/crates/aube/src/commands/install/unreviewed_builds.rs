use super::lifecycle::UnreviewedBuild;
use crate::state;

/// Materialize the warm-path replay set. The on-disk state file
/// stores spec_keys only; suspicion data is re-derived per-install
/// from the live tree and not persisted, so warm-path replays
/// always carry empty `suspicions`. Live installs that need
/// content-sniff data build `UnreviewedBuild`s directly from
/// `unreviewed_dep_builds`.
pub(super) fn from_state(cwd: &std::path::Path) -> Vec<UnreviewedBuild> {
    state::read_state_unreviewed_builds(cwd)
        .into_iter()
        .map(|spec_key| UnreviewedBuild {
            spec_key,
            suspicions: Vec::new(),
        })
        .collect()
}

/// Hold back an install's `ERR_AUBE_IGNORED_BUILDS` in `pending`, so a
/// command that does more after the install — `remove`'s `postuninstall`,
/// the next workspace member — finishes that work and raises it last, as
/// pnpm does (`defer_ignored_builds`). `Ok(None)` means it was held; any
/// other error propagates at once.
pub(crate) fn defer_ignored_builds<T>(
    result: miette::Result<T>,
    pending: &mut Option<miette::Report>,
) -> miette::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(report)
            if report.code().is_some_and(|code| {
                code.to_string() == aube_codes::errors::ERR_AUBE_IGNORED_BUILDS
            }) =>
        {
            *pending = Some(report);
            Ok(None)
        }
        Err(report) => Err(report),
    }
}

/// Report the builds this install skipped because nothing approved them.
/// Under `strictDepBuilds` (the default, as in pnpm 12) that is
/// `ERR_AUBE_IGNORED_BUILDS`; otherwise the `ignored build scripts for
/// ...` warning. Both the full install and the warm-path short-circuit
/// report the same way, so a repeat install keeps failing (or nudging)
/// while a decision is still pending — pnpm's re-raise from
/// `.modules.yaml`. Suspicious-script findings print in both modes, ahead
/// of the error, since they are what the approval decision should weigh.
///
/// pnpm 12 never fails a global install on one (`cli_args/global/install.rs`):
/// it warns, and `add -g --allow-build` is the approval. A global `add` or
/// `update` re-enters the install from inside the global package dir,
/// which is how `cwd` identifies it — checked only once there
/// is something to report, so an ordinary install never resolves the global
/// layout.
pub(super) fn report(
    unreviewed: &[UnreviewedBuild],
    strict: bool,
    cwd: &std::path::Path,
) -> miette::Result<()> {
    if unreviewed.is_empty() {
        return Ok(());
    }
    emit_suspicions(unreviewed);
    let spec_keys: Vec<&str> = unreviewed.iter().map(|b| b.spec_key.as_str()).collect();
    if strict && !crate::commands::global::is_global_install_dir(cwd) {
        return Err(miette::miette!(
            code = aube_codes::errors::ERR_AUBE_IGNORED_BUILDS,
            help = format!(
                "Run `{}` to pick which dependencies may run their build scripts, or set `strictDepBuilds=false` to install without them.",
                aube_util::cmd("approve-builds")
            ),
            "Ignored build scripts: {}",
            spec_keys.join(", ")
        ));
    }
    // Cap the inline list so a napi-rs / prebuilt-variants tree
    // (tens of per-platform binding packages) doesn't splat into
    // one hard-to-scan line. Users who want the full list run
    // `aube ignored-builds`.
    const MAX_INLINE: usize = 5;
    let list = if spec_keys.len() <= MAX_INLINE {
        spec_keys.join(", ")
    } else {
        format!(
            "{}, and {} more",
            spec_keys[..MAX_INLINE].join(", "),
            spec_keys.len() - MAX_INLINE
        )
    };
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_IGNORED_BUILD_SCRIPTS,
        count = unreviewed.len(),
        packages = ?spec_keys,
        "ignored build scripts for {} package(s): {}. Run `{}` to review and enable them.",
        unreviewed.len(),
        list,
        aube_util::cmd("approve-builds")
    );
    Ok(())
}

fn emit_suspicions(unreviewed: &[UnreviewedBuild]) {
    for build in unreviewed {
        if build.suspicions.is_empty() {
            continue;
        }
        let detail: Vec<String> = build
            .suspicions
            .iter()
            .map(|s| format!("{}: {}", s.hook, s.kind.description()))
            .collect();
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_SUSPICIOUS_LIFECYCLE_SCRIPT,
            package = build.spec_key.as_str(),
            findings = ?detail,
            "suspicious lifecycle script in {}: {}. Inspect the script in `node_modules/.aube/<dep_path>/node_modules/<name>/package.json` before approving the build.",
            build.spec_key,
            detail.join("; ")
        );
    }
}
