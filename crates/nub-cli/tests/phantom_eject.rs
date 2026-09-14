//! The phantom eject against a real install. A package whose own code imports a
//! name it never declared cannot be served from the shared global virtual store,
//! so a nub project keeps it in its own `node_modules/.store`: on the install
//! that first fetches it, on every install from the lockfile after that, and
//! across a change to what the eject decides on a warm tree.
//!
//! Offline by construction: an in-process registry on an ephemeral port serves
//! one package, packed by `nub pack` from a fixture importing an undeclared name.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

/// The internal switch that turns the eject off. It stands in for every change
/// to what the eject decides — a scanner version, the curated list — because
/// all of them reach the engine through the same policy fingerprint.
const EJECT_DISABLE: &str = "__NUB_PHANTOM_EJECT_DISABLE";

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// A unique temp dir under the system temp root, never under $HOME, so manifest
/// and lockfile walk-ups cannot reach a stray ancestor.
fn tmpdir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-phantom-eject-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One user's machine: a home holding the store and cache, and a registry.
struct Machine {
    home: PathBuf,
    registry: String,
}

impl Machine {
    fn nub(&self, dir: &Path, args: &[&str], eject_disabled: bool) -> Output {
        let mut command = Command::new(nub_binary());
        command
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env("NUB_CACHE_DIR", self.home.join("cache"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("npm_config_userconfig", self.home.join("npmrc"))
            .env("npm_config_registry", &self.registry)
            // Under CI a nub project keeps its whole virtual store in the
            // project, which leaves nothing shared to eject from.
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS")
            .env_remove(EJECT_DISABLE);
        if eject_disabled {
            command.env(EJECT_DISABLE, "1");
        }
        command.output().expect("failed to spawn nub")
    }
}

/// `phantom-importer@1.0.0`, whose entry point requires a package it does not
/// declare, as a tarball and its integrity.
fn pack_phantom_importer(machine: &Machine) -> (Vec<u8>, String) {
    let packer = tmpdir("pack");
    std::fs::write(
        packer.join("package.json"),
        r#"{"name":"phantom-importer","version":"1.0.0","files":["index.js"]}"#,
    )
    .unwrap();
    std::fs::write(packer.join("index.js"), "require('undeclared-phantom');\n").unwrap();
    let out = machine.nub(&packer, &["pack"], false);
    assert!(out.status.success(), "nub pack failed: {}", combined(&out));
    let tarball = packer.join("phantom-importer-1.0.0.tgz");
    let integrity = Command::new("node")
        .arg("-e")
        .arg("process.stdout.write('sha512-' + require('crypto').createHash('sha512').update(require('fs').readFileSync(process.argv[1])).digest('base64'))")
        .arg(&tarball)
        .output()
        .expect("node computes the tarball's integrity");
    assert!(integrity.status.success(), "{}", combined(&integrity));
    (
        std::fs::read(&tarball).unwrap(),
        String::from_utf8(integrity.stdout).unwrap(),
    )
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Whether the installed package's real directory is inside the project rather
/// than in the store the machine's cache holds.
fn is_project_local(project: &Path) -> bool {
    let package = std::fs::canonicalize(project.join("node_modules/phantom-importer"))
        .expect("phantom-importer must be installed");
    package.starts_with(std::fs::canonicalize(project).unwrap())
}

#[test]
fn a_phantom_importer_stays_out_of_the_shared_store_and_follows_the_eject_setting() {
    let home = tmpdir("home");
    std::fs::write(home.join("npmrc"), "").unwrap();
    let packing = Machine {
        home: home.clone(),
        registry: "http://127.0.0.1:9/".to_owned(),
    };
    let registry = Registry::serving(pack_phantom_importer(&packing));
    let machine = Machine {
        home,
        registry: registry.url.clone(),
    };
    let project = tmpdir("project");
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"consumer","version":"1.0.0","dependencies":{"phantom-importer":"1.0.0"}}"#,
    )
    .unwrap();
    let install = |eject_disabled: bool| {
        let out = machine.nub(&project, &["install"], eject_disabled);
        assert!(
            out.status.success(),
            "nub install failed: {}",
            combined(&out)
        );
        is_project_local(&project)
    };

    assert!(
        install(false),
        "the install that fetches the package keeps it out of the shared store"
    );
    std::fs::remove_dir_all(project.join("node_modules")).unwrap();
    assert!(
        install(false),
        "an install from the lockfile, with the package already stored, keeps it out too"
    );
    assert!(
        !install(true),
        "a warm tree re-links to the shared store once the eject decides otherwise"
    );
    assert!(
        install(false),
        "and moves back into the project once the eject is on again"
    );
}

struct Registry {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Registry {
    /// Serve the packed tarball as `phantom-importer@1.0.0`, dated long enough
    /// ago to clear any `minimumReleaseAge` window.
    fn serving((tarball, integrity): (Vec<u8>, String)) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let tarball_path = "/phantom-importer/-/phantom-importer-1.0.0.tgz";
        let packument = serde_json::json!({
            "name": "phantom-importer",
            "dist-tags": { "latest": "1.0.0" },
            "time": {
                "created": "2020-01-01T00:00:00.000Z",
                "modified": "2020-01-01T00:00:00.000Z",
                "1.0.0": "2020-01-01T00:00:00.000Z",
            },
            "versions": {
                "1.0.0": {
                    "name": "phantom-importer",
                    "version": "1.0.0",
                    "dist": {
                        "tarball": format!("{}{}", url.trim_end_matches('/'), tarball_path),
                        "integrity": integrity,
                    },
                },
            },
        });
        let mut responses = HashMap::new();
        responses.insert(
            "/phantom-importer".to_owned(),
            serde_json::to_vec(&packument).unwrap(),
        );
        responses.insert(tarball_path.to_owned(), tarball);

        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let responses = Arc::new(responses);
        let thread = thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let responses = responses.clone();
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

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

/// Serve one request, then close gracefully: read the request to its end,
/// half-close, and drain, so the client never sees an abortive close. The
/// blocking and graceful-close hazards are documented on the fixture in
/// `init_cmd.rs`.
fn serve_one(mut stream: TcpStream, responses: &HashMap<String, Vec<u8>>) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = [0_u8; 4096];
    let mut request = Vec::new();
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
        .to_owned();
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
    let (status, body) = match responses.get(path) {
        Some(body) => ("200 OK", body.as_slice()),
        None => ("404 Not Found", b"not found".as_slice()),
    };
    let content_type = if path.ends_with(".tgz") {
        "application/octet-stream"
    } else {
        "application/json"
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    stream.shutdown(Shutdown::Write)?;
    while stream.read(&mut buf)? > 0 {}
    Ok(())
}
