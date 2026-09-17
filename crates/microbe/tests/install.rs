//! End-to-end installs against an in-memory registry: real tarballs, real integrity strings,
//! real packuments, no network. The transport is the seam, so the whole install path runs.

use base64::Engine;
use microbe::{Error, Microbe, Transport};
use sha2::{Digest, Sha512};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A package to publish into the fake registry.
struct Pkg {
    name: &'static str,
    version: &'static str,
    deps: &'static [(&'static str, &'static str)],
    optional: &'static [(&'static str, &'static str)],
    bin: Option<&'static str>,
    os: &'static [&'static str],
    install_script: bool,
}

const fn pkg(name: &'static str, version: &'static str) -> Pkg {
    Pkg {
        name,
        version,
        deps: &[],
        optional: &[],
        bin: None,
        os: &[],
        install_script: false,
    }
}

#[derive(Clone)]
struct FakeRegistry {
    urls: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeRegistry {
    fn publish(pkgs: &[Pkg]) -> Self {
        let mut urls = BTreeMap::new();
        let mut packuments: BTreeMap<&str, (Vec<serde_json::Value>, &str)> = BTreeMap::new();
        for p in pkgs {
            let tarball_url = format!("https://fake/{}/-/{}.tgz", p.name, p.version);
            let tgz = tarball(p);
            let integrity = format!(
                "sha512-{}",
                base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&tgz))
            );
            urls.insert(tarball_url.clone(), tgz);
            let mut manifest = serde_json::json!({
                "name": p.name, "version": p.version,
                "dependencies": map(p.deps), "optionalDependencies": map(p.optional),
                "os": p.os, "hasInstallScript": p.install_script,
                "dist": { "tarball": tarball_url, "integrity": integrity },
            });
            if let Some(b) = p.bin {
                manifest["bin"] = serde_json::json!(b);
            }
            let entry = packuments.entry(p.name).or_insert((Vec::new(), p.version));
            entry.0.push(manifest);
            entry.1 = p.version; // last published wins `latest`
        }
        for (name, (versions, latest)) in packuments {
            let versions: serde_json::Map<String, serde_json::Value> = versions
                .into_iter()
                .map(|m| (m["version"].as_str().unwrap().to_string(), m))
                .collect();
            let doc =
                serde_json::json!({ "dist-tags": { "latest": latest }, "versions": versions });
            urls.insert(
                format!("https://fake/{}", name.replace('/', "%2f")),
                serde_json::to_vec(&doc).unwrap(),
            );
        }
        FakeRegistry {
            urls: Arc::new(Mutex::new(urls)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn corrupt_tarball(&self, name: &str, version: &str) {
        let url = format!("https://fake/{name}/-/{version}.tgz");
        self.urls.lock().unwrap().get_mut(&url).unwrap().push(0);
    }

    fn packument_requests(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|u| !u.ends_with(".tgz"))
            .count()
    }
}

impl Transport for FakeRegistry {
    fn get(&self, url: &str, _accept: &str) -> Result<Vec<u8>, Error> {
        self.requests.lock().unwrap().push(url.to_string());
        self.urls
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or(Error::Status {
                url: url.to_string(),
                status: 404,
            })
    }
}

fn map(pairs: &[(&str, &str)]) -> serde_json::Value {
    serde_json::Value::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect(),
    )
}

/// A registry-shaped tarball: everything under `package/`, a `package.json`, an `index.js`
/// that exports the version, and the bin script when declared.
fn tarball(p: &Pkg) -> Vec<u8> {
    let mut manifest = serde_json::json!({ "name": p.name, "version": p.version });
    if let Some(b) = p.bin {
        manifest["bin"] = serde_json::json!(b);
    }
    let files: Vec<(String, String, u32)> = [
        ("package.json".to_string(), manifest.to_string(), 0o644),
        (
            "index.js".to_string(),
            format!("module.exports = '{}@{}';", p.name, p.version),
            0o644,
        ),
    ]
    .into_iter()
    .chain(p.bin.map(|b| {
        (
            b.to_string(),
            "#!/usr/bin/env node\nconsole.log('hi')\n".to_string(),
            0o644,
        )
    }))
    .collect();
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut ar = tar::Builder::new(gz);
    for (path, body, mode) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        ar.append_data(&mut h, format!("package/{path}"), body.as_bytes())
            .unwrap();
    }
    ar.into_inner().unwrap().finish().unwrap()
}

fn installed_version(dir: &Path) -> String {
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("package.json")).unwrap()).unwrap();
    v["version"].as_str().unwrap().to_string()
}

fn microbe(reg: &FakeRegistry) -> Microbe {
    Microbe::with_transport(reg.clone()).registry("https://fake")
}

