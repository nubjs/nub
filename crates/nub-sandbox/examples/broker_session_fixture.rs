//! Native TLS credential-broker fixture, including the actual confined child and helper entry.
//! Trust changes are confined to this fixture's environment; no OS trust store is modified.

#[path = "../tests/common/tool_output.rs"]
mod tool_output;

use base64::Engine as _;
use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECRET: &str = "NUB_NATIVE_BROKER_SECRET";
const PORT: &str = "NUB_NATIVE_BROKER_PORT";
const BAD_PORT: &str = "NUB_NATIVE_BROKER_BAD_PORT";
const WITHHELD: &str = "NUB_NATIVE_BROKER_WITHHELD";

fn main() {
    match std::env::args().nth(1).as_deref() {
        #[cfg(windows)]
        Some("--relay") => nub_sandbox::serve_windows_egress_helper(),
        Some("--child") => child(),
        _ => parent(),
    }
}

fn deadlines(socket: &TcpStream) {
    socket
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(15)))
        .unwrap();
}

fn head(stream: &mut impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        assert!(bytes.len() < 64 * 1024, "bounded fixture header");
    }
    Ok(String::from_utf8(bytes).unwrap())
}

fn client_config(certs: Vec<CertificateDer<'static>>) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots.add(cert).unwrap();
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

fn tls_request(
    socket: TcpStream,
    config: Arc<rustls::ClientConfig>,
    marker: &str,
) -> io::Result<String> {
    tls_request_to(socket, config, marker, "localhost")
}

fn tls_request_to(
    socket: TcpStream,
    config: Arc<rustls::ClientConfig>,
    marker: &str,
    server_name: &'static str,
) -> io::Result<String> {
    deadlines(&socket);
    let conn =
        rustls::ClientConnection::new(config, ServerName::try_from(server_name).unwrap()).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, socket);
    write!(
        tls,
        "POST /{marker} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {marker}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{marker}",
        marker.len()
    )?;
    tls.flush()?;
    let reply = head(&mut tls)?;
    let mut body = [0; 2];
    tls.read_exact(&mut body)?;
    assert_eq!(&body, b"ok");
    Ok(reply)
}

#[cfg(not(target_os = "linux"))]
fn tunnel(token: &str, endpoint: &str, target: &str) -> (TcpStream, String) {
    let mut socket = TcpStream::connect(endpoint).unwrap();
    deadlines(&socket);
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
    write!(
        socket,
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
    )
    .unwrap();
    let reply = head(&mut socket).unwrap();
    (socket, reply)
}

fn child() {
    #[cfg(windows)]
    assert!(nub_sandbox::windows_token_report().contains("is_appcontainer=true"));
    assert!(std::fs::read(std::env::var_os(WITHHELD).unwrap()).is_err());
    let marker = std::env::var(SECRET).unwrap();
    assert!(
        marker.starts_with("nub-credential-v1-"),
        "only an opaque marker enters the child"
    );
    let bundle = std::fs::read(std::env::var_os("SSL_CERT_FILE").unwrap()).unwrap();
    assert!(!String::from_utf8_lossy(&bundle).contains("PRIVATE KEY"));
    let certs = pem::parse_many(bundle)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.tag() == "CERTIFICATE")
        .map(|entry| CertificateDer::from(entry.into_contents()))
        .collect();
    let config = client_config(certs);
    #[cfg(not(target_os = "linux"))]
    let proxy = std::env::var("HTTPS_PROXY").unwrap();
    #[cfg(not(target_os = "linux"))]
    let (token, endpoint) = {
        let (token, endpoint) = proxy
            .strip_prefix("http://")
            .unwrap()
            .split_once('@')
            .unwrap();
        assert!(
            tunnel("wrong-token", endpoint, "denied.example:443")
                .1
                .starts_with("HTTP/1.1 407")
        );
        assert!(
            tunnel(token, endpoint, "denied.example:443")
                .1
                .starts_with("HTTP/1.1 403")
        );
        (token, endpoint)
    };
    #[cfg(target_os = "linux")]
    {
        // Linux routes raw connects in the supervisor; no proxy token enters the child.
        assert!(std::env::var_os("HTTPS_PROXY").is_none());
        let port = std::env::var(PORT).unwrap().parse::<u16>().unwrap();
        let socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        assert!(
            tls_request_to(socket, Arc::clone(&config), &marker, "denied.example").is_err(),
            "an allowed address must not admit an unapproved TLS hostname"
        );
    }
    for (key, accepted) in [(PORT, true), (BAD_PORT, false)] {
        let port = std::env::var(key).unwrap().parse::<u16>().unwrap();
        #[cfg(target_os = "linux")]
        let socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        #[cfg(not(target_os = "linux"))]
        let socket = {
            let (socket, reply) = tunnel(token, endpoint, &format!("localhost:{port}"));
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
            socket
        };
        let response = tls_request(socket, Arc::clone(&config), &marker);
        if accepted {
            assert!(response.unwrap().starts_with("HTTP/1.1 200"));
        } else {
            assert!(
                response.is_err(),
                "an untrusted upstream must not receive a credential"
            );
        }
    }
    println!("BROKER_NATIVE_CHILD_OK");
}

