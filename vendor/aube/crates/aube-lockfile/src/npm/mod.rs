//! Reader and writer for npm's package-lock.json (v2/v3) and npm-shrinkwrap.json.
//!
//! The v2/v3 format uses a flat `packages` map keyed by install path:
//! - `""` is the root project
//! - `"node_modules/foo"` is a top-level dep
//! - `"node_modules/foo/node_modules/bar"` is a nested dep
//!
//! Each entry carries `version`, `integrity`, `dependencies`, `dev`,
//! `optional`, etc. On read, we flatten into one `LockedPackage` per
//! unique `(name, version)` pair, discarding the nesting (aube uses a
//! hoisted virtual store layout). On write, we walk the flat graph and
//! rebuild a hoist + nest layout so consumers (npm, aube's own parser)
//! get a valid v3 package-lock.json back.
//!
//! v1 lockfiles (npm 5-6, uses nested `dependencies` tree) are rejected.

mod layout;
mod raw;
mod read;
mod source;
mod write;

pub(crate) use layout::{
    build_hoist_tree, canonical_key_from_dep_path, child_canonical_key, dep_path_tail,
    dep_value_as_version, segments_to_install_path,
};
pub use read::parse;
pub use write::write;

#[cfg(test)]
mod tests;
