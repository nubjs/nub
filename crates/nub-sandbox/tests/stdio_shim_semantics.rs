//! The Windows build jail's `child_process` shim must be indistinguishable from the real thing
//! for everything a lifecycle script does.
//!
//! WHY THIS RUNS EVERYWHERE. What the shim repairs is a Windows fact — the global NPFS
//! namespace is closed to a LowBox token, so libuv's named-pipe stdio spins forever inside
//! `uv_spawn`. How faithfully it reproduces `child_process` is NOT a Windows fact: it is
//! ordinary JS about buffering, `close` ordering and error shaping, and it is the half most
//! likely to regress. Behind a Windows-only probe it would go unmeasured on the machines
//! people develop on, so the shim carries a force seam and this test drives it directly.
//!
//! THE ONE THAT IS EASY TO GET WRONG. A raw `spawn()` reads its bytes out of a scratch file at
//! child EXIT, which means the shim has to hold `close` back until the streams it synthesised
//! have drained — Node's own `_closesNeeded` bookkeeping, raised by hand. Get it wrong and the
//! consumer's `close` handler runs before a single `data` event, so it observes empty output
//! and exit code 0: a SILENT wrong answer, which is the failure mode this whole jail refuses to
//! ship. `spawn` is therefore asserted on its bytes, not merely on completing.
//!
//! Each test drives ONE Node process that exercises its group and prints one `key=value` line
//! per behaviour, so a failure names the behaviour rather than an exit status.

use std::path::PathBuf;
use std::process::Command;

const SHIM: &str = include_str!("../src/backend/windows_stdio_shim.js");

/// Run `driver` (ESM) with the shim preloaded, and return its stdout.
///
/// `None` when there is no `node` to drive — the crate's other suites do not require one, and
/// a hard failure here would make `cargo test` depend on the developer's PATH.
fn run_under_shim(driver: &str) -> Option<String> {
    let node = which_node()?;
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = dir.path().join("shim.mjs");
    let script = dir.path().join("driver.mjs");
    std::fs::write(&shim, SHIM).expect("write shim");
    std::fs::write(&script, driver).expect("write driver");

    let out = Command::new(&node)
        .env("__NUB_JAIL_STDIO_SHIM_FORCE", "1")
        // The child spawns its own children; without this they would run unshimmed and the
        // test would measure stock `child_process` while reporting on the shim.
        .env("NODE_PATH", "")
        .arg("--import")
        .arg(&shim)
        .arg(&script)
        .output()
        .expect("run the driver");
    assert!(
        out.status.success(),
        "driver failed ({}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn which_node() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

#[test]
fn the_shim_returns_every_child_process_familys_output() {
    let driver = r#"
import cp from "node:child_process";
const node = process.execPath;
const say = (k, v) => console.log(`${k}=${JSON.stringify(v)}`);
const echo = (text) => [node, ["-e", `process.stdout.write(${JSON.stringify(text)})`]];

const [f, a] = echo("async");
cp.execFile(f, a, (error, stdout) => {
  say("execFile", error ? `ERR ${error.message}` : stdout);

  // A raw spawn, consumed as a stream: the assertion that the deferred `close` lands AFTER
  // the data it is meant to have delivered.
  const [sf, sa] = echo("streamed");
  const child = cp.spawn(sf, sa);
  let seen = "";
  child.stdout.on("data", (chunk) => (seen += chunk));
  child.on("close", (code) => {
    say("spawn", `${seen}/${code}`);
    const [xf, xa] = echo("sync");
    say("execFileSync", cp.execFileSync(xf, xa, { encoding: "utf8" }));
    say("spawnSync", cp.spawnSync(xf, xa, { encoding: "utf8" }).stdout);
    say("execSync", cp.execSync(`"${node}" -e "process.stdout.write('shelled')"`, { encoding: "utf8" }));
  });
});
"#;
    let Some(out) = run_under_shim(driver) else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    for expected in [
        r#"execFile="async""#,
        r#"spawn="streamed/0""#,
        r#"execFileSync="sync""#,
        r#"spawnSync="sync""#,
        r#"execSync="shelled""#,
    ] {
        assert!(out.contains(expected), "missing {expected} in:\n{out}");
    }
}

#[test]
fn the_shim_reports_failure_the_way_child_process_does() {
    let driver = r#"
import cp from "node:child_process";
const node = process.execPath;
const say = (k, v) => console.log(`${k}=${JSON.stringify(v)}`);

// A non-zero exit must still throw, carrying the status AND the stderr the caller needs to
// see why — reading it back out of a scratch file is exactly where that could be lost.
try {
  cp.execFileSync(node, ["-e", "process.stderr.write('boom'); process.exit(3)"], { stdio: ["pipe", "pipe", "pipe"] });
  say("nonzero", "did not throw");
} catch (error) {
  say("nonzero", `${error.status}/${String(error.stderr)}`);
}

// No file can emulate a duplex IPC channel, so this must refuse IMMEDIATELY rather than
// become the unkillable spin the shim exists to remove.
try {
  cp.fork("unreachable.js");
  say("fork", "did not throw");
} catch (error) {
  say("fork", `${error.code}/${error.message.includes("dependenciesMeta")}`);
}

// A spawn that fails outright emits `error` INSTEAD of `exit`; a caller waiting on `close`
// must still be woken.
const bad = cp.spawn("nub-no-such-binary-9c1f", []);
let sawError = false;
bad.on("error", () => (sawError = true));
bad.on("close", () => say("failed-spawn", `${sawError}/closed`));
setTimeout(() => { say("failed-spawn", "never closed"); process.exit(1); }, 5000).unref();
"#;
    let Some(out) = run_under_shim(driver) else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    assert!(out.contains(r#"nonzero="3/boom""#), "in:\n{out}");
    assert!(
        out.contains(r#"fork="ERR_NUB_SANDBOX_NO_IPC/true""#),
        "the refusal must name the per-package opt-out, in:\n{out}"
    );
    assert!(out.contains(r#"failed-spawn="true/closed""#), "in:\n{out}");
}
