//! The default-export `fetch` handler: `nub <file>` serves an entry whose default
//! export is an object with a `fetch` method, over the cross-runtime contract
//! Cloudflare Workers, Bun, Deno and Vercel share.
//!
//! Every served fixture is launched with `PORT=0`, so the kernel picks a free port and
//! the test reads the chosen one off the `Listening on …` line. Nothing here binds a
//! fixed port, which is what lets these run beside each other and beside the rest of
//! the suite. Requests go over a raw socket rather than a client crate: the contract
//! being asserted is bytes on the wire — a repeated `Set-Cookie`, a chunked body, an
//! absent body on a 204 — and a client that normalizes those away would hide exactly
//! the defects this file exists to catch.
//!
//! The mechanism is tier-independent (both preload entries call the same installer),
//! so the host Node covers whichever tier it falls on.
//!
//! Three tests here assert that something is NOT served, and an absence assertion
//! cannot fail on its own. Their positive control is the rest of this file: each one
//! runs a fixture that another test proves does get served, so a green absence means
//! the exclusion held rather than that the feature was never wired. Measured with the
//! installer stubbed out: twenty of these go red, and exactly those three stay green.
//!
//! One invariant this file deliberately does NOT test: that the detection pass never
//! evaluates the entry ahead of the user's preloads. It cannot be tested here, because
//! the ordering only becomes racy on the tier where nub's preload is itself an
//! `--import`, so a single-Node run passes structurally whatever the code does.
//! `version_tiers::cjs_preload_runs_once_after_hooks_on_both_tiers` is the guard —
//! it runs the tier matrix, and it is what caught the regression.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Generous on purpose: this box routinely runs a build fleet, and a starved host is
/// not a defect in the feature.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ or release/
    path.push("nub");
    path
}

fn fixture(name: &str) -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    Path::new(&manifest)
        .join("../../tests/fixtures/fetch-handler")
        .join(name)
}

/// A running server, killed when the test drops it — including when an assertion
/// panics, which is what keeps a failure from leaking a listener into the next run.
struct Server {
    child: Child,
    /// The host the server reported, which is the one to connect to: `listen(port,
    /// "localhost")` binds whatever `localhost` resolves to, and on an IPv6 host that
    /// is `::1`, where a hardcoded `127.0.0.1` is refused.
    host: String,
    port: u16,
    /// The `Listening on …` line verbatim, which is where the chosen host shows up.
    startup_line: String,
    stdout: mpsc::Receiver<String>,
    stderr: mpsc::Receiver<String>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(name: &str, env: &[(&str, &str)]) -> Server {
    let f = fixture(name);
    launch(f.parent().unwrap(), &[f.to_str().unwrap()], env, name)
}

/// `nub <args…>` in `dir`, waited for until it reports its listener.
fn launch(dir: &Path, args: &[&str], env: &[(&str, &str)], name: &str) -> Server {
    let mut cmd = Command::new(nub_binary());
    cmd.args(args)
        .current_dir(dir)
        .env("PORT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("failed to spawn nub");
    // Both pipes drain through a thread: a live server never closes either, so anything
    // that reads one to EOF hangs instead of failing.
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    // Built before the wait below, so `Drop` owns the process from here on. A startup
    // timeout panics, and without that ownership the panic would abandon a server still
    // holding its port.
    let mut server = Server {
        child,
        host: String::new(),
        port: 0,
        startup_line: String::new(),
        stdout,
        stderr,
    };
    let mut seen = Vec::new();
    loop {
        let line = server.stderr.recv_timeout(STARTUP_TIMEOUT).unwrap_or_else(|_| {
            panic!(
                "{name}: no `Listening on` line within {STARTUP_TIMEOUT:?}; stderr so far: {seen:?}"
            )
        });
        if let Some((host, port)) = listening_address(&line) {
            server.host = host;
            server.port = port;
            server.startup_line = line;
            return server;
        }
        seen.push(line);
    }
}

fn drain<R: std::io::Read + Send + 'static>(pipe: R) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// The host and port out of `Listening on http://<host>:<port>`, with an IPv6 literal's
/// brackets stripped so the result is what `TcpStream::connect` wants.
fn listening_address(line: &str) -> Option<(String, u16)> {
    let authority = line.strip_prefix("Listening on http://")?;
    let (host, port) = authority.rsplit_once(':')?;
    let port = port.trim().parse().ok()?;
    Some((
        host.trim_matches(|c| c == '[' || c == ']').to_string(),
        port,
    ))
}

/// Run a fixture to completion and hand back `(stdout, stderr, exit code)`.
fn run_to_completion(name: &str, args: &[&str], env: &[(&str, &str)]) -> (String, String, i32) {
    let f = fixture(name);
    let mut cmd = Command::new(nub_binary());
    cmd.args(args)
        .arg(&f)
        .current_dir(f.parent().unwrap())
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

struct Reply {
    status: u16,
    reason: String,
    /// Lowercased names, in the order the server sent them, so a repeated header
    /// stays visible as separate entries.
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values(name).first().copied()
    }
}

/// One HTTP/1.1 exchange on a fresh connection. `Connection: close` means the reply
/// ends at EOF, so nothing here has to guess a length.
fn request(s: &Server, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let port = s.port;
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);

    let mut sock = TcpStream::connect((s.host.as_str(), port))
        .unwrap_or_else(|e| panic!("connect to {}:{port}: {e}", s.host));
    sock.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    sock.write_all(req.as_bytes()).expect("write request");
    sock.flush().unwrap();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).expect("read reply");
    parse(&raw)
}

