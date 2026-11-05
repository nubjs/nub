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
mod threadpool;
mod transform;
mod tsconfig;

use napi_derive::napi;

pub use cache::transform_cached;
pub use detect::detect_module_info;
pub use resolve::resolve_ts;
pub use threadpool::on_first_loop_turn;
pub use threadpool::warm_threadpool;
pub use transform::transform;
pub use tsconfig::load_tsconfig;

/// The data-format parsers live in `nub-data-formats` so this addon and
/// `nub compile` share ONE implementation. The compiler inlines a data import
/// into the bundle at build time; if it parsed with its own copy, a document
/// could mean one thing run and another compiled. Each returns the JavaScript
/// expression the module's default export is, not a value: a NaN, a `-0`, a
/// `BigInt`, a `Date` and an own `__proto__` key all survive as source text,
/// and the preload emits it verbatim.
#[napi]
pub fn yaml_to_js(source: String) -> napi::Result<String> {
    data_to_js(nub_data_formats::parse_yaml(&source))
}

#[napi]
pub fn toml_to_js(source: String) -> napi::Result<String> {
    data_to_js(nub_data_formats::parse_toml(&source))
}

#[napi]
pub fn json5_to_js(source: String) -> napi::Result<String> {
    data_to_js(nub_data_formats::parse_json5(&source))
}

#[napi]
pub fn jsonc_to_js(source: String) -> napi::Result<String> {
    data_to_js(nub_data_formats::parse_jsonc(&source))
}

fn data_to_js(parsed: Result<nub_data_formats::Value, String>) -> napi::Result<String> {
    parsed
        .map(|value| value.to_default_export())
        .map_err(napi::Error::from_reason)
}
