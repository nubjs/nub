use crate::{
    CappedReader, Error, MAX_TARBALL_DECOMPRESSED_BYTES, MAX_TARBALL_ENTRIES,
    MAX_TARBALL_ENTRY_BYTES,
};
use aube_util::url::redact_url;
use std::path::{Path, PathBuf};

/// Render a git argv tail for error messages with any embedded
/// userinfo stripped. A raw `{args:?}` would otherwise dump the
/// full `git+https://<token>@host/repo.git` URL right back into
/// the error string that ships to CI logs.
fn redact_args(args: &[&str]) -> String {
    let mut s = String::from("[");
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('"');
        s.push_str(&redact_url(a));
        s.push('"');
    }
    s.push(']');
    s
}

/// Reject values that would be interpreted by git as an option when
/// handed to a subcommand as a positional argument. Defense against
/// the CVE-2017-1000117 class of argv injection.
///
/// Modern git releases refuse dash-prefixed URLs at the CLI layer,
/// but this check still matters:
///
/// - self-hosted runners still ship older git binaries,
/// - the same helper is reused for committish values fed to
///   `git checkout`, where a `--` terminator can't be used because it
///   would turn the committish into a pathspec.
///
/// A NUL byte is also rejected. It never appears in a legitimate url,
/// ref, or commit, and is a recurring split point for tool pipelines
/// downstream.
pub(crate) fn validate_git_positional(value: &str, kind: &str) -> Result<(), Error> {
    if value.starts_with('-') {
        return Err(Error::Git(format!(
            "refusing to pass {kind} starting with `-` to git: {value:?}"
        )));
    }
    if value.contains('\0') {
        return Err(Error::Git(format!(
            "refusing to pass {kind} containing NUL byte to git"
        )));
    }
    Ok(())
}

