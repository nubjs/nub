//! Workspace detection, package.json parsing, script resolution,
//! and workspace filtering.

pub mod detect;
pub mod env;
pub mod filter;
pub mod sandbox_inventory;
pub mod scripts;
pub mod shell_escape;
