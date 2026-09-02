//! The build jail's per-package network gate, driven through the REAL delivery path.
//!
//! WHY THIS RUNS EVERYWHERE, and why it builds its `NODE_OPTIONS` from
//! `nub_sandbox::net_gate_node_options` rather than hand-rolling one. What the gate exists for
//! is a Windows fact — Windows is the one platform with no unprivileged OS egress lever. What it
//! DOES is ordinary JS about prototype patching and `child_process` env handling, and that is
//! the half that regresses. Behind a Windows-only probe it would go unmeasured on the machines
//! people develop on. Compiling the stamp with the production generator is what makes a green
//! run evidence about the shipped path instead of about a copy of it.
//!
//! THE TWO BEHAVIOURS PINNED HERE ARE BOTH INVISIBLE WHEN BROKEN, which is the whole reason
//! they are tested rather than reviewed:
//!
//!  1. `NODE_OPTIONS` RE-STAMPING. `delete process.env.NODE_OPTIONS` before a spawn defeated the
//!     gate in one line. The shim captures the value at module evaluation and re-stamps it at
//!     both `child_process` seams, which is what makes spawned-`node` coverage work at all.
//!  2. THE SINGLE-SEAM PATCH. `net.Socket.prototype.connect` covers every TCP client, and those
//!     clients call it with a SINGLE pre-normalized array `[options, cb]`. An `Array.isArray`
//!     guard that excluded that shape silently disabled the entire TCP gate while DNS and UDP
//!     kept denying correctly — a maximally misleading symptom, since the gate looks installed.
//!
//! Verdicts key on `err.nubReason === "ERR_NUB_JAIL_NET_DENIED"`, which ONLY the gate sets. That
//! is deliberate: the target is `192.0.2.1` (RFC 5737 TEST-NET-1, documented as unroutable), so
//! an ungated arm fails too — with a transport error, never with the gate's signature. The
//! discriminator is therefore load- and network-independent, and no arm depends on reaching
//! anything.
//!
//! EVERY ATTEMPT IS BOUNDED, and the bound is load-bearing rather than tidiness. A working gate
//! denies on the next tick, so the healthy path never waits; but an escaped connection to an
//! unroutable address hangs until the OS gives up, which took 301s when the mutation controls for
//! this suite were run. A regression must fail in seconds and say `ESCAPED`, not look like a
//! hung CI job.

use std::path::PathBuf;
use std::process::Command;

/// A package whose CATALOG ENTRY withholds network, so the compiled policy is `allow:false`.
///
/// ⛔⛔ THIS WAS `chalk`, CHOSEN FOR HAVING NO ENTRY AT ALL, AND THAT STOPPED MEANING DENIED. An
/// uncatalogued package now takes `baseline_caps()`, whose `network` is true, so every arm of this suite
/// went green-for-the-wrong-reason: the gate never intercepted and the expected denial lines simply never
/// appeared. What the suite is FOR — the gate's DNS/UDP/TCP deny semantics inside the confined Node — needs
/// a package the live policy actually denies, which now means one whose entry withholds egress rather than
/// one the catalog has never heard of.
/// ⛔⛔ AND IT WAS `classic-level` AFTER THAT, WHICH STOPPED MEANING DENIED FOR A SECOND REASON. Its
/// withdrawal rested only on corpus records that passed at the BASE PROFILE — the rung the catalog
/// spells as no entry, which grants egress — so no arm had ever run it without the network. It was
/// granted egress along with 269 other cells of that shape, and every arm of this suite would again
/// have gone green for the wrong reason. The lesson both times: the subject must be a package whose
/// denial a measurement actually reached, not one that merely reads as denied today.
///
/// ⛔⛔ AND IT WAS `stream-chat-react-native-core` AFTER THAT, WHICH FELL TO A THIRD VARIANT OF THE
/// SAME MISTAKE. Its licence in `baseline_egress_withdrawals.rs` was a corpus record whose grant was
/// non-empty and carried no `network` — read at the time as "an arm ran with egress denied and
/// passed". Every such record carries `arms-unfalsifiable`, none of them ever DROPPED egress as a
/// measured capability, and `d0179017e5` had already ruled that an unfalsifiable arm licenses no
/// narrowing; the whole class was withdrawn and 45 cells went back to the baseline with it.
///
/// THE FIXTURE IS NOW PLATFORM-SPECIFIC, and that is the point rather than an inconvenience. The
/// only denials left are COLD-INSTALL SWEEP verdicts — a real install of the real package against
/// the shipped grant — and no single package is cold-swept on all three platforms. Picking one per
/// platform is what keeps every arm of this suite resting on the strongest licence class the
/// catalog has instead of on the widest-reaching name. Both entries are a single unbanded
/// `default`, so no version resolves onto a different answer, and neither carries a curated
/// exception or a `package_network` row that could quietly decide this fixture's net axis.
#[cfg(target_os = "windows")]
const DENIED_PACKAGE: &str = "blake-hash";
#[cfg(not(target_os = "windows"))]
const DENIED_PACKAGE: &str = "pizzip";