/// Resolve a git ref (branch name, tag, or partial commit) to a full
/// 40-char commit SHA by shelling out to `git ls-remote`. `committish`
/// of `None` means resolve `HEAD`. An input that already looks like a
/// full 40-char hex SHA is returned as-is without touching the network.
///
/// Matches the pnpm flow: try exact ref, then `refs/tags/<ref>`,
/// `refs/heads/<ref>`, falling back to the HEAD of the repo when the
/// caller passes `None`.
pub fn git_resolve_ref(url: &str, committish: Option<&str>) -> Result<String, Error> {
    validate_git_positional(url, "git url")?;
    // Already a full commit SHA? No network round-trip needed.
    if let Some(c) = committish
        && c.len() == 40
        && c.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return Ok(c.to_ascii_lowercase());
    }
    // Always list all refs in one shot — filtering server-side with
    // `git ls-remote <url> HEAD` only works when the remote's HEAD
    // symbolic ref resolves, and some hosts (and our bare-repo test
    // fixtures) leave HEAD dangling. Listing everything also lets us
    // fall back to `main` / `master` without a second network call.
    //
    // `--` terminates git's own option parsing so an attacker-supplied
    // url that slips a leading `-` past `validate_git_positional` (we
    // don't expect this, but defense in depth) can't land as an option.
    let out = std::process::Command::new("git")
        .args(["ls-remote", "--", url])
        .output()
        .map_err(|e| Error::Git(format!("spawn git ls-remote {}: {e}", redact_url(url))))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Git(format!(
            "git ls-remote {} failed: {}",
            redact_url(url),
            redact_url(stderr.trim())
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut head: Option<String> = None;
    let mut main_branch: Option<String> = None;
    let mut master_branch: Option<String> = None;
    let mut tag_match: Option<String> = None;
    let mut head_match: Option<String> = None;
    let mut first: Option<String> = None;
    for line in stdout.lines() {
        let mut parts = line.split('\t');
        let sha = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").trim();
        if sha.is_empty() || name.is_empty() {
            continue;
        }
        if first.is_none() {
            first = Some(sha.to_string());
        }
        match name {
            "HEAD" => head = Some(sha.to_string()),
            "refs/heads/main" => main_branch = Some(sha.to_string()),
            "refs/heads/master" => master_branch = Some(sha.to_string()),
            _ => {}
        }
        if let Some(want) = committish {
            if name == format!("refs/tags/{want}") || name == format!("refs/tags/{want}^{{}}") {
                tag_match = Some(sha.to_string());
            } else if name == format!("refs/heads/{want}") {
                head_match = Some(sha.to_string());
            }
        }
    }
    if let Some(want) = committish {
        if let Some(sha) = tag_match.or(head_match) {
            return Ok(sha);
        }
        // ls-remote only advertises branches and tags, so an
        // abbreviated commit SHA never matches a ref name. Pass it
        // through unchanged — `git_shallow_clone` resolves the prefix
        // by fetching and running `git checkout`, and the resolver
        // promotes the rev-parsed full SHA back into `GitSource`
        // before writing the lockfile (see `resolve_git_source`).
        //
        // Lower bound is 7 to stay in lockstep with `git_commit_matches`:
        // a shorter prefix would clear this gate but then trip the
        // post-checkout verification with a confusing mismatch error.
        // 7 is also git's own default `core.abbrev`, so anything users
        // copy out of a git UI lands at or above the cutoff.
        let looks_hex =
            want.len() >= 7 && want.len() < 40 && want.chars().all(|c| c.is_ascii_hexdigit());
        if looks_hex {
            return Ok(want.to_ascii_lowercase());
        }
        Err(Error::Git(format!(
            "git ls-remote {}: no ref matched {want}",
            redact_url(url)
        )))
    } else {
        head.or(main_branch)
            .or(master_branch)
            .or(first)
            .ok_or_else(|| {
                Error::Git(format!(
                    "git ls-remote {}: no refs advertised",
                    redact_url(url)
                ))
            })
    }
}

/// Shallow-clone `url` at `commit` into a fresh temp directory and
/// return the temp path. The caller is responsible for removing the
/// returned directory once it's imported into the store.
///
/// Uses the `git init` / `git fetch --depth 1` / `git checkout` dance
/// rather than `git clone --depth 1 --branch` so we can fetch a raw
/// commit hash that isn't advertised as a branch tip — pnpm does the
/// same for exactly this reason.
/// Return true if `url`'s hostname matches any entry in `hosts`
/// using the same exact-match semantics pnpm uses for
/// `git-shallow-hosts`. No wildcards, no subdomain folding —
/// `github.com` does *not* match `api.github.com`.
///
/// Handles the three URL shapes aube actually hands to git:
///   - `https://host/path`, `git://host/path`, `git+https://host/path`
///   - `git+ssh://git@host/path`
///   - `ssh://git@host/path`
///
/// Anything we can't parse (malformed, bare paths) returns `false`,
/// which means "not in the shallow list" — a full clone is the safe
/// default for weird inputs.
pub fn git_host_in_list(url: &str, hosts: &[String]) -> bool {
    let Some(host) = git_url_host(url) else {
        return false;
    };
    hosts.iter().any(|h| h == host)
}

/// Extract the hostname from a git remote URL string. Public for
/// testability; not expected to be useful to external callers.
pub fn git_url_host(url: &str) -> Option<&str> {
    // Strip the scheme if present. `git+` prefixes (`git+https://`,
    // `git+ssh://`) wrap a regular URL — drop them before parsing.
    let rest = url.strip_prefix("git+").unwrap_or(url);
    let after_scheme = match rest.split_once("://") {
        Some((_, r)) => r,
        // No scheme: could be scp-style `git@host:owner/repo.git`,
        // which has no `://`. Handle that below. Anything else (a
        // bare path, a malformed string) has no host.
        None => {
            // scp-style: `user@host:path`
            let (userhost, _) = rest.split_once(':')?;
            let host = userhost
                .rsplit_once('@')
                .map(|(_, h)| h)
                .unwrap_or(userhost);
            if host.is_empty() || host.contains('/') {
                return None;
            }
            return Some(host);
        }
    };
    // Drop optional `user@` prefix.
    let authority = after_scheme
        .split_once('/')
        .map(|(a, _)| a)
        .unwrap_or(after_scheme);
    let host_with_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    // Drop optional `:port`. IPv6 literals are wrapped in brackets
    // (`[::1]` / `[::1]:22`) and their address itself contains `:`s,
    // so blindly splitting on the last `:` would slice off part of
    // the address. Detect the bracket form first and pull out what's
    // between `[` and `]`; only plain hostname:port strings fall
    // through to the generic split.
    let host = if let Some(inner) = host_with_port.strip_prefix('[') {
        inner.split_once(']').map(|(h, _)| h).unwrap_or(inner)
    } else {
        host_with_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_with_port)
    };
    if host.is_empty() { None } else { Some(host) }
}

