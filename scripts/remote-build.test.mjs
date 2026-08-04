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
import { readFileSync } from "node:fs";
import { parseArgs, jobScript, instanceCreateArgs, filterSourceFiles, rsyncPushArgs, placementCandidates, isStray, remoteJobCommand } from "./remote-build.ts";

// GCE returns ZONE_RESOURCE_POOL_EXHAUSTED for a specific shape in a specific zone, and it
// took out the first image bake. A 10-way fanout requests ten instances at once, so a
// single (zone, machine) pair is not a viable placement strategy.
test("placement tries the preferred machine first, then falls back across zones and shapes", () => {
  const c = placementCandidates("c3-standard-8");
  assert.equal(c[0].machine, "c3-standard-8", "preferred machine must be tried first");
  assert.ok(new Set(c.map((x) => x.zone)).size > 1, "must span multiple zones");
  assert.ok(new Set(c.map((x) => x.machine)).size > 1, "must span multiple machine types");
  const seen = c.map((x) => `${x.zone}/${x.machine}`);
  assert.equal(new Set(seen).size, seen.length, "candidates must not repeat");
});

test("placement does not duplicate the preferred machine when it is also a fallback", () => {
  const c = placementCandidates("n2-standard-8");
  const n2 = c.filter((x) => x.machine === "n2-standard-8");
  assert.equal(n2.length, new Set(n2.map((x) => x.zone)).size, "one entry per zone, no dupes");
});

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

test("parseArgs defaults to a spot clippy run with fanout 1", () => {
  const a = parseArgs([]);
  assert.equal(a.job, "clippy");
  assert.equal(a.fanout, 1);
  assert.equal(a.onDemand, false);
  assert.equal(a.keep, false);
});

test("parseArgs reads job and fanout", () => {
  const a = parseArgs(["--job", "test", "--fanout", "10"]);
  assert.equal(a.job, "test");
  assert.equal(a.fanout, 10);
});

// Every job runs on a fresh clone, so every job needs the prerequisites. A regression
// that drops the node guard from ONE job type would produce a silently degraded binary
// from that path only — which is exactly the kind of bug that survives a smoke test.
for (const job of ["clippy", "test"]) {
  test(`jobScript(${job}) guards the prerequisites a fresh clone lacks`, () => {
    const s = jobScript(job, "fast");
    assert.match(s, /command -v node/, "must fail loudly without node (else the primer silently empties)");
    assert.match(s, /runtime\/addons\/nub-native\.node/, "must stage the addon placeholder (nub-core/build.rs panics without it)");
    assert.match(s, /npm install/, "must install node_modules");
  });
}

// Verified against .github/workflows/ci.yml: the Clippy job runs FOUR things. nub-native
// is its own workspace (panic=unwind cdylib) `exclude`d from the root, so a root-only
// clippy goes green on code that CI then rejects — the exact --all-targets-shaped gap
// AGENTS.md warns about.
//
// This test has twice pinned drift in place rather than catching it, so it is worth stating
// what it is FOR: every leg CI runs must appear here, or a remote gate reports green on code
// CI rejects. It previously asserted the invocation WITHOUT `--profile fast` while claiming
// to be verified against ci.yml, so the job built a second dependency graph under `dev` and
// could not reuse the golden image's artifacts. And it asserted THREE legs after
// `check-path-literals.sh` (ci.yml:237) had joined `check-env-reads.sh`, so that lint was
// missing from every remote clippy run. And a THIRD time: it asserted FOUR legs while
// ci.yml:258 also runs `cd crates/nub-launcher && cargo clippy --all-targets -- -D warnings
// && cargo build && cargo test` — its own workspace, invisible to the root gate, and the half
// that ships inside every compiled artifact. Count the legs in ci.yml, do not trust this name.
test("jobScript(clippy) reproduces every leg of the CI clippy gate", () => {
  const s = jobScript("clippy", "fast");
  assert.match(s, /cargo clippy --all-targets --all-features --profile fast -- -D warnings/);
  assert.match(s, /crates\/nub-native && cargo clippy --all-features --profile fast -- -D warnings/, "root clippy does NOT cover nub-native");
  assert.match(
    s,
    /crates\/nub-launcher && cargo clippy --all-targets -- -D warnings && cargo build && cargo test/,
    "ci.yml:258 gates nub-launcher, a separate workspace the root clippy never sees",
  );
  assert.match(s, /tests\/brand-lint\/check-env-reads\.sh/);
  assert.match(s, /tests\/brand-lint\/check-path-literals\.sh/, "ci.yml:237 runs a second brand lint");
});

