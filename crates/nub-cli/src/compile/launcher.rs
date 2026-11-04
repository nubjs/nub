//! Acquiring the per-target `nub-launcher` template `nub compile` injects into.
//!
//! A launcher is target-specific twice over — it IS the target's executable
//! format, and it carries the container reader plus the nub-core runtime logic
//! the payload depends on — so a foreign `--platform` needs that triple's own
//! prebuilt template and there is nothing to fall back to. An install ships
//! exactly ONE: its own. release.yml's build matrix is per-platform and each job
//! copies only the launcher it just built, so every distribution channel (the npm
//! platform package, the release archive, the Homebrew keg, the winget install
//! dir) carries a single template next to `nub`.
//!
//! A foreign template is therefore FETCHED from the release that published this
//! `nub`, verified, and cached — the shape `--smol` provisioning already uses for
//! Node: download a published artifact, check it against its published digest,
//! commit it to the shared cache, never fetch it twice. The considered
//! alternative was shipping all eight in every package; that adds ~4 MB of dead
//! weight to every install for a capability only a release pipeline uses, and it
//! would force the eight parallel build jobs to rendezvous before any package
//! could be assembled. The design record is `wiki/commands/compile.md`.
//!
//! The transport is `nub upgrade`'s — same base URL, same curl, same `.sha256`
//! sidecar convention — because these are assets of the same release and one
//! mechanism for all of them is one mechanism to keep correct. It also keeps the
//! `file://` test seam working here: an in-process HTTP client cannot serve one,
//! and its tests answer to the ambient proxy configuration, which is exactly the
//! kind of environment dependence a hermetic suite must not have.
//!
//! THE FETCH IS VERSION-EXACT, NEVER "LATEST". The launcher and the `nub` that
//! wrote the payload share a container format and the runtime half of every
//! compile-time decision, so they must come from one release. A mismatch is
//! caught — `nub_core::compile::decode` refuses an unknown `FORMAT_VERSION` —
//! but it is caught on the end user's machine, which is the wrong place.
//!
//! An internal development override still wins over everything and is the one
//! path that needs no network and no published release.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::TargetPlatform;
use nub_core::node::discovery;

use super::inject;
use crate::cli::{curl_download, fetch_expected_sha256, sha256_hex};

/// Internal development override, and the only path that works with no network.
const TEMPLATE_ENV: &str = "__NUB_LAUNCHER_TEMPLATE";

/// Cache subdirectory, under the nub cache root and keyed by THIS binary's
/// version — the key is what makes a cache hit version-exact.
const CACHE_DIR: &str = "compile-launchers";

/// Find the launcher template for `target` — fetching + caching it when the
/// target is foreign and the install carries only its own — verify it really is
/// that platform's executable, and return its bytes.
///
/// The verification is HERE rather than at injection time so every source — the
/// override, a sibling, the cache, a fetch — fails in the first second. Injection
/// happens after the target's ~100 MB Node has been downloaded and recompressed,
/// which is a long time to wait to be told the template was for the wrong
/// architecture. The bytes come back with it because injection needs them and
/// reading the file twice to hand back a path would be silly; every message that
/// names the template is raised here, where the path is still in scope.
pub fn locate(target: &TargetPlatform) -> Result<Vec<u8>> {
    // Canonicalized so a channel that exposes `nub` through a symlink (winget's
    // portable command alias) anchors the sibling lookup to the real install dir.
    // `current_exe` resolves symlinks on Linux (`/proc/self/exe`) but NOT on
    // macOS, where it returns the path used to exec.
    let exe = std::env::current_exe()
        .ok()
        .map(|p| fs::canonicalize(&p).unwrap_or(p));
    let sources = Sources {
        override_path: std::env::var_os(TEMPLATE_ENV).map(PathBuf::from),
        nub_dir: exe.as_deref().and_then(Path::parent).map(Path::to_path_buf),
        cache_root: discovery::cache_dir(),
        // Same base the self-upgrade channel uses, so `NUB_RELEASE_BASE_URL`
        // redirects both. Read ONCE here rather than inside the fetch, which
        // keeps the network half a pure function of its inputs and testable
        // without touching process-global env.
        base_url: crate::cli::release_download_base(),
    };
    let path = match find_local(target, &sources)? {
        Some(found) => found,
        // Every source lands at the same `verify_template` below. A cache entry
        // carries a second check on top of it — `find_local` re-hashes it —
        // because the fetch's verification runs before the entry is visible and
        // so cannot speak for what a later crash left behind.
        None => fetch(target, &sources)?,
    };
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    inject::verify_template(target, &bytes)
        .with_context(|| format!("{} is not usable as a launcher template", path.display()))?;
    Ok(bytes)
}

