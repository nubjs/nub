/// Dep-type filter derived from `--prod` / `--dev` on list-style commands
/// (`list`, `why`). Both commands take the same two flags with the same
/// semantics — this enum is the shared derivation.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DepFilter {
    /// Include every dep type.
    All,
    /// `--prod`: include `Production` and `Optional`, drop `Dev`.
    ProdOnly,
    /// `--dev`: include only `Dev`.
    DevOnly,
}

impl DepFilter {
    /// Collapse the two mutually-exclusive boolean flags into a filter.
    /// `(true, _)` wins because clap enforces `conflicts_with = "dev"`.
    pub(crate) fn from_flags(prod: bool, dev: bool) -> Self {
        match (prod, dev) {
            (true, _) => Self::ProdOnly,
            (_, true) => Self::DevOnly,
            _ => Self::All,
        }
    }

    /// Does this filter keep the given dep type?
    pub(crate) fn keeps(self, dep_type: aube_lockfile::DepType) -> bool {
        use aube_lockfile::DepType;
        matches!(
            (self, dep_type),
            (Self::All, _)
                | (Self::ProdOnly, DepType::Production | DepType::Optional)
                | (Self::DevOnly, DepType::Dev)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::DepType;

    #[test]
    fn all_keeps_everything() {
        let f = DepFilter::from_flags(false, false);
        assert!(f.keeps(DepType::Production));
        assert!(f.keeps(DepType::Dev));
        assert!(f.keeps(DepType::Optional));
    }

    #[test]
    fn prod_keeps_production_and_optional() {
        let f = DepFilter::from_flags(true, false);
        assert!(f.keeps(DepType::Production));
        assert!(f.keeps(DepType::Optional));
        assert!(!f.keeps(DepType::Dev));
    }

    #[test]
    fn dev_keeps_only_dev() {
        let f = DepFilter::from_flags(false, true);
        assert!(!f.keeps(DepType::Production));
        assert!(!f.keeps(DepType::Optional));
        assert!(f.keeps(DepType::Dev));
    }

    #[test]
    fn prod_wins_over_dev_when_both_set() {
        // clap should prevent this combination via conflicts_with, but we
        // still want deterministic behavior if it ever gets through.
        let f = DepFilter::from_flags(true, true);
        assert!(f.keeps(DepType::Production));
        assert!(!f.keeps(DepType::Dev));
    }
}
