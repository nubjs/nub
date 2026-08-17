//! Node.js binary discovery, version detection, flag injection, and
//! process spawning.

pub mod discovery;
pub mod feature_matrix;
pub mod flags;
pub mod shim;
// Single-binary runtime extraction — only compiled in release builds that embed
// the runtime blob (`embed-runtime`). The default dev build resolves `runtime/`
// via the in-repo walk in `spawn::find_preload`, so this module is absent.
#[cfg(feature = "embed-runtime")]
pub(crate) mod runtime_cache;
#[cfg(feature = "embed-runtime")]
pub(crate) mod runtime_tree;

/// Verify compile's extracted runtime tree, self-healing one mismatch from the
/// embedded blob before failing closed.
#[cfg(feature = "embed-runtime")]
pub fn verify_or_heal_embedded_runtime_tree(dir: &std::path::Path) -> bool {
    runtime_cache::verify_or_heal_embedded_runtime_tree(dir)
}
pub mod spawn;
pub mod version;
