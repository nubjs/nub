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

/// Expand the root manifest's `workspaces` globs against the on-disk
/// tree, returning each member's project-relative directory (POSIX
/// `/`-separated, the importer-key form) that contains a `package.json`.
/// Mirrors npm/yarn workspace globbing: a `packages/*` pattern matches
/// direct child directories; an explicit `packages/app` matches that one
/// directory. Shared by both the classic and berry readers to reconstruct
/// member importers from disk (a berry yarn.lock records members but merges
/// their dep types; classic records no member structure at all).
pub(super) fn discover_workspace_members(project_dir: &Path, patterns: &[String]) -> Vec<String> {
    let mut members: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for pattern in patterns {
        // Negation patterns (`!packages/excluded`) and the root itself
        // aren't member sources here.
        if pattern.starts_with('!') || pattern == "." {
            continue;
        }
        let glob_pat = project_dir.join(pattern);
        let Some(glob_str) = glob_pat.to_str() else {
            continue;
        };
        let Ok(paths) = glob::glob(glob_str) else {
            continue;
        };
        for entry in paths.flatten() {
            if !entry.is_dir() || !entry.join("package.json").is_file() {
                continue;
            }
            let Ok(rel) = entry.strip_prefix(project_dir) else {
                continue;
            };
            // Importer keys are POSIX-relative (`packages/app`), never
            // the host's `\`-separated form.
            let rel_posix = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if !rel_posix.is_empty() {
                members.insert(rel_posix);
            }
        }
    }
    members.into_iter().collect()
}

/// Does `version` satisfy the semver `range`? An empty/whitespace range
/// means "any" (npm/yarn/pnpm treat it as `*`; `node_semver` rejects it).
/// Mirrors the resolver's `version_satisfies` for the workspace-link
/// match — a bare uncached parse, since the workspace-sibling path runs
/// only over the handful of member-to-member deps, not a resolver hot loop.
pub(super) fn version_satisfies(version: &str, range: &str) -> bool {
    let range = if range.trim().is_empty() { "*" } else { range };
    let (Ok(v), Ok(r)) = (
        node_semver::Version::parse(version),
        aube_semver::parse_range(range),
    ) else {
        return false;
    };
    r.satisfies_node(&v)
}

#[cfg(test)]
mod tests;