/// Clone a git repo into a deterministic per-(url, commit) cache dir
/// and check out `commit`. When `shallow` is true, aube uses
/// `fetch --depth 1 origin <sha>` and falls back to a full fetch if
/// the server rejects by-SHA shallow fetches; when false, aube skips
/// straight to the full-fetch path. Callers decide shallow vs. full
/// by consulting the `gitShallowHosts` setting via
/// [`git_host_in_list`].
///
/// Returns `(clone_dir, head_sha)` where `head_sha` is the 40-char
/// `git rev-parse HEAD` of the checked-out tree. Callers can pass
/// `commit` as either a full SHA or an abbreviated hex prefix; the
/// returned SHA is always the canonical full-length form so the
/// resolver can pin the lockfile to it.
pub fn git_shallow_clone(
    url: &str,
    commit: &str,
    shallow: bool,
) -> Result<(PathBuf, String), Error> {
    use std::process::Command;
    validate_git_positional(url, "git url")?;
    validate_git_positional(commit, "git commit")?;
    // Deterministic path keyed by url+commit so two callers in the
    // same process (resolver → installer) reuse the same checkout
    // instead of re-cloning. Two different repos that happen to
    // share a commit hash can't collide because the url is in the
    // hash. PID is intentionally NOT in the path — that's what made
    // the old version leak a fresh dir on every call.
    //
    // `shallow` is deliberately *not* part of the cache key: the
    // checkout a full clone leaves behind is a strict superset of
    // the one a shallow clone leaves behind (both have the requested
    // commit at HEAD; only the `.git/shallow` marker and object
    // count differ). Two installs that hit the same (url, commit)
    // under different shallow settings can reuse each other's work,
    // and `import_directory` ignores `.git/` so the store sees
    // identical output either way.
    // Keep git scratch out of world-writable /tmp. Predictable names
    // under $TMPDIR are the classic symlink pre-plant vector. Attacker
    // creates /tmp/aube-git-<k>-<c> as a symlink into $HOME/.ssh, then
    // the remove_dir_all below walks right through it and nukes the
    // victim's keys. 0700 on the cache root blocks the same race on a
    // shared user dir.
    let git_root = crate::dirs::cache_dir()
        .map(|d| d.join("git"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&git_root).map_err(|e| Error::Io(git_root.clone(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&git_root, std::fs::Permissions::from_mode(0o700))
        {
            warn!(
                "failed to chmod 0700 {}: {e}. Git scratch dir may be world-accessible, check filesystem permissions",
                git_root.display()
            );
        }
    }
    // Cache key derives from `(url, commit_input)`. When the caller
    // passes an abbreviated SHA, the initial target lands under that
    // key; after the clone, we re-key to the canonical full SHA so
    // a follow-up call (typically the installer reading the
    // lockfile-pinned full SHA) hits the same checkout instead of
    // re-cloning.
    let cache_key = |key_input: &str| -> (String, String) {
        let mut hasher = blake3::Hasher::new();
        hasher.update(url.as_bytes());
        hasher.update(b"\0");
        hasher.update(key_input.as_bytes());
        let digest = hasher.finalize();
        let key: String = digest
            .as_bytes()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        let short = key_input
            .get(..key_input.len().min(12))
            .unwrap_or(key_input)
            .to_string();
        (key, short)
    };
    let (key, commit_short) = cache_key(commit);
    let target = git_root.join(format!("aube-git-{key}-{commit_short}"));

    // Fast path: a previous call already finished this (url, commit)
    // pair and left a complete checkout at `target`. Verify cheaply
    // with `git rev-parse HEAD`; if it matches, reuse. A mismatch
    // means we're looking at an abandoned partial-failure stub from
    // an older aube version — it'll get replaced by the atomic
    // rename below.
    if target.join(".git").is_dir()
        && let Ok(out) = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&target)
            .output()
        && out.status.success()
    {
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if git_commit_matches(&head, commit) {
            return Ok((target, head));
        }
    }

    // Clone into a scratch dir first and atomically rename into
    // place. This solves two problems simultaneously:
    //   1. Partial-failure cleanup — if any git command fails, we
    //      drop the scratch dir and `target` is untouched, so a
    //      retry starts from a clean slate.
    //   2. Concurrent `aube install` races — two processes won't
    //      collide on `target` because each clones into its own
    //      PID-scoped scratch, and only one `rename` wins. The
    //      loser discovers `target` already has the right HEAD
    //      and reuses it.
    // Random suffix from tempfile::Builder. The old <pid> suffix was
    // guessable, so a local attacker could pre-plant a symlink at the
    // exact scratch path before git init ever ran. CSPRNG bytes make
    // that race unwinnable.
    let scratch = tempfile::Builder::new()
        .prefix(&format!("aube-git-{key}-{commit_short}."))
        .tempdir_in(&git_root)
        .map_err(|e| Error::Io(git_root.clone(), e))?
        .keep();

    let run_in = |dir: &Path, args: &[&str]| -> Result<(), Error> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map_err(|e| Error::Git(format!("spawn git {}: {e}", redact_args(args))))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Git(format!(
                "git {} failed: {}",
                redact_args(args),
                redact_url(stderr.trim())
            )));
        }
        Ok(())
    };

    let do_clone = || -> Result<String, Error> {
        run_in(&scratch, &["init", "-q"])?;
        run_in(&scratch, &["remote", "add", "--", "origin", url])?;
        // Shallow fetch by raw SHA only works when the remote allows
        // uploads of any reachable object (GitHub/GitLab/Bitbucket
        // do; many self-hosted servers don't). Fall back to a full
        // fetch on any failure. When `shallow` is false — caller
        // said the host isn't on the shallow list — skip the depth=1
        // attempt entirely to avoid a guaranteed-wasted round trip.
        let shallow_ok = shallow
            && run_in(
                &scratch,
                &["fetch", "--depth", "1", "-q", "--", "origin", commit],
            )
            .is_ok();
        if !shallow_ok {
            run_in(&scratch, &["fetch", "-q", "--", "origin"])?;
        }
        // `git checkout -- <commit>` treats <commit> as a pathspec, so
        // we cannot use the argv separator here. `validate_git_positional`
        // at function entry already rejected a leading `-` on `commit`.
        run_in(&scratch, &["checkout", "-q", commit])?;
        // Confirm the checkout landed exactly on the expected commit
        // before the scratch clone is renamed into place. Git's own
        // SHA-1 object addressing protects against a server returning
        // a different blob for a given SHA, but a local git
        // misconfiguration (default branch mismatch, rewritten ref,
        // stale reflog) could still leave HEAD on something else —
        // mirrors the defensive check the reuse path at line 1260
        // already performs.
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&scratch)
            .output()
            .map_err(|e| Error::Git(format!("spawn git rev-parse: {e}")))?;
        if !out.status.success() {
            return Err(Error::Git(format!(
                "git rev-parse HEAD failed: {}",
                redact_url(String::from_utf8_lossy(&out.stderr).trim())
            )));
        }
        let actual = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !git_commit_matches(&actual, commit) {
            return Err(Error::Git(format!(
                "git clone HEAD {actual} does not match requested commit {commit}"
            )));
        }
        Ok(actual)
    };
    let head_sha = match do_clone() {
        Ok(sha) => sha,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
    };

    // `rename` is atomic on the same filesystem. Two outcomes:
    //  - Target doesn't exist → we win and it's ours.
    //  - Target already exists (another process raced us, or there
    //    was a stale partial-failure stub above) → rename fails
    //    with ENOTEMPTY/EEXIST. Verify the existing target has our
    //    commit and reuse it; otherwise remove it and retry once.
    match aube_util::fs_atomic::rename_with_retry(&scratch, &target) {
        Ok(()) => Ok((
            canonicalize_clone_dir(&target, commit, &head_sha, &cache_key),
            head_sha,
        )),
        Err(_) => {
            if target.join(".git").is_dir()
                && let Ok(out) = Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(&target)
                    .output()
                && out.status.success()
            {
                let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if git_commit_matches(&head, commit) {
                    let _ = std::fs::remove_dir_all(&scratch);
                    return Ok((
                        canonicalize_clone_dir(&target, commit, &head, &cache_key),
                        head,
                    ));
                }
            }
            // Stale target — clear and retry the rename. Any
            // remaining race here would be between two installs
            // both trying to replace a stale target, which is still
            // safe because each scratch is PID-scoped.
            let _ = std::fs::remove_dir_all(&target);
            aube_util::fs_atomic::rename_with_retry(&scratch, &target).map_err(|e| {
                let _ = std::fs::remove_dir_all(&scratch);
                Error::Git(format!("rename clone into place: {e}"))
            })?;
            Ok((
                canonicalize_clone_dir(&target, commit, &head_sha, &cache_key),
                head_sha,
            ))
        }
    }
}

