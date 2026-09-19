//! A package fetched from a tarball URL resolves like its registry copy
//! would: optional deps (platform-filtered), peers and platform fields.

use super::*;
use crate::tests::make_packument;
use aube_lockfile::{LockedPackage, LockfileGraph};
use aube_manifest::PackageJson;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn tgz(manifest: serde_json::Value, padding: usize) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut add = |path: &str, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
    };
    add("package/package.json", manifest.to_string().as_bytes());
    if padding > 0 {
        add("package/native.node", &vec![0_u8; padding]);
    }
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
    std::io::Write::write_all(&mut gz, &tar).unwrap();
    gz.finish().unwrap()
}

struct Fixture {
    base: String,
    /// Request paths whose whole body reached the client.
    completed: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

/// `parent` (a URL tarball) with two URL-tarball optionals — one any
/// platform can use, one for an OS nothing runs, padded to 32 MiB so a
/// full download is observable — plus a required peer and an optional
/// meta-only peer. `peer-a` itself comes from the registry.
async fn fixture() -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut routes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    routes.insert(
        "/parent.tgz".to_string(),
        tgz(
            serde_json::json!({
                "name": "parent",
                "version": "1.0.0",
                "optionalDependencies": {
                    "native-any": format!("{base}/native-any.tgz"),
                    "native-elsewhere": format!("{base}/native-elsewhere.tgz"),
                    "opt-registry": "^1.0.0",
                },
                "peerDependencies": { "peer-a": "^1.0.0" },
                "peerDependenciesMeta": { "peer-opt": { "optional": true } },
            }),
            0,
        ),
    );
    routes.insert(
        "/native-any.tgz".to_string(),
        tgz(
            serde_json::json!({ "name": "native-any", "version": "1.0.0" }),
            0,
        ),
    );
    routes.insert(
        "/native-elsewhere.tgz".to_string(),
        tgz(
            serde_json::json!({
                "name": "native-elsewhere",
                "version": "1.0.0",
                "os": ["plan9"],
            }),
            32 << 20,
        ),
    );
    routes.insert(
        "/opt-registry".to_string(),
        serde_json::to_vec(&make_packument("opt-registry", &["1.0.0"], "1.0.0")).unwrap(),
    );
    routes.insert(
        "/peer-a".to_string(),
        serde_json::to_vec(&make_packument("peer-a", &["1.0.0"], "1.0.0")).unwrap(),
    );

    let routes = Arc::new(routes);
    let completed = Arc::new(Mutex::new(Vec::new()));
    let done = completed.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let routes = routes.clone();
            let done = done.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request.split(' ').nth(1).unwrap_or("/").to_string();
                let Some(body) = routes.get(&path) else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    return;
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                for chunk in body.chunks(64 * 1024) {
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                }
                done.lock().unwrap().push(path);
            });
        }
    });
    Fixture {
        base,
        completed,
        server,
    }
}

async fn resolve(fx: &Fixture, accept_all: bool) -> LockfileGraph {
    let client = Arc::new(aube_registry::client::RegistryClient::new(&format!(
        "{}/",
        fx.base
    )));
    let mut resolver = Resolver::new(client)
        .with_supported_architectures(SupportedArchitectures {
            accept_all,
            ..Default::default()
        })
        .with_dependency_policy(DependencyPolicy {
            block_exotic_subdeps: false,
            ..Default::default()
        });
    let mut manifest = PackageJson::default();
    manifest
        .dependencies
        .insert("parent".to_string(), format!("{}/parent.tgz", fx.base));
    resolver.resolve(&manifest, None).await.unwrap()
}

fn package<'a>(graph: &'a LockfileGraph, name: &str) -> Option<&'a LockedPackage> {
    graph.packages.values().find(|p| p.name == name)
}

#[tokio::test]
async fn url_tarball_optionals_and_peers_resolve_like_registry_ones() {
    let fx = fixture().await;
    let graph = resolve(&fx, false).await;

    let parent = package(&graph, "parent").expect("parent resolved");
    assert!(
        parent.optional_dependencies.contains_key("native-any"),
        "host-compatible URL optional is wired as optional: {:?}",
        parent.optional_dependencies
    );
    assert!(package(&graph, "native-any").is_some());
    assert_eq!(
        parent
            .optional_dependencies
            .get("opt-registry")
            .map(String::as_str),
        Some("1.0.0"),
        "a registry optional of a URL package resolves to a pin"
    );
    assert_eq!(
        parent
            .declared_dependencies
            .get("opt-registry")
            .map(String::as_str),
        Some("^1.0.0"),
        "the manifest's declared range survives for the npm / bun / yarn writers"
    );
    assert!(
        package(&graph, "native-elsewhere").is_none(),
        "an optional for another OS is dropped when the lockfile is host-only"
    );

    assert_eq!(
        parent.peer_dependencies.get("peer-a").map(String::as_str),
        Some("^1.0.0")
    );
    assert!(parent.peer_dependencies_meta["peer-opt"].optional);
    assert!(
        parent.dependencies.contains_key("peer-a"),
        "the auto-installed peer is linked beside the package: {:?}",
        parent.dependencies
    );
    assert!(
        parent.dep_path.contains("(peer-a@1.0.0)"),
        "the peer lands in the dep_path suffix: {}",
        parent.dep_path
    );

    assert!(
        !fx.completed
            .lock()
            .unwrap()
            .contains(&"/native-elsewhere.tgz".to_string()),
        "the other-OS tarball was downloaded in full"
    );
    fx.server.abort();
}

#[tokio::test]
async fn other_platform_url_optional_is_dropped_even_for_a_portable_lockfile() {
    // A registry optional for another platform still lands in a portable
    // lockfile, because the packument carries its integrity. A URL tarball
    // carries integrity only in the bytes, so recording one that was never
    // downloaded would write an entry the strict reader refuses.
    let fx = fixture().await;
    let graph = resolve(&fx, true).await;

    assert!(package(&graph, "native-elsewhere").is_none());
    assert!(package(&graph, "native-any").unwrap().integrity.is_some());
    assert!(
        !fx.completed
            .lock()
            .unwrap()
            .contains(&"/native-elsewhere.tgz".to_string()),
        "the other-OS tarball was downloaded in full"
    );
    fx.server.abort();
}
