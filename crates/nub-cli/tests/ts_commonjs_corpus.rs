//! Every import and export form in tests/ts-commonjs/corpus.json, compiled to CommonJS
//! by Nub and by the standalone loader with `esModuleInterop` on and off, observes at
//! runtime what TypeScript 5.9.3's CommonJS emit observes, mutations included.
//! tests/ts-commonjs/record.mjs records the corpus from the cases in cases.mjs, so this
//! test needs no TypeScript.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Run {
    label: String,
    root: PathBuf,
    node_dir: Option<PathBuf>,
    standalone: bool,
    expected: String,
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// The difference, or `None` when the run printed what `tsc`'s emit printed. A
/// standalone run is the loader `@nubjs/runner` ships, on the same Node.
fn check(run: &Run) -> Option<String> {
    let mut command = if run.standalone {
        let node = run
            .node_dir
            .as_ref()
            .map_or(PathBuf::from("node"), |dir| dir.join("node"));
        let mut command = Command::new(node);
        let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
        command.arg(format!(
            "--import={}/loader-register.mjs",
            runtime.display()
        ));
        command
    } else {
        Command::new(env!("CARGO_BIN_EXE_nub"))
    };
    if let Some(dir) = &run.node_dir {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![dir.clone()];
        dirs.extend(std::env::split_paths(&path));
        command.env("PATH", std::env::join_paths(dirs).unwrap());
    }
    let out = command
        .arg("main.ts")
        .current_dir(&run.root)
        .env("XDG_CACHE_HOME", run.root.join(".cache"))
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("__NUB_RUNTIME_CONFIG")
        .env_remove("__NUB_TSCONFIG_REPORTED")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && stdout == run.expected {
        return None;
    }
    let mut report = format!("--- {} (exit {:?})\n", run.label, out.status.code());
    let (want, got): (Vec<_>, Vec<_>) = (run.expected.lines().collect(), stdout.lines().collect());
    for i in 0..want.len().max(got.len()) {
        if want.get(i) != got.get(i) {
            report.push_str(&format!(
                "  tsc: {}\n  nub: {}\n",
                want.get(i).unwrap_or(&"<none>"),
                got.get(i).unwrap_or(&"<none>")
            ));
        }
    }
    if !out.status.success() {
        report.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Some(report)
}

#[test]
fn commonjs_output_observes_what_tsc_emit_observes() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ts-commonjs/corpus.json");
    let corpus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let shared = corpus["shared"].as_object().unwrap();
    let cases = corpus["cases"].as_array().unwrap();

    // The Node on PATH, plus a compat-tier one, whose loader hands CommonJS to Node
    // differently, when CI provides it.
    let mut nodes = vec![("PATH node".to_string(), None)];
    match std::env::var_os("TEST_NODE_BIN_22_13_0") {
        Some(dir) => nodes.push(("Node 22.13".to_string(), Some(PathBuf::from(dir)))),
        None => eprintln!("skipping the Node 22.13 leg: TEST_NODE_BIN_22_13_0 is not set"),
    }

    // Nub, and on Unix the standalone loader, whose entry Node loads with the hook's
    // source rather than through its own CommonJS loader.
    let mut runners = vec![("nub", false)];
    if cfg!(unix) {
        runners.push(("standalone", true));
    }

    let dir = tempfile::tempdir().unwrap();
    let mut runs = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        for (mode, interop) in [("esModuleInterop", true), ("noEsModuleInterop", false)] {
            for (n, (node, node_dir)) in nodes.iter().enumerate() {
                for &(runner, standalone) in &runners {
                    let root = dir.path().join(format!("{i}-{interop}-{n}-{runner}"));
                    let files = shared.iter().chain(case["files"].as_object().unwrap());
                    for (name, body) in files {
                        write(&root, name, body.as_str().unwrap());
                    }
                    write(&root, "package.json", "{}");
                    write(
                        &root,
                        "tsconfig.json",
                        &format!(
                            r#"{{"compilerOptions":{{"module":"commonjs","target":"es2022","esModuleInterop":{interop}}}}}"#
                        ),
                    );
                    let name = case["name"].as_str().unwrap();
                    runs.push(Run {
                        label: format!("{name} [{mode}, {node}, {runner}]"),
                        root,
                        node_dir: node_dir.clone(),
                        standalone,
                        expected: case["expected"][mode].as_str().unwrap().to_string(),
                    });
                }
            }
        }
    }

    // Each run is a Node process of its own; eight at a time.
    let failures: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = runs
            .chunks(runs.len().div_ceil(8))
            .map(|chunk| scope.spawn(move || chunk.iter().filter_map(check).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect()
    });
    assert!(
        failures.is_empty(),
        "{} of {} runs differ from tsc's emit:\n{}",
        failures.len(),
        runs.len(),
        failures.join("\n")
    );
}