/// Re-key an abbreviated-SHA cache directory to its canonical
/// full-SHA path so a follow-up `git_shallow_clone` call (e.g. the
/// installer reading the lockfile-pinned full SHA) reuses the
/// existing checkout instead of cloning again. No-op when `commit`
/// already matches `head_sha`. Best-effort: if the rename fails
/// (cross-FS, race, perms), leaves the original path intact and
/// the caller pays one extra clone next time.
fn canonicalize_clone_dir(
    target: &Path,
    commit: &str,
    head_sha: &str,
    cache_key: &dyn Fn(&str) -> (String, String),
) -> PathBuf {
    if commit.eq_ignore_ascii_case(head_sha) {
        return target.to_path_buf();
    }
    let parent = match target.parent() {
        Some(p) => p,
        None => return target.to_path_buf(),
    };
    let (key, short) = cache_key(head_sha);
    let canonical = parent.join(format!("aube-git-{key}-{short}"));
    if canonical.join(".git").is_dir() {
        // Race: another caller already wrote the canonical entry.
        // Drop our duplicate so disk doesn't bloat with two copies.
        let _ = std::fs::remove_dir_all(target);
        return canonical;
    }
    match aube_util::fs_atomic::rename_with_retry(target, &canonical) {
        Ok(()) => canonical,
        Err(_) => target.to_path_buf(),
    }
}

