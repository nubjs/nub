/// Three resolved OSV-routing settings, fetched in one
/// `with_settings_ctx` pass so the lockfile-found and no-lockfile
/// arms can call the routing helper with the same shape. Paranoid
/// upgrade for `advisoryCheck` is applied here so the router sees
/// the final policy.
pub(super) struct OsvRoutingSettings {
    pub(super) advisory_check: aube_settings::resolved::AdvisoryCheck,
    pub(super) advisory_check_on_install: aube_settings::resolved::AdvisoryCheckOnInstall,
    pub(super) advisory_bloom_check: aube_settings::resolved::AdvisoryBloomCheck,
    pub(super) advisory_check_every_install: bool,
}

pub(super) fn resolve_osv_routing_settings(cwd: &std::path::Path) -> OsvRoutingSettings {
    crate::commands::with_settings_ctx(cwd, |ctx| {
        let advisory_check = if aube_settings::resolved::paranoid(ctx) {
            aube_settings::resolved::AdvisoryCheck::Required
        } else {
            aube_settings::resolved::advisory_check(ctx)
        };
        OsvRoutingSettings {
            advisory_check,
            advisory_check_on_install: aube_settings::resolved::advisory_check_on_install(ctx),
            advisory_bloom_check: aube_settings::resolved::advisory_bloom_check(ctx),
            advisory_check_every_install: aube_settings::resolved::advisory_check_every_install(
                ctx,
            ),
        }
    })
}
