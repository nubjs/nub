//! A `--import` that calls `module.registerHooks` leaves CommonJS loading as it does
//! under plain `node --import`. On Node 22.15–24.11 such an import puts Nub on its async
//! worker tier, where the hook's `next()` used to throw on every CommonJS file, so
//! the `node 22.15` CI leg is the one that can go red. Other Nodes pass trivially, and
//! Nodes below 22.15 have no `registerHooks` to call.

use std::path::Path;
use std::process::{Command, Output};

fn write(root: &Path, name: &str, body: &str) {
    std::fs::write(root.join(name), body).unwrap();
}

fn without_nub_env(mut command: Command, root: &Path) -> Output {
    command
        .current_dir(root)
        .env_remove("NODE_COMPAT")
        .env_remove("NODE_OPTIONS")
        .env_remove("__NUB_RUNTIME_CONFIG")
        .output()
        .unwrap()
}

fn nub(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nub"));
    command.args(args);
    without_nub_env(command, root)
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `register.mjs` installs a pass-through hook through a static `node:module` import.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "c.cjs", "module.exports = 'from-cjs';\n");
    write(
        root,
        "main.mjs",
        "import c from './c.cjs';\nconsole.log(c);\n",
    );
    write(root, "main.cjs", "console.log(require('./c.cjs'));\n");
    write(
        root,
        "register.mjs",
        r#"import nodeModule from "node:module";
if (nodeModule.registerHooks) await import("./static.mjs");
"#,
    );
    write(
        root,
        "static.mjs",
        r#"import { registerHooks } from "node:module";
registerHooks({ load: (url, context, next) => next(url, context) });
"#,
    );
    dir
}

#[test]
fn a_foreign_register_leaves_commonjs_files_running() {
    let dir = fixture();
    let root = dir.path();
    for entry in ["main.mjs", "main.cjs"] {
        assert_eq!(
            stdout(&nub(root, &["--import", "./register.mjs", entry])),
            "from-cjs",
            "entry {entry}"
        );
    }

    // The standalone runner picks the same tier from its own entry. Unix-only: `--import`
    // needs a URL for an absolute Windows path, and the behaviour is not platform-specific.
    #[cfg(unix)]
    {
        let runner =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime/loader-register.mjs");
        let mut command = Command::new("node");
        command
            .arg(format!("--import={}", runner.display()))
            .args(["--import=./register.mjs", "main.mjs"]);
        assert_eq!(stdout(&without_nub_env(command, root)), "from-cjs");
    }
}

/// The recovery answers only Node's own missing source. A loader's wrong `source` for a
/// `.cjs` file stays that loader's error, and the raw file never runs.
#[test]
fn a_foreign_loaders_invalid_source_still_fails_the_run() {
    let dir = fixture();
    let root = dir.path();
    write(
        root,
        "foreign.mjs",
        r#"import nodeModule from "node:module";
nodeModule.register("./foreign-hooks.mjs", import.meta.url);
if (nodeModule.registerHooks) await import("./static.mjs");
"#,
    );
    write(
        root,
        "foreign-hooks.mjs",
        r#"export async function load(url, context, next) {
  if (url.endsWith("/rejected.cjs")) return { format: "module", source: 42, shortCircuit: true };
  return next(url, context);
}
"#,
    );
    write(root, "rejected.cjs", "console.log('raw file ran');\n");
    write(root, "rejected.mjs", "import './rejected.cjs';\n");

    // The same register and loader run an ordinary CommonJS file.
    assert_eq!(
        stdout(&nub(root, &["--import", "./foreign.mjs", "main.mjs"])),
        "from-cjs"
    );

    let output = nub(root, &["--import", "./foreign.mjs", "rejected.mjs"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "ran to success: {stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert!(
        stderr.contains("ERR_INVALID_RETURN_PROPERTY_VALUE"),
        "{stderr}"
    );
}
