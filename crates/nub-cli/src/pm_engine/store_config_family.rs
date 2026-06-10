//! Store/config family — global-store and cache forensics plus settings
//! through the embedded aube engine: `store`, `cache`, `cat-file`,
//! `cat-index`, `find-hash`, `config` (+`c`, and the hidden `get`/`set`
//! shorthands), and the hidden npm-fallbacks `pkg`, `set-script`.
//! Registered in [`super::ENGINE_VERBS`]; every verb is a stub pending the
//! Surface phase.
//!
//! Filling in a verb means: parse `args` with the spec's aube args type
//! (`clap::Parser::parse_from`, see `aube_args` in the registry), build an
//! [`super::EngineSession`], call the corresponding `aube::commands::*::run`
//! on `session.runtime`, and route failures through
//! `super::present::emit_report`. Family-specific cautions for the fill-in:
//! `store path` must print the *resolved* store dir — under nub's embedder
//! defaults that is `$XDG_DATA_HOME/nub/store/v1`, and the printed path is
//! data on stdout, not a diagnostic (no rewrite needed; it's already
//! nub-named via the `storeDir` default); `config` reads/writes `.npmrc`
//! verbatim (never inject nub-only keys); `get`/`set` parse
//! `commands::config::{GetArgs,SetArgs}` rather than `ConfigArgs`.

use anyhow::Result;

use super::{VerbSpec, stub_error};

/// Stub dispatcher — see the module doc for the fill-in recipe.
pub(crate) fn run_verb(
    _spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    Err(stub_error(typed, args, pm_hint))
}