/// A version the entry admits. Each fixture has one unbanded `default`, so any version resolves to
/// the denial — but the precondition below asserts it rather than trusting that.
#[cfg(target_os = "windows")]
const DENIED_VERSION: &str = "2.0.0";
#[cfg(not(target_os = "windows"))]
const DENIED_VERSION: &str = "3.0.5";

/// Unroutable by RFC 5737, and NOT loopback — the gate exempts loopback, so a `127.0.0.1` target
/// would be permitted and every arm would pass for the wrong reason.
const TARGET: &str = "192.0.2.1";

/// Run `driver` (ESM) with the gate delivered exactly as the build jail delivers it: compiled by
/// the production generator onto `NODE_OPTIONS`, so the driver's own children inherit it.
///
/// `None` when there is no `node` to drive — the crate's other suites do not require one, and a
/// hard failure here would make `cargo test` depend on the developer's PATH.
fn run_under_gate(driver: &str) -> Option<String> {
    assert!(
        // The SHARED decision, which is what the gate now compiles from — asking the v1 table here would
        // let this precondition pass while the gate allows the package, which is exactly the divergence
        // that made these tests wrong in the first place.
        !nub_sandbox::build_jail_net_allowed_for(Some(DENIED_PACKAGE), Some(DENIED_VERSION)),
        "`{DENIED_PACKAGE}` is no longer denied egress; this suite needs a DENIED package"
    );

    let node = which_node()?;
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("driver.mjs");
    std::fs::write(&script, driver).expect("write driver");

    let out = Command::new(&node)
        .env(
            "NODE_OPTIONS",
            nub_sandbox::net_gate_node_options(Some(DENIED_PACKAGE), Some(DENIED_VERSION)),
        )
        .env("TARGET", TARGET)
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

fn assert_lines(out: &str, expected: &[&str]) {
    for line in expected {
        assert!(
            out.lines().any(|l| l == *line),
            "missing `{line}` in driver output:\n{out}"
        );
    }
}

/// BEHAVIOUR 2 — the call shape, pinned at its cause rather than its symptom.
///
/// The first assertion is the one that matters: every real client hands `connect` a SINGLE
/// argument that IS an array. A guard written against the documented `(options)` / `(port, host)`
/// forms alone therefore misses all of them. The second assertion is the symptom that guard
/// produced, kept alongside so a failure says both *what* broke and *that* egress reopened.
#[test]
fn the_tcp_seam_sees_the_pre_normalized_array_shape_every_client_uses() {
    let driver = r#"
import net from "node:net";
import http from "node:http";

// Wraps the ALREADY-PATCHED connect, so what this records is exactly what each client passes
// through to the gate's seam.
const gated = net.Socket.prototype.connect;
const shapes = [];
net.Socket.prototype.connect = function (...args) {
  shapes.push(`${args.length}/${Array.isArray(args[0]) ? "array" : typeof args[0]}`);
  return gated.apply(this, args);
};

const say = (k, v) => console.log(`${k}=${v}`);
const denied = (e) => (e?.nubReason === "ERR_NUB_JAIL_NET_DENIED" ? "DENIED" : `other:${e?.code}`);
const target = process.env.TARGET;

// A denial arrives on the next tick, so 2s is generous for the healthy path; anything still in
// flight after it has ESCAPED the gate and is waiting on an unroutable address.
const bounded = (p) => Promise.race([p, new Promise((r) => setTimeout(() => r("ESCAPED"), 2000).unref())]);

const viaNet = bounded(new Promise((r) => {
  const s = net.connect(80, target, () => r("CONNECTED"));
  s.on("error", (e) => r(denied(e)));
}));
const viaHttp = bounded(new Promise((r) => {
  http.request({ host: target, port: 80 }, () => r("CONNECTED")).on("error", (e) => r(denied(e))).end();
}));
const viaFetch = bounded(fetch(`http://${target}/`).then(
  () => "CONNECTED",
  (e) => denied(e.cause ?? e),
));

say("net.connect", await viaNet);
say("http.request", await viaHttp);
say("fetch", await viaFetch);
// The shape every client actually used. One entry per client, and `1/array` is the form the
// broken guard excluded.
say("shapes", [...new Set(shapes)].sort().join(","));
// An ESCAPED socket is still live and would hold the loop open until the OS gives up on the
// unroutable address — 75s, which reads as a hung test rather than a failed one.
process.exit(0);
"#;
    let Some(out) = run_under_gate(driver) else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    assert_lines(
        &out,
        &[
            "shapes=1/array",
            "net.connect=DENIED",
            "http.request=DENIED",
            "fetch=DENIED",
        ],
    );
}

/// BEHAVIOUR 1 — re-stamping, measured against the exact one-line bypass that defeated it.
///
/// `env wiped` and `env replaced` are the two ways a script can hand a child an environment
/// without the gate. Bare inheritance is the control: it would pass even with re-stamping
/// removed, so on its own it proves nothing, and its presence here is what makes the other two
/// arms attributable to the re-stamp rather than to inheritance.
#[test]
fn node_options_is_restamped_across_both_child_process_seams() {
    let driver = r#"
import cp from "node:child_process";

// Reports whether the GATE is armed in the child, not whether a connection failed: only the
// gate sets `nubReason`, so this cannot be confused with a transport error.
// Bounded for the same reason as the seam test: an escaped connection to an unroutable address
// would otherwise hang the child until the OS times out.
const probe = `
  const net = require("node:net");
  const done = (v) => { console.log(v); process.exit(0); };
  setTimeout(() => done("ESCAPED"), 2000).unref();
  const s = net.connect(80, process.env.TARGET, () => done("UNGATED"));
  s.on("error", (e) => done(e.nubReason === "ERR_NUB_JAIL_NET_DENIED" ? "GATED" : "UNGATED:" + e.code));
`;
const say = (k, v) => console.log(`${k}=${String(v).trim()}`);
const sync = (env) => cp.spawnSync(process.execPath, ["-e", probe], { encoding: "utf8", env }).stdout;

const wiped = { ...process.env };
delete wiped.NODE_OPTIONS;

say("sync inherited", sync(undefined));
say("sync env wiped", sync(wiped));
say("sync env replaced", sync({ TARGET: process.env.TARGET, PATH: process.env.PATH }));

// The async seam is a DIFFERENT patch (`ChildProcess.prototype.spawn`'s `envPairs`), so it needs
// its own arm — the sync family never reaches it.
const asyncOut = await new Promise((r) => {
  const c = cp.spawn(process.execPath, ["-e", probe], { env: wiped });
  let o = "";
  c.stdout.on("data", (d) => (o += d));
  c.on("close", () => r(o));
});
say("async env wiped", asyncOut);
"#;
    let Some(out) = run_under_gate(driver) else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    assert_lines(
        &out,
        &[
            "sync inherited=GATED",
            "sync env wiped=GATED",
            "sync env replaced=GATED",
            "async env wiped=GATED",
        ],
    );
}

/// The other two egress surfaces `connect` does not cover: a DNS query NAME and a datagram are
/// each an exfil channel in their own right. Thin on purpose — the seam analysis above is where
/// the subtlety is; this is breadth.
#[test]
fn dns_and_udp_are_denied_alongside_tcp() {
    let driver = r#"
import dns from "node:dns";
import dgram from "node:dgram";
const say = (k, v) => console.log(`${k}=${v}`);
const denied = (e) => (e?.nubReason === "ERR_NUB_JAIL_NET_DENIED" ? "DENIED" : `other:${e?.code}`);

say("dns.lookup", await new Promise((r) => dns.lookup("example.com", (e) => r(denied(e)))));
say("dns.resolve", await new Promise((r) => dns.resolve("example.com", (e) => r(denied(e)))));
const sock = dgram.createSocket("udp4");
say("dgram.send", await new Promise((r) => sock.send("x", 53, process.env.TARGET, (e) => r(denied(e)))));
sock.close();
"#;
    let Some(out) = run_under_gate(driver) else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    assert_lines(
        &out,
        &[
            "dns.lookup=DENIED",
            "dns.resolve=DENIED",
            "dgram.send=DENIED",
        ],
    );
}

/// Loopback stays reachable under a deny. Not a concession: it cannot carry data off the box, and
/// a build that starts a local server is common enough that denying it would break packages —
/// the cost this design is optimised against. A regression here is silent and would only surface
/// as mysterious install failures.
#[test]
fn loopback_survives_a_full_deny() {
    let driver = r#"
import net from "node:net";
const server = net.createServer((c) => c.end()).listen(0, "127.0.0.1");
await new Promise((r) => server.on("listening", r));
const port = server.address().port;
const verdict = await new Promise((r) => {
  const s = net.connect(port, "127.0.0.1", () => { s.end(); r("REACHED"); });
  s.on("error", (e) => r(e.nubReason === "ERR_NUB_JAIL_NET_DENIED" ? "DENIED" : `other:${e.code}`));
});
console.log(`loopback=${verdict}`);
server.close();
"#;
    let Some(out) = run_under_gate(driver) else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    assert_lines(&out, &["loopback=REACHED"]);
}

/// Both shims on one `NODE_OPTIONS`, in BOTH orders, driven through the production composition.
///
/// They patch the same `child_process` seams, so composition is where they could plausibly
/// break each other: the stdio shim rewrites how a child is spawned and the gate rewrites the
/// env that spawn carries. The assertion is that `child_process` still returns real output AND
/// the gate is still armed — either alone would miss the interesting failure, where one shim
/// wins and the other silently stops applying.
#[test]
fn both_shims_compose_in_either_order() {
    let Some(node) = which_node() else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    let stamped = nub_sandbox::build_jail_node_options(Some(DENIED_PACKAGE), Some(DENIED_VERSION));
    let (a, b) = stamped.split_at(stamped.find(" --import").expect("two terms") + 1);
    let reversed = format!("{b} {}", a.trim_end());

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("compose.cjs");
    std::fs::write(
        &script,
        r#"
const cp = require("node:child_process");
const net = require("node:net");
// `child_process` must still work through both patched layers — the stdio shim's whole job.
console.log(`execFileSync=${cp.execFileSync(process.execPath, ["-e", "process.stdout.write('ok')"], { encoding: "utf8" })}`);
console.log(`spawnSync=${cp.spawnSync(process.execPath, ["-e", "process.exit(3)"]).status}`);
console.log(`envKept=${cp.spawnSync(process.execPath, ["-e", "process.stdout.write(process.env.MYVAR||'MISSING')"], { encoding: "utf8", env: { ...process.env, MYVAR: "kept" } }).stdout}`);
// ...and the gate must still be armed after all that patching.
const done = (v) => { console.log(`gate=${v}`); process.exit(0); };
setTimeout(() => done("ESCAPED"), 2000).unref();
const s = net.connect(80, process.env.TARGET, () => done("ESCAPED"));
s.on("error", (e) => done(e.nubReason === "ERR_NUB_JAIL_NET_DENIED" ? "ARMED" : `other:${e.code}`));
"#,
    )
    .expect("write compose driver");

    for (label, value) in [("stdio,gate", &stamped), ("gate,stdio", &reversed)] {
        let out = Command::new(&node)
            .env("NODE_OPTIONS", value)
            // The stdio shim self-disables off Windows; forced on so composition is measured
            // wherever this test runs rather than only on the Windows leg.
            .env("__NUB_JAIL_STDIO_SHIM_FORCE", "1")
            .env("TARGET", TARGET)
            .arg(&script)
            .output()
            .expect("run the compose driver");
        assert!(
            out.status.success(),
            "{label}: driver failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        for expected in [
            "execFileSync=ok",
            "spawnSync=3",
            "envKept=kept",
            "gate=ARMED",
        ] {
            assert!(
                stdout.lines().any(|l| l == expected),
                "{label}: missing `{expected}` in:\n{stdout}"
            );
        }
    }
}

/// The same composition, reached through an ESM NAMED import — the access path
/// [`both_shims_compose_in_either_order`] cannot see, because its driver is CJS and uses
/// `require`.
///
/// WHY A SEPARATE TEST RATHER THAN A LINE IN THAT ONE. A builtin's ESM named exports are a
/// SNAPSHOT taken when its facade is first created, so `import { spawnSync } from
/// "node:child_process"` and `require("node:child_process").spawnSync` can be DIFFERENT
/// functions — one patched, one not. A CJS driver reports the patched one no matter what, which
/// is why the composition test stayed green while an ESM caller bypassed both shims entirely.
///
/// The observable is hermetic and can genuinely fail: the driver hands the child an env with
/// `NODE_OPTIONS` blanked, and the gate's `spawnSync` repair re-forces it. Reached through the
/// snapshot instead, the blanking survives and the child reports MISSING.
#[test]
fn an_esm_named_import_of_spawn_sync_is_still_repaired_in_either_order() {
    let Some(node) = which_node() else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    let stamped = nub_sandbox::build_jail_node_options(Some(DENIED_PACKAGE), Some(DENIED_VERSION));
    let (a, b) = stamped.split_at(stamped.find(" --import").expect("two terms") + 1);
    let reversed = format!("{b} {}", a.trim_end());

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("esm.mjs");
    std::fs::write(
        &script,
        r#"
import { spawnSync } from "node:child_process";
const r = spawnSync(
  process.execPath,
  ["-e", "process.stdout.write(process.env.NODE_OPTIONS ? 'STAMPED' : 'MISSING')"],
  { encoding: "utf8", env: { ...process.env, NODE_OPTIONS: "" } },
);
console.log("esmNamed=" + r.stdout);
"#,
    )
    .expect("write the esm driver");

    for (label, value) in [("stdio,gate", &stamped), ("gate,stdio", &reversed)] {
        let out = Command::new(&node)
            .env("NODE_OPTIONS", value)
            .env("__NUB_JAIL_STDIO_SHIM_FORCE", "1")
            .env("TARGET", TARGET)
            .arg(&script)
            .output()
            .expect("run the esm driver");
        assert!(
            out.status.success(),
            "{label}: driver failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.lines().any(|l| l == "esmNamed=STAMPED"),
            "{label}: an ESM named import of spawnSync bypassed the gate repair; got:\n{stdout}"
        );
    }
}

/// An ADMITTED package must not be intercepted at all — the `allow` half of the boolean. Pinned
/// because the early return is what keeps a permitted install byte-identical to unjailed Node,
/// and because a gate that denied a listed package would break exactly the installs the catalog
/// exists to keep working.
#[test]
fn an_admitted_package_is_not_intercepted() {
    let Some(node) = which_node() else {
        eprintln!("no node on PATH — skipping");
        return;
    };
    // The first UNSCOPED entry, so the version handed over below cannot be the reason the
    // gate lets it through — a version-scoped entry probed at the wrong version would assert
    // the deny path under a test named for the allow path.
    let Some((admitted, _)) = nub_sandbox::PACKAGE_NETWORK_ALLOWED
        .iter()
        .find(|(_, range)| range.is_none())
        .copied()
    else {
        eprintln!("catalog admits no unscoped package — nothing to assert");
        return;
    };
    assert!(
        nub_sandbox::build_jail_net_allowed_for(Some(admitted), Some("1.0.0")),
        "`{admitted}` was chosen as the unscoped control and must be admitted at any version"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("driver.mjs");
    std::fs::write(
        &script,
        r#"
import net from "node:net";
import dns from "node:dns";
// The gate replaces these when it installs, so an untouched prototype IS the assertion that a
// listed package runs on stock Node.
console.log(`patched=${/nubReason|POLICY/.test(String(net.Socket.prototype.connect)) || /nubReason|POLICY/.test(String(dns.lookup))}`);
"#,
    )
    .expect("write driver");

    let out = Command::new(&node)
        .env(
            "NODE_OPTIONS",
            nub_sandbox::net_gate_node_options(Some(admitted), Some("1.0.0")),
        )
        .arg(&script)
        .output()
        .expect("run the driver");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("patched=false"),
        "`{admitted}` is admitted and must run uninstrumented: {stdout}"
    );
}
