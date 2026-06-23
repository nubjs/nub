pub mod adaptive;
pub mod buf;
pub mod cache;
pub mod collections;
pub mod concurrency;
pub mod diag;
pub mod diag_kernel;
pub mod engine_context;
pub mod env;

// Convenience re-exports of the diagnostics public API so binaries can
// reference `aube_util::DiagConfig` instead of `aube_util::diag::DiagConfig`.
pub use diag::{DiagConfig, Slot, Span, jstr};
pub mod fs;
pub mod fs_atomic;
pub mod hash;
pub mod http;
pub mod identity;
pub mod io;

// Convenience re-exports so consumers can reference `aube_util::Embedder`
// / `aube_util::embedder()` without naming the module.
pub use identity::{
    AUBE, Embedder, cmd, embedder, lockfile_basename, prog, set_embedder, workspace_markers,
};

// Convenience re-exports for the runtime embedder seam (the per-invocation
// counterpart to `Embedder`).
pub use engine_context::{
    EngineContext, engine_context, set_engine_context, update_engine_context,
};
pub mod path;
pub mod pkg;
pub mod snapshot;
pub mod url;

use serde::{Deserialize, Deserializer};

/// Deserialize npm platform fields (`os`, `cpu`, `libc`) from either a
/// string or an array of strings. Missing fields, nulls, and non-string
/// array entries mean "no constraint" and are dropped.
pub fn string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::String(s)) => vec![s],
        Some(serde_json::Value::Array(values)) => values
            .into_iter()
            .filter_map(|value| match value {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        Some(_) => Vec::new(),
    })
}
