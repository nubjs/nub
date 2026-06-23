//! Parser for bun's `bun.lock` (text JSONC format, bun 1.1+).
//!
//! The `bun.lockb` binary format is NOT supported — users should run
//! `bun install --save-text-lockfile` first (or upgrade to bun 1.2+
//! where text is the default).
//!
//! Format overview:
//!
//! ```jsonc
//! {
//!   "lockfileVersion": 1,
//!   "workspaces": {
//!     "": {
//!       "name": "my-app",
//!       "dependencies": { "foo": "^1.0.0" },
//!       "devDependencies": { "bar": "^2.0.0" }
//!     }
//!   },
//!   "packages": {
//!     "foo": ["foo@1.2.3", "", { "dependencies": { "nested": "^3.0.0" } }, "sha512-..."],
//!     "nested": ["nested@3.1.0", "", {}, "sha512-..."]
//!   }
//! }
//! ```
//!
//! Each `packages` entry is a 4-tuple `[ident, resolved_url, metadata, integrity]`,
//! where `ident` is `name@version` and `metadata` may carry transitive
//! `dependencies` / `optionalDependencies`.
//!
//! The file uses JSONC: trailing commas and `//`/`/* */` comments are
//! allowed. We pre-process the content to strip those before handing it
//! to `serde_json`.

mod jsonc;
mod raw;
mod read;
mod source;
mod write;

#[cfg(test)]
mod tests;

pub use read::parse;
pub use write::write;