/// Where a template may already exist on this machine, plus where to get one that
/// does not.
struct Sources {
    override_path: Option<PathBuf>,
    /// The directory holding the running `nub` — the distribution contract.
    nub_dir: Option<PathBuf>,
    cache_root: Option<PathBuf>,
    base_url: String,
}

/// The local half of the lookup: the override, then a sibling of `nub`, then the
/// cache. `Ok(None)` means nothing here has it and the caller may fetch.
///
/// Split out from [`locate`] so the ordering and the (absent) network boundary
/// are testable without one.
fn find_local(target: &TargetPlatform, sources: &Sources) -> Result<Option<PathBuf>> {
    if let Some(p) = &sources.override_path {
        if p.is_file() {
            return Ok(Some(p.clone()));
        }
        // An override that points nowhere is a typo, not a reason to go to the
        // network: silently fetching would hide the mistake behind a working
        // build and ship a template the user did not choose.
        bail!(
            "launcher template override points at a missing file: {}",
            p.display()
        );
    }

    if let Some(dir) = &sources.nub_dir {
        for name in sibling_names(target) {
            let cand = dir.join(name);
            if cand.is_file() {
                return Ok(Some(cand));
            }
        }
    }

    if let Some(root) = &sources.cache_root {
        let cached = cache_path(root, target);
        // A hit is re-hashed rather than trusted for existing. The fetch's
        // checksum runs before the entry is visible, so nothing else in the
        // system would ever notice an entry a crash left truncated — and a short
        // launcher still carries the header the format gate reads, so it would be
        // injected into every binary this version compiles. Falling through to
        // the fetch replaces it; deleting it here would race a concurrent compile
        // that has it open.
        if cached.is_file() && cache_entry_is_intact(&cached) {
            return Ok(Some(cached));
        }
    }
    Ok(None)
}

/// Whether a cache entry still matches the digest recorded beside it.
///
/// Missing sidecars fail closed. Every entry this implementation publishes
/// records its digest before making the launcher visible, and compiled
/// executables have not shipped an older sidecar-less cache contract. Trusting a
/// bare file would let a crash, manual copy, or local attacker bypass the only
/// byte-integrity check on a template injected into every artifact.
fn cache_entry_is_intact(entry: &Path) -> bool {
    let Ok(recorded) = fs::read_to_string(sidecar_path(entry)) else {
        return false;
    };
    let Some(expected) = recorded.split_whitespace().next() else {
        return false;
    };
    fs::read(entry).is_ok_and(|bytes| sha256_hex(&bytes).eq_ignore_ascii_case(expected))
}

