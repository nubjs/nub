// Does `python_toolchain_grant` FIRE on Windows, and if it fires does it RESOLVE?
//
// WHY THIS EXISTS. The Windows corpus breaks are dominated by node-gyp reporting `Could not
// find any Python installation to use` (96 occurrences confined against 9 unconfined), and
// all three of node-gyp's own discovery routes fail in sequence. nub is supposed to make
// route 1 unnecessary: it pre-resolves Python OUTSIDE the jail and names the winner in
// `npm_config_python`. Whether that pre-resolution happens at all on Windows had never been
// measured — the failure is INVISIBLE, because an unresolved grant leaves the variable
// unset and node-gyp then reports its OWN search failing, which looks the same whether the
// gyp-manifest gate bailed, nothing resolved off PATH, or the probe spawn died.
//
// So this probe reads BOTH HALVES of the answer:
//   - nub's side  — `build_jail.python_grant` diag lines, which name the gate result, the
//                   PATH key spellings actually present, the resolved candidates, the stage
//                   that rejected each, and the winner. Requires `NUB_DIAG_PRINT=1`.
//   - the child's side — node-gyp's own Python lines from inside the jail.
// A resolved grant whose value never reaches the child is a third outcome neither half sees
// alone, and this jail has hit exactly that shape before (`NODE_EXECUTABLE`, scrubbed by the
// env allowlist after being resolved correctly).
//
// THE PACKAGES ARE MIXED ON PURPOSE, and kept to three. A prior control shard of 13
// homogeneous native-build packages in one project produced absolute verdicts that
// contradicted the same packages' corpus results, because 13 concurrent node-gyp builds
// contend. Each package here gets its OWN project so nothing is shared, and the verdict is
// a diagnostic line rather than a pass/fail, so contention cannot forge it.
//
// Usage: NUB_DIAG_PRINT=1 node probe.mjs <path-to-nub-binary>

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

const nub = process.argv[2] ? resolve(process.argv[2]) : null;
if (!nub || !existsSync(nub)) {
  console.error(`usage: node probe.mjs <path-to-nub-binary> (got ${nub})`);
  process.exit(2);
}

// `deasync` is the plainest possible case — `scripts.install` is literally `node-gyp
// rebuild` against a root `binding.gyp`, so it reaches Python discovery with nothing in
// front of it. `ssh2` keeps its manifest at `lib/protocol/crypto/binding.gyp`, which is the
// one package known to exercise `has_gyp_manifest`'s subdirectory search rather than its
// root check — the gate half of the question, on the exact shape that motivated the search.
// `bcrypt` goes through node-pre-gyp, so it only reaches node-gyp after a prebuilt lookup
// declines: a different class, which is what keeps this from measuring one code path three
// times.
const PACKAGES = [
  { name: "deasync", spec: "deasync@0.1.30" },
  { name: "ssh2", spec: "ssh2@1.15.0" },
  { name: "bcrypt", spec: "bcrypt@5.1.1" },
];

const work = join(homedir(), `.nub-gyp-python-probe-${process.pid}`);
rmSync(work, { recursive: true, force: true });
mkdirSync(work, { recursive: true });

// The grant line is emitted per lifecycle spawn, so it is the thing to count. A run that
// emits ZERO of them has told us nothing and must not be readable as "the grant is fine" —
// the workflow asserts on this count, not on any install's exit status.
let grantLines = 0;

for (const pkg of PACKAGES) {
  console.log(`\n======== package: ${pkg.spec} ========`);
  const proj = join(work, pkg.name);
  mkdirSync(proj, { recursive: true });
  writeFileSync(
    join(proj, "package.json"),
    JSON.stringify(
      {
        name: `nub-gyp-python-${pkg.name}`,
        version: "1.0.0",
        private: true,
        dependencies: { [pkg.name]: pkg.spec.slice(pkg.spec.lastIndexOf("@") + 1) },
      },
      null,
      2,
    ),
  );

  const run = (label, args) => {
    // `NUB_DIAG_PRINT` goes to nub's OWN stderr, not the confined child's — the grant is
    // computed in nub's process before any policy exists, so this is not an env var the
    // jail could scrub on the way in.
    const r = spawnSync(nub, args, {
      cwd: proj,
      encoding: "utf8",
      env: { ...process.env, NUB_DIAG_PRINT: "1" },
    });
    const err = r.stderr ?? "";
    console.log(`${pkg.name}/${label} rc=${r.status} signal=${r.signal ?? "none"} error=${r.error ? r.error.message : "none"}`);
    console.log(`---- ${label} stdout ----\n${r.stdout ?? ""}`);
    console.log(`---- ${label} stderr ----\n${err}`);
    for (const line of err.split(/\r?\n/)) {
      if (line.includes("build_jail.python_grant")) {
        grantLines += 1;
        console.log(`grant:${pkg.name} ${line.trim()}`);
      }
      // node-gyp's own verdict, quoted so the two halves sit beside each other in the log.
      if (/Could not find any Python|checking if "|is not set from environment variable PYTHON|Found Python/.test(line)) {
        console.log(`gyp:${pkg.name} ${line.trim()}`);
      }
    }
    return r;
  };

  run("install", ["install", "--no-frozen-lockfile"]);
  run("approve-builds", ["approve-builds", "--all"]);
}

rmSync(work, { recursive: true, force: true });
console.log(`\nprop:grant-diag-emitted=${grantLines > 0 ? "PASS" : "FAIL"} lines=${grantLines}`);
process.exit(grantLines > 0 ? 0 : 1);
