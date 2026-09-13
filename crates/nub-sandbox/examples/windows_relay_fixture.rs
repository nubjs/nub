//! Native functional fixture: run as an ordinary Windows user, without runtime adapters.
//! It exercises the production helper entry, not process-handle access rights.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--relay") => nub_sandbox::backend::serve_windows_egress_helper(),
        Some("--child") => fixture::child(),
        _ => fixture::parent(),
    }
}

#[cfg(windows)]
mod fixture {
    use base64::Engine as _;
    use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::time::Duration;

    const SECRET: &str = "NUB_RELAY_FIXTURE_SECRET";
    const VALUE: &str = "synthetic-relay-fixture-credential";

    fn request(token: &str, port: u16, target: &str) -> (TcpStream, String) {
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
        write!(
            socket,
            "CONNECT {target} HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
        )
        .unwrap();
        let mut reply = Vec::new();
        while !reply.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            reply.push(byte[0]);
            assert!(reply.len() < 65536);
        }
        (socket, String::from_utf8(reply).unwrap())
    }

    pub(super) fn child() {
        let url = std::env::var("HTTP_PROXY").unwrap();
        let (token, endpoint) = url
            .strip_prefix("http://")
            .unwrap()
            .split_once('@')
            .unwrap();
        let port: u16 = endpoint
            .strip_prefix("127.0.0.1:")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            request("wrong-command", port, "denied.example:443")
                .1
                .starts_with("HTTP/1.1 407")
        );
        assert!(
            request(token, port, "denied.example:443")
                .1
                .starts_with("HTTP/1.1 403")
        );
        if let Ok(ca) = std::env::var("SSL_CERT_FILE") {
            let pem = std::fs::read_to_string(&ca).expect("read-only public CA leaf");
            assert!(pem.contains("BEGIN CERTIFICATE"));
            assert!(!pem.contains("PRIVATE KEY"));
            assert!(std::fs::OpenOptions::new().write(true).open(&ca).is_err());
            assert_ne!(std::env::var(SECRET).unwrap(), VALUE);
            println!(
                "{}",
                json!({"ca": ca, "proxy": url, "marker": std::env::var(SECRET).unwrap()})
            );
        } else {
            let target = std::env::var("RELAY_FIXTURE_UPSTREAM").unwrap();
            let (mut socket, reply) = request(token, port, &target);
            assert!(reply.starts_with("HTTP/1.1 200"));
            socket.write_all(b"relay-ping").unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
            let mut reply = Vec::new();
            socket.read_to_end(&mut reply).unwrap();
            assert_eq!(reply, b"relay-pong");
            println!("{}", json!({"proxy": url}));
        }
    }

    pub(super) fn parent() {
        let exe = std::env::current_exe().unwrap();
        nub_sandbox::backend::set_windows_egress_helper_command(vec![
            exe.clone().into(),
            "--relay".into(),
        ]);
        let root = tempfile::Builder::new()
            .prefix("nub-relay-fixture-")
            .tempdir()
            .unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut ambient: BTreeMap<_, _> = std::env::vars()
            .filter(|(key, _)| {
                ["PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT"]
                    .contains(&key.to_ascii_uppercase().as_str())
            })
            .collect();
        ambient.insert(SECRET.to_string(), VALUE.to_string());
        let ctx = CompileCtx::new(
            Homes {
                home: root.path().join("withheld"),
                cache: root.path().join("cache"),
                tmp: root.path().join("tmp"),
                project: project.clone(),
            },
            project.clone(),
            ScopeCapabilities::approved(),
            ambient,
        );
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let upstream = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(20)))
                    .unwrap();
                let mut bytes = Vec::new();
                socket.read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, b"relay-ping");
                socket.write_all(b"relay-pong").unwrap();
            }
        });
        // This standalone process owns the fixture environment; acquisition reads it once.
        unsafe { std::env::set_var(SECRET, VALUE) };
        for broker in [false, true] {
            let mut raw = json!({"fs": {(project.display().to_string()): "rw", (exe.display().to_string()): "r", "$tmp": "rw"}, "net": ["localhost"]});
            if broker {
                raw["secrets"] = json!({SECRET: {"brokerTo": ["localhost"]}});
            }
            let mut policy = compile(&raw, &ctx).unwrap();
            policy.env.constructed.insert(
                "RELAY_FIXTURE_UPSTREAM".into(),
                format!("localhost:{upstream_port}"),
            );
            let sandbox = Sandbox::new(&policy).unwrap();
            let mut reports = Vec::new();
            for _ in 0..2 {
                let prepared = sandbox
                    .prepare(CommandSpec::new(&exe).args(["--child"]).cwd(&project))
                    .unwrap();
                assert!(
                    prepared.degradation.lost.is_empty(),
                    "{:?}",
                    prepared.degradation
                );
                let output = prepared.output().unwrap();
                assert!(
                    output.status.success(),
                    "stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                reports.push(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap());
            }
            assert_ne!(
                reports[0]["proxy"], reports[1]["proxy"],
                "command-local channel credentials"
            );
            if broker {
                assert_eq!(reports[0]["ca"], reports[1]["ca"]);
                assert_eq!(reports[0]["marker"], reports[1]["marker"]);
                let ca = PathBuf::from(reports[0]["ca"].as_str().unwrap());
                drop(sandbox);
                assert!(!ca.exists(), "session CA cleanup");
            }
        }
        upstream.join().unwrap();
        unsafe { std::env::remove_var(SECRET) };
        println!("WINDOWS_PARENT_RELAY_OK connection auth denial CA marker reuse cleanup");
    }
}