// CI runs the WHOLE workspace (`cargo test`, not `-p nub-cli`) and builds the REAL addon
// first, because the data-format loader tests dlopen it — the 11-byte placeholder would
// make them fail on a malformed library rather than skip.
test("jobScript(test) matches CI: whole workspace, real addon staged over the placeholder", () => {
  const s = jobScript("test", "fast");
  assert.match(s, /\ncargo test$/, "CI runs the whole workspace, not -p nub-cli");
  assert.match(s, /crates\/nub-native && cargo build/);
  assert.match(s, /cp "\$CARGO_TARGET_DIR\/debug\/libnub_native\.so" runtime\/addons\/nub-native\.node/);
  // Anchored to the COMMAND, not the raw text: PREPARE is shared by both jobs and its
  // comments legitimately mention the other gate ("a clippy or test run"), which a bare
  // /clippy/ match reads as the test job running clippy. NOT anchored to line-start — the
  // clippy job's second leg is a subshell, `(cd crates/nub-native && cargo clippy …)`, and
  // a `\ncargo clippy` pattern would let exactly that shape through.
  assert.doesNotMatch(s, /cargo clippy/);
});

// The bake compiles the dependency graph then `rm -rf ~/src`. A target dir inside ~/src
// would be destroyed with it, so every builder would pay a full cold compile while the
// image claimed to carry warm artifacts. Both sides must agree on the path.
test("every job uses a target dir that survives the bake's cleanup of ~/src", () => {
  for (const job of ["clippy", "test"]) {
    const s = jobScript(job, "fast");
    assert.match(s, /export CARGO_TARGET_DIR="\$HOME\/\.cargo-shared-target"/, `${job} must reuse the baked artifacts`);
    assert.ok(!/CARGO_TARGET_DIR="?\$HOME\/src/.test(s), `${job} must not target a dir the bake deletes`);
  }
});

// sshd runs `bash -s` non-interactively; Ubuntu's .bashrc returns before any appended
// PATH line, so rustup's env is never applied and cargo dies with 127.
test("every job sources cargo's env, since a non-interactive ssh shell never does", () => {
  for (const job of ["clippy", "test"]) {
    assert.match(jobScript(job, "fast"), /\. "\$HOME\/\.cargo\/env"/, `${job} would die with cargo: command not found`);
  }
});

// The worst bug found during development, because every observable signal said "fine":
// `bash -s` reads the script from stdin as it executes, so a stdin-reading command inside it
// (npm, cargo) swallows the rest of the un-executed script. bash then hits EOF and exits 0.
// A real bake stopped after `cargo fetch`, skipped the whole warm compile, and still
// published a golden image. The script must therefore never arrive via stdin.
test("the remote job runs from a FILE, never from stdin", () => {
  const cmd = remoteJobCommand("echo hi", Buffer.from("echo hi").toString("base64"));
  assert.doesNotMatch(cmd, /bash\s+-s/, "bash -s reads the script from stdin and can be truncated by it");
  assert.match(cmd, /base64 -d > "\$f"/, "script must be landed on disk");
  assert.match(cmd, /bash "\$f"/, "and executed from that file");
  assert.match(cmd, /exit \$rc/, "the job's real exit status must propagate, not rm's");
  assert.match(cmd, /rm -f "\$f"/, "must not litter /tmp on the builder");
});

test("filterSourceFiles drops entries missing from the worktree and de-dupes the union", () => {
  const listed = ["a.rs", "gone.rs", "a.rs", "tests/node-suite/x.js", "b.rs"].join("\n");
  const out = filterSourceFiles(listed, (f) => f !== "gone.rs");
  assert.deepEqual(out, ["a.rs", "b.rs"], "a deleted-but-tracked file aborts the whole rsync");
});

// Layer 2 is the ONLY orphan-proofing a SIGKILL cannot defeat, so it must cover EVERY
// instance the tool creates — there is no exception to maintain. `--instance-termination-action`
// applies to `--max-run-duration`, not only spot preemption, which is what makes this possible.
for (const [label, terminate, spot] of [["builder", "DELETE", true], ["image bake", "STOP", false]]) {
  test(`the ${label} VM terminates server-side, so a SIGKILLed launcher cannot orphan it`, () => {
    const args = instanceCreateArgs("v1", "ssh-ed25519 KEY", {
      machine: "c3-standard-8", zone: "us-central1-a", onDemand: !spot, fromImage: spot, terminate,
    });
    assert.ok(args.includes("--max-run-duration"), "missing the server-side TTL");
    const idx = args.indexOf("--instance-termination-action");
    assert.notEqual(idx, -1, "missing the termination action");
    assert.equal(args[idx + 1], terminate);
    assert.equal(args.includes("SPOT"), spot);
    assert.ok(args.includes("--labels") && args.includes("nub-builder=1"), "must stay reapable");
  });
}

// A builder must DELETE (a merely-stopped VM still bills its disk); the bake must STOP,
// because the very next thing it does is image that disk.
test("a builder deletes itself but the bake only stops, so its disk survives imaging", () => {
  const b = instanceCreateArgs("b", "K", { machine: "c3-standard-8", zone: "z", onDemand: false, fromImage: true, terminate: "DELETE" });
  const k = instanceCreateArgs("k", "K", { machine: "c3-standard-8", zone: "z", onDemand: true, fromImage: false, terminate: "STOP" });
  assert.equal(b[b.indexOf("--instance-termination-action") + 1], "DELETE");
  assert.equal(k[k.indexOf("--instance-termination-action") + 1], "STOP");
  assert.notEqual(
    b[b.indexOf("--max-run-duration") + 1],
    k[k.indexOf("--max-run-duration") + 1],
    "the bake needs a longer TTL than a build",
  );
});

// --reap is layer 3, the safety net. With ~20 agents sharing one GCP project, a reap with no
// age gate destroys a sibling's in-flight build — the net becoming the hazard.
test("reap treats only VMs older than the bake TTL as stray", () => {
  const now = Date.parse("2026-07-28T12:00:00Z");
  assert.equal(isStray("2026-07-28T11:59:00Z", now, 90 * 60_000), false, "a 1-minute-old VM is someone's live build");
  assert.equal(isStray("2026-07-28T10:00:00Z", now, 90 * 60_000), true, "2h old is past every TTL");
  assert.equal(isStray("not-a-timestamp", now, 90 * 60_000), false, "unparseable age must never read as abandoned");
});

test("filterSourceFiles drops the node-suite submodule and keeps everything else", () => {
  const out = filterSourceFiles(
    ["Cargo.toml", "crates/nub-cli/src/main.rs", "tests/node-suite/test/x.js", "tests/pnp/run.sh", ""].join("\n"),
  );
  assert.deepEqual(out, ["Cargo.toml", "crates/nub-cli/src/main.rs", "tests/pnp/run.sh"]);
});


// THE DRIFT THIS TOOL ALREADY PAID FOR, now mechanically enforced. The bake warmed
// `cargo build -p nub-cli --profile fast` while the clippy job ran
// `cargo clippy --all-targets --all-features --profile fast`. Cargo fingerprints on the command
// shape, so those artifacts were unusable for four independent reasons at once — different
// driver, different profile directory, different package scope, different feature set — and the
// image advertised warm while every builder cold-compiled. Three separate comments asked the
// next person to keep these in lockstep; none of them could fail. This can.
//
// Reads the bake block out of the SOURCE rather than importing it: the warm script is a
// template literal local to `buildImage`, and extracting it into an exported function means
// re-emitting a body that contains escaped backticks — which silently truncated it when tried.
// Prefix matching, not equality: the bake may warm MORE than the job runs, and `cargo test` is
// deliberately warmed as `cargo test --workspace --no-run`, which stops at link.
test("the bake warms every cargo invocation the jobs actually run", () => {
  const src = readFileSync(new URL("./remote-build.ts", import.meta.url), "utf8");
  const from = src.indexOf("const warm = `set -euxo pipefail");
  const to = src.indexOf('echo "warm target dir', from);
  assert.ok(from > 0 && to > from, "could not locate the bake's warm block in the source");
  const bake = src.slice(from, to);

  const cargoLines = (script) =>
    script
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => !l.startsWith("#") && !l.startsWith("//") && /(^|\W)cargo\s/.test(l))
      .map((l) => l.split("||")[0].trim());

  const warmed = cargoLines(bake);
  assert.ok(warmed.length >= 4, `only found ${warmed.length} cargo lines in the bake — parse broke`);

  // Prefix alone is not enough: a bake-side flag that changes the artifact universe still
  // prefix-matches. `cargo test --release --workspace --no-run` satisfies `cargo test` while
  // targeting a different directory entirely — the exact class this test exists to catch — so
  // the profile is compared explicitly. Absent flags mean cargo's default, `dev`.
  // The tails matter: two of the four bake legs are subshells, so a flag can be followed by
  // `)` rather than whitespace or end-of-string. A `(\s|$)` probe reads
  // `(cd crates/nub-native && cargo build --release)` as profile "dev" and waves it through.
  const profileOf = (cmd) =>
    /(^|\s)--release([\s)]|$)/.test(cmd) ? "release" : (cmd.match(/--profile\s+([^\s)]+)/)?.[1] ?? "dev");

  for (const job of ["clippy", "test"]) {
    for (const cmd of cargoLines(jobScript(job, "fast"))) {
      const match = warmed.find((w) => w.startsWith(cmd));
      assert.ok(
        match,
        `bake does not warm jobScript(${job})'s \`${cmd}\` — builders will cold-compile it. ` +
          `Bake warms:\n  ${warmed.join("\n  ")}`,
      );
      assert.equal(
        profileOf(match),
        profileOf(cmd),
        `bake warms \`${match}\` but jobScript(${job}) runs \`${cmd}\` — different profile, ` +
          `so different target directory, so the warmed artifacts are unusable`,
      );
    }
  }
});
