//! Nub N-API addon: data-format parsers + the in-process TS/JSX transpiler,
//! exposed to the JS preload.
//!
//! The parser functions take a source string and return a parsed value as a JS
//! object (via napi's serde-json bridge). The [`transform`](transform::transform)
//! function transpiles TS/JSX, mirroring `oxc-transform@0.140.0`'s `transformSync`
//! for byte-for-byte emit parity.

// `collapsible_if` fires on intentional nested `if let { if let }` sites;
// collapsing every site is cosmetic churn (and tsconfig.rs is a verbatim
// get-tsconfig mirror), so allow it.
#![allow(clippy::collapsible_if)]

mod cache;
mod detect;
mod resolve;
mod transform;
mod tsconfig;

use napi_derive::napi;

pub use cache::transform_cached;
pub use detect::detect_module_info;
pub use resolve::resolve_ts;
pub use transform::transform;
pub use tsconfig::load_tsconfig;

/// The data-format parsers live in `nub-data-formats` so this addon and
/// `nub compile` share ONE implementation. The compiler inlines a data import
/// into the bundle at build time; if it parsed with its own copy, a document
/// could mean one thing run and another compiled. These wrappers exist only to
/// put the shared `String` error behind napi's error type.
#[napi]
pub fn parse_yaml(source: String) -> napi::Result<serde_json::Value> {
    nub_data_formats::parse_yaml(&source).map_err(napi::Error::from_reason)
}

/// Parse TOML source into a JS value.
#[napi]
pub fn parse_toml(source: String) -> napi::Result<serde_json::Value> {
    nub_data_formats::parse_toml(&source).map_err(napi::Error::from_reason)
}

/// Parse JSON5 source into a JS value.
#[napi]
pub fn parse_json5(source: String) -> napi::Result<serde_json::Value> {
    nub_data_formats::parse_json5(&source).map_err(napi::Error::from_reason)
}

/// Parse JSONC (JSON with comments) source into a JS value.
#[napi]
pub fn parse_jsonc(source: String) -> napi::Result<serde_json::Value> {
    nub_data_formats::parse_jsonc(&source).map_err(napi::Error::from_reason)
}