fn get(s: &Server, path: &str) -> Reply {
    request(s, "GET", path, &[], "")
}

fn parse(raw: &[u8]) -> Reply {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("reply must have a header terminator");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body_bytes = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line");
    let mut parts = status_line.splitn(3, ' ');
    parts.next();
    let status: u16 = parts
        .next()
        .expect("status code")
        .parse()
        .expect("numeric status");
    let reason = parts.next().unwrap_or("").to_string();

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();

    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.contains("chunked"));
    let body = if chunked {
        dechunk(body_bytes)
    } else {
        String::from_utf8_lossy(body_bytes).into_owned()
    };
    Reply {
        status,
        reason,
        headers,
        body,
    }
}

/// Node answers a streamed response with `Transfer-Encoding: chunked`, so the wire
/// form has to be decoded before the body can be compared.
fn dechunk(mut rest: &[u8]) -> String {
    let mut out = Vec::new();
    while let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") {
        let size_line = String::from_utf8_lossy(&rest[..eol]).into_owned();
        let size = usize::from_str_radix(size_line.split(';').next().unwrap().trim(), 16)
            .unwrap_or_else(|_| panic!("bad chunk size {size_line:?}"));
        rest = &rest[eol + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── The served shapes ────────────────────────────────────────────────

/// The portable contract: an ES-module default export with a `fetch` method taking a
/// `Request` and returning a `Response`, reachable over HTTP with no boilerplate.
#[test]
fn esm_default_export_is_served() {
    let s = start("server.mjs", &[]);
    let r = get(&s, "/greet?q=hi");
    assert_eq!(r.status, 200, "body was {:?}", r.body);
    assert_eq!(r.body, "hello from /greet?hi");
}

/// A `.ts` entry has to be transpiled before its default export exists at all, so
/// this covers the transpile path as much as the handler one.
#[test]
fn typescript_entry_is_served() {
    let s = start("server.ts", &[]);
    let r = get(&s, "/nub");
    assert_eq!(r.status, 200, "body was {:?}", r.body);
    assert_eq!(r.body, "hello nub");
}

/// The CommonJS spelling, `module.exports = { fetch }`, which is what a `.ts` entry
/// in a package without `"type": "module"` also transpiles into.
#[test]
fn commonjs_entry_is_served() {
    let s = start("server.cjs", &[]);
    assert_eq!(get(&s, "/").body, "hello from commonjs");
}

/// A handler assembled after a top-level await is still found: the detection pass
/// awaits the entry's own module job rather than assuming evaluation has landed.
#[test]
fn top_level_await_settles_before_detection() {
    let s = start("tla.mjs", &[]);
    assert_eq!(get(&s, "/").body, "ready after await");
}

/// A preload on its own `--import` token may still be pending when the detection
/// pass first runs, so the entry cannot be imported then; and a preload that holds a
/// timer keeps the loop from ever draining, so waiting for idleness would never
/// serve. The listener binds anyway, on the load hook's word that Node has started
/// the entry — for an ES module entry, for a CommonJS one the ESM loader owns once an
/// `--import` is present, and when it is the entry's own body holding the loop.
#[test]
fn a_foreign_preload_holding_the_loop_does_not_block_binding() {
    let holds_loop = "data:text/javascript,setInterval(()=>{},1000);";
    let quiet = "data:text/javascript,globalThis.quiet=1;";
    for (preload, name, body) in [
        (holds_loop, "server.mjs", "hello from /?"),
        (holds_loop, "server.cjs", "hello from commonjs"),
        (quiet, "holds-loop.mjs", "holds the loop"),
    ] {
        let f = fixture(name);
        let s = launch(
            f.parent().unwrap(),
            &["--import", preload, f.to_str().unwrap()],
            &[],
            name,
        );
        assert_eq!(
            get(&s, "/").body,
            body,
            "{name} behind `--import {preload}`"
        );
    }
}

/// A preload that imports the entry ahead of the program under a query of its own is
/// a second module, and a foreign hook may give Node's own import of the entry a
/// query too. Only the load Node makes after resolving the entry as its main — with
/// no parent — is the entry's; matched on pathname alone, the preload's copy was
/// served in its place. `version_tiers` runs the same shape on the loader-worker
/// tiers.
#[test]
fn a_preload_import_of_the_entry_is_not_served_in_its_place() {
    let dir = fixture("../entry-url-rewrite");
    let s = launch(
        &dir,
        &[
            "--import",
            "./rewrite.mjs",
            "--import",
            "./warm.mjs",
            "holds.mjs",
        ],
        &[],
        "entry beside a preload's copy of it",
    );
    assert_eq!(get(&s, "/").body, "held:?v=1");
    let mut lines = Vec::new();
    while let Ok(line) = s.stdout.recv_timeout(Duration::from_millis(500)) {
        lines.push(line);
    }
    assert_eq!(
        lines,
        ["holds:?warm", "holds:?v=1"],
        "the preload's copy first, the entry once"
    );
}

// ── The request and response bridge ──────────────────────────────────

/// Method, headers and a request body reach the handler, and the handler's status,
/// headers and body come back out.
#[test]
fn request_and_response_round_trip() {
    let s = start("server.mjs", &[]);
    let r = request(&s, "POST", "/echo", &[("x-send", "inbound")], "body bytes");
    assert_eq!(r.status, 201, "body was {:?}", r.body);
    assert_eq!(
        r.value("x-method"),
        Some("POST"),
        "method reached the handler"
    );
    assert_eq!(
        r.value("x-seen"),
        Some("inbound"),
        "request header reached the handler"
    );
    assert_eq!(
        r.body, "body bytes",
        "request body streamed through to the response"
    );
}

/// Two `Set-Cookie` headers stay two headers. The `Headers` iterator joins repeats
/// with a comma, which corrupts any cookie carrying an `Expires` date, so the writer
/// reads cookies through `getSetCookie()` instead.
#[test]
fn repeated_set_cookie_stays_separate() {
    let s = start("server.mjs", &[]);
    let r = get(&s, "/cookies");
    let cookies = r.values("set-cookie");
    assert_eq!(
        cookies,
        vec!["a=1; Expires=Wed, 21 Oct 2026 07:28:00 GMT", "b=2"],
        "each Set-Cookie needs its own header line"
    );
}

/// A `ReadableStream` body is piped through rather than buffered, and arrives whole.
#[test]
fn streamed_body_arrives_complete() {
    let s = start("server.mjs", &[]);
    let r = get(&s, "/stream");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "chunk-one\nchunk-two\n");
}

/// A 204 carries no body, and a HEAD gets the status and headers with none either.
#[test]
fn bodyless_responses_send_no_body() {
    let s = start("server.mjs", &[]);
    let no_content = get(&s, "/empty");
    assert_eq!(no_content.status, 204);
    assert_eq!(no_content.body, "", "a 204 must not carry a body");

    let head = request(&s, "HEAD", "/greet", &[], "");
    assert_eq!(head.status, 200);
    assert_eq!(head.body, "", "a HEAD response must not carry a body");
}

/// A non-default status reason survives the hand-off to `ServerResponse`.
#[test]
fn status_and_reason_are_preserved() {
    let s = start("server.mjs", &[]);
    let r = get(&s, "/teapot");
    assert_eq!(r.status, 418);
    assert_eq!(
        r.reason, "I'm a Teapot",
        "Node supplies the reason for a known status"
    );
    assert_eq!(r.body, "short and stout");
}

/// A handler that throws, and one that returns something that is not a `Response`,
/// both answer 500 rather than hanging the connection — and both report the cause,
/// the way an uncaught error in user code does.
#[test]
fn handler_faults_answer_500_and_report() {
    let s = start("server.mjs", &[]);
    assert_eq!(get(&s, "/throw").status, 500);
    assert_eq!(get(&s, "/not-a-response").status, 500);

    let mut reported = String::new();
    // Two faults, so two reports; read until both have arrived rather than assuming
    // either ordering.
    while !(reported.contains("handler blew up") && reported.contains("must return a Response")) {
        match s.stderr.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => reported.push_str(&format!("{line}\n")),
            Err(_) => panic!("faults were not reported on stderr; saw: {reported}"),
        }
    }
    // The server is still up after both.
    assert_eq!(get(&s, "/still-here").status, 200);
}

