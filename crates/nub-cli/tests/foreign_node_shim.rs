//! A version manager's `node` shim first on PATH (#832). With nothing active for the
//! directory, mise's `not_found_system_fallback` looks `node` up again without its
//! own directory, which inside a nub run finds nub's PATH shim: nub resolved the
//! manager's shim, launched it, and was launched back, one process per round.
//!
//! The fixture shim does what mise does — it strips only its own directory for the
//! lookup and `exec`s the result, leaving PATH untouched for the process it runs —
//! so no version manager is needed. It counts its invocations and refuses past a
//! cap, so a regression fails here instead of filling the process table.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SHIM_CAP: usize = 40;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/ (or fast/)
    path.push("nub");
    path
}

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-foreign-shim-{tag}-{}-{nanos:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A real Node binary from the inherited PATH, never one of nub's own shims.
fn real_node() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| !dir.to_string_lossy().contains("node-shim"))
        .map(|dir| dir.join("node"))
        .find(|node| node.is_file())
        .and_then(|node| node.canonicalize().ok())
}

/// Writes `<root>/<name>/node`, logging each invocation's arguments to
/// `<root>/<name>.log`. `target` is the command it `exec`s; `None` re-resolves `node`
/// on PATH without the fixture's shim directories, as mise's fallback steps over its
/// own (and over other managers' shims it recognises), so the lookup lands on nub.
fn write_shim(root: &Path, name: &str, target: Option<&Path>) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let log = root.join(format!("{name}.log"));
    let target = match target {
        Some(node) => format!("'{}'", node.display()),
        None => format!(
            "\"$(PATH=$(printf %s \"$PATH\" | tr : '\\n' | grep -vF '{}/' | paste -sd: -) \
             command -v node)\"",
            root.display()
        ),
    };
    let script = format!(
        "#!/bin/sh\n\
         echo \"$*\" >> '{log}'\n\
         if [ \"$(wc -l < '{log}')\" -gt {SHIM_CAP} ]; then echo 'shim loop' >&2; exit 99; fi\n\
         exec {target} \"$@\"\n",
        log = log.display(),
    );
    let shim = dir.join("node");
    std::fs::write(&shim, script).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn run_nub(root: &Path, path: &[PathBuf], args: &[&str]) -> std::process::Output {
    let path = std::env::join_paths(
        path.iter()
            .cloned()
            .chain([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]),
    )
    .unwrap();
    std::fs::write(root.join("package.json"), r#"{"name":"p","private":true}"#).unwrap();
    Command::new(nub_binary())
        .args(args)
        .current_dir(root)
        .env("PATH", path)
        // A fresh version cache, so the `--version` probe through each shim runs.
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("spawn nub")
}

fn shim_calls(root: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(root.join(format!("{name}.log")))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn assert_ok(out: &std::process::Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One fallback shim, then two: with both, re-entry through the second must not
/// pick the first again, so every shim handed back on the way is skipped.
#[test]
fn fallback_shims_ahead_of_the_real_node_are_each_launched_once() {
    let Some(node) = real_node() else {
        eprintln!("skipping: no Node on PATH");
        return;
    };
    for shims in [&["mise"][..], &["mise", "volta"]] {
        let root = tmp(&format!("loop-{}", shims.len()));
        let mut path: Vec<_> = shims
            .iter()
            .map(|name| write_shim(&root, name, None))
            .collect();
        path.push(node.parent().unwrap().to_path_buf());

        let out = run_nub(&root, &path, &["-p", "process.execPath"]);
        // Probes (`--version` and the flag probes) reach the Node behind a shim and
        // are fine; the launch must happen once per shim, not once per round trip.
        for name in shims {
            let calls = shim_calls(&root, name);
            let launches = calls
                .iter()
                .filter(|call| call.ends_with("-p process.execPath"))
                .count();
            assert_eq!(
                launches,
                1,
                "nub relaunched {name} ({} shims); calls: {calls:?}\nstderr:\n{}",
                shims.len(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert_ok(&out);
        assert_eq!(
            Path::new(String::from_utf8_lossy(&out.stdout).trim()),
            node,
            "the run lands on the Node after the shims"
        );
    }
}

#[test]
fn a_working_shim_still_runs_the_nested_node_a_script_starts() {
    let Some(node) = real_node() else {
        eprintln!("skipping: no Node on PATH");
        return;
    };
    let root = tmp("nested");
    let shim_dir = write_shim(&root, "mise", Some(&node));
    std::fs::write(
        root.join("outer.js"),
        "process.stdout.write(require('child_process').execSync('node inner.js').toString())",
    )
    .unwrap();
    std::fs::write(root.join("inner.js"), "console.log('inner ran')").unwrap();

    let out = run_nub(
        &root,
        &[shim_dir, node.parent().unwrap().to_path_buf()],
        &["outer.js"],
    );
    assert_ok(&out);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "inner ran");
    let calls = shim_calls(&root, "mise");
    assert!(
        calls.iter().any(|call| call.contains("inner.js")),
        "the nested `node` must still resolve through the shim; calls: {calls:?}"
    );
}

/// Each level is a real Node starting `node` through nub's PATH hijack: nesting,
/// not a hand-back, however deep it goes.
#[test]
fn a_deep_chain_of_nested_node_runs_stays_on_the_same_node() {
    let Some(node) = real_node() else {
        eprintln!("skipping: no Node on PATH");
        return;
    };
    let root = tmp("deep");
    std::fs::write(
        root.join("level.js"),
        "const n = Number(process.argv[2]);\n\
         if (n < 20) process.stdout.write(require('child_process').execSync(`node level.js ${n + 1}`));\n\
         else console.log(`depth ${n} ${process.execPath}`);\n",
    )
    .unwrap();

    let out = run_nub(
        &root,
        &[node.parent().unwrap().to_path_buf()],
        &["level.js", "0"],
    );
    assert_ok(&out);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("depth 20 {}", node.display())
    );
}
