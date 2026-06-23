//! Version-file discovery: `.node-version` and `.nvmrc`, searched
//! upward from the project directory. `devEngines.runtime` is *not*
//! read here — the caller already has parsed manifests and merges it
//! in at higher precedence via [`effective_request`].

use crate::error::Error;
use crate::spec::{NodeRequest, NodeSpec, RequestSource};
use std::path::Path;

/// Walk upward from `start_dir` looking for `.node-version` then
/// `.nvmrc` (same-directory precedence: `.node-version` wins; nearer
/// directory beats farther). The walk stops after checking the home
/// directory (inclusive) or the filesystem root, whichever comes
/// first — predictable, and matches what nvm/fnm users expect in
/// monorepos where the version file sits above the invocation dir.
///
/// A file that exists but doesn't parse as a version request is
/// logged and treated as absent rather than failing the command — a
/// stray `.nvmrc` containing prose shouldn't break `aubr test`.
pub fn find_version_file(start_dir: &Path) -> Option<NodeRequest> {
    let home = aube_util::env::home_dir();
    let mut dir = Some(start_dir);
    while let Some(d) = dir {
        for (file, source) in [
            (".node-version", RequestSource::NodeVersionFile),
            (".nvmrc", RequestSource::Nvmrc),
        ] {
            let path = d.join(file);
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let trimmed = first_meaningful_line(&raw);
            if trimmed.is_empty() {
                continue;
            }
            match NodeSpec::parse(trimmed) {
                Ok(spec) => {
                    return Some(NodeRequest {
                        spec,
                        raw: trimmed.to_string(),
                        // Version files have no onFail vocabulary; the
                        // whole point of writing one is "use this
                        // version", so missing versions download. The
                        // runtimeOnFail setting overrides at the
                        // integration layer.
                        on_fail: aube_manifest::OnFail::Download,
                        source,
                        origin: path,
                    });
                }
                Err(_) => {
                    tracing::warn!(
                        path = %path.display(),
                        content = trimmed,
                        "ignoring unparseable node version file"
                    );
                }
            }
        }
        if home.as_deref() == Some(d) {
            break;
        }
        dir = d.parent();
    }
    None
}

/// First non-empty, non-comment line. nvm tolerates comments
/// (`# comment`) and surrounding whitespace in `.nvmrc`.
fn first_meaningful_line(raw: &str) -> &str {
    raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("")
}

/// Build the effective request for a project: `devEngines.runtime`
/// (passed in pre-parsed by the caller) beats version files.
///
/// `dev_engines` carries the manifest's node runtime entry, if any:
/// `(version range, declared onFail, manifest path)`. An entry without
/// a `version` is treated as "no requirement".
pub fn effective_request(
    dev_engines: Option<(&str, Option<aube_manifest::OnFail>, &Path)>,
    start_dir: &Path,
) -> Result<Option<NodeRequest>, Error> {
    if let Some((range, on_fail, manifest_path)) = dev_engines {
        let spec = NodeSpec::parse(range)?;
        return Ok(Some(NodeRequest {
            spec,
            raw: range.to_string(),
            // The OpenJS spec defaults a missing onFail to `error`.
            on_fail: on_fail.unwrap_or(aube_manifest::OnFail::Error),
            source: RequestSource::DevEngines,
            origin: manifest_path.to_path_buf(),
        }));
    }
    Ok(find_version_file(start_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nvmrc_in_parent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nvmrc"), "v22.1.0\n").unwrap();
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let req = find_version_file(&nested).unwrap();
        assert_eq!(req.source, RequestSource::Nvmrc);
        assert_eq!(req.spec, NodeSpec::Exact("22.1.0".parse().unwrap()));
        assert_eq!(req.on_fail, aube_manifest::OnFail::Download);
    }

    #[test]
    fn node_version_beats_nvmrc_in_same_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nvmrc"), "20").unwrap();
        std::fs::write(tmp.path().join(".node-version"), "22").unwrap();
        let req = find_version_file(tmp.path()).unwrap();
        assert_eq!(req.source, RequestSource::NodeVersionFile);
    }

    #[test]
    fn nearer_nvmrc_beats_farther_node_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".node-version"), "20").unwrap();
        let nested = tmp.path().join("proj");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".nvmrc"), "22").unwrap();
        let req = find_version_file(&nested).unwrap();
        assert_eq!(req.source, RequestSource::Nvmrc);
    }

    #[test]
    fn unparseable_file_is_skipped_and_walk_continues() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nvmrc"), "22").unwrap();
        let nested = tmp.path().join("proj");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".nvmrc"), "definitely not a version !!!").unwrap();
        let req = find_version_file(&nested).unwrap();
        assert_eq!(req.origin, tmp.path().join(".nvmrc"));
    }

    #[test]
    fn comments_and_blank_lines_are_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".nvmrc"),
            "# pinned for CI\n\n  lts/jod  \n",
        )
        .unwrap();
        let req = find_version_file(tmp.path()).unwrap();
        assert_eq!(req.spec, NodeSpec::LtsCodename("jod".into()));
    }

    #[test]
    fn no_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        // The walk can escape the tempdir upward; only assert when the
        // ancestor chain is clean of version files (always true on CI,
        // usually true locally — gate on it instead of flaking).
        let mut ancestor_has_file = false;
        let mut d = tmp.path().parent();
        while let Some(p) = d {
            if p.join(".nvmrc").exists() || p.join(".node-version").exists() {
                ancestor_has_file = true;
                break;
            }
            d = p.parent();
        }
        if !ancestor_has_file {
            assert!(find_version_file(tmp.path()).is_none());
        }
    }

    #[test]
    fn dev_engines_beats_version_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nvmrc"), "20").unwrap();
        let manifest = tmp.path().join("package.json");
        let req = effective_request(Some(("^22", None, manifest.as_path())), tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(req.source, RequestSource::DevEngines);
        // Spec default: missing onFail means error.
        assert_eq!(req.on_fail, aube_manifest::OnFail::Error);
    }

    #[test]
    fn dev_engines_on_fail_is_honored() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("package.json");
        let req = effective_request(
            Some((
                "^22",
                Some(aube_manifest::OnFail::Download),
                manifest.as_path(),
            )),
            tmp.path(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(req.on_fail, aube_manifest::OnFail::Download);
    }
}
