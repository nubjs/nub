//! Centralized registry of aube's CLI/config settings.
//!
//! Every setting aube honors lives in the workspace-root
//! `settings.toml`: its name, type, default, implementation status,
//! and the source surfaces (CLI flag, env var, `.npmrc`,
//! `pnpm-workspace.yaml`) that can populate it. `build.rs` turns
//! that TOML file into two generated artifacts:
//!
//! - [`meta::SETTINGS`] — a `&'static [SettingMeta]` slice so other
//!   crates can introspect the full settings surface (for
//!   `aube config`, docs generation, parity audits).
//! - [`values::resolved`] — one typed Rust function per supported
//!   scalar setting. The
//!   function signature *is* the type check — `auto_install_peers`
//!   returns `Option<bool>`, `store_dir` returns `Option<String>`,
//!   and calling either on the wrong type is a compile error.
//!
//! Downstream crates depend on this one so they never have to
//! hand-maintain a getter whose spelling can drift from
//! `settings.toml`.

pub mod meta;
pub mod values;

pub use meta::{SettingMeta, all, find};
pub use values::{
    ResolveCtx, embedder_defaults, parse_bool, resolved, set_embedder_defaults,
    set_global_cli_overrides, workspace_yaml_value,
};