/// Extract a codeload-style HTTPS tarball (e.g. the bytes of a GET to
/// `https://codeload.github.com/<owner>/<repo>/tar.gz/<sha>`) into a
/// deterministic per-(url, commit) cache directory and return a path
/// shaped like `git_shallow_clone`'s output: the extracted tree at
/// the top level, with the `<owner>-<repo>-<sha>/` wrapper component
/// codeload adds stripped off so callers can join `subpath` and read
/// `package.json` exactly the same way they do for a clone.
///
/// `commit` must be a 40-char SHA — codeload tarballs do not embed
/// `.git/`, so there is no post-extraction `rev-parse HEAD` to verify
/// the extracted tree is the requested commit. The lockfile resolver
/// (or an upstream `git ls-remote`) is responsible for pinning a SHA
/// before this is called. The returned `head_sha` is `commit`
/// lowercased.
///
/// Cache layout uses a separate `aube-codeload-` prefix from the
/// `aube-git-` prefix `git_shallow_clone` writes, so a per-dep
/// fallback from one path to the other doesn't trip on the other
/// caller's marker files.
pub fn extract_codeload_tarball(
    bytes: &[u8],
    url: &str,
    commit: &str,
    integrity: Option<&str>,
) -> Result<(PathBuf, String), Error> {
    let git_root = crate::dirs::cache_dir()
        .map(|d| d.join("git"))
        .unwrap_or_else(std::env::temp_dir);
    extract_codeload_tarball_at(&git_root, bytes, url, commit, integrity)
}

