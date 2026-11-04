//! `nub init` through the real binary — scaffold contents, refuse-don't-
//! clobber, the JS variant, and the non-TTY prompt fallback. Everything here
//! is offline by construction: scaffold-only cases use `--no-install`, while
//! the install-by-default case uses an in-process registry. Design record:
//! internal/commands/init.md.

use base64::Engine as _;
use sha2::{Digest, Sha512};
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

/// Unique temp project dir under the system temp root (never under $HOME, so
/// manifest walk-ups can't escape into stray ancestors).
fn tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-init-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Piped stdio — every run here exercises the non-TTY path (prompts must
/// fall back to defaults, never hang).
fn run_init(dir: &Path, args: &[&str]) -> Out {
    run_init_with_home(dir, args, None)
}

fn run_init_with_home(dir: &Path, args: &[&str], home: Option<&Path>) -> Out {
    let mut command = Command::new(nub_binary());
    command
        .arg("init")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::piped());
    if let Some(home) = home {
        command
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("XDG_CONFIG_HOME", home.join("xdg-config"))
            .env("XDG_DATA_HOME", home.join("xdg-data"))
            .env("XDG_CACHE_HOME", home.join("xdg-cache"))
            .env("NUB_SELF_SHIM", "0");
    }
    let out = command.output().expect("failed to spawn nub");
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        code: out.status.code().unwrap_or(-1),
    }
}

fn package_tarball(name: &str, version: &str) -> Vec<u8> {
    // Deterministic npm tarballs containing only package/package.json. Keeping
    // these inline makes the registry fixture portable without test-only deps.
    let encoded = match (name, version) {
        ("@nubjs/types", "0.4.13") => {
            "H4sIAAAAAAAC/+3JQQrCMBBG4RwlzFrS1KZduPIqUYJYMQ1NK4h4d6MuBNcigu/bvOGf5LcHvwtVetb0eYjqw2zROfdo8V5r2+Xrvu913XaN0lZ9wZwnP2qt/tRFoj8GWck6zps+V9M5hSwLOYUx74dYHtY4UzdyVQAAAAAAAAAAAAAAAACAH3IDKfXZJwAoAAA="
        }
        ("@nubjs/types", "0.5.0") => {
            "H4sIAAAAAAAC/+3JQQrCMBBG4RwlZC3pRJouXHmVKEGsmIamFUS8u1EXgmsRwfdt3vBPDttD2MUmP2v7MiT1YVJ1bfto9V4R7173fXfOd0ulRX3BXKYwaq3+1MWkcIxmZdZp3vSlmc45FrMwpziW/ZDqQ6y3Yq4KAAAAAAAAAAAAAAAAAPBLbnGuIdkAKAAA"
        }
        ("@types/node", "26.1.1") => {
            "H4sIAAAAAAAC/+3IsQrCMBhF4TxK+GdJk9JmcPJVggZRMQ1NFYr47kYdBGcRwfMt53JzWB/CNjb5WbMvQ1IfZivfdY9W77W2d699/53rfau0VV9wKlMYtVZ/6iIpHKMsZTXNOZYmDZsoCznHseyGVP/WG2ecXBUAAAAAAAAAAAAAAAAA4JfcAHOReCIAKAAA"
        }
        ("typescript", "7.0.2") => {
            "H4sIAAAAAAAC/+3IwQoCIRhFYR9F/nWYEzZCbyODxBQ5olMQ0btntQhaRwSdb3MuN4dhH7ZxmZ81uzol9WG26Z17tHmvtc6/9v3vunXvlbbqC451DkVr9acuksIhykbmc451KGOeZSGnWOo4pXZ7Y81KrgoAAAAAAAAAAAAAAAAA8FturWWPWwAoAAA"
        }
        _ => panic!("missing registry fixture tarball for {name}@{version}"),
    };
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded.trim_end_matches('='))
        .unwrap()
}

fn integrity(bytes: &[u8]) -> String {
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
    )
}

