//! The Windows build jail's `child_process` shim must be indistinguishable from the real thing
//! for everything a lifecycle script does.
//!
//! WHY THIS RUNS EVERYWHERE. What the shim repairs is a Windows fact — the global NPFS namespace
//! is closed to a LowBox token, so libuv's named-pipe stdio spins forever inside `uv_spawn`. How
//! faithfully it reproduces `child_process` is NOT a Windows fact: it is ordinary JS about
//! streaming, `close` ordering, `maxBuffer`, channel semantics and error shaping, and it is the
//! half most likely to regress. Behind a Windows-only probe it would go unmeasured on the machines
//! people develop on, so the shim carries a force seam and these tests drive it directly.
//!
//! MOST OF THIS FILE IS A DIFFERENTIAL, deliberately: every case with an unshimmed equivalent is
//! run TWICE from one driver, once preloaded and once not, and the two transcripts must match
//! verbatim. A case that only passes in the shimmed arm proves nothing — it could be asserting the
//! shim's own bug back to itself. Only the DELIBERATE divergences (the two typed refusals, and the
//! sync family's scratch-file requirement) assert against a fixed expectation, and each names why
//! no differential is possible.
//!
//! THE TWO THAT ARE EASY TO GET WRONG, both of which the shipped file-backed version got wrong: a
//! consumer's `close` handler must not run before the `data` it is meant to have delivered, and
//! `execFile`'s `maxBuffer` path calls `child.stdout.destroy()` — which, if it does not settle the
//! close accounting, loses the CALLBACK ENTIRELY, silently, exit 0. Both are asserted on observed
//! bytes and an observed callback, never on a process merely completing.

use std::path::PathBuf;
use std::process::Command;

/// The DELIVERED payload, not the source file: the jail ships the shim comment-stripped, because
/// percent-encoding charges ~3x per punctuation byte and the raw source does not fit in a
/// `CreateProcessW` environment block at all. Driving the source instead would leave the strip —
/// a real transformation on the shipping path — entirely unmeasured.
fn shim_js() -> String {
    nub_sandbox::build_jail_stdio_preload_js()
}

/// A second `--import` term standing in for the per-package egress gate, which production stamps
/// onto the same `NODE_OPTIONS`. Its only job is to be DETECTABLE two processes down.
const MARKER: &str = "globalThis.__nubTestPreloadMarker = true;\n";

fn which_node() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