#[test]
fn installs_the_tree_flat_and_reports_bins() {
    let reg = FakeRegistry::publish(&[
        Pkg {
            deps: &[("a", "^1"), ("b", "1.x")],
            bin: Some("cli.js"),
            ..pkg("@scope/tool", "2.0.0")
        },
        Pkg {
            deps: &[("b", ">=1")],
            ..pkg("a", "1.5.0")
        },
        pkg("b", "1.0.0"),
        pkg("b", "1.9.0"),
    ]);
    let dir = tempdir();
    let installed = microbe(&reg).install("@scope/tool", dir.path()).unwrap();

    assert_eq!(
        (installed.name.as_str(), installed.version.as_str()),
        ("@scope/tool", "2.0.0")
    );
    assert_eq!(installed.packages, 3, "root, a, and one shared b");
    let nm = dir.path().canonicalize().unwrap().join("node_modules");
    assert_eq!(installed.dir, nm.join("@scope/tool"));
    assert_eq!(installed_version(&nm.join("a")), "1.5.0");
    assert_eq!(
        installed_version(&nm.join("b")),
        "1.9.0",
        "highest version satisfying both ranges"
    );
    assert!(
        !nm.join("a/node_modules").exists(),
        "no nesting when the flat copy satisfies"
    );

    let cli = nm.join("@scope/tool/cli.js");
    assert_eq!(
        installed.bins,
        BTreeMap::from([("tool".to_string(), cli.clone())])
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(&cli).unwrap().permissions().mode() & 0o111,
            0,
            "bin is executable"
        );
    }
    assert_eq!(
        reg.packument_requests(),
        3,
        "one packument fetch per name, b shared by two dependents"
    );
}

#[test]
fn version_conflict_nests_under_the_dependent() {
    let reg = FakeRegistry::publish(&[
        Pkg {
            deps: &[("dep", "^2"), ("old", "1.0.0")],
            ..pkg("root", "1.0.0")
        },
        Pkg {
            deps: &[("dep", "^1")],
            ..pkg("old", "1.0.0")
        },
        pkg("dep", "1.0.0"),
        pkg("dep", "2.0.0"),
    ]);
    let dir = tempdir();
    let installed = microbe(&reg).install("root@1.0.0", dir.path()).unwrap();
    let nm = dir.path().canonicalize().unwrap().join("node_modules");
    assert_eq!(
        installed_version(&nm.join("dep")),
        "2.0.0",
        "root's own dep stays flat"
    );
    assert_eq!(
        installed_version(&nm.join("old/node_modules/dep")),
        "1.0.0",
        "the conflicting version nests"
    );
    assert_eq!(installed.packages, 4);
}

#[test]
fn optional_dependencies_skip_other_platforms_and_tolerate_failure() {
    let other_os: &[&str] = if cfg!(target_os = "macos") {
        &["linux"]
    } else {
        &["darwin"]
    };
    let reg = FakeRegistry::publish(&[
        Pkg {
            optional: &[
                ("native-here", "*"),
                ("native-elsewhere", "*"),
                ("missing", "*"),
            ],
            ..pkg("root", "1.0.0")
        },
        pkg("native-here", "1.0.0"),
        Pkg {
            os: other_os,
            ..pkg("native-elsewhere", "1.0.0")
        },
    ]);
    let dir = tempdir();
    let installed = microbe(&reg).install("root", dir.path()).unwrap();
    let nm = dir.path().canonicalize().unwrap().join("node_modules");
    assert!(nm.join("native-here").is_dir());
    assert!(
        !nm.join("native-elsewhere").exists(),
        "os-filtered optional dependency is not fetched"
    );
    assert!(
        !nm.join("missing").exists(),
        "an unpublished optional dependency does not fail the install"
    );
    assert_eq!(installed.packages, 2);
}

#[test]
fn integrity_mismatch_fails_the_install() {
    let reg = FakeRegistry::publish(&[pkg("root", "1.0.0")]);
    reg.corrupt_tarball("root", "1.0.0");
    let dir = tempdir();
    let err = microbe(&reg).install("root", dir.path()).unwrap_err();
    assert!(matches!(err, Error::Integrity { .. }), "{err}");
}

#[test]
fn install_scripts_are_reported_not_run() {
    let reg = FakeRegistry::publish(&[
        Pkg {
            deps: &[("native", "*")],
            ..pkg("root", "1.0.0")
        },
        Pkg {
            install_script: true,
            ..pkg("native", "3.2.1")
        },
    ]);
    let dir = tempdir();
    let installed = microbe(&reg).install("root", dir.path()).unwrap();
    assert_eq!(
        installed.skipped_install_scripts,
        vec!["native@3.2.1".to_string()]
    );
}

#[test]
fn unknown_package_and_unsatisfiable_range_are_distinct_errors() {
    let reg = FakeRegistry::publish(&[pkg("root", "1.0.0")]);
    let dir = tempdir();
    assert!(matches!(
        microbe(&reg).install("nope", dir.path()).unwrap_err(),
        Error::Status { status: 404, .. }
    ));
    assert!(matches!(
        microbe(&reg).install("root@^9", dir.path()).unwrap_err(),
        Error::NoVersion { .. }
    ));
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> TempDir {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("microbe-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}