struct RegistryFixture {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RegistryFixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());

        let packages: [(&str, &[&str]); 3] = [
            ("@nubjs/types", &["0.4.13", "0.5.0"]),
            ("@types/node", &["26.1.1"]),
            ("typescript", &["7.0.2"]),
        ];
        let mut responses = HashMap::<String, (String, Vec<u8>)>::new();
        for (name, versions) in packages {
            let mut version_entries = serde_json::Map::new();
            let mut times = serde_json::Map::new();
            for version in versions {
                let tarball = package_tarball(name, version);
                let tarball_path =
                    format!("/tarballs/{}-{version}.tgz", name.replace(['@', '/'], "_"));
                version_entries.insert(
                    (*version).to_string(),
                    serde_json::json!({
                        "name": name,
                        "version": version,
                        "dist": {
                            "tarball": format!("{url}{}", &tarball_path[1..]),
                            "integrity": integrity(&tarball),
                        }
                    }),
                );
                times.insert(
                    (*version).to_string(),
                    serde_json::Value::String(
                        if *version == "0.5.0" {
                            "2999-01-01T00:00:00.000Z"
                        } else {
                            "2020-01-01T00:00:00.000Z"
                        }
                        .to_string(),
                    ),
                );
                responses.insert(
                    tarball_path,
                    ("application/octet-stream".to_string(), tarball),
                );
            }
            let latest = versions.last().unwrap();
            let packument = serde_json::to_vec(&serde_json::json!({
                "name": name,
                "dist-tags": { "latest": latest },
                "versions": version_entries,
                "time": times,
            }))
            .unwrap();
            let encoded_name = name.replace('/', "%2f");
            responses.insert(
                format!("/{encoded_name}"),
                ("application/json".to_string(), packument),
            );
        }

        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let responses = Arc::new(responses);
        let server = thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let responses = responses.clone();
                        thread::spawn(move || {
                            // A dead client mid-exchange is not a fixture bug,
                            // and a panic on this detached thread would not fail
                            // the test anyway — the assertions live in the test.
                            let _ = serve_one(stream, &responses);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("registry fixture failed: {error}"),
                }
            }
        });

        Self {
            url,
            stop,
            thread: Some(server),
        }
    }
}

