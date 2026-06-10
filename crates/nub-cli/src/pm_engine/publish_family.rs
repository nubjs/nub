//! Publish family — registry writes, packaging, and auth through the
//! embedded aube engine: `publish`, `pack`, `version`, `deprecate`,
//! `undeprecate`, `dist-tag` (+`dist-tags`), `unpublish`, `login`
//! (+`adduser`), `logout`, and the hidden npm-fallbacks `whoami`, `owner`,
//! `token`, `stage`. Registered in [`super::ENGINE_VERBS`]; every verb is a
//! stub pending the Surface phase.
//!
//! Filling in a verb means: parse `args` with the spec's aube args type
//! (`clap::Parser::parse_from`, see `aube_args` in the registry), build an
//! [`super::EngineSession`], call the corresponding `aube::commands::*::run`
//! on `session.runtime`, and route failures through
//! `super::present::emit_report`. Family-specific cautions for the fill-in:
//! `login`/`logout` mutate the user's `~/.npmrc` (auth tokens — honor it,
//! never hardcode registries); the npm-fallback verbs spawn `npm` and
//! return its exit code (`commands::npm_fallback::run` is synchronous, no
//! session needed); `publish`/`pack` consult the workspace filter.

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