/// Everything outside the RFC 3986 unreserved set, so the payload survives `NODE_OPTIONS`'
/// whitespace split. Mirrors `percent_encode_strict` in `compiler/defaults.rs`, which is private —
/// the point here is to deliver the preload the way the jail does, not to re-test the encoder.
fn data_url(js: &str) -> String {
    let mut out = String::from("data:text/javascript,");
    for byte in js.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Run `driver` (ESM) and return its stdout.
///
/// `preload` is the `NODE_OPTIONS` chain, delivered as `data:` URLs exactly the way the build jail
/// delivers it — which is also what makes `import.meta.url` inside the shim byte-identical to the
/// term naming it, the identity its `NODE_OPTIONS` preservation turns on. An empty chain is the
/// unshimmed control arm.
///
/// `None` when there is no `node` to drive: the crate's other suites do not require one, and a hard
/// failure here would make `cargo test` depend on the developer's PATH.
fn run(driver: &str, preload: &[&str], extra_env: &[(&str, &str)]) -> Option<String> {
    let node = which_node()?;
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("driver.mjs");
    std::fs::write(&script, driver).expect("write driver");

    let mut cmd = Command::new(&node);
    if preload.is_empty() {
        cmd.env("NODE_OPTIONS", "");
        cmd.env_remove("__NUB_JAIL_STDIO_SHIM_FORCE");
    } else {
        let chain = preload
            .iter()
            .map(|js| format!("--import {}", data_url(js)))
            .collect::<Vec<_>>()
            .join(" ");
        cmd.env("NODE_OPTIONS", chain);
        cmd.env("__NUB_JAIL_STDIO_SHIM_FORCE", "1");
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let out = cmd.arg(&script).output().expect("run the driver");
    assert!(
        out.status.success(),
        "driver failed ({}) with {} preload term(s):\n{}",
        out.status,
        preload.len(),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn shimmed(driver: &str) -> Option<String> {
    run(driver, &[&shim_js()], &[])
}

/// The `> `-prefixed observations, which are the only lines a differential compares. Anything
/// nondeterministic — a pid, a path, a raw duration — must never reach one.
fn marks(out: &str) -> Vec<String> {
    out.lines()
        .filter(|line| line.starts_with("> "))
        .map(str::to_owned)
        .collect()
}

/// Run one driver both ways and require the transcripts to agree. Returns the shared transcript so
/// the caller can also assert what it actually said — agreeing on nothing is not a pass.
fn differential(driver: &str) -> Option<Vec<String>> {
    let plain = marks(&run(driver, &[], &[])?);
    let with_shim = marks(&shimmed(driver)?);
    assert_eq!(
        plain, with_shim,
        "the shimmed transcript must match unshimmed `child_process` verbatim"
    );
    assert!(!plain.is_empty(), "the driver printed no observations");
    Some(plain)
}

fn no_node() -> bool {
    if which_node().is_none() {
        eprintln!("no node on PATH — skipping");
        return true;
    }
    false
}

fn require(out: &[String], expected: &str) {
    assert!(
        out.iter().any(|line| line == expected),
        "missing `{expected}` in:\n{out:#?}"
    );
}

#[test]
fn every_child_process_family_returns_its_output() {
    if no_node() {
        return;
    }
    let driver = r#"
import cp from "node:child_process";
const node = process.execPath;
const say = (k, v) => console.log(`> ${k}=${JSON.stringify(v)}`);
const echo = (text) => [node, ["-e", `process.stdout.write(${JSON.stringify(text)})`]];

const [f, a] = echo("async");
cp.execFile(f, a, (error, stdout) => {
  say("execFile", error ? `ERR ${error.message}` : stdout);

  // A raw spawn consumed as a stream: the assertion that the deferred `close` lands AFTER the
  // data it is meant to have delivered.
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
    let Some(out) = differential(driver) else {
        return;
    };
    require(&out, r#"> execFile="async""#);
    require(&out, r#"> spawn="streamed/0""#);
    require(&out, r#"> execFileSync="sync""#);
    require(&out, r#"> spawnSync="sync""#);
    require(&out, r#"> execSync="shelled""#);
}

#[test]
fn spawn_delivers_output_as_it_is_produced() {
    if no_node() {
        return;
    }
    // The property the file-backed rewrite CANNOT have: it reads the scratch file at child exit, so
    // both writes land together. Bucketed rather than timed, because the assertion has to survive a
    // contended host — the child's own 700ms sleep is the only interval measured, and contention can
    // only widen it.
    let driver = r#"
import cp from "node:child_process";
const child = cp.spawn(process.execPath, ["-e",
  `process.stdout.write("first\\n"); setTimeout(() => { process.stdout.write("second\\n"); process.exit(0); }, 700);`]);
const at = [];
const t0 = Date.now();
child.stdout.on("data", (d) => {
  for (const _ of String(d).split("\n").filter(Boolean)) at.push(Date.now() - t0);
});
child.on("close", () => {
  const gap = at.length === 2 ? at[1] - at[0] : -1;
  console.log(`> chunks=${at.length}`);
  console.log(`> delivery=${gap > 300 ? "as-produced" : "all-at-exit"}`);
});
"#;
    let Some(out) = differential(driver) else {
        return;
    };
    require(&out, "> chunks=2");
    require(&out, "> delivery=as-produced");
}

#[test]
fn maxbuffer_applies_and_the_callback_still_fires() {
    if no_node() {
        return;
    }
    // A file has no backpressure, so under the file-backed path the child ran to completion — and
    // worse, `execFile`'s maxBuffer handler calls `child.stdout.destroy()`, which stranded the close
    // accounting and lost the callback outright (measured: no output at all, exit 0). Every field
    // asserted here is load-independent: an error code, a kill flag, and a size BUCKET.
    let driver = r#"
import cp from "node:child_process";
// Bounded, so even a regression terminates instead of filling a disk.
const src = `let n = 0; const t = setInterval(() => { process.stdout.write("z".repeat(4096)); if (++n >= 400) clearInterval(t); }, 10);`;
const child = cp.execFile(process.execPath, ["-e", src], { maxBuffer: 8192 }, (error, stdout) => {
  console.log(`> code=${error && error.code}`);
  console.log(`> child-killed=${child.killed}`);
  console.log(`> captured=${String(stdout).length <= 65536 ? "bounded" : "unbounded"}`);
});
"#;
    let Some(out) = differential(driver) else {
        return;
    };
    require(&out, "> code=ERR_CHILD_PROCESS_STDIO_MAXBUFFER");
    require(&out, "> child-killed=true");
    require(&out, "> captured=bounded");
}

#[test]
fn the_channel_is_indistinguishable_from_forks_own() {
    if no_node() {
        return;
    }
    // `fork` used to throw here: a file cannot emulate a duplex pipe. It now rides a channel over
    // the same container-private namespace, so every observable below has an unshimmed twin and the
    // differential IS the assertion. Nested on purpose — the fix has to survive its own recursion,
    // which means a second private pipe created from inside an already-confined descendant.
    let driver = r#"
import cp from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nub-chan-"));
const write = (name, src) => { const p = path.join(dir, name); fs.writeFileSync(p, src); return p; };

const echoer = write("echo.cjs",
  "process.on('message', (m) => { if (m.q === 'bye') process.disconnect(); else process.send({ a: m.n, connected: process.connected }); });");
const relay = write("relay.cjs",
  `const cp = require('node:child_process');
   const g = cp.fork(${JSON.stringify(path.join(dir, "echo.cjs"))}, [], { stdio: 'inherit' });
   g.on('message', (m) => { process.send({ viaGrandchild: m.a }); g.kill(); });
   process.on('message', (m) => g.send({ n: m.n }));`);
const burstee = write("burst.cjs",
  `const seen = [];
   process.on('message', (m) => { if (m.i !== undefined) seen.push(m.i); else process.send({ ordered: seen.join(',') }); });`);
const chatty = write("chatty.cjs",
  "process.stdout.write('piped-too'); process.on('message', () => process.send({ both: true }));");

const child = cp.fork(echoer, [], { stdio: "inherit" });
console.log(`> connected=${child.connected} channel=${child.channel ? "present" : "absent"}`);
console.log(`> send=${child.send({ n: 41 })}`);
child.on("message", (m) => {
  console.log(`> message=${JSON.stringify(m)}`);
  child.send({ q: "bye" });
});
child.on("disconnect", () => console.log(`> disconnected connected=${child.connected}`));
child.on("exit", (code) => {
  console.log(`> exit=${code}`);

  const nested = cp.fork(relay, [], { stdio: "inherit" });
  nested.on("message", (m) => { console.log(`> nested=${JSON.stringify(m)}`); nested.kill(); });
  nested.send({ n: 7 });
  nested.on("exit", () => {
    const burst = cp.fork(burstee, [], { stdio: "inherit" });
    for (let i = 0; i < 200; i++) burst.send({ i });
    burst.send({ done: 1 });
    burst.on("message", (m) => {
      const want = Array.from({ length: 200 }, (_, i) => i).join(",");
      console.log(`> burst-ordered=${m.ordered === want}`);
      burst.kill();

      // A SILENT `fork`: three piped slots AND an 'ipc' slot resolved by one call (`fork` itself
      // defaults to inheriting 0/1/2). This is where the two halves of the override interact, so it
      // is the arm a stacked-override ordering bug breaks — the refusal would fire before the
      // streaming ever ran.
      const both = cp.fork(chatty, [], { silent: true });
      let piped = "";
      both.stdout.on("data", (d) => (piped += d));
      both.send({ go: 1 });
      both.on("message", (m) => {
        console.log(`> piped-and-ipc=${JSON.stringify(m)}/${JSON.stringify(piped)}`);
        both.kill();
        fs.rmSync(dir, { recursive: true, force: true });
      });
    });
  });
});
"#;
    let Some(out) = differential(driver) else {
        return;
    };
    require(&out, "> connected=true channel=present");
    require(&out, "> send=true");
    require(&out, r#"> message={"a":41,"connected":true}"#);
    require(&out, "> disconnected connected=false");
    require(&out, r#"> nested={"viaGrandchild":7}"#);
    require(&out, "> burst-ordered=true");
    require(&out, r#"> piped-and-ipc={"both":true}/"piped-too""#);
}

#[test]
fn a_descendant_keeps_the_whole_preload_chain() {
    if no_node() {
        return;
    }
    // THE SECURITY-SHAPED ONE. Production puts this shim and the per-package egress gate on ONE
    // `NODE_OPTIONS`, which is also how both reach every descendant. A `fork` that rebuilt `env`
    // strips it, so the shim re-synthesises it — and it must re-synthesise the PARENT'S OWN value
    // rather than re-derive from `import.meta.url`, or the gate silently vanishes from every
    // grandchild while the channel keeps working. MARKER stands in for the gate: a grandchild that
    // reports a channel but NO marker is exactly the regression this pins.
    let driver = r#"
import cp from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nub-chain-"));
const leaf = path.join(dir, "leaf.cjs");
fs.writeFileSync(leaf, "process.send({ marker: !!globalThis.__nubTestPreloadMarker, send: typeof process.send });");
const mid = path.join(dir, "mid.cjs");
fs.writeFileSync(mid,
  `const cp = require('node:child_process');
   const g = cp.fork(${JSON.stringify(leaf)}, [], { stdio: 'inherit', env: { PATH: process.env.PATH }, execArgv: [] });
   g.on('message', (m) => { process.send({ depth: 2, ...m }); g.kill(); });`);

// Both hostile shapes at once, at every level: a rebuilt env AND an emptied execArgv.
const child = cp.fork(mid, [], { stdio: "inherit", env: { PATH: process.env.PATH }, execArgv: [] });
child.on("message", (m) => {
  console.log(`> grandchild=${JSON.stringify(m)}`);
  child.kill();
  fs.rmSync(dir, { recursive: true, force: true });
});
"#;
    let Some(out) = run(driver, &[&shim_js(), MARKER], &[]) else {
        return;
    };
    require(
        &marks(&out),
        r#"> grandchild={"depth":2,"marker":true,"send":"function"}"#,
    );
}

#[test]
fn only_the_sync_family_needs_a_writable_directory() {
    if no_node() {
        return;
    }
    // The async path's scratch-file precondition is GONE — it needs no filesystem grant at all,
    // which matters in a jail whose purpose is confining writes. `spawnSync` cannot follow:
    // `ParseStdioOption` has no `'wrap'` case and a blocked loop cannot drain a pipe, so it still
    // needs a file, and now refuses in a typed, actionable way instead of returning to the spin.
    // Driven through the seam rather than with real permissions because off Windows the address IS
    // a filesystem path, so a read-only temp dir would break the streamed path for an unrelated
    // reason and the test would measure nothing.
    let driver = r#"
import cp from "node:child_process";
const node = process.execPath;
const child = cp.spawn(node, ["-e", "process.stdout.write('streamed-without-a-directory')"]);
let seen = "";
child.stdout.on("data", (c) => (seen += c));
child.on("close", (code) => {
  console.log(`> async=${JSON.stringify(seen)}/${code}`);
  try {
    cp.spawnSync(node, ["-e", "0"], { encoding: "utf8" });
    console.log("> sync=did-not-refuse");
  } catch (error) {
    console.log(`> sync=${error.code}/${error.message.includes("allowBuilds")}`);
  }
});
"#;
    let Some(out) = run(driver, &[&shim_js()], &[("__NUB_JAIL_NO_SCRATCH", "1")]) else {
        return;
    };
    let seen = marks(&out);
    require(&seen, r#"> async="streamed-without-a-directory"/0"#);
    require(&seen, "> sync=ERR_NUB_SANDBOX_NO_IPC/true");
}

#[test]
fn the_shim_reports_failure_the_way_child_process_does() {
    if no_node() {
        return;
    }
    let driver = r#"
import cp from "node:child_process";
const node = process.execPath;
const say = (k, v) => console.log(`> ${k}=${JSON.stringify(v)}`);

// A non-zero exit must still throw, carrying the status AND the stderr the caller needs to see
// why — reading it back out of a scratch file is exactly where that could be lost.
try {
  cp.execFileSync(node, ["-e", "process.stderr.write('boom'); process.exit(3)"], { stdio: ["pipe", "pipe", "pipe"] });
  say("nonzero", "did not throw");
} catch (error) {
  say("nonzero", `${error.status}/${String(error.stderr)}`);
}

// A spawn that fails outright emits `error` INSTEAD of `exit`; a caller waiting on `close` must
// still be woken.
const bad = cp.spawn("nub-no-such-binary-9c1f", []);
let sawError = false;
bad.on("error", () => (sawError = true));
bad.on("close", () => say("failed-spawn", `${sawError}/closed`));
setTimeout(() => { say("failed-spawn", "never closed"); process.exit(1); }, 5000).unref();
"#;
    let Some(out) = differential(driver) else {
        return;
    };
    require(&out, r#"> nonzero="3/boom""#);
    require(&out, r#"> failed-spawn="true/closed""#);
}

#[test]
fn the_two_deliberate_refusals_stay_fast_and_typed() {
    if no_node() {
        return;
    }
    // No differential is possible: unshimmed `child_process` SUPPORTS both of these, so agreeing
    // with it is precisely what the shim cannot do. Handle passing needs `uv_write2`/`SCM_RIGHTS`,
    // which no userland stream carries, and `serialization: 'advanced'` rides libuv's own IPC
    // frames. The contract is that each fails immediately and namefully — the exercise is pointless
    // if it trades a hang for a hang.
    let driver = r#"
import cp from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nub-refuse-"));
const idle = path.join(dir, "idle.cjs");
fs.writeFileSync(idle, "setTimeout(() => {}, 200);");

try {
  cp.fork(idle, [], { stdio: "inherit", serialization: "advanced" });
  console.log("> advanced=accepted");
} catch (error) {
  console.log(`> advanced=${error.code}/${error.message.includes("allowBuilds")}`);
}

const child = cp.fork(idle, [], { stdio: "inherit" });
const server = net.createServer();
server.listen(0, () => {
  try {
    child.send({ s: 1 }, server);
    console.log("> handle=accepted");
  } catch (error) {
    console.log(`> handle=${error.code}/${error.message.includes("allowBuilds")}`);
  }
  server.close();
  child.kill();
  fs.rmSync(dir, { recursive: true, force: true });
});
"#;
    let Some(out) = shimmed(driver) else {
        return;
    };
    let seen = marks(&out);
    require(&seen, "> advanced=ERR_NUB_SANDBOX_NO_IPC/true");
    require(&seen, "> handle=ERR_NUB_SANDBOX_NO_IPC/true");
}
