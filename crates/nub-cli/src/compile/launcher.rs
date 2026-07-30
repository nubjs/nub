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
//! THE FETCH IS VERSION-EXACT, NEVER "LATEST". The launcher and the `nub` that
//! wrote the payload share a container format and the runtime half of every
//! compile-time decision, so they must come from one release. A mismatch is
//! caught — `nub_core::compile::decode` refuses an unknown `FORMAT_VERSION` —
//! but it is caught on the end user's machine, which is the wrong place.
//!
//! `NUB_LAUNCHER_TEMPLATE` still wins over everything and is the offline answer:
//! it is the one spelling that needs no network and no published release.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::TargetPlatform;
use nub_core::node::discovery;
use nub_core::version_management::download;

use super::inject;

/// Explicit override, and the only path that works with no network.
const TEMPLATE_ENV: &str = "NUB_LAUNCHER_TEMPLATE";

/// Cache subdirectory, under the nub cache root and keyed by THIS binary's
/// version — the key is what makes a cache hit version-exact.
const CACHE_DIR: &str = "compile-launchers";

/// Find the launcher template for `target`, fetching + caching it when the target
/// is foreign and the install carries only its own.
pub fn locate(target: &TargetPlatform) -> Result<PathBuf> {
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
    if let Some(found) = find_local(target, &sources)? {
        return Ok(found);
    }
    fetch(target, &sources)
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
            "NUB_LAUNCHER_TEMPLATE points at a missing file: {}",
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
        if cached.is_file() {
            return Ok(Some(cached));
        }
    }
    Ok(None)
}

/// Download the target's template from this release, verify it, and commit it to
/// the cache. Returns the cached path.
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

    eprintln!("Fetching the {} launcher …", target.triple());
    // Concurrent compiles for the same triple must not write each other's bytes;
    // the rename at the end is atomic, so a loser simply overwrites with the
    // identical verified artifact.
    let staged = dir.join(format!(
        "{}.{}.part",
        asset_name(target),
        std::process::id()
    ));
    let _guard = FileGuard(staged.clone());

    let actual = match download::download_to_file_auth(&url, &staged, None, |_, _| {}) {
        Ok(sha) => sha,
        Err(e) => return Err(unavailable(target, sources, &format!("{e:#}"))),
    };
    let expected = match published_sha256(&format!("{url}.sha256")) {
        Ok(sha) => sha,
        Err(e) => return Err(unavailable(target, sources, &format!("{e:#}"))),
    };
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!(
            "checksum mismatch for {url}\n  expected: {expected}\n  actual:   {actual}\n\
             Refusing to inject into a corrupted or tampered launcher."
        );
    }

    // Format gate BEFORE the artifact is committed. `inject` checks this too, but
    // that is after the ~100 MB Node download for the target — and a bad cache
    // entry would then be re-read by every later compile.
    let bytes = fs::read(&staged).with_context(|| format!("reading {}", staged.display()))?;
    let format = target.format();
    if inject::detect_format(&bytes) != Some(format) {
        bail!(
            "{url} is not a {} — the release asset for {} is not the launcher it claims to be.",
            inject::format_name(format),
            target.triple()
        );
    }

    fs::rename(&staged, &dest)
        .with_context(|| format!("installing the launcher at {}", dest.display()))?;
    Ok(dest)
}

/// Read the published `.sha256` sidecar. Format is `sha256sum`'s
/// `<hex>␠␠<filename>` — the same sidecar shape `nub upgrade` verifies archives
/// against, published by the same release step.
fn published_sha256(url: &str) -> Result<String> {
    let body = download::fetch_text_auth(url, None).with_context(|| format!("fetching {url}"))?;
    body.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("empty checksum file at {url}"))
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

