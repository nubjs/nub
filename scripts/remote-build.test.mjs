// Tests for scripts/remote-build.ts — the two contracts that fail SILENTLY if broken,
// plus arg handling. Pure functions only (no gcloud, no ssh, no VM).
// Run: node --test scripts/remote-build.test.mjs
//
// The silent-failure modes these exist to pin:
//   1. A builder without `node`, or without a staged addon placeholder, produces a
//      DEGRADED binary or a build-script panic rather than an obvious error.
//   2. A VM created without the self-destruct flags becomes a billing orphan; a VM
//      created WITH them during the image bake destroys the disk being imaged. Both
//      were real bugs during development.
import { test } from "node:test";
import assert from "node:assert/strict";
import { parseArgs, jobScript, instanceCreateArgs, filterSourceFiles, rsyncPushArgs } from "./remote-build.ts";

// macOS ships openrsync (2.6.9-compatible). An rsync-3.x-only flag fails the entire sync,
// and it cost a full image bake to find. This pins the flag set to what 2.6.9 accepts.
test("rsync push uses no rsync-3.x-only flags (macOS ships openrsync 2.6.9)", () => {
  const args = rsyncPushArgs("/tmp/list.txt", "/src", "1.2.3.4", "ssh -i k");
  for (const banned of ["--delete-missing-args", "--info", "--outbuf", "--mkpath", "--atimes"]) {
    assert.ok(!args.includes(banned), `${banned} is rsync 3.x only and breaks on macOS`);
  }
  assert.ok(args.some((a) => a.startsWith("--files-from=")), "must use the allowlist, not an --exclude blocklist");
  assert.ok(!args.some((a) => a.startsWith("--exclude")), "a blocklist makes rsync walk ~99 GB of gitignored tree and time out");
});

test("parseArgs defaults to a spot darwin fast build with fanout 1", () => {
  const a = parseArgs([]);
  assert.equal(a.job, "build");
  assert.equal(a.profile, "fast");
  assert.equal(a.fanout, 1);
  assert.equal(a.onDemand, false);
  assert.equal(a.keep, false);
});

test("parseArgs reads job, profile and fanout", () => {
  const a = parseArgs(["--job", "clippy", "--profile", "release", "--fanout", "10"]);
  assert.equal(a.job, "clippy");
  assert.equal(a.profile, "release");
  assert.equal(a.fanout, 10);
});

// Every job runs on a fresh clone, so every job needs the prerequisites. A regression
// that drops the node guard from ONE job type would produce a silently degraded binary
// from that path only — which is exactly the kind of bug that survives a smoke test.
for (const job of ["build", "clippy", "test"]) {
  test(`jobScript(${job}) guards the prerequisites a fresh clone lacks`, () => {
    const s = jobScript(job, "fast");
    assert.match(s, /command -v node/, "must fail loudly without node (else the primer silently empties)");
    assert.match(s, /runtime\/addons\/nub-native\.node/, "must stage the addon placeholder (nub-core/build.rs panics without it)");
    assert.match(s, /npm install/, "must install node_modules");
  });
}

test("jobScript(build) cross-compiles the darwin target at the requested profile", () => {
  const s = jobScript("build", "release");
  assert.match(s, /cargo zigbuild --target aarch64-apple-darwin/);
  assert.match(s, /--profile release/);
});

test("jobScript(clippy) runs the exact CI gate, and test does not", () => {
  assert.match(jobScript("clippy", "fast"), /cargo clippy --all-targets --all-features -- -D warnings/);
  assert.match(jobScript("test", "fast"), /cargo test -p nub-cli/);
  assert.doesNotMatch(jobScript("test", "fast"), /clippy/);
});

test("a builder VM self-destructs server-side, so a SIGKILLed launcher cannot orphan it", () => {
  const args = instanceCreateArgs("b1", "ssh-ed25519 KEY", {
    machine: "c3-standard-8", onDemand: false, fromImage: true, selfDestruct: true,
  });
  assert.ok(args.includes("--max-run-duration"), "missing the server-side TTL");
  const idx = args.indexOf("--instance-termination-action");
  assert.notEqual(idx, -1, "missing the termination action");
  assert.equal(args[idx + 1], "DELETE", "termination action must DELETE, not STOP (a stopped VM still bills its disk)");
  assert.ok(args.includes("SPOT"), "builders default to spot");
});

// The inverse, and a real bug that shipped once: the image bake STOPS the instance and
// images its disk. A server-side DELETE mid-bake takes the disk with it.
test("the image-bake VM does NOT self-destruct, but is still labelled for --reap", () => {
  const args = instanceCreateArgs("bake", "ssh-ed25519 KEY", {
    machine: "c3-standard-8", onDemand: true, fromImage: false, selfDestruct: false,
  });
  assert.ok(!args.includes("--max-run-duration"), "a mid-bake delete would destroy the disk being imaged");
  assert.ok(!args.includes("--instance-termination-action"));
  assert.ok(!args.includes("SPOT"), "a preemption mid-bake wastes the whole run");
  assert.ok(args.includes("--labels") && args.includes("nub-builder=1"), "must stay reapable");
});

test("filterSourceFiles drops the node-suite submodule and keeps everything else", () => {
  const out = filterSourceFiles(
    ["Cargo.toml", "crates/nub-cli/src/main.rs", "tests/node-suite/test/x.js", "tests/pnp/run.sh", ""].join("\n"),
  );
  assert.deepEqual(out, ["Cargo.toml", "crates/nub-cli/src/main.rs", "tests/pnp/run.sh"]);
});