/// Download the target's template from this release, verify it, and commit it to
/// the cache with its digest recorded beside it. Returns the cached path.
fn fetch(target: &TargetPlatform, sources: &Sources) -> Result<PathBuf> {
    let url = asset_url(&sources.base_url, target);
    let Some(root) = &sources.cache_root else {
        // Nothing is fetched without somewhere to keep it: a per-invocation
        // temp copy would re-download on every compile.
        return Err(unavailable(target, sources, "no writable cache directory"));
    };

    let dest = cache_path(root, target);
    let dir = dest.parent().expect("cache path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    super::note(&format!("Fetching the {} launcher …", target.triple()));
    // PID-scoped, so two concurrent compiles for the same triple cannot write
    // each other's bytes mid-download. They still race on the commit below.
    let staged = dir.join(format!(
        "{}.{}.part",
        asset_name(target),
        std::process::id()
    ));
    let _guard = FileGuard(staged.clone());

    if let Err(e) = curl_download(&url, &staged) {
        return Err(unavailable(target, sources, &format!("{e:#}")));
    }
    let expected = match fetch_expected_sha256(&format!("{url}.sha256")) {
        Ok(sha) => sha,
        Err(e) => return Err(unavailable(target, sources, &format!("{e:#}"))),
    };
    let bytes = fs::read(&staged).with_context(|| format!("reading {}", staged.display()))?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!(
            "checksum mismatch for {url}\n  expected: {expected}\n  actual:   {actual}\n\
             Refusing to inject into a corrupted or tampered launcher."
        );
    }

    // Format + architecture gate BEFORE the artifact is committed. `locate` runs
    // the same check on whatever it resolves, but a bad cache entry would still
    // be a bad cache entry — refuse it here so it never lands.
    inject::verify_template(target, &bytes).with_context(|| {
        format!("{url} is not the launcher the release claims to publish there")
    })?;

    // Durability BEFORE visibility. The rename below publishes this entry to
    // every later compile of this version, and a crash can commit the rename
    // while the bytes behind it are still short — a truncated launcher that then
    // passes the format gate forever. Opened for write because Windows'
    // `FlushFileBuffers` requires that access right.
    fs::OpenOptions::new()
        .write(true)
        .open(&staged)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("flushing {} to disk", staged.display()))?;

    // The digest lands first so a visible entry is never the only thing present:
    // an entry with nothing to check itself against is one a hit has to trust.
    commit_sidecar(dir, &dest, &actual, target)?;

    // Losing the commit race is harmless only when the winner is the same
    // verified release asset. Windows refuses a rename over an existing file,
    // which also happens when a previous crash left a corrupt cache entry: do
    // not mistake that repair case for a good concurrent winner.
    commit_template(&staged, &dest, &actual, |src, dest| fs::rename(src, dest))?;

    // The rename itself is only durable once the directory entry is synced.
    // Best effort: a lost rename costs a refetch, not a bad cache entry. Windows
    // has no portable equivalent — a directory handle needs
    // `FILE_FLAG_BACKUP_SEMANTICS`, which `File::open` does not set.
    #[cfg(unix)]
    let _ = fs::File::open(dir).and_then(|d| d.sync_all());

    Ok(dest)
}

/// Publish a verified staged launcher, repairing an existing corrupt entry.
///
/// A normal POSIX rename replaces `dest`; Windows instead reports that the
/// destination exists. In that branch, an intact destination is a concurrent
/// winner and is safe to adopt. A mismatched one is the cache-corruption path
/// that brought us here, so remove it and retry. The retry may itself lose to a
/// concurrent verified winner; accept that winner only when its bytes match the
/// digest we just verified.
fn commit_template<F>(staged: &Path, dest: &Path, expected: &str, mut rename: F) -> Result<()>
where
    F: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let install = |error: std::io::Error| {
        anyhow::Error::new(error).context(format!("installing the launcher at {}", dest.display()))
    };

    match rename(staged, dest) {
        Ok(()) => Ok(()),
        Err(_first) if entry_matches_digest(dest, expected) => Ok(()),
        Err(_first) if dest.is_file() => {
            fs::remove_file(dest).with_context(|| {
                format!("removing corrupt launcher cache entry {}", dest.display())
            })?;
            match rename(staged, dest) {
                Ok(()) => Ok(()),
                Err(_retry) if entry_matches_digest(dest, expected) => Ok(()),
                Err(retry) => Err(install(retry)),
            }
        }
        Err(first) => Err(install(first)),
    }
}

/// Whether `entry` is the exact launcher digest the current fetch verified.
///
/// This deliberately does not use [`cache_entry_is_intact`]: the fetch has
/// already verified `expected` from the immutable release, so the destination's
/// bytes alone prove whether an existing entry is the winner of this race.
fn entry_matches_digest(entry: &Path, expected: &str) -> bool {
    fs::read(entry).is_ok_and(|bytes| sha256_hex(&bytes).eq_ignore_ascii_case(expected))
}

