//! A registry package whose dependency an override points at a local copy, under
//! the shared store. That parent's sibling link cannot reach a package that only
//! materializes per project, so it has to leave the store with its child. Offline
//! by construction: the registry is in-process.

use base64::Engine as _;
use sha2::{Digest, Sha512};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

// Inline `npm pack` output, so the fixture needs no test-only tar dependency.
const PARENT_TGZ: &str = "H4sIAAAAAAAC/+2VTWrDMBCFve4pBq1aKO7If4GUXiUg7KFRa8uKZBdD8N07jt1AoMvUJEXfZtCbkZAW78mq8lO904s2FQ3xh4/+AEQssgx+009kKURpXqCURYE8hxI3ScI1WoHed8rxVa7wSAbO9U5o2qqvKabBtq7z8AaODr129CjKva4r8fT6EAX+L3bx/1I5AVqzsv/l1Lv0v8yzPPh/DY4gjGpIbEFY5ch04hnEFzmvWzOJMsYYJ61R+iT8fBSTVpElXppSk+cenzWHBo/t5o0wwhgCJBAIBG6Qb7XJrscADAAA";
const SIBLING_TGZ: &str = "H4sIAAAAAAAC/+3VQQuCMBQH8J37FI+da72lTij6MCuHLHMTpyFE370p5iE6lmDsd3nwZ4ft8H+r5LmQudpqk6mOXRz5AUQUcQyf8kGUAokSgZwLgf4cckx3iZ9kBq1rZO2v8oVHejDNhSht1l4VU11l68bBEajTp6s2+cYW9LAiwX+rxv6P028Aa+bufyre+8+ThIf+z+EO1MhS0f1UfLoGelO109b0KWfIsM9KqYfg9VNQeIT1EARBsFhP5HHK/wAMAAA=";

struct Registry {
    url: String,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Registry {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());

        let packages = [
            (
                "parent",
                PARENT_TGZ,
                serde_json::json!({ "child": "^1.0.0" }),
            ),
            ("sibling", SIBLING_TGZ, serde_json::json!({})),
        ];
        let mut responses = HashMap::new();
        for (name, encoded, dependencies) in packages {
            let tarball = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap();
            let integrity = format!(
                "sha512-{}",
                base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tarball))
            );
            let packument = serde_json::json!({
                "name": name,
                "dist-tags": { "latest": "1.0.0" },
                "versions": { "1.0.0": {
                    "name": name,
                    "version": "1.0.0",
                    "dependencies": dependencies,
                    "dist": { "tarball": format!("{url}{name}.tgz"), "integrity": integrity },
                }},
                "time": { "1.0.0": "2020-01-01T00:00:00.000Z" },
            });
            responses.insert(format!("/{name}"), serde_json::to_vec(&packument).unwrap());
            responses.insert(format!("/{name}.tgz"), tarball);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let responses = Arc::new(responses);
        let server = thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let responses = responses.clone();
                        // A dead client is not a fixture bug; the assertions live
                        // in the test.
                        thread::spawn(move || {
                            let _ = serve_one(stream, &responses);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("registry fixture failed: {error}"),
                }
            }
        });
        Self {
            url,
            stop,
            server: Some(server),
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Reads the request to its end, then half-closes and drains: closing a socket
/// with unread bytes aborts the response the client is still reading. BSD `accept`
/// also inherits the listener's non-blocking flag, hence the explicit reset.
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
    let request = String::from_utf8_lossy(&request);
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let (status, body) = match responses.get(path) {
        Some(body) => ("200 OK", body.as_slice()),
        None => ("404 Not Found", b"not found".as_slice()),
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    stream.shutdown(Shutdown::Write)?;
    while stream.read(&mut buf)? > 0 {}
    Ok(())
}

fn tmpdir() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-override-local-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn install(project: &Path, home: &Path) {
    let out = Command::new(nub_binary())
        .arg("install")
        .current_dir(project)
        // `CI` is what turns the shared store off.
        .env_remove("CI")
        .env_remove("NODE_COMPAT")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("NUB_SELF_SHIM", "0")
        .output()
        .expect("spawn nub install");
    assert!(
        out.status.success(),
        "nub install failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn require_parent(project: &Path) -> String {
    let out = Command::new("node")
        .args(["-p", "require('parent')"])
        .current_dir(project)
        .output()
        .expect("spawn node");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn an_override_to_a_local_copy_loads_through_a_registry_parent_in_the_shared_store() {
    let registry = Registry::start();
    let root = tmpdir();
    let project = root.join("project");
    let home = root.join("home");
    std::fs::create_dir_all(project.join("vendor/child")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        project.join("vendor/child/package.json"),
        r#"{"name":"child","version":"1.0.0","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        project.join("vendor/child/index.js"),
        "module.exports = 'local-child';",
    )
    .unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"app","version":"1.0.0","private":true,
            "dependencies":{"parent":"1.0.0","sibling":"1.0.0"},
            "overrides":{"child":"file:./vendor/child"}}"#,
    )
    .unwrap();
    std::fs::write(
        project.join(".npmrc"),
        format!("registry={}\nfetch-retries=0\n", registry.url),
    )
    .unwrap();

    install(&project, &home);
    assert_eq!(require_parent(&project), "local-child");

    // The control that makes the line above mean something: `sibling` has no local
    // child, so it is still in the shared store. If the store were off, every
    // package would load and this test would pass with the bug present.
    let project_real = project.canonicalize().unwrap();
    let in_project = |name: &str| {
        project
            .join("node_modules")
            .join(name)
            .canonicalize()
            .unwrap()
            .starts_with(&project_real)
    };
    assert!(
        !in_project("sibling"),
        "sibling must stay in the shared store"
    );
    assert!(in_project("parent"), "parent must leave the shared store");

    // The same answer from the lockfile the first install wrote.
    std::fs::remove_dir_all(project.join("node_modules")).unwrap();
    install(&project, &home);
    assert_eq!(require_parent(&project), "local-child");

    // A later install keeps the ejected parent, and has to move its link when the
    // override moves.
    std::fs::create_dir_all(project.join("vendor/other")).unwrap();
    std::fs::write(
        project.join("vendor/other/package.json"),
        r#"{"name":"child","version":"1.0.0","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        project.join("vendor/other/index.js"),
        "module.exports = 'other-child';",
    )
    .unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"app","version":"1.0.0","private":true,
            "dependencies":{"parent":"1.0.0","sibling":"1.0.0"},
            "overrides":{"child":"file:./vendor/other"}}"#,
    )
    .unwrap();
    install(&project, &home);
    assert_eq!(require_parent(&project), "other-child");

    let _ = std::fs::remove_dir_all(&root);
}
