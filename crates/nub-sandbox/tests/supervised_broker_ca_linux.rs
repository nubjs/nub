//! End-to-end delivery of a TLS broker's public trust bundle into the Linux supervisor.
//!
//! The child is re-entered under the real `Sandbox` API: this catches the seam between
//! session proxy acquisition, the bespoke supervised fork, its descriptor sweep, Landlock,
//! and the environment vector passed to `execve`.
#![cfg(target_os = "linux")]

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

const CASE: &str = "NUB_SUPERVISED_BROKER_CA_CASE";
const ROOT: &str = "NUB_SUPERVISED_BROKER_CA_ROOT";
const SECRET: &str = "NUB_SUPERVISED_BROKER_CA_SECRET";
const CA_ENV_KEYS: &[&str] = &[
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "PIP_CERT",
    "NPM_CONFIG_CAFILE",
    "npm_config_cafile",
    "CARGO_HTTP_CAINFO",
    "AWS_CA_BUNDLE",
    "DENO_CERT",
];

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[test]
fn supervised_broker_ca_child() {
    if std::env::var_os(CASE).as_deref() != Some(std::ffi::OsStr::new("child")) {
        return;
    }

    let root = PathBuf::from(std::env::var_os(ROOT).expect("fixture root"));
    let marker = std::env::var(SECRET).expect("broker marker is constructed for the child");
    assert!(
        marker.starts_with("nub-credential-v1-"),
        "child receives the opaque broker marker, never the plaintext secret"
    );
    assert_ne!(
        marker, "broker-secret",
        "broker secret must not enter the child's constructed environment"
    );
    let bundle = PathBuf::from(std::env::var_os("SSL_CERT_FILE").expect("broker CA path"));
    for key in CA_ENV_KEYS {
        assert_eq!(
            std::env::var_os(key).as_deref(),
            Some(bundle.as_os_str()),
            "{key} must name the broker's one public trust bundle"
        );
    }

    let contents = std::fs::read_to_string(&bundle).expect("confined child reads broker CA");
    assert!(contents.contains("-----BEGIN CERTIFICATE-----"));
    assert!(
        !contents.contains("PRIVATE KEY"),
        "the child must receive public certificates, never the MITM private key"
    );
    let mut writable = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&bundle)
        .expect("the public descriptor may be reopened but remains sealed");
    assert!(
        writable.write_all(b"x").is_err(),
        "F_SEAL_WRITE must reject an actual CA write"
    );
    assert!(
        writable.set_len(0).is_err(),
        "F_SEAL_SHRINK must reject an actual CA truncation"
    );
    assert!(
        std::fs::read_dir(bundle.parent().expect("CA descriptor parent")).is_err(),
        "the CA transport must not grant its containing directory"
    );

    let metadata = std::fs::metadata(&bundle).expect("bundle identity remains available");
    println!(
        "SUPERVISED_BROKER_CA_OK:{}:{}:{}",
        root.display(),
        metadata.dev(),
        metadata.ino()
    );
}

fn fixture() -> tempfile::TempDir {
    let home = std::env::var_os("HOME").expect("HOME for Linux fixture");
    let root = tempfile::Builder::new()
        .prefix(".nub-supervised-broker-ca-")
        .tempdir_in(home)
        .expect("fixture directory");
    std::fs::create_dir(root.path().join("project")).expect("project directory");
    root
}

fn policy(root: &Path) -> nub_sandbox::SandboxPolicy {
    let project = root.join("project");
    let executable_parent = std::env::current_exe()
        .expect("test executable")
        .parent()
        .expect("test executable parent")
        .to_path_buf();
    let ambient = BTreeMap::from([(SECRET.to_string(), "broker-secret".to_string())]);
    let context = CompileCtx::new(
        Homes {
            home: root.join("withheld-home"),
            cache: root.join("withheld-cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project.clone(),
        ScopeCapabilities::approved(),
        ambient,
    );
    let mut policy = compile(
        &json!({
            "fs": {
                (project.display().to_string()): "rw",
                (executable_parent.display().to_string()): "r",
                "$tmp": "rw"
            },
            "net": ["broker.example"],
            "secrets": {SECRET: {"brokerTo": ["broker.example"]}}
        }),
        &context,
    )
    .expect("broker policy compiles");
    policy.env.constructed.extend([
        (CASE.to_string(), "child".to_string()),
        (ROOT.to_string(), root.display().to_string()),
    ]);
    policy
}

fn command(root: &Path) -> CommandSpec {
    CommandSpec::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "supervised_broker_ca_child", "--nocapture"])
        .cwd(root.join("project"))
}

fn output(sandbox: &Sandbox, root: &Path) -> String {
    let prepared = sandbox
        .prepare(command(root))
        .expect("supervised child prepares");
    assert!(
        prepared.degradation.lost.is_empty(),
        "broker child degraded: {:#?}",
        prepared.degradation
    );
    let output = prepared.output().expect("supervised child runs");
    assert!(
        output.status.success(),
        "broker child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("child stdout UTF-8")
        .lines()
        .find(|line| line.starts_with("SUPERVISED_BROKER_CA_OK:"))
        .expect("child reports CA identity")
        .to_string()
}

#[test]
fn supervised_tls_broker_delivers_one_read_only_session_ca() {
    let _lock = ENV_LOCK.lock().expect("broker env lock");
    // Broker acquisition intentionally captures the process environment, not CompileCtx's
    // snapshot. Keep this test-local secret out of the child's constructed environment.
    unsafe { std::env::set_var(SECRET, "broker-secret") };
    struct UnsetSecret;
    impl Drop for UnsetSecret {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(SECRET) };
        }
    }
    let _unset = UnsetSecret;

    let root = fixture();
    let sandbox = Sandbox::new(&policy(root.path())).expect("broker session acquires");
    let first = output(&sandbox, root.path());
    let second = output(&sandbox, root.path());
    assert_eq!(
        first, second,
        "commands in one session must inherit the same immutable CA object"
    );
}