fn upstream(
    key: CertifiedKey,
    secret: Option<String>,
    count: usize,
) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![key.cert.der().clone()],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.key_pair.serialize_der())),
    )
    .unwrap();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let config = Arc::new(config);
        let mut events = Vec::new();
        for index in 0..count {
            let deadline = Instant::now() + Duration::from_secs(40);
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "upstream accept deadline");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("upstream accept: {error}"),
                }
            };
            // Darwin inherits O_NONBLOCK from the accepting listener; TLS I/O here is blocking.
            socket.set_nonblocking(false).unwrap();
            deadlines(&socket);
            let conn = rustls::ServerConnection::new(Arc::clone(&config)).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, socket);
            let request = head(&mut tls);
            if secret.is_none() && index > 0 {
                assert!(request.is_err(), "untrusted TLS upstream received HTTP");
                events.push("untrusted-upstream-blocked".into());
                continue;
            }
            let request = request.unwrap();
            let marker = request
                .lines()
                .next()
                .unwrap()
                .strip_prefix("POST /")
                .unwrap()
                .strip_suffix(" HTTP/1.1")
                .unwrap();
            let expected = secret.as_deref().unwrap_or("direct-control");
            assert!(request.contains(&format!("Authorization: Bearer {expected}\r\n")));
            let mut body = vec![0; marker.len()];
            tls.read_exact(&mut body).unwrap();
            assert_eq!(body, marker.as_bytes(), "URL/body markers stay opaque");
            if secret.is_some() {
                assert!(marker.starts_with("nub-credential-v1-"));
                assert_ne!(marker, expected);
            }
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            tls.conn.send_close_notify();
            tls.flush().unwrap();
            events.push("verified-request".into());
        }
        events
    });
    (port, handle)
}

fn parent() {
    let exe = std::env::current_exe().unwrap();
    nub_sandbox::set_windows_egress_helper_command(vec![exe.clone().into(), "--relay".into()]);
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap();
    let root = tempfile::Builder::new()
        .prefix(".nub-broker-native-")
        .tempdir_in(home)
        .unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let withheld = root.path().join("ungranted-canary");
    std::fs::write(&withheld, b"read-withheld").unwrap();
    assert_eq!(std::fs::read(&withheld).unwrap(), b"read-withheld");
    let good = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let bad = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let trust = root.path().join("fixture-root.pem");
    std::fs::write(&trust, good.cert.pem()).unwrap();
    let control_config = client_config(vec![bad.cert.der().clone()]);
    let mut bytes = [0; 32];
    getrandom::getrandom(&mut bytes).unwrap();
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    // This standalone process owns its environment. All mutation precedes worker threads.
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &trust);
        std::env::set_var("SSL_CERT_DIR", "");
        std::env::set_var(SECRET, &secret);
    }
    let (good_port, good_upstream) = upstream(good, Some(secret), 2);
    let (bad_port, bad_upstream) = upstream(bad, None, 3);
    let direct = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, bad_port)).unwrap();
    assert!(
        tls_request(direct, control_config, "direct-control")
            .unwrap()
            .starts_with("HTTP/1.1 200")
    );
    let environment: BTreeMap<_, _> = std::env::vars()
        .filter(|(key, _)| {
            ["PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT"]
                .contains(&key.to_ascii_uppercase().as_str())
        })
        .collect();
    let mut ambient = environment.clone();
    ambient.insert(SECRET.into(), std::env::var(SECRET).unwrap());
    let ctx = CompileCtx::new(
        Homes {
            home: root.path().join("withheld-home"),
            cache: root.path().join("cache"),
            tmp: root.path().join("tmp"),
            project: project.clone(),
        },
        project.clone(),
        ScopeCapabilities::approved(),
        ambient,
    );
    // A raw loopback connect has no observed DNS name on Linux. Admit its exact address while
    // retaining the separate SNI hostname gate; this does not grant private ranges generally.
    let network = if cfg!(target_os = "linux") {
        json!(["localhost", "127.0.0.1"])
    } else {
        json!(["localhost"])
    };
    let mut policy = compile(&json!({
        "fs": {(project.display().to_string()): "rw", (exe.display().to_string()): "r", "$tmp": "rw"},
        "net": network, "secrets": {SECRET: {"brokerTo": ["localhost"]}}
    }), &ctx).unwrap();
    policy.env.constructed.extend(environment);
    policy.env.constructed.extend([
        (PORT.into(), good_port.to_string()),
        (BAD_PORT.into(), bad_port.to_string()),
        (WITHHELD.into(), withheld.display().to_string()),
    ]);
    let sandbox = Sandbox::new(&policy).unwrap();
    for _ in 0..2 {
        let prepared = sandbox
            .prepare(
                CommandSpec::new(&exe)
                    .args(["--child"])
                    .cwd(&project)
                    .redact_stdout(true)
                    .redact_stderr(true),
            )
            .unwrap();
        assert!(
            prepared.degradation.lost.is_empty(),
            "{:?}",
            prepared.degradation
        );
        let output = tool_output::output(prepared);
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("BROKER_NATIVE_CHILD_OK"));
    }
    drop(sandbox);
    assert_eq!(
        good_upstream.join().unwrap(),
        ["verified-request", "verified-request"]
    );
    assert_eq!(
        bad_upstream.join().unwrap(),
        [
            "verified-request",
            "untrusted-upstream-blocked",
            "untrusted-upstream-blocked"
        ]
    );
    println!(
        "BROKER_NATIVE_SESSION_OK verified TLS injection untrusted upstream denial host denial confined child reuse"
    );
}