// ── What must NOT be served ──────────────────────────────────────────

/// A script with no default export, and one whose default export is not a handler,
/// both run and exit exactly as they do on plain Node. The shape of the user's own
/// module is the entire gate, so this is the additivity guarantee.
#[test]
fn a_file_without_the_shape_is_not_served() {
    for (name, marker) in [
        ("plain.mjs", "plain script ran"),
        ("no-fetch.mjs", "no-fetch script ran"),
    ] {
        let (stdout, stderr, code) = run_to_completion(name, &[], &[("PORT", "0")]);
        assert_eq!(code, 0, "{name} must exit cleanly; stderr: {stderr}");
        assert!(stdout.contains(marker), "{name} must still run: {stdout:?}");
        assert!(
            !stderr.contains("Listening on"),
            "{name} must not bind a listener: {stderr:?}"
        );
    }
}

/// Compat mode is plain Node, and plain Node does nothing with a default-exported
/// `fetch`. Both spellings of the opt-out are covered because they are independent
/// signals: the flag is per-invocation, the variable is tree-wide.
#[test]
fn compat_mode_does_not_serve() {
    for (args, env) in [
        (["--node"].as_slice(), [("PORT", "0")].as_slice()),
        (
            [].as_slice(),
            [("PORT", "0"), ("NODE_COMPAT", "1")].as_slice(),
        ),
    ] {
        let (_stdout, stderr, code) = run_to_completion("server.mjs", args, env);
        assert_eq!(code, 0, "must exit cleanly; stderr: {stderr}");
        assert!(
            !stderr.contains("Listening on"),
            "{args:?} / {env:?} must not bind a listener: {stderr:?}"
        );
    }
}

