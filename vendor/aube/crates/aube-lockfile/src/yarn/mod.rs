//! Parser for yarn.lock, covering both classic (v1) and berry (v2+).
//!
//! ## Classic (v1)
//!
//! Line-based, similar to YAML but not quite:
//!
//! ```text
//! # comment
//! "@scope/pkg@^1.0.0", "@scope/pkg@^1.1.0":
//!   version "1.2.3"
//!   resolved "https://..."
//!   integrity sha512-...
//!   dependencies:
//!     other-pkg "^2.0.0"
//! ```
//!
//! Top-level blocks are keyed by one or more comma-separated specifiers
//! (`name@range`). The body is indented 2 spaces. Nested sections like
//! `dependencies:` add another 2 spaces of indentation.
//!
//! ## Berry (v2+)
//!
//! Proper YAML with a `__metadata:` header and per-block
//! `resolution:` / `checksum:` / `languageName` / `linkType` fields:
//!
//! ```yaml
//! __metadata:
//!   version: 8
//!   cacheKey: 10c0
//!
//! "@scope/pkg@npm:^1.0.0, @scope/pkg@npm:^1.1.0":
//!   version: 1.1.0
//!   resolution: "@scope/pkg@npm:1.1.0"
//!   dependencies:
//!     foo: "npm:^2.0.0"
//!   checksum: 10c0/aabbcc...
//!   languageName: node
//!   linkType: hard
//! ```
//!
//! Multi-spec headers are serialized as a single YAML string containing
//! `", "`-separated specifiers. Values carry a protocol prefix: `npm:`
//! for registry packages (the common case), `workspace:` for monorepo
//! refs, `file:` / `link:` / `portal:` for local paths, `patch:` for
//! patched packages, and full URLs for `git:` / `http(s):` sources.
//!
//! yarn.lock does not distinguish direct deps from transitive ones, so we
//! cross-reference specifiers against the project's package.json to populate
//! `importers["."]`.

mod berry;
mod classic;

use crate::{Error, LockfileGraph};
use std::path::Path;

pub use berry::write_berry;
pub use classic::write_classic;

/// Parse a yarn.lock file into a LockfileGraph, dispatching between
/// classic v1 and berry v2+ based on content.
///
/// The manifest is needed to identify direct dependencies (yarn.lock has
/// no notion of direct vs transitive).
pub fn parse(path: &Path, manifest: &aube_manifest::PackageJson) -> Result<LockfileGraph, Error> {
    let content = crate::read_lockfile(path)?;
    if is_berry(&content) {
        berry::parse_berry_str(path, &content, manifest)
    } else {
        classic::parse_classic_str(path, &content, manifest)
    }
}

/// True when `content` looks like a yarn berry (v2+) lockfile.
///
/// Detection is content-based because both classic and berry live in the
/// same `yarn.lock` filename. Berry always emits a top-level
/// `__metadata:` mapping (it's what yarn's own cache-key bookkeeping
/// reads), so its presence is a reliable marker.
pub fn is_berry(content: &str) -> bool {
    content
        .lines()
        .any(|l| l.trim_start().starts_with("__metadata:"))
}

/// Like [`is_berry`], but reads from disk. Returns `false` on IO
/// errors (including "file doesn't exist") so callers that branch on
/// the result can fall through to the classic path or skip the file
/// entirely without an extra error branch.
///
/// Reads only a 4 KiB prefix rather than the full file. Berry's
/// `__metadata:` header always appears in the first couple of lines
/// (yarn emits the two-line comment banner then the mapping
/// directly), so scanning more than that wastes I/O — `parse_one`
/// calls `yarn::parse` immediately after, which reads the file
/// fully, so keeping the detect cheap avoids doubling the cost for
/// monorepo-scale lockfiles.
///
/// Byte-level scan: `__metadata:` is pure ASCII so matching raw
/// bytes is safe even if the 4 KiB window happens to cut a
/// multi-byte UTF-8 sequence mid-character (a non-concern for yarn's
/// own output, but cheap insurance against future format tweaks).
pub fn is_berry_path(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4096];
    let n = f.read(&mut buf).unwrap_or(0);
    let needle = b"__metadata:";
    // Must appear at the start of a line: either the file head or
    // directly after a newline. A preceding `#` comment line is fine
    // because the newline before `__metadata` is what matters.
    buf[..n]
        .windows(needle.len())
        .enumerate()
        .any(|(i, w)| w == needle && (i == 0 || buf[i - 1] == b'\n'))
}

#[cfg(test)]
mod tests;
