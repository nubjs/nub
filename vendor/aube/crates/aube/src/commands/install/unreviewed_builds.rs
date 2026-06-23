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

/// Emit the `ignored build scripts for ...` warning. Same wording
/// fires from the full install path and the warm-path short-circuit
/// so users see the nudge on every repeat install while
/// `allowBuilds` placeholders are still pending review.
pub(super) fn emit_warning(unreviewed: &[UnreviewedBuild]) {
    if unreviewed.is_empty() {
        return;
    }
    // Cap the inline list so a napi-rs / prebuilt-variants tree
    // (tens of per-platform binding packages) doesn't splat into
    // one hard-to-scan line. Users who want the full list run
    // `aube ignored-builds`.
    const MAX_INLINE: usize = 5;
    let spec_keys: Vec<&str> = unreviewed.iter().map(|b| b.spec_key.as_str()).collect();
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
        "ignored build scripts for {} package(s): {}. Run `{}` to review and enable them, or set `strictDepBuilds=true` to fail installs that have unreviewed builds.",
        unreviewed.len(),
        list,
        aube_util::cmd("approve-builds")
    );
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
