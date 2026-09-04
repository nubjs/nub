fn main() {
    // Windows defaults the main-thread stack to 1 MB. The `async_main`
    // dispatcher's future state machine — which holds locals across every
    // await in every CLI command arm — exceeds that on debug builds and
    // crashes startup with `thread 'main' has overflowed its stack`.
    // Bump the reserve to 8 MB (matching Linux/macOS) via the MSVC
    // linker's `/STACK:` flag. No-op on every other target.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    generate_bundled_package_extensions();
    println!("cargo:rustc-env=AUBE_BUILD_DATE={}", build_date());
    println!("cargo:rerun-if-changed=build.rs");
}

fn generate_bundled_package_extensions() {
    use std::{collections::BTreeSet, fmt::Write as _, path::PathBuf};

    const CATALOGS: [&str; 2] = [
        "assets/yarn-compat-package-extensions.json",
        "assets/pnpm-compat-package-extensions.json",
    ];

    let mut selectors = BTreeSet::new();
    let mut strings = StringTable::default();
    let mut pairs = Vec::new();
    let mut peer_meta = Vec::new();
    let mut extensions = Vec::new();
    for path in CATALOGS {
        println!("cargo:rerun-if-changed={path}");
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("failed to read {path}: {err}"));
        let entries: Vec<(String, serde_json::Value)> = serde_json::from_str(&raw)
            .unwrap_or_else(|err| panic!("invalid bundled package extensions in {path}: {err}"));
        for (selector, body) in entries {
            assert!(
                selectors.insert(selector.clone()),
                "duplicate bundled package-extension selector: {selector}"
            );
            let body = body.as_object().unwrap_or_else(|| {
                panic!("bundled package extension {selector:?} must be an object")
            });
            extensions.push(GeneratedExtension {
                selector: strings.intern(&selector),
                dependencies: collect_string_map(
                    &mut strings,
                    &mut pairs,
                    body,
                    &selector,
                    "dependencies",
                ),
                optional_dependencies: collect_string_map(
                    &mut strings,
                    &mut pairs,
                    body,
                    &selector,
                    "optionalDependencies",
                ),
                peer_dependencies: collect_string_map(
                    &mut strings,
                    &mut pairs,
                    body,
                    &selector,
                    "peerDependencies",
                ),
                peer_dependencies_meta: collect_peer_meta(
                    &mut strings,
                    &mut peer_meta,
                    body,
                    &selector,
                ),
            });
        }
    }

    let mut generated = format!("static BUNDLED_STRINGS: &str = {:?};\n", strings.data);
    generated
        .push_str("static BUNDLED_STRING_PAIRS: &[(BundledStringRef, BundledStringRef)] = &[\n");
    for (name, value) in pairs {
        writeln!(generated, "    ({name:?}, {value:?}),").unwrap();
    }
    generated.push_str("];\nstatic BUNDLED_PEER_META: &[(BundledStringRef, bool)] = &[\n");
    for (name, optional) in peer_meta {
        writeln!(generated, "    ({name:?}, {optional}),").unwrap();
    }
    generated.push_str(
        "];\nstatic STANDALONE_BUNDLED_PACKAGE_EXTENSIONS: &[BundledPackageExtension] = &[\n",
    );
    for extension in extensions {
        writeln!(
            generated,
            "    BundledPackageExtension {{ selector: {:?}, dependencies: {:?}, optional_dependencies: {:?}, peer_dependencies: {:?}, peer_dependencies_meta: {:?} }},",
            extension.selector,
            extension.dependencies,
            extension.optional_dependencies,
            extension.peer_dependencies,
            extension.peer_dependencies_meta,
        )
        .unwrap();
    }
    generated.push_str("];\n");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    std::fs::write(out_dir.join("bundled_package_extensions.rs"), generated)
        .expect("failed to write generated bundled package extensions");
}

type StringRef = (u32, u32);
type TableRef = (u32, u32);

#[derive(Default)]
struct StringTable {
    data: String,
    entries: std::collections::BTreeMap<String, StringRef>,
}

impl StringTable {
    fn intern(&mut self, value: &str) -> StringRef {
        if let Some(reference) = self.entries.get(value) {
            return *reference;
        }
        let start = u32::try_from(self.data.len()).expect("bundled string table must fit in u32");
        let len = u32::try_from(value.len()).expect("bundled string must fit in u32");
        self.data.push_str(value);
        self.entries.insert(value.to_owned(), (start, len));
        (start, len)
    }
}

struct GeneratedExtension {
    selector: StringRef,
    dependencies: TableRef,
    optional_dependencies: TableRef,
    peer_dependencies: TableRef,
    peer_dependencies_meta: TableRef,
}

fn collect_string_map(
    strings: &mut StringTable,
    pairs: &mut Vec<(StringRef, StringRef)>,
    body: &serde_json::Map<String, serde_json::Value>,
    selector: &str,
    field: &str,
) -> TableRef {
    let start = u32::try_from(pairs.len()).expect("bundled pair table must fit in u32");
    if let Some(value) = body.get(field) {
        let entries = value.as_object().unwrap_or_else(|| {
            panic!("bundled package extension {selector:?}.{field} must be an object")
        });
        for (name, range) in entries {
            let range = range.as_str().unwrap_or_else(|| {
                panic!("bundled package extension {selector:?}.{field}.{name} must be a string")
            });
            pairs.push((strings.intern(name), strings.intern(range)));
        }
    }
    let len = u32::try_from(pairs.len()).expect("bundled pair table must fit in u32") - start;
    (start, len)
}

fn collect_peer_meta(
    strings: &mut StringTable,
    peer_meta: &mut Vec<(StringRef, bool)>,
    body: &serde_json::Map<String, serde_json::Value>,
    selector: &str,
) -> TableRef {
    let start = u32::try_from(peer_meta.len()).expect("bundled peer-meta table must fit in u32");
    if let Some(value) = body.get("peerDependenciesMeta") {
        let entries = value.as_object().unwrap_or_else(|| {
            panic!("bundled package extension {selector:?}.peerDependenciesMeta must be an object")
        });
        for (name, meta) in entries {
            let meta = meta.as_object().unwrap_or_else(|| {
                panic!(
                    "bundled package extension {selector:?}.peerDependenciesMeta.{name} must be an object"
                )
            });
            let optional = meta
                .get("optional")
                .map(|value| {
                    value.as_bool().unwrap_or_else(|| {
                        panic!(
                            "bundled package extension {selector:?}.peerDependenciesMeta.{name}.optional must be a boolean"
                        )
                    })
                })
                .unwrap_or(false);
            peer_meta.push((strings.intern(name), optional));
        }
    }
    let len =
        u32::try_from(peer_meta.len()).expect("bundled peer-meta table must fit in u32") - start;
    (start, len)
}

/// Capture the build host's UTC date as `YYYY-MM-DD` for the `aube
/// --version` line. Shell-out keeps it dep-free; falls back to
/// `unknown` if the host's `date` / `Get-Date` isn't reachable.
///
/// `Get-Date -UFormat` only controls the *format-specifier style*, not
/// the timezone — so the Windows path explicitly converts to UTC via
/// `.ToUniversalTime()` so build dates stay consistent with the
/// Unix `date -u` path on either side of midnight.
fn build_date() -> String {
    let (cmd, args): (&str, &[&str]) = if cfg!(windows) {
        (
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "(Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')",
            ],
        )
    } else {
        ("date", &["-u", "+%Y-%m-%d"])
    };
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
