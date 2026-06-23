use crate::{GitSource, HostedGit, HostedGitHost, LocalSource, LockedPackage, RemoteTarballSource};
use std::path::PathBuf;

/// True when a package's local source is a git dependency in *either*
/// representation: a plain git-clone source (`LocalSource::Git`) or a
/// host-served codeload archive (`RemoteTarball { git_hosted: true }`).
/// Upstream #857 reclassified github/gitlab/bitbucket-shorthand git
/// deps to the codeload-tarball form, so the npm writer's git
/// special-casing must recognize both — a plain (non-git) remote
/// tarball stays excluded.
pub(crate) fn is_git_local_source(src: Option<&LocalSource>) -> bool {
    matches!(
        src,
        Some(LocalSource::Git(_))
            | Some(LocalSource::RemoteTarball(RemoteTarballSource {
                git_hosted: true,
                ..
            }))
    )
}

/// Recover `(HostedGit, sha)` from a codeload-style archive URL that a
/// hosted git dep resolves to — the inverse of [`HostedGit::tarball_url`].
/// Returns `None` for any URL that isn't a recognized host archive
/// pinned to a 40-char commit SHA, so non-git remote tarballs and
/// unpinned archives fall through untouched.
fn hosted_git_from_codeload(url: &str) -> Option<(HostedGit, String)> {
    let valid_sha = |s: &str| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit());
    let hosted = |host, owner: &str, repo: &str, sha: &str| {
        valid_sha(sha).then(|| {
            (
                HostedGit {
                    host,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                },
                sha.to_ascii_lowercase(),
            )
        })
    };

    // GitHub: https://codeload.github.com/<owner>/<repo>/tar.gz/<sha>
    if let Some(rest) = url.strip_prefix("https://codeload.github.com/") {
        return match rest.split('/').collect::<Vec<_>>().as_slice() {
            [owner, repo, "tar.gz", sha] => hosted(HostedGitHost::GitHub, owner, repo, sha),
            _ => None,
        };
    }

    // GitLab: https://gitlab.com/<owner>/<repo>/-/archive/<sha>/<repo>-<sha>.tar.gz
    if let Some(rest) = url.strip_prefix("https://gitlab.com/")
        && let Some((before, after)) = rest.split_once("/-/archive/")
        && let Some((owner, repo)) = before.split_once('/')
        && let Some((sha, _)) = after.split_once('/')
    {
        return hosted(HostedGitHost::GitLab, owner, repo, sha);
    }

    // Bitbucket: https://bitbucket.org/<owner>/<repo>/get/<sha>.tar.gz
    if let Some(rest) = url.strip_prefix("https://bitbucket.org/")
        && let Some((before, tail)) = rest.split_once("/get/")
        && let Some((owner, repo)) = before.split_once('/')
        && let Some(sha) = tail.strip_suffix(".tar.gz")
    {
        return hosted(HostedGitHost::Bitbucket, owner, repo, sha);
    }

    None
}

pub(super) fn local_git_source_from_resolved(resolved: &str) -> Option<LocalSource> {
    let (url, committish, subpath) = crate::parse_git_spec(resolved)?;
    let resolved = committish.clone()?;
    Some(LocalSource::Git(GitSource {
        url,
        committish,
        resolved,
        integrity: None,
        subpath,
    }))
}

/// Convert a `file:<path>` value in a non-`link:true` entry's
/// `resolved` field to the matching local source. npm writes this
/// shape for `npm install file:../foo-1.0.0.tgz` (local tarballs)
/// and for some directory deps that pre-date the modern `link: true`
/// emission. Without recognizing it, the entry parses as a plain
/// registry package; lockfile-reuse then matches by name+version and
/// the fetcher 404s on the literal package name.
///
/// Tarball vs. Directory is decided purely by the `.tgz`/`.tar.gz`
/// suffix: the lockfile path is authoritative, and we don't have the
/// project root here to stat the target. False classification is
/// recoverable on the next install — `LocalSource::parse` from the
/// manifest re-runs the FS-aware check.
pub(super) fn local_file_source_from_resolved(resolved: &str) -> Option<LocalSource> {
    let rest = resolved.strip_prefix("file:")?;
    let path = PathBuf::from(rest);
    if LocalSource::path_looks_like_tarball(&path) {
        Some(LocalSource::Tarball(path))
    } else {
        Some(LocalSource::Directory(path))
    }
}

pub(super) fn npm_resolved_field(pkg: &LockedPackage) -> Option<String> {
    // A host-served git dep (#857) resolves through a codeload archive,
    // so `tarball_url` holds that codeload URL — but npm's canonical
    // `resolved` for a hosted git dep is the provider sshurl, NOT the
    // archive URL. Emit the canonical git form here (and ahead of the
    // `tarball_url` fallback) so `npm ci` accepts the line and a
    // follow-up `npm install` doesn't churn it.
    if let Some(LocalSource::RemoteTarball(RemoteTarballSource {
        url,
        git_hosted: true,
        ..
    })) = &pkg.local_source
        && let Some((hosted, sha)) = hosted_git_from_codeload(url)
    {
        return Some(format!("git+{}#{sha}", hosted.ssh_url()));
    }
    pkg.tarball_url.clone().or_else(|| match &pkg.local_source {
        Some(LocalSource::Git(git)) => {
            // npm canonicalizes hosted git deps to the provider's
            // sshurl form (`git+ssh://git@github.com/owner/repo.git`)
            // no matter what protocol the spec used — `github:`
            // shorthand and `git+https://` specs both land that way
            // (verified against npm 11.13.0). Anything else churns
            // the line on the next `npm install`. Non-hosted URLs
            // keep their stored form behind the `git+` tag.
            let url = if let Some(hosted) = crate::parse_hosted_git(&git.url) {
                format!("git+{}", hosted.ssh_url())
            } else if git.url.starts_with("git://") || git.url.starts_with("git+") {
                git.url.clone()
            } else {
                format!("git+{}", git.url)
            };
            match &git.subpath {
                Some(subpath) => Some(format!("{url}#{}&path:/{subpath}", git.resolved)),
                None => Some(format!("{url}#{}", git.resolved)),
            }
        }
        _ => None,
    })
}