/// The signal is deleted before user code runs, so a child never inherits it. Without
/// that, a server which forks a worker pool would hand every worker its own port —
/// and here the child, being the same file, would report a second `Listening on`.
#[test]
fn the_serve_signal_does_not_reach_a_child() {
    let s = start("spawns-child.mjs", &[]);
    assert_eq!(get(&s, "/").body, "parent handler");

    // Read through the channel, not to EOF: this server stays up, so its stdout never
    // closes. The child reports its own outcome on the parent's stdout.
    let mut lines = Vec::new();
    while !lines
        .iter()
        .any(|l: &String| l.starts_with("child-stdout:"))
    {
        match s.stdout.recv_timeout(Duration::from_secs(60)) {
            Ok(line) => lines.push(line),
            Err(_) => panic!("the child never reported; parent stdout so far: {lines:?}"),
        }
    }
    assert!(
        lines.contains(&"child-stdout:child ran to completion".to_string()),
        "the child must run and exit rather than bind: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("Listening on")),
        "the child must not report a listener: {lines:?}"
    );
}

/// `node <file>` through the PATH hijack keeps meaning what `node` means. Augmenting
/// the hijack is deliberate, but binding a port is a visible behavior change, so a
/// script's `node server.js` stays a script run while `nub server.js` serves.
#[cfg(unix)]
#[test]
fn the_node_hijack_does_not_serve() {
    let dir = std::env::temp_dir().join(format!("nub-fetch-hijack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("node");
    let _ = std::fs::remove_file(&shim);
    std::os::unix::fs::symlink(nub_binary(), &shim).unwrap();

    let f = fixture("server.mjs");
    let out = Command::new(&shim)
        .arg(&f)
        .current_dir(f.parent().unwrap())
        .env("PORT", "0")
        .stdin(Stdio::null())
        .output()
        .expect("failed to spawn the node shim");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.status.success(),
        "the shim run must exit cleanly: {stderr}"
    );
    assert!(
        !stderr.contains("Listening on"),
        "`node <file>` must not bind a listener: {stderr:?}"
    );
}

// ── Behind a wrapper ────────────────────────────────────────────────

/// A project whose environment an external loader owns: a `.env.schema` beside a
/// `#!/usr/bin/env node` loader in `node_modules/.bin`, which is the shape `varlock`
/// ships. Nub spawns the loader in FRONT of Node, and the loader — itself a Node
/// process, running nub's inherited preload — copies its environment into the
/// application. The handler echoes the variable the loader adds, so a matching
/// response proves it was served from a process launched THROUGH the loader rather
/// than from one nub spawned directly.
#[cfg(unix)]
fn wrapped_project(tag: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("nub-fetch-wrapped-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bin = dir.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{ "name": "wrapped", "version": "1.0.0", "type": "module" }"#,
    )
    .unwrap();
    std::fs::write(dir.join(".env.schema"), "# ---\nA=1\n").unwrap();
    std::fs::write(
        dir.join("server.mjs"),
        "export default {\n  fetch() {\n    return new Response(`FROM_LOADER=${process.env.FROM_LOADER ?? \"<unset>\"}`);\n  },\n};\n",
    )
    .unwrap();
    let loader = bin.join("varlock");
    std::fs::write(
        &loader,
        "#!/usr/bin/env node\n\
         const { spawnSync } = require(\"node:child_process\");\n\
         const argv = process.argv.slice(2);\n\
         const cmd = argv.slice(argv.indexOf(\"--\") + 1);\n\
         const res = spawnSync(cmd[0], cmd.slice(1), { stdio: \"inherit\", env: { ...process.env, FROM_LOADER: \"yes\" } });\n\
         process.exit(res.status ?? 1);\n",
    )
    .unwrap();
    std::fs::set_permissions(&loader, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// The serve signal names the application, not whatever process nub spawns. The
/// loader in front of Node runs nub's preload too, and a signal it consumed there
/// never reached the application, which then ran as a plain script.
#[cfg(unix)]
#[test]
fn a_handler_behind_an_env_owner_loader_is_served() {
    let dir = wrapped_project("run");
    let s = launch(&dir, &["server.mjs"], &[], "behind the loader");
    assert_eq!(get(&s, "/").body, "FROM_LOADER=yes");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Same through `nub watch`, whose supervisor re-execs the child with the signal in
/// its environment and the loader still in front.
#[cfg(unix)]
#[test]
fn a_watched_handler_behind_an_env_owner_loader_is_served() {
    let dir = wrapped_project("watch");
    let s = launch(
        &dir,
        &["watch", "server.mjs"],
        &[],
        "behind the loader, watched",
    );
    assert_eq!(get(&s, "/").body, "FROM_LOADER=yes");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Only the entry identifies the application, and an application argument is just
/// an argument, whatever it holds. One that names the loader's own bin must not let
/// the loader — whose `argv[1]` IS that bin — claim the signal on that match; and
/// one carrying a control byte must not split the signal's record of the argv, which
/// once put the entry at the wrong position and served nothing.
#[cfg(unix)]
#[test]
fn an_application_argument_cannot_disturb_the_claim() {
    for (tag, args) in [
        ("argv", &["server.mjs", "node_modules/.bin/varlock"][..]),
        ("bytes", &["server.mjs", "a\x1fb", "--", "c"][..]),
    ] {
        let dir = wrapped_project(tag);
        let s = launch(
            &dir,
            args,
            &[],
            &format!("behind the loader, args {args:?}"),
        );
        assert_eq!(get(&s, "/").body, "FROM_LOADER=yes", "args {args:?}");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── Address selection ───────────────────────────────────────────────

/// `PORT` outranks a `port` the source committed, because an environment that sets it
/// is a platform placing the process. With `PORT` unset the export's own value is
/// honored.
#[test]
fn port_precedence_puts_the_environment_first() {
    let s = start("configured.mjs", &[]);
    assert_ne!(s.port, 41999, "PORT=0 must outrank the export's `port`");
    assert_eq!(get(&s, "/").body, "configured");

    // The export's value, with nothing in the environment to outrank it. Bound by the
    // test first so the fixture's own literal is the thing that fails.
    let held = TcpListener::bind(("127.0.0.1", 41999));
    if held.is_ok() {
        drop(held);
        let f = fixture("configured.mjs");
        let mut child = Command::new(nub_binary())
            .arg(&f)
            .current_dir(f.parent().unwrap())
            .env_remove("PORT")
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let err = child.stderr.take().unwrap();
        let line = BufReader::new(err)
            .lines()
            .map_while(Result::ok)
            .find(|l| l.starts_with("Listening on"));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            line.as_deref(),
            Some("Listening on http://127.0.0.1:41999"),
            "the export's `port` and `hostname` are honored when the environment is silent"
        );
    }
}

/// `HOST` outranks the export's `hostname`. The fixture asks for `127.0.0.1` and the
/// environment asks for `localhost`: the same interface, so the server answers either
/// way, and the reported host is what says which source won.
#[test]
fn host_env_outranks_the_export() {
    let s = start("configured.mjs", &[("HOST", "localhost")]);
    assert_eq!(get(&s, "/").body, "configured");
    let reported = format!("Listening on http://localhost:{}", s.port);
    assert_eq!(
        s.startup_line, reported,
        "HOST must outrank the export's `hostname`"
    );
}

/// A port already in use is fatal, matching Bun and Deno. Quietly serving a port
/// nobody asked for is worse than stopping, and a dev loop retries in a second.
#[test]
fn a_taken_port_is_fatal() {
    let held = TcpListener::bind(("127.0.0.1", 0)).expect("bind a port to hold");
    let taken = held.local_addr().unwrap().port();
    let (_stdout, stderr, code) = run_to_completion(
        "server.mjs",
        &[],
        &[("PORT", &taken.to_string()), ("HOST", "127.0.0.1")],
    );
    assert_eq!(
        code, 1,
        "a failed bind must exit non-zero; stderr: {stderr}"
    );
    assert!(
        stderr.contains("already in use"),
        "the failure must name the cause: {stderr:?}"
    );
}

/// A port the parser rejects fails with the source named, before any socket work.
#[test]
fn an_unusable_port_names_its_source() {
    let (_stdout, stderr, code) = run_to_completion("server.mjs", &[], &[("PORT", "not-a-port")]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("PORT must be an integer"), "got {stderr:?}");
}
