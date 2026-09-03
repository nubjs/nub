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

// This addon must NEVER link the MSVC C runtime statically. It is a cdylib that Node
// loads into its own process, so a static CRT gives it a private heap and a private
// copy of the CRT's global state: anything allocated on one side of the `node.exe`
// boundary and freed on the other becomes heap corruption — silent, late, and nothing
// like the honest link error you would want.
//
// The guard is here because the mistake is easy to make and impossible to see. The
// shipped `nub.exe` and the compiled-artifact launcher DO want `+crt-static` (without
// it they die at 0xC0000135 on a clean Windows box), and the obvious way to arrange
// that — a `+crt-static` in the repo-root `.cargo/config.toml` — reaches this crate
// too. Cargo CONCATENATES rustflags from every config file up the directory tree
// rather than letting the nearest one win, and `+crt-static` is sticky: a later
// `-crt-static` does not undo it, in either order, with no diagnostic. So there is no
// way to opt back out from here, and a build that should have failed would instead
// have shipped. Those binaries get the flag from their own build steps instead.
#[cfg(all(windows, target_feature = "crt-static"))]
compile_error!(
    "nub-native must link the MSVC CRT dynamically: it is a cdylib loaded into node.exe, \
     and a static CRT gives it a private heap (cross-boundary frees corrupt memory). \
     Something put `-C target-feature=+crt-static` in this build — most likely a \
     `.cargo/config.toml` above this crate, whose rustflags cargo concatenates and which \
     cannot be overridden from here. Scope that flag to the nub.exe/launcher build steps."
);

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