/// Record `hex` beside the cache entry, in the `sha256sum` format so the check a
/// hit performs can also be run by hand. Staged and renamed like the entry
/// itself, so a concurrent compile never reads a half-written digest, and
/// tolerant of losing that race for the same reason the entry's commit is.
fn commit_sidecar(dir: &Path, dest: &Path, hex: &str, target: &TargetPlatform) -> Result<()> {
    let name = asset_name(target);
    let staged = dir.join(format!("{name}.{}.sha256.part", std::process::id()));
    let _guard = FileGuard(staged.clone());
    let contents = format!("{hex}  {name}\n");
    fs::write(&staged, &contents).with_context(|| format!("writing {}", staged.display()))?;

    let sidecar = sidecar_path(dest);
    let install = |error: std::io::Error| {
        anyhow::Error::new(error).context(format!("recording the digest at {}", sidecar.display()))
    };
    match fs::rename(&staged, &sidecar) {
        Ok(()) => Ok(()),
        Err(_first) if fs::read_to_string(&sidecar).is_ok_and(|s| s == contents) => Ok(()),
        Err(_first) if sidecar.is_file() => {
            if let Err(remove) = fs::remove_file(&sidecar)
                && remove.kind() != std::io::ErrorKind::NotFound
                && !fs::read_to_string(&sidecar).is_ok_and(|s| s == contents)
            {
                return Err(anyhow::Error::new(remove)
                    .context(format!("removing stale digest {}", sidecar.display())));
            }
            match fs::rename(&staged, &sidecar) {
                Ok(()) => Ok(()),
                Err(_retry) if fs::read_to_string(&sidecar).is_ok_and(|s| s == contents) => Ok(()),
                Err(retry) => Err(install(retry)),
            }
        }
        Err(first) => Err(install(first)),
    }
}

/// Where a cache entry's digest lives.
fn sidecar_path(entry: &Path) -> PathBuf {
    let mut p = entry.as_os_str().to_os_string();
    p.push(".sha256");
    PathBuf::from(p)
}

// ---- names, paths, URLs -------------------------------------------------------

/// The template's filename — what release.yml publishes and what an install
/// carries beside `nub`. Renaming it here without renaming it there breaks both
/// the sibling lookup and the fetch, which is why a test pins the pair.
fn asset_name(target: &TargetPlatform) -> String {
    format!("nub-launcher-{}{}", target.triple(), target.exe_suffix())
}

/// Filenames to accept beside the running `nub`, in order. The unsuffixed
/// `nub-launcher` is the HOST's by construction — substituting it for a foreign
/// target would inject into the wrong format — so only the host may take it.
/// Both spellings are tried on Windows because a template may be published
/// either way; on other targets they coincide, so the list stays short.
fn sibling_names(target: &TargetPlatform) -> Vec<String> {
    let triple = target.triple();
    let suffix = target.exe_suffix();
    let mut names = vec![format!("nub-launcher-{triple}{suffix}")];
    if !suffix.is_empty() {
        names.push(format!("nub-launcher-{triple}"));
    }
    if target.is_host() {
        names.push(format!("nub-launcher{suffix}"));
        if !suffix.is_empty() {
            names.push("nub-launcher".to_string());
        }
    }
    names
}

/// Where a fetched template lives. Keyed by this binary's own version, so an
/// upgraded `nub` never reuses the previous release's launcher and a downgrade
/// finds its own still cached.
fn cache_path(cache_root: &Path, target: &TargetPlatform) -> PathBuf {
    cache_root
        .join(CACHE_DIR)
        .join(env!("CARGO_PKG_VERSION"))
        .join(asset_name(target))
}

/// The immutable release tag carrying this binary's launchers.
///
/// Every build, including a canary, pins the template to its exact package
/// version. The release workflow publishes a matching immutable prerelease for
/// each canary before updating the rolling `canary` release used by installers.
/// A launcher and payload writer are a format-coupled pair, so a mutable tag
/// would allow a later canary launcher to be injected into an earlier build.
fn release_tag() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// The release-asset URL for the target's template.
fn asset_url(base: &str, target: &TargetPlatform) -> String {
    format!(
        "{}/{}/{}",
        base.trim_end_matches('/'),
        release_tag(),
        asset_name(target)
    )
}

