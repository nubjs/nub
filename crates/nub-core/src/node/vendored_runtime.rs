//! The npm packages the runtime imports from its own `node_modules/`.
//!
//! One list for two readers: `build.rs` refuses to embed a staged runtime missing
//! any of them, and `nub compile` names them in a source map by their place in
//! the runtime. Keep it in step with release.yml's vendoring step.

pub const VENDORED_RUNTIME_PACKAGES: &[&str] = &[
    "@js-temporal/polyfill",
    "@oxc-project/runtime",
    "@petamoriken/float16",
    "jsbi",
    "urlpattern-polyfill",
];