/// Serve one request, then close the connection *gracefully*.
///
/// Closing a socket that still holds unread bytes in its receive buffer is an
/// ABORTIVE close: it discards whatever is still queued in the send buffer, so
/// the client sees a transport-level error instead of the response it was
/// mid-read of. Not Windows-specific — a CI probe measured 0/15 successes with
/// the abortive close against 15/15 with this one on windows, ubuntu AND macos;
/// Windows surfaces it as `ConnectionReset` 10054, macOS stalls to timeout. The
/// age-floor test pins `fetch-retries=0`, so there is no cushion. Hence: read
/// the request to its end, then half-close (FIN) and drain to EOF so the final
/// close has nothing left to abort over.
///
/// `set_nonblocking(false)` is REQUIRED, not belt-and-braces, and for macOS
/// rather than Windows: BSD `accept` inherits the listener's non-blocking flag
/// onto the accepted socket (Linux explicitly does not). Without it the request
/// read returns `WouldBlock` before the bytes land, the path parses as `/`, and
/// the fixture serves a 404 for a package it holds — a second, independent bug.
fn serve_one(
    mut stream: TcpStream,
    responses: &HashMap<String, (String, Vec<u8>)>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let mut buf = [0_u8; 4096];
    let mut request = Vec::new();
    // These are all bodyless GETs, so end-of-headers is end-of-request.
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
    let (status, content_type, body) = if let Some((content_type, body)) = responses.get(path) {
        ("200 OK", content_type.as_str(), body.as_slice())
    } else {
        ("404 Not Found", "text/plain", b"not found".as_slice())
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

impl Drop for RegistryFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[test]
fn default_install_uses_a_mature_types_release_under_the_strict_age_floor() {
    let registry = RegistryFixture::start();
    let dir = tmpdir("strict-install");
    let home = tmpdir("strict-install-home");
    std::fs::write(
        dir.join(".npmrc"),
        format!("registry={}\nfetch-retries=0\n", registry.url),
    )
    .unwrap();

    let out = run_init_with_home(
        &dir,
        &["-y", "--no-git", "--name", "strict-install"],
        Some(&home),
    );
    assert_eq!(
        out.code, 0,
        "stdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    let installed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("node_modules/@nubjs/types/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(installed["version"], "0.4.13");
    assert!(
        !std::fs::read_to_string(dir.join(".npmrc"))
            .unwrap()
            .contains("minimumReleaseAgeExclude"),
        "strict mode must not persist a release-age exclusion"
    );
}

#[test]
fn scaffolds_five_files_with_identity_pin_and_type_devdeps() {
    let dir = tmpdir("basic");
    let out = run_init(&dir, &["-y", "--no-install", "--name", "basic"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    for f in [
        "package.json",
        "tsconfig.json",
        "index.ts",
        ".gitignore",
        "README.md",
    ] {
        assert!(dir.join(f).exists(), "{f} must be written");
    }
    assert!(dir.join(".git").exists(), "git init runs by default");

    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    let ver = env!("CARGO_PKG_VERSION");
    assert_eq!(pkg["packageManager"], format!("nub@{ver}"));
    assert_eq!(pkg["devEngines"]["packageManager"]["name"], "nub");
    assert_eq!(
        pkg["devDependencies"]["@nubjs/types"],
        format!(">=0.4.13 <={ver}"),
        "the type range must let the age gate choose an older mature release"
    );
    assert!(pkg["devDependencies"]["@types/node"].is_string());
    assert!(pkg["devDependencies"]["typescript"].is_string());
    assert_eq!(pkg["type"], "module");

    let tsconfig = std::fs::read_to_string(dir.join("tsconfig.json")).unwrap();
    assert!(
        tsconfig.contains(r#""types": ["node", "@nubjs/types"]"#),
        "tsconfig must wire the type packages: {tsconfig}"
    );
    assert!(
        out.stdout
            .contains("Your project is ready!\nTry running `nub index.ts`"),
        "summary names the next command: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains(
            "created basic\n\
             ├── package.json\n\
             ├── tsconfig.json\n\
             ├── index.ts\n\
             ├── .gitignore\n\
             ├── README.md\n\
             └── .git/"
        ),
        "summary renders the generated structure as a tree: {}",
        out.stdout
    );
}

#[test]
fn js_variant_skips_tsconfig_and_type_devdeps() {
    let dir = tmpdir("js");
    let out = run_init(
        &dir,
        &["-y", "--js", "--no-install", "--no-git", "--name", "My App"],
    );
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(dir.join("index.js").exists());
    assert!(!dir.join("tsconfig.json").exists(), "no tsconfig for JS");
    assert!(!dir.join(".git").exists(), "--no-git must skip git init");

    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(pkg["name"], "my-app", "--name is sanitized");
    assert!(pkg.get("devDependencies").is_none());
    assert_eq!(pkg["scripts"]["start"], "nub index.js");
    assert!(
        out.stdout.contains(
            "created my-app\n\
             ├── package.json\n\
             ├── index.js\n\
             ├── .gitignore\n\
             └── README.md"
        ),
        "no-git JS tree closes on README: {}",
        out.stdout
    );
}

/// `git check-ignore` is the only honest test of a `.gitignore`: the scaffold's
/// pattern list has a negation in it, so matching on file contents would pin
/// the spelling without proving the effect.
fn git_ignores(dir: &Path, path: &str) -> bool {
    Command::new("git")
        .args(["check-ignore", "-q", path])
        .current_dir(dir)
        .status()
        .expect("failed to spawn git check-ignore")
        .success()
}

#[test]
fn scoped_name_reaches_the_manifest_and_the_env_example_stays_tracked() {
    let dir = tmpdir("scoped");
    let out = run_init(&dir, &["-y", "--no-install", "--name", "@My Org/My App"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);

    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        pkg["name"], "@my-org/my-app",
        "the scope must survive sanitization"
    );

    for secret in [".env", ".env.local", ".env.production", ".env.development"] {
        assert!(git_ignores(&dir, secret), "{secret} must stay out of git");
    }
    assert!(
        !git_ignores(&dir, ".env.example"),
        ".env.example is the committed template — it must stay tracked"
    );
}

#[test]
fn refuses_existing_files_and_force_overwrites() {
    let dir = tmpdir("conflict");
    std::fs::write(dir.join("package.json"), "{\"name\":\"keep\"}").unwrap();
    let out = run_init(&dir, &["-y", "--no-install"]);
    assert_ne!(out.code, 0, "must refuse on conflict");
    assert!(
        out.stderr.contains("package.json") && out.stderr.contains("--force"),
        "refusal names the conflict and the override: {}",
        out.stderr
    );
    // Refusal is all-or-nothing: no sibling file may have been written.
    assert!(!dir.join("index.ts").exists());
    let kept = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(kept.contains("keep"), "refusal must not touch the file");

    let out = run_init(&dir, &["-y", "--no-install", "--force", "--no-git"]);
    assert_eq!(out.code, 0, "--force overwrites: {}", out.stderr);
    let pkg = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(!pkg.contains("keep"), "--force must replace the manifest");
}

#[test]
fn rejects_positionals_with_a_create_hint() {
    let dir = tmpdir("arg");
    let out = run_init(&dir, &["react-app"]);
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.contains("nubx create-react-app"),
        "the error hints the create flow: {}",
        out.stderr
    );
    assert!(!dir.join("package.json").exists(), "nothing scaffolded");
}

#[test]
fn empty_sanitizing_directory_name_falls_back_to_app() {
    // A non-Latin basename sanitizes to nothing; the manifest must never
    // carry an invalid `"name": ""`.
    let dir = tmpdir("unicode").join("中文项目");
    std::fs::create_dir_all(&dir).unwrap();
    let out = run_init(&dir, &["-y", "--no-install", "--no-git"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(pkg["name"], "app");
}

#[test]
fn non_tty_without_yes_takes_defaults_and_never_hangs() {
    // No `-y`: with piped stdin the prompts must self-skip to defaults —
    // the CI/piped contract.
    let dir = tmpdir("notty");
    let out = run_init(&dir, &["--no-install", "--no-git"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(dir.join("index.ts").exists(), "defaults to TypeScript");
}