/// The error for a template that is nowhere and could not be fetched. It has to
/// carry the whole lookup, because every entry is a place the user can act: the
/// sibling is what a normal install has, and `why` is the reason the network
/// attempt did not close the gap.
fn unavailable(target: &TargetPlatform, sources: &Sources, why: &str) -> anyhow::Error {
    let triple = target.triple();
    let cached = sources
        .cache_root
        .as_ref()
        .map(|r| cache_path(r, target).display().to_string())
        .unwrap_or_else(|| "(no cache directory)".to_string());
    anyhow!(
        "no nub-launcher template for --platform {triple}.\n\
         \x20\x20Compiling for {triple} needs the launcher Nub publishes for it. Looked in:\n\
         \x20\x20\x20\x20{}\x20— beside this nub binary\n\
         \x20\x20\x20\x20{cached}\n\
         \x20\x20\x20\x20{url}\n\
         \x20\x20{why}",
        asset_name(target),
        url = asset_url(&sources.base_url, target),
    )
}

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nub_core::compile::SUPPORTED_TRIPLES;

    fn fresh_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nub-launcher-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Local-only sources: no cache root, so nothing here can reach the network.
    fn local(nub_dir: Option<&Path>) -> Sources {
        Sources {
            override_path: None,
            nub_dir: nub_dir.map(Path::to_path_buf),
            cache_root: None,
            base_url: "file:///nonexistent".to_string(),
        }
    }

    fn foreign() -> TargetPlatform {
        SUPPORTED_TRIPLES
            .iter()
            .map(|t| TargetPlatform::parse(t).unwrap())
            .find(|t| !t.is_host())
            .unwrap()
    }

    /// Pins the Rust half of the release↔lookup contract: the name
    /// `.github/workflows/release.yml` copies the template to, and publishes as a
    /// release asset. Renaming it here fails this test; renaming it in the
    /// workflow does not, so the two have to be changed together.
    #[test]
    fn the_release_shipped_filename_is_what_the_host_lookup_accepts() {
        let dir = fresh_dir("shipped");
        let host = TargetPlatform::host().unwrap();
        let shipped = dir.join(asset_name(&host));
        fs::write(&shipped, b"template").unwrap();
        assert_eq!(
            find_local(&host, &local(Some(&dir))).unwrap(),
            Some(shipped)
        );
        assert_eq!(
            asset_name(&TargetPlatform::parse("win32-x64").unwrap()),
            "nub-launcher-win32-x64.exe"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The unsuffixed `nub-launcher` sibling is the HOST's — a foreign target
    /// that borrowed it would inject into the wrong executable format.
    #[test]
    fn only_the_host_falls_back_to_the_unsuffixed_template() {
        let dir = fresh_dir("templates");
        fs::write(dir.join("nub-launcher"), b"host").unwrap();
        let foreign = foreign();

        assert_eq!(
            find_local(&TargetPlatform::host().unwrap(), &local(Some(&dir))).unwrap(),
            Some(dir.join("nub-launcher"))
        );
        assert_eq!(
            find_local(&foreign, &local(Some(&dir))).unwrap(),
            None,
            "{} must not borrow the host's template",
            foreign.triple()
        );

        let own = dir.join(asset_name(&foreign));
        fs::write(&own, b"foreign").unwrap();
        assert_eq!(find_local(&foreign, &local(Some(&dir))).unwrap(), Some(own));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A cached template is used without a fetch only when its release digest is
    /// present and matches. That is the assertion that a second cross-compile
    /// costs no network without trusting an unauthenticated local file.
    #[test]
    fn a_cached_template_satisfies_a_foreign_target() {
        let dir = fresh_dir("cached");
        let foreign = foreign();
        let mut sources = local(None);
        sources.cache_root = Some(dir.clone());
        assert_eq!(find_local(&foreign, &sources).unwrap(), None);

        let cached = cache_path(&dir, &foreign);
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        fs::write(&cached, b"foreign").unwrap();
        fs::write(
            sidecar_path(&cached),
            format!("{}  launcher\n", sha256_hex(b"foreign")),
        )
        .unwrap();
        assert_eq!(find_local(&foreign, &sources).unwrap(), Some(cached));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_entry_without_a_digest_is_not_reused() {
        let dir = fresh_dir("cached-no-digest");
        let foreign = foreign();
        let mut sources = local(None);
        sources.cache_root = Some(dir.clone());
        let cached = cache_path(&dir, &foreign);
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        fs::write(&cached, b"unverified").unwrap();
        assert_eq!(
            find_local(&foreign, &sources).unwrap(),
            None,
            "a bare cache file cannot authenticate the launcher bytes"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn publishing_repairs_a_stale_digest_sidecar() {
        let dir = fresh_dir("stale-sidecar");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let dest = cache_path(&dir, &target);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(sidecar_path(&dest), "stale  launcher\n").unwrap();

        let expected = sha256_hex(b"verified launcher");
        commit_sidecar(dest.parent().unwrap(), &dest, &expected, &target)
            .expect("a stale digest must not make every future cache hit refetch");
        let recorded = fs::read_to_string(sidecar_path(&dest)).unwrap();
        assert_eq!(recorded, format!("{expected}  {}\n", asset_name(&target)));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The cache key is this binary's exact version: an upgraded `nub` must not
    /// reuse the previous release's launcher, since the two halves share a
    /// container format.
    #[test]
    fn the_cache_is_keyed_by_this_binarys_exact_version() {
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let path = cache_path(Path::new("/cache"), &target);
        assert_eq!(
            path,
            Path::new("/cache")
                .join("compile-launchers")
                .join(env!("CARGO_PKG_VERSION"))
                .join("nub-launcher-linux-x64")
        );
    }

    /// The fetch resolves against the immutable release that published THIS
    /// binary — including a canary's exact `v<version>` prerelease, never a
    /// rolling channel tag or "latest", because the launcher and payload writer
    /// share a container format.
    #[test]
    fn the_asset_url_pins_this_release() {
        let target = TargetPlatform::parse("win32-arm64").unwrap();
        let url = asset_url("https://example.test/download", &target);
        assert_eq!(
            url,
            format!(
                "https://example.test/download/{}/nub-launcher-win32-arm64.exe",
                release_tag()
            )
        );
        assert!(
            url.contains(env!("CARGO_PKG_VERSION")),
            "every build must pin its own immutable version: {url}"
        );
        assert_eq!(release_tag(), format!("v{}", env!("CARGO_PKG_VERSION")));
    }

    /// A release ships only the HOST's template, so this error is the whole
    /// cross-compile UX when the network cannot close the gap. It must name the
    /// triple, the file it wanted, and the URL it tried.
    #[test]
    fn an_unavailable_template_names_every_place_it_looked() {
        let dir = fresh_dir("no-template");
        let target = TargetPlatform::parse("win32-arm64").unwrap();
        let mut sources = local(Some(&dir));
        sources.cache_root = Some(dir.clone());
        let msg = format!("{:#}", unavailable(&target, &sources, "404 Not Found"));
        assert!(msg.contains("win32-arm64"), "should name the triple: {msg}");
        assert!(
            msg.contains("nub-launcher-win32-arm64.exe"),
            "should name the file it looked for: {msg}"
        );
        assert!(
            msg.contains(&asset_url(&sources.base_url, &target)),
            "should name the URL it tried: {msg}"
        );
        assert!(msg.contains("404 Not Found"), "should carry why: {msg}");
        assert!(
            !msg.contains("NUB_LAUNCHER_TEMPLATE"),
            "must not advertise an internal override as configuration: {msg}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// An override pointing nowhere is a typo. Falling through to the network
    /// would hide it behind a working build that used a template the user did not
    /// choose, so it aborts instead.
    #[test]
    fn a_broken_override_aborts_rather_than_falling_through_to_the_fetch() {
        let dir = fresh_dir("bad-override");
        let mut sources = local(Some(&dir));
        sources.override_path = Some(dir.join("nope"));
        sources.cache_root = Some(dir.clone());
        let err = find_local(&foreign(), &sources).unwrap_err();
        assert!(
            !format!("{err:#}").contains("NUB_LAUNCHER_TEMPLATE"),
            "{err:#}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The override outranks a present sibling AND a present cache entry — it is
    /// the one spelling that must work with no network and no published release.
    #[test]
    fn the_override_outranks_the_sibling_and_the_cache() {
        let dir = fresh_dir("override-wins");
        let host = TargetPlatform::host().unwrap();
        fs::write(dir.join(asset_name(&host)), b"sibling").unwrap();
        let chosen = dir.join("chosen");
        fs::write(&chosen, b"override").unwrap();
        let mut sources = local(Some(&dir));
        sources.override_path = Some(chosen.clone());
        sources.cache_root = Some(dir.clone());
        assert_eq!(find_local(&host, &sources).unwrap(), Some(chosen));
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- the fetch path, against a file:// release fixture --------------------
    //
    // A real directory served over `file://` rather than a localhost HTTP server:
    // curl reads it with no socket at all, so these tests cannot be perturbed by
    // an ambient proxy, a busy port, or a firewall. Same seam the `nub upgrade`
    // tests use against the same release layout.

    /// Lay out `<root>/<tag>/<asset>` + its `.sha256` sidecar and return the base
    /// URL. `sha` is taken rather than computed so a test can publish a WRONG
    /// digest, which is the whole point of one of them.
    fn publish(root: &Path, name: &str, body: &[u8], sha: &str) -> String {
        let dir = root.join(release_tag());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
        fs::write(
            dir.join(format!("{name}.sha256")),
            format!("{sha}  {name}\n"),
        )
        .unwrap();
        format!("file://{}", root.display())
    }

    /// A minimal ELF image for `machine` (`e_machine`: 0x3e x86-64, 0xb7
    /// aarch64) — the two header fields the template gate reads.
    fn elf_bytes(machine: u16) -> Vec<u8> {
        let mut b = vec![0x7f, b'E', b'L', b'F'];
        b.resize(4096, 0);
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b
    }

    const ELF_X64: u16 = 0x3e;
    const ELF_ARM64: u16 = 0xb7;

    fn sources_at(base: String, cache: &Path) -> Sources {
        Sources {
            override_path: None,
            nub_dir: None,
            cache_root: Some(cache.to_path_buf()),
            base_url: base,
        }
    }

    /// The whole point of the feature: a foreign target with nothing local pulls
    /// its template from this release, verifies it, and lands it in the cache —
    /// where the NEXT compile finds it with no fetch at all.
    #[test]
    fn a_foreign_template_is_fetched_verified_and_cached() {
        let dir = fresh_dir("fetch-ok");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let body = elf_bytes(ELF_X64);
        let base = publish(
            &dir.join("rel"),
            &asset_name(&target),
            &body,
            &sha256_hex(&body),
        );
        let cache = dir.join("cache");
        let sources = sources_at(base, &cache);

        let path = fetch(&target, &sources).expect("the fixture publishes this template");
        assert_eq!(path, cache_path(&cache, &target));
        assert_eq!(fs::read(&path).unwrap(), body);
        assert_eq!(
            find_local(&target, &sources).unwrap(),
            Some(path),
            "the cached template must satisfy the next compile without a fetch"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The fetch verifies before the entry is visible, so a hit re-hashing
    /// against the recorded digest is the only thing standing between a
    /// crash-truncated entry and every binary this version compiles: the
    /// truncation leaves the header the format gate reads intact.
    #[test]
    fn a_cache_entry_that_no_longer_matches_its_digest_is_not_reused() {
        let dir = fresh_dir("cache-corrupt");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let body = elf_bytes(ELF_X64);
        let base = publish(
            &dir.join("rel"),
            &asset_name(&target),
            &body,
            &sha256_hex(&body),
        );
        let cache = dir.join("cache");
        let sources = sources_at(base, &cache);

        let path = fetch(&target, &sources).expect("the fixture publishes this template");
        assert!(
            sidecar_path(&path).is_file(),
            "the fetch must record a digest"
        );
        assert_eq!(find_local(&target, &sources).unwrap(), Some(path.clone()));

        fs::write(&path, &body[..64]).unwrap();
        assert_eq!(
            find_local(&target, &sources).unwrap(),
            None,
            "a truncated entry must not satisfy a compile"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Losing the commit race must not fail the compile. Exercised here through
    /// the ordinary POSIX rename-over-existing; the Windows arm of the same
    /// contract (a sharing violation while the winner reads its copy) needs a
    /// Windows host and is covered by the fallback's own comment.
    #[test]
    fn a_racer_that_finds_the_template_already_installed_still_succeeds() {
        let dir = fresh_dir("fetch-race");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let body = elf_bytes(ELF_X64);
        let base = publish(
            &dir.join("rel"),
            &asset_name(&target),
            &body,
            &sha256_hex(&body),
        );
        let cache = dir.join("cache");
        let sources = sources_at(base, &cache);

        let dest = cache_path(&cache, &target);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, &body).unwrap();

        assert_eq!(fetch(&target, &sources).unwrap(), dest);
        assert_eq!(fs::read(&dest).unwrap(), body);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Windows does not overwrite an existing destination. A corrupt entry is
    /// therefore a repair case, not a benign commit race: remove it and publish
    /// the staged bytes on retry.
    #[test]
    fn a_no_overwrite_commit_repairs_a_corrupt_destination() {
        let dir = fresh_dir("commit-repair");
        let staged = dir.join("staged");
        let dest = dir.join("dest");
        let good = elf_bytes(ELF_X64);
        fs::write(&staged, &good).unwrap();
        let expected = sha256_hex(&good);
        let mut calls = 0;

        commit_template(&staged, &dest, &expected, |from, to| {
            calls += 1;
            if calls == 1 {
                fs::write(to, b"truncated").unwrap();
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            fs::rename(from, to)
        })
        .expect("the retry must replace the corrupt destination");

        assert_eq!(calls, 2);
        assert_eq!(fs::read(&dest).unwrap(), good);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A no-overwrite failure can also mean a concurrent verified fetch won.
    /// That winner is safe to adopt without deleting or retrying it.
    #[test]
    fn a_no_overwrite_commit_adopts_a_matching_concurrent_winner() {
        let dir = fresh_dir("commit-winner");
        let staged = dir.join("staged");
        let dest = dir.join("dest");
        let good = elf_bytes(ELF_X64);
        fs::write(&staged, &good).unwrap();
        fs::write(&dest, &good).unwrap();
        let expected = sha256_hex(&good);
        let mut calls = 0;

        commit_template(&staged, &dest, &expected, |_, _| {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
        })
        .expect("a matching concurrent winner is safe to use");

        assert_eq!(calls, 1, "an intact winner must not be removed and retried");
        assert_eq!(fs::read(&dest).unwrap(), good);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A tampered or truncated asset must never be injected into — and must never
    /// reach the cache, where every later compile would silently reuse it.
    #[test]
    fn a_checksum_mismatch_is_refused_and_leaves_the_cache_empty() {
        let dir = fresh_dir("fetch-badsha");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let body = elf_bytes(ELF_X64);
        let base = publish(
            &dir.join("rel"),
            &asset_name(&target),
            &body,
            &sha256_hex(b"other bytes"),
        );
        let cache = dir.join("cache");

        let err = fetch(&target, &sources_at(base, &cache)).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert!(
            !cache_path(&cache, &target).exists(),
            "a failed verification must not leave a cache entry"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// An asset that is not the target's executable format fails HERE, not after
    /// the ~100 MB Node download the next pipeline step starts.
    #[test]
    fn an_asset_of_the_wrong_format_is_refused_before_it_is_cached() {
        let dir = fresh_dir("fetch-badfmt");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let body = b"<!doctype html>not an executable".to_vec();
        let base = publish(
            &dir.join("rel"),
            &asset_name(&target),
            &body,
            &sha256_hex(&body),
        );
        let cache = dir.join("cache");

        let msg = format!(
            "{:#}",
            fetch(&target, &sources_at(base, &cache)).unwrap_err()
        );
        assert!(msg.contains("ELF"), "should name the format wanted: {msg}");
        assert!(!cache_path(&cache, &target).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The silent one, and the reason the gate reads more than the magic bytes: a
    /// same-format template for the OTHER architecture passes every other check —
    /// the checksum is the publisher's own, the container parses, the payload
    /// injects and decodes — and produces a binary that cannot run on the machine
    /// it was built for. Caught here, or not at all until a user reports it.
    #[test]
    fn a_same_format_template_for_the_wrong_arch_is_refused() {
        let dir = fresh_dir("fetch-badarch");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let body = elf_bytes(ELF_ARM64);
        let base = publish(
            &dir.join("rel"),
            &asset_name(&target),
            &body,
            &sha256_hex(&body),
        );
        let cache = dir.join("cache");

        let msg = format!(
            "{:#}",
            fetch(&target, &sources_at(base, &cache)).unwrap_err()
        );
        assert!(
            msg.contains("arm64") && msg.contains("x64"),
            "must name what it got and what it needed: {msg}"
        );
        assert!(!cache_path(&cache, &target).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The offline / not-published case. It must be the actionable error, not a
    /// panic and not a bare transport dump — this is what a user on an
    /// unpublished build or a disconnected CI runner actually sees.
    #[test]
    fn an_unpublished_template_produces_the_actionable_error() {
        let dir = fresh_dir("fetch-404");
        let target = TargetPlatform::parse("linux-arm64").unwrap();
        let empty = dir.join("rel");
        fs::create_dir_all(&empty).unwrap();
        let cache = dir.join("cache");

        let msg = format!(
            "{:#}",
            fetch(
                &target,
                &sources_at(format!("file://{}", empty.display()), &cache)
            )
            .unwrap_err()
        );
        assert!(msg.contains("linux-arm64"), "{msg}");
        assert_eq!(TEMPLATE_ENV, "__NUB_LAUNCHER_TEMPLATE");
        assert!(
            !msg.contains("NUB_LAUNCHER_TEMPLATE"),
            "must not advertise an internal override as configuration: {msg}"
        );
        assert!(!cache_path(&cache, &target).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
