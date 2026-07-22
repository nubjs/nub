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
pub mod spawn;
pub mod version;
