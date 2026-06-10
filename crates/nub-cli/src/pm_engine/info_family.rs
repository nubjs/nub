//! Info family — read-only project/graph/registry queries through the
//! embedded aube engine: `list` (+`ls`, and the hidden long forms
//! `la`/`ll`), `why`, `outdated`, `audit`, `licenses`, `deprecations`,
//! `peers`, `query`, `check`, `bin`, `root`, `sbom`, `view`
//! (+`info`/`show`/`v`), and the hidden npm-fallback `search`. Registered in
//! [`super::ENGINE_VERBS`]; every verb is a stub pending the Surface phase.
//!
//! Filling in a verb means: parse `args` with the spec's aube args type
//! (`clap::Parser::parse_from`, see `aube_args` in the registry), build an
//! [`super::EngineSession`], call the corresponding `aube::commands::*::run`
//! on `session.runtime`, and route every failure through
//! `super::present::emit_report` (stdout is the data channel for these
//! verbs — `list`/`view`/`root` print results to stdout exactly as the
//! engine does; only diagnostics flow through the rewrite). Verbs taking a
//! workspace filter (`list`, `outdated`, `query`, `why`) also need the
//! effective-filter plumbing aube derives from `-r`/`--filter`.

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