/// The release tag carrying this binary's launchers. A stable build pins its own
/// `v<version>`. A canary build has no versioned release — the pipeline recreates
/// one rolling `canary` tag per built commit — so it reads that, accepting that
/// the assets there may already be a newer canary. Same channel split
/// `nub upgrade` makes, and a genuine incompatibility still fails closed at
/// `decode`'s format-version gate.
fn release_tag() -> String {
    let version = env!("CARGO_PKG_VERSION");
    if version.contains("-canary.") {
        "canary".to_string()
    } else {
        format!("v{version}")
    }
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
/// override is the offline answer, the sibling is what a normal install has, and
/// `why` is the reason the network attempt did not close the gap.
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
         \x20\x20{why}\n\
         \x20\x20Set NUB_LAUNCHER_TEMPLATE to a launcher built for {triple} to compile offline.",
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
    use sha2::{Digest, Sha256};

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
            base_url: "http://127.0.0.1:1/unused".to_string(),
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

    /// A cached template is used without a fetch — that is the whole point of the
    /// cache, and the assertion that a second cross-compile costs no network.
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
        assert_eq!(find_local(&foreign, &sources).unwrap(), Some(cached));
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

    /// The fetch resolves against the release that published THIS binary — a
    /// stable build asks for its own `v<version>` tag and never for "latest",
    /// because the launcher and the payload writer share a container format.
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
            url.contains(env!("CARGO_PKG_VERSION")) || release_tag() == "canary",
            "a stable build must pin its own version: {url}"
        );
    }

    /// A release ships only the HOST's template, so this error is the whole
    /// cross-compile UX when the network cannot close the gap. It must name the
    /// triple, the file it wanted, the URL it tried, and the offline escape.
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
            msg.contains("NUB_LAUNCHER_TEMPLATE"),
            "should name the offline escape: {msg}"
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
            format!("{err:#}").contains("NUB_LAUNCHER_TEMPLATE"),
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

    // ---- the fetch path, against a local release fixture ----------------------

    /// A one-shot HTTP server serving a fixed URL→body map on 127.0.0.1. Enough
    /// to stand in for the release-asset host: the fetch makes two plain GETs
    /// (asset, `.sha256`) and needs a real 404 for the not-published case.
    struct ReleaseFixture {
        base: String,
        _thread: std::thread::JoinHandle<()>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl ReleaseFixture {
        fn start(routes: Vec<(String, Vec<u8>)>) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = stop.clone();
            let _thread = std::thread::spawn(move || {
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    let Ok((mut sock, _)) = listener.accept() else {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    };
                    let mut buf = [0u8; 2048];
                    let n = sock.read(&mut buf).unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body = routes.iter().find(|(p, _)| *p == path).map(|(_, b)| b);
                    let _ = match body {
                        Some(b) => sock
                            .write_all(
                                &[
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                        b.len()
                                    )
                                    .into_bytes(),
                                    b.clone(),
                                ]
                                .concat(),
                            ),
                        None => sock.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        ),
                    };
                }
            });
            Self {
                base,
                _thread,
                stop,
            }
        }
    }

    impl Drop for ReleaseFixture {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A minimal ELF image — enough for `detect_format` to classify it, which is
    /// the only thing the fetch inspects.
    fn elf_bytes() -> Vec<u8> {
        let mut b = vec![0x7f, b'E', b'L', b'F'];
        b.resize(4096, 0);
        b
    }

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn fixture_sources(base: &str, cache: &Path) -> Sources {
        Sources {
            override_path: None,
            nub_dir: None,
            cache_root: Some(cache.to_path_buf()),
            base_url: base.to_string(),
        }
    }

    fn routes(name: &str, body: &[u8], sha: &str) -> Vec<(String, Vec<u8>)> {
        let path = format!("/{}/{name}", release_tag());
        vec![
            (path.clone(), body.to_vec()),
            (
                format!("{path}.sha256"),
                format!("{sha}  {name}\n").into_bytes(),
            ),
        ]
    }

    /// The whole point of the feature: a foreign target with nothing local pulls
    /// its template from this release, verifies it, and lands it in the cache —
    /// where the NEXT compile finds it with no network at all.
    #[test]
    fn a_foreign_template_is_fetched_verified_and_cached() {
        let dir = fresh_dir("fetch-ok");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let body = elf_bytes();
        let name = asset_name(&target);
        let fixture = ReleaseFixture::start(routes(&name, &body, &digest(&body)));
        let sources = fixture_sources(&fixture.base, &dir);

        let path = fetch(&target, &sources).expect("the fixture publishes this template");
        assert_eq!(path, cache_path(&dir, &target));
        assert_eq!(fs::read(&path).unwrap(), body);
        assert_eq!(
            find_local(&target, &sources).unwrap(),
            Some(path),
            "the cached template must satisfy the next compile without a fetch"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A tampered or truncated asset must never be injected into — and must never
    /// reach the cache, where every later compile would silently reuse it.
    #[test]
    fn a_checksum_mismatch_is_refused_and_leaves_the_cache_empty() {
        let dir = fresh_dir("fetch-badsha");
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let body = elf_bytes();
        let name = asset_name(&target);
        let fixture = ReleaseFixture::start(routes(&name, &body, &digest(b"other bytes")));
        let sources = fixture_sources(&fixture.base, &dir);

        let err = fetch(&target, &sources).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert!(
            !cache_path(&dir, &target).exists(),
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
        let name = asset_name(&target);
        let fixture = ReleaseFixture::start(routes(&name, &body, &digest(&body)));
        let sources = fixture_sources(&fixture.base, &dir);

        let err = fetch(&target, &sources).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ELF"), "should name the format wanted: {msg}");
        assert!(!cache_path(&dir, &target).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The offline / not-published case. It must be the actionable error, not a
    /// panic and not a bare transport dump — this is what a user on an
    /// unpublished build or a disconnected CI runner actually sees.
    #[test]
    fn an_unpublished_template_produces_the_actionable_error() {
        let dir = fresh_dir("fetch-404");
        let target = TargetPlatform::parse("linux-arm64").unwrap();
        let fixture = ReleaseFixture::start(Vec::new());
        let sources = fixture_sources(&fixture.base, &dir);

        let msg = format!("{:#}", fetch(&target, &sources).unwrap_err());
        assert!(msg.contains("linux-arm64"), "{msg}");
        assert!(
            msg.contains("NUB_LAUNCHER_TEMPLATE"),
            "the offline escape must be named: {msg}"
        );
        assert!(!cache_path(&dir, &target).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
