/// How `aube install` should treat an existing lockfile relative to the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrozenMode {
    /// Hard-fail if the lockfile drifts from the manifest. Default in CI.
    Frozen,
    /// Use the lockfile when it's fresh, re-resolve when it's stale. Default outside CI.
    Prefer,
    /// Always re-resolve, never trust the lockfile.
    No,
    /// Re-resolve, but seed the resolver with the existing lockfile so
    /// unchanged specs keep their pinned versions and only drifted
    /// entries get re-resolved. Corresponds to `--fix-lockfile`.
    Fix,
}

/// CLI override for `--frozen-lockfile` / `--no-frozen-lockfile` /
/// `--prefer-frozen-lockfile`. These three flags are mutually
/// exclusive (clap enforces this), so at most one state is reachable
/// — `None` on the enclosing `Option` means none was supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrozenOverride {
    Frozen,
    No,
    Prefer,
}

impl FrozenOverride {
    /// The long-form flag name that produced this override, for user-facing messages.
    pub fn cli_flag(self) -> &'static str {
        match self {
            Self::Frozen => "--frozen-lockfile",
            Self::No => "--no-frozen-lockfile",
            Self::Prefer => "--prefer-frozen-lockfile",
        }
    }

    /// `(setting_name, "true"|"false")` entry to thread this override
    /// into the `ResolveCtx::cli` bag. `--no-frozen-lockfile` is the
    /// `frozen-lockfile=false` side of the same setting.
    pub fn cli_flag_bag_entry(self) -> (&'static str, &'static str) {
        match self {
            Self::Frozen => ("frozen-lockfile", "true"),
            Self::No => ("frozen-lockfile", "false"),
            Self::Prefer => ("prefer-frozen-lockfile", "true"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GlobalVirtualStoreFlags {
    pub enable: bool,
    pub disable: bool,
}

impl GlobalVirtualStoreFlags {
    /// Serialize the two global flags into CLI bag entries for the
    /// `enableGlobalVirtualStore` setting. The bag's *value* is what the
    /// bool setting should resolve to — `bool_from_cli` reads the raw
    /// string as-is without inverting on flag name. Both
    /// `enable-global-virtual-store` and `disable-global-virtual-store`
    /// appear in `settings.toml`'s `sources.cli` for the same setting,
    /// so pushing either key with the appropriate value resolves it:
    /// `--enable-...` ⇒ `true`, `--disable-...` ⇒ `false`.
    pub fn to_cli_flag_bag(self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if self.enable {
            out.push((
                "enable-global-virtual-store".to_string(),
                "true".to_string(),
            ));
        }
        if self.disable {
            out.push((
                "disable-global-virtual-store".to_string(),
                "false".to_string(),
            ));
        }
        out
    }

    pub fn is_set(self) -> bool {
        self.enable || self.disable
    }
}

impl FrozenMode {
    /// Resolve the user's flag combination to a single mode. If no CLI
    /// override is given, honor `preferFrozenLockfile` from the
    /// workspace config; otherwise fall back to the env-aware default.
    pub fn from_override(cli: Option<FrozenOverride>, yaml_prefer_frozen: Option<bool>) -> Self {
        match cli {
            Some(FrozenOverride::Frozen) => Self::Frozen,
            Some(FrozenOverride::No) => Self::No,
            Some(FrozenOverride::Prefer) => Self::Prefer,
            None => match yaml_prefer_frozen {
                Some(true) => Self::Prefer,
                Some(false) => Self::No,
                None => Self::default_for_env(),
            },
        }
    }

    /// pnpm's default: `frozen-lockfile=true` in CI, `prefer-frozen-lockfile=true` otherwise.
    fn default_for_env() -> Self {
        if aube_util::env::is_ci() {
            Self::Frozen
        } else {
            Self::Prefer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_frozen_beats_yaml() {
        let m = FrozenMode::from_override(Some(FrozenOverride::Frozen), Some(false));
        assert!(matches!(m, FrozenMode::Frozen));
    }

    #[test]
    fn yaml_prefer_true_maps_to_prefer() {
        let m = FrozenMode::from_override(None, Some(true));
        assert!(matches!(m, FrozenMode::Prefer));
    }

    #[test]
    fn yaml_prefer_false_maps_to_no() {
        let m = FrozenMode::from_override(None, Some(false));
        assert!(matches!(m, FrozenMode::No));
    }
}