/// Return the cached codeload extract for `(url, commit)` without
/// touching the network. Callers should consult this *before*
/// downloading a codeload tarball — once the resolver has populated
/// the cache during BFS, the install-time materialization should
/// reuse it instead of paying a second HTTPS round-trip only to have
/// `extract_codeload_tarball` short-circuit and discard the bytes.
/// Mirrors `git_shallow_clone`'s top-of-function fast path.
///
/// Returns `None` for any input that couldn't possibly correspond to
/// a cached entry — invalid URL/commit shapes, abbreviated SHAs, no
/// resolvable cache root — so callers can chain straight into the
/// fetch path on `None` without untangling an `Err`.
pub fn codeload_cache_lookup(
    url: &str,
    commit: &str,
    integrity: Option<&str>,
) -> Option<(PathBuf, String)> {
    let git_root = crate::dirs::cache_dir()
        .map(|d| d.join("git"))
        .unwrap_or_else(std::env::temp_dir);
    let (target, head_sha) = codeload_cache_paths(&git_root, url, commit, integrity)?;
    target.is_dir().then_some((target, head_sha))
}

/// Return the SRI integrity sidecar for a cached codeload extract.
///
/// Fresh extracts write this next to the cache directory so resolver
/// re-runs from a warm cache keep emitting the same lockfile integrity.
pub fn codeload_cache_integrity(
    url: &str,
    commit: &str,
    integrity: Option<&str>,
) -> Option<String> {
    let git_root = crate::dirs::cache_dir()
        .map(|d| d.join("git"))
        .unwrap_or_else(std::env::temp_dir);
    let (target, _) = codeload_cache_paths(&git_root, url, commit, integrity)?;
    target
        .is_dir()
        .then(|| read_codeload_integrity(&target))
        .flatten()
}

/// Compute the deterministic `(target, head_sha)` pair for a
/// `(url, commit)` cache lookup, without touching the FS. Returns
/// `None` for any input shape that `extract_codeload_tarball` would
/// reject with `Err`, so the lookup and write paths agree on which
/// inputs even *can* have a cache entry.
pub(crate) fn codeload_integrity_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| format!("{s}.integrity"))
        .unwrap_or_else(|| "aube-codeload.integrity".to_string());
    target.with_file_name(file_name)
}

fn codeload_integrity(bytes: &[u8]) -> String {
    crate::sha512_integrity(bytes)
}

