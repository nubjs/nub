/// Which dependency sections to include in the materialized
/// `node_modules` tree. Collapses the `--prod` / `--dev` /
/// `--no-optional` CLI flags (clap enforces `--prod` and `--dev`
/// mutually exclusive) into a single, total enum — no invalid combos.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DepSelection {
    /// Everything: prod + dev + optional. Default.
    #[default]
    All,
    /// Prod + dev, skip optional (`--no-optional`).
    NoOptional,
    /// Prod + optional, skip dev (`--prod`).
    Prod,
    /// Prod only (`--prod --no-optional`).
    ProdNoOptional,
    /// Dev only (`--dev`).
    Dev,
    /// Dev only, also skip optionals reachable via dev transitives
    /// (`--dev --no-optional`).
    DevNoOptional,
}

impl DepSelection {
    pub fn from_flags(prod: bool, dev: bool, no_optional: bool) -> Self {
        // clap enforces `prod` and `dev` are mutually exclusive on the
        // install surface; treat the double-true case as `dev` for the
        // benefit of any direct `InstallOptions` construction (tests).
        match (prod, dev, no_optional) {
            (_, true, false) => Self::Dev,
            (_, true, true) => Self::DevNoOptional,
            (true, false, false) => Self::Prod,
            (true, false, true) => Self::ProdNoOptional,
            (false, false, false) => Self::All,
            (false, false, true) => Self::NoOptional,
        }
    }

    /// Only keep `Dev` dep edges.
    pub fn dev_only(self) -> bool {
        matches!(self, Self::Dev | Self::DevNoOptional)
    }

    /// Drop `Dev` dep edges.
    pub fn prod_only(self) -> bool {
        matches!(self, Self::Prod | Self::ProdNoOptional)
    }

    /// Drop `Optional` dep edges.
    pub fn skip_optional(self) -> bool {
        matches!(
            self,
            Self::NoOptional | Self::ProdNoOptional | Self::DevNoOptional
        )
    }

    /// True when any section filter applies and the resolved graph
    /// must be pruned before linking.
    pub fn is_filtered(self) -> bool {
        !matches!(self, Self::All)
    }

    /// True when the install omitted dep sections on the prod/dev
    /// axis. `--no-optional` alone does not count — matches the old
    /// `opts.prod || opts.dev` bool recorded in the state file.
    pub fn prod_or_dev_axis(self) -> bool {
        self.prod_only() || self.dev_only()
    }

    /// Human-readable flag echo for tracing output.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "",
            Self::NoOptional => "--no-optional",
            Self::Prod => "--prod",
            Self::ProdNoOptional => "--prod --no-optional",
            Self::Dev => "--dev",
            Self::DevNoOptional => "--dev --no-optional",
        }
    }
}
