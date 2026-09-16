//! The closed preset table. A `"sandbox": "<preset>"` string opts into a
//! nub-implemented named policy set. The resolver is a CLOSED table — an unknown
//! preset is a hard error naming the supported set (same discipline as the env
//! type grammar), so adding a preset later is non-breaking.
//!
//! A preset expands to the equivalent granular surface `Value`, which the pipeline
//! then folds — one code path, no separate preset→IR translator to keep in sync.
//!
//! **THE TABLE IS EMPTY.** `build-jail` was its only entry and went with the build
//! jail. The slot stays because the grammar still accepts a preset name: keeping it
//! means `--sandbox buildjail` reports that no preset is supported, where deleting it
//! would push every bare word down the file-ref arm and report a missing file.

use super::CompileError;
use serde_json::Value;

/// Resolve a preset name to its granular surface object.
///
/// Every name is an error today, naming the empty supported set. A future preset is
/// one arm of the `match` this replaced.
pub fn resolve(name: &str) -> Result<Value, CompileError> {
    Err(CompileError::unknown_preset(name, &[]))
}
