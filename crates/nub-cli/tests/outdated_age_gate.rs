//! `nub outdated` against a registry that dates nothing (#722, #581).
//!
//! The previous engine age-gated `outdated`: a registry serving no publish
//! times made the gate fail closed, so the command printed "All dependencies
//! up to date." plus a stderr warning naming `nub update` as the command that
//! would fail. That warning was the fix for #722's report/installer
//! disagreement.
//!
//! The pnpm 12 engine does not age-gate `outdated` at all. Its report is the
//! plain `Package | Current | Latest` table — there is no `wanted` column for a
//! gated pick to disagree with — and a registry that dates nothing changes
//! nothing about it. So the warning, the codes it carried, and the
//! `wanted`-versus-`latest` narrowing that shaped these tests are all gone.
//!
//! What the fixture pins now is that absence: an undated registry is reported
//! exactly like a dated one. Both cases were measured on the pnpm engine and
//! differentialled against pnpm 12.4.1, which produces the identical table and
//! exit code. `WARNING` is kept as the retired string these tests watch for, so
//! re-introducing an age gate into `outdated` goes red here.
//!
//! Offline by construction: an in-process registry on an ephemeral port, and a
//! handcrafted lockfile so nothing installs.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// A unique temp project dir under the system temp root (never under $HOME, so
/// manifest/lockfile walk-ups can't escape into stray ancestors).
fn tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-outdated-age-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A project pinned at `1.0.0` whose manifest range can still reach `1.1.0`,
/// pointed at `registry_url` with the window on. `fetch-retries=0` keeps a
/// fixture hiccup from being retried into a timeout.
///
/// The lockfile is a real pnpm one and is parsed by a real YAML parser, so the
/// specifier is quoted: a range beginning `>` is a block-scalar indicator, and
/// unquoted it fails the load outright rather than producing the range you
/// meant. `minimumReleaseAge` stays in `.npmrc`, where this project's own
/// config model reads it; pnpm takes it from `pnpm-workspace.yaml` instead, and
/// neither home makes `outdated` gate its report.
fn project(tag: &str, registry_url: &str, specifier: &str) -> PathBuf {
    let dir = tmpdir(tag);
    std::fs::write(
        dir.join("package.json"),
        format!(
            r#"{{"name":"outdated-age","version":"1.0.0","dependencies":{{"undated-pkg":"{specifier}"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join(".npmrc"),
        format!("registry={registry_url}\nfetch-retries=0\nminimumReleaseAge=1440\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("pnpm-lock.yaml"),
        format!(
            "lockfileVersion: '9.0'\n\n\
             importers:\n\n\
             \x20\x20.:\n\
             \x20\x20\x20\x20dependencies:\n\
             \x20\x20\x20\x20\x20\x20undated-pkg:\n\
             \x20\x20\x20\x20\x20\x20\x20\x20specifier: '{specifier}'\n\
             \x20\x20\x20\x20\x20\x20\x20\x20version: 1.0.0\n\n\
             packages:\n\n\
             \x20\x20undated-pkg@1.0.0:\n\
             \x20\x20\x20\x20resolution: {{integrity: sha512-deadbeef}}\n\n\
             snapshots:\n\n\
             \x20\x20undated-pkg@1.0.0: {{}}\n"
        ),
    )
    .unwrap();
    dir
}

fn run_outdated(dir: &Path) -> (String, String, i32) {
    let out = Command::new(nub_binary())
        .arg("outdated")
        .current_dir(dir)
        .env("XDG_DATA_HOME", tmpdir("xdg-data"))
        .env("XDG_CACHE_HOME", tmpdir("xdg-cache"))
        .output()
        .expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// Serves one packument for `undated-pkg` and nothing else.
///
/// `latest_tag` selects the `latest` dist-tag; `times` is the packument's
/// `time` map, and an empty one is a registry that dates nothing — the input
/// these tests exist to hold `outdated` steady against. A far-future `modified`
/// keeps any document-level maturity shortcut shut without a date that goes
/// stale.
struct Registry {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Registry {
    fn start(latest_tag: &str, times: &[(&str, &str)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());

        let mut versions = serde_json::Map::new();
        for v in ["1.0.0", "1.1.0", "2.0.0", "3.0.0"] {
            versions.insert(
                v.to_string(),
                serde_json::json!({
                    "name": "undated-pkg",
                    "version": v,
                    "dist": {
                        "tarball": format!("{url}undated-pkg-{v}.tgz"),
                        "integrity": "sha512-deadbeef",
                    }
                }),
            );
        }
        let mut packument = serde_json::json!({
            "name": "undated-pkg",
            "dist-tags": { "latest": latest_tag },
            "versions": versions,
            "modified": "2999-01-01T00:00:00.000Z",
        });
        if !times.is_empty() {
            packument["time"] = serde_json::json!(
                times
                    .iter()
                    .map(|(v, t)| ((*v).to_string(), serde_json::json!(t)))
                    .collect::<serde_json::Map<_, _>>()
            );
        }

        let mut responses = HashMap::new();
        responses.insert(
            "/undated-pkg".to_string(),
            serde_json::to_vec(&packument).unwrap(),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let responses = Arc::new(responses);
        let thread = thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let responses = responses.clone();
                        // A dead client mid-exchange is not a fixture bug, and a
                        // panic on this detached thread would not fail the test
                        // anyway — the assertions live in the test.
                        thread::spawn(move || {
                            let _ = serve_one(stream, &responses);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("registry fixture failed: {e}"),
                }
            }
        });

        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}

/// Serve one request, then close *gracefully*.
///
/// Closing a socket still holding unread bytes is an ABORTIVE close: it discards
/// the send buffer, so the client sees a transport error instead of the response
/// it was mid-read of. Not Windows-specific — read the request to its end, then
/// half-close and drain to EOF. `set_nonblocking(false)` is required rather than
/// belt-and-braces: BSD `accept` inherits the listener's non-blocking flag onto
/// the accepted socket, so without it the read returns `WouldBlock` before the
/// bytes land and the fixture 404s a package it holds. (Both hazards are
/// documented at length on the sibling fixture in `init_cmd.rs`.)
fn serve_one(mut stream: TcpStream, responses: &HashMap<String, Vec<u8>>) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let mut buf = [0_u8; 4096];
    let mut request = Vec::new();
    // Bodyless GETs, so end-of-headers is end-of-request.
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf)? {
            0 => break,
            size => request.extend_from_slice(&buf[..size]),
        }
    }

    let first_line = String::from_utf8_lossy(&request)
        .lines()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
    let (status, body) = match responses.get(path) {
        Some(body) => ("200 OK", body.as_slice()),
        None => ("404 Not Found", b"not found".as_slice()),
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    stream.shutdown(Shutdown::Write)?;
    while stream.read(&mut buf)? > 0 {}
    Ok(())
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

/// The previous engine's age-gate warning. No longer emitted by anything;
/// asserted absent so that re-introducing an age gate into `outdated` fails
/// here rather than silently changing what the command reports.
const WARNING: &str = "undated-pkg has no registry publish times";

/// A registry that dates nothing does not change what `outdated` reports: the
/// upgrade is offered exactly as it would be for a fully dated registry, and
/// the drift exit code is unchanged.
///
/// Was the inverse — "All dependencies up to date." on stdout plus a stderr
/// warning naming `nub update`, exit 0 — because the age gate fed `outdated`
/// and failed closed on missing publish times. Nothing gates the report now.
/// Paired with `a_dated_registry_reports_the_upgrade_and_stays_quiet`, which
/// runs the same assertions over a fully dated registry: the two agreeing is
/// the contract.
#[test]
fn an_undatable_registry_is_reported_like_any_other() {
    let registry = Registry::start("1.1.0", &[]);
    let dir = project("undated", &registry.url, "^1.0.0");

    let (stdout, stderr, code) = run_outdated(&dir);

    assert!(
        stdout.contains("undated-pkg") && stdout.contains("1.1.0"),
        "the upgrade is reported whether or not the registry dates it.\nstdout: {stdout}"
    );
    assert!(
        !stdout.contains("All dependencies up to date"),
        "an available upgrade must not be reported as no work.\nstdout: {stdout}"
    );
    assert!(
        !stderr.contains(WARNING),
        "the age-gate warning is retired; nothing should emit it.\nstderr: {stderr}"
    );
    assert_eq!(
        code, 1,
        "drift exits 1.\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// The comparison case. Same fixture, same window, every version dated and
/// old. It must report exactly what the undated registry above reports — that
/// the two agree is what says publish times no longer steer `outdated`, and
/// without it the test above would pass against a command that had simply
/// stopped reporting anything.
#[test]
fn a_dated_registry_reports_the_upgrade_and_stays_quiet() {
    let registry = Registry::start(
        "1.1.0",
        &[
            ("1.0.0", "2020-01-01T00:00:00.000Z"),
            ("1.1.0", "2020-01-02T00:00:00.000Z"),
            ("2.0.0", "2020-01-03T00:00:00.000Z"),
            ("3.0.0", "2020-01-04T00:00:00.000Z"),
        ],
    );
    let dir = project("dated", &registry.url, "^1.0.0");

    let (stdout, stderr, code) = run_outdated(&dir);

    assert!(
        !stderr.contains(WARNING),
        "the age-gate warning is retired; nothing should emit it.\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("1.1.0"),
        "the upgrade clears the window and must be offered.\nstdout: {stdout}"
    );
    assert_eq!(
        code, 1,
        "drift exits 1.\nstdout: {stdout}\nstderr: {stderr}"
    );
}

// `stale_latest_tag_alone_does_not_warn` lived here. It existed to narrow the
// age-gate warning's predicate to the `wanted` column, by constructing a
// registry whose `latest` tag was undatable while the manifest range still
// resolved to a dated version. The pnpm 12 engine reports neither a `wanted`
// column nor the warning, so both sides of that distinction are gone and the
// test's surviving assertion — that no warning fires — is one no fixture could
// ever make fail. `an_undatable_registry_is_reported_like_any_other` carries
// what remains checkable.