pub(crate) fn read_codeload_integrity(target: &Path) -> Option<String> {
    std::fs::read_to_string(codeload_integrity_path(target))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn codeload_cache_paths(
    cache_root: &Path,
    url: &str,
    commit: &str,
    integrity: Option<&str>,
) -> Option<(PathBuf, String)> {
    if validate_git_positional(url, "git url").is_err()
        || validate_git_positional(commit, "git commit").is_err()
    {
        return None;
    }
    if commit.len() != 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let head_sha = commit.to_ascii_lowercase();
    let mut hasher = blake3::Hasher::new();
    hasher.update(url.as_bytes());
    hasher.update(b"\0");
    hasher.update(head_sha.as_bytes());
    if let Some(integrity) = integrity {
        hasher.update(b"\0");
        hasher.update(integrity.as_bytes());
    }
    let digest = hasher.finalize();
    let key: String = digest
        .as_bytes()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    let short = head_sha[..12].to_string();
    Some((
        cache_root.join(format!("aube-codeload-{key}-{short}")),
        head_sha,
    ))
}

/// Inner form of [`extract_codeload_tarball`] that takes the cache
/// root explicitly. Public callers go through the wrapper above so
/// the cache root resolution is uniform; tests pass an in-test
/// `tempfile::tempdir()` directly to avoid mutating `XDG_CACHE_HOME`,
/// which `cargo test`'s default parallel scheduling would race
/// across multiple tests in the same binary.
pub(crate) fn extract_codeload_tarball_at(
    git_root: &Path,
    bytes: &[u8],
    url: &str,
    commit: &str,
    integrity: Option<&str>,
) -> Result<(PathBuf, String), Error> {
    use std::io::Read;
    let (target, head_sha) =
        codeload_cache_paths(git_root, url, commit, integrity).ok_or_else(|| {
        Error::Git(format!(
            "extract_codeload_tarball: invalid (url, commit) — commit must be a full 40-char SHA, got {commit}"
        ))
    })?;
    let key_short = target
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("aube-codeload-"))
        .unwrap_or("");

    std::fs::create_dir_all(git_root).map_err(|e| Error::Io(git_root.to_path_buf(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(git_root, std::fs::Permissions::from_mode(0o700)) {
            warn!(
                "failed to chmod 0700 {}: {e}. Git scratch dir may be world-accessible, check filesystem permissions",
                git_root.display()
            );
        }
    }

    // Reuse a prior successful extraction for this exact (url, commit).
    // The atomic-rename pattern below makes a populated `target` always
    // a complete tree — no half-extracted state to worry about.
    if target.is_dir() {
        if read_codeload_integrity(&target).is_none() {
            let integrity = codeload_integrity(bytes);
            let _ = std::fs::write(codeload_integrity_path(&target), &integrity);
        }
        return Ok((target, head_sha));
    }

    let integrity = codeload_integrity(bytes);

    // Extract into a scratch dir and atomic-rename into place. Same
    // failure-recovery and concurrent-install reasoning as
    // `git_shallow_clone`'s scratch dance.
    let scratch = tempfile::Builder::new()
        .prefix(&format!("aube-codeload-{key_short}."))
        .tempdir_in(git_root)
        .map_err(|e| Error::Io(git_root.to_path_buf(), e))?
        .keep();

    let extract_into = |target: &Path| -> Result<(), Error> {
        let gz = flate2::read::GzDecoder::new(bytes);
        let capped = CappedReader::new(gz, MAX_TARBALL_DECOMPRESSED_BYTES);
        let buffered = std::io::BufReader::with_capacity(256 * 1024, capped);
        let mut archive = tar::Archive::new(buffered);
        let mut entries_seen: usize = 0;
        for entry in archive.entries().map_err(|e| Error::Tar(e.to_string()))? {
            entries_seen += 1;
            if entries_seen > MAX_TARBALL_ENTRIES {
                return Err(Error::Tar(format!(
                    "tarball exceeds entry cap of {MAX_TARBALL_ENTRIES}"
                )));
            }
            let mut entry = entry.map_err(|e| Error::Tar(e.to_string()))?;
            let entry_type = entry.header().entry_type();
            // Codeload archives carry directories, regular files, and
            // (rarely) symlinks. Reject everything else for the same
            // reason `import_tarball` does — the linker imports this
            // tree into the store and we don't want the same node-tar
            // CVE class biting us through the git path.
            if matches!(
                entry_type,
                tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
            ) {
                continue;
            }
            let raw_path = entry
                .path()
                .map_err(|e| Error::Tar(e.to_string()))?
                .to_path_buf();
            // Strip the leading `<owner>-<repo>-<sha>/` wrapper
            // codeload prepends. If an entry is at depth 0 (the
            // wrapper directory itself) just create the target dir;
            // if at depth >= 1 lop off the first component.
            let mut comps = raw_path.components();
            let _wrapper = comps.next();
            let rel: PathBuf = comps.collect();
            if rel.as_os_str().is_empty() {
                continue;
            }
            // Reject any path that would escape the target (`..`,
            // absolute) — `tar::Entry::unpack` does this internally
            // but we're materializing manually so it's our job.
            for c in rel.components() {
                use std::path::Component;
                if !matches!(c, Component::Normal(_)) {
                    return Err(Error::Tar(format!(
                        "tarball entry has unsafe path component: {}",
                        raw_path.display()
                    )));
                }
            }
            let dest = target.join(&rel);
            if entry_type.is_dir() {
                std::fs::create_dir_all(&dest).map_err(|e| Error::Io(dest.clone(), e))?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.to_path_buf(), e))?;
            }
            match entry_type {
                tar::EntryType::Regular | tar::EntryType::Continuous => {
                    let declared = entry
                        .header()
                        .size()
                        .map_err(|e| Error::Tar(e.to_string()))?;
                    if declared > MAX_TARBALL_ENTRY_BYTES {
                        return Err(Error::Tar(format!(
                            "tarball entry exceeds per-entry cap: {declared} bytes > {MAX_TARBALL_ENTRY_BYTES}"
                        )));
                    }
                    let mut out =
                        std::fs::File::create(&dest).map_err(|e| Error::Io(dest.clone(), e))?;
                    let mut limited = entry.by_ref().take(MAX_TARBALL_ENTRY_BYTES);
                    std::io::copy(&mut limited, &mut out)
                        .map_err(|e| Error::Io(dest.clone(), e))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Ok(mode) = entry.header().mode() {
                            // Mask to 0o755 / 0o644 — codeload archives
                            // sometimes carry executable bits; preserve
                            // them so build scripts work, but never
                            // honor setuid/setgid/sticky.
                            let safe = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                            let _ = std::fs::set_permissions(
                                &dest,
                                std::fs::Permissions::from_mode(safe),
                            );
                        }
                    }
                }
                tar::EntryType::Symlink => {
                    let link_target = entry
                        .link_name()
                        .map_err(|e| Error::Tar(e.to_string()))?
                        .ok_or_else(|| Error::Tar("symlink without target".into()))?
                        .into_owned();
                    // Reject absolute or `..`-laden symlink targets so
                    // a hostile archive can't plant a link out of the
                    // extraction tree. The store-import pass would
                    // then resolve the link inside the prepared dir
                    // and read whatever the attacker pointed at.
                    if link_target.is_absolute()
                        || link_target.components().any(|c| {
                            matches!(
                                c,
                                std::path::Component::ParentDir | std::path::Component::RootDir
                            )
                        })
                    {
                        return Err(Error::Tar(format!(
                            "tarball symlink {} -> {} escapes target",
                            raw_path.display(),
                            link_target.display()
                        )));
                    }
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&link_target, &dest)
                        .map_err(|e| Error::Io(dest.clone(), e))?;
                    #[cfg(windows)]
                    {
                        // Windows symlink creation requires SeCreateSymbolicLink
                        // (Developer Mode or admin), which most install hosts
                        // lack. Silently dropping the entry would leave a
                        // half-extracted tree that the linker would walk
                        // straight into a "missing file" error several
                        // layers down with no breadcrumbs back to the git
                        // dep that's actually broken. Surface it now —
                        // packages that genuinely need symlinks can fall
                        // through to the `git clone` path on the next
                        // install attempt by removing the cached extract,
                        // since `git clone` materializes symlinks via
                        // git's own (admin-aware) write path.
                        return Err(Error::Tar(format!(
                            "tarball symlink {} -> {} not supported on Windows; \
                             remove the codeload cache entry and retry to fall back to `git clone`",
                            raw_path.display(),
                            link_target.display()
                        )));
                    }
                }
                _ => {
                    return Err(Error::Tar(format!(
                        "tarball entry type {entry_type:?} is not allowed"
                    )));
                }
            }
        }
        Ok(())
    };

    if let Err(e) = extract_into(&scratch) {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e);
    }

    match aube_util::fs_atomic::rename_with_retry(&scratch, &target) {
        Ok(()) => {
            let _ = std::fs::write(codeload_integrity_path(&target), &integrity);
            Ok((target, head_sha))
        }
        Err(_) => {
            // Two concurrent extracts of the same (url, commit) — the
            // loser sees `target` already populated. Drop the loser's
            // scratch and reuse the winner's directory.
            if target.is_dir() {
                let _ = std::fs::remove_dir_all(&scratch);
                if read_codeload_integrity(&target).is_none() {
                    let _ = std::fs::write(codeload_integrity_path(&target), &integrity);
                }
                return Ok((target, head_sha));
            }
            let _ = std::fs::remove_dir_all(&target);
            aube_util::fs_atomic::rename_with_retry(&scratch, &target).map_err(|e| {
                let _ = std::fs::remove_dir_all(&scratch);
                Error::Git(format!("rename codeload extract into place: {e}"))
            })?;
            let _ = std::fs::write(codeload_integrity_path(&target), &integrity);
            Ok((target, head_sha))
        }
    }
}

pub(crate) fn git_commit_matches(actual: &str, requested: &str) -> bool {
    actual == requested
        || (requested.len() >= 7
            && requested.len() < 40
            && requested.chars().all(|c| c.is_ascii_hexdigit())
            && actual.starts_with(requested))
}
