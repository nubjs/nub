// Does a NATIVE ADDON actually COMPILE inside nub's build jail? — an end-to-end probe.
//
// WHY IT EXISTS. Every other build-jail probe in this repo asserts a POLICY: that a grant
// derives, that a launch succeeds, that an egress is refused. None of them compiles
// anything, so all of them stay green on a host where node-gyp can launch and then fail
// for want of headers. That is exactly the Windows state this probe was written to catch:
// the Windows distribution ships no `include/node` and no `node.lib`, node-gyp SKIPS its
// own header download whenever `npm_config_nodedir` is set, and the Windows jail is net
// deny-all — three facts that compose into "no native addon can build" while every
// policy-level probe reports success.
//
// SO THE VERDICT IS AN ARTIFACT ON DISK. `prop:addon-artifact` passes only when a real
// `.node` file exists and is non-empty, and `prop:addon-loads` only when a Node process
// outside the jail `require`s it and gets the value the C++ returns. Exit codes are
// deliberately NOT the verdict: a lifecycle script can exit 0 having done nothing, and
// this repo has measured exactly that.
//
// TWO CONTROLS, both of which must hold in the FIXED and the POISONED build alike, so a
// red verdict cannot be a broken probe:
//   - `prop:jailed-child-ran`  — the confined script wrote a marker into its own package
//                                dir, the one place it may write. Proves the script RAN,
//                                which also refutes the store serving a previously-built
//                                copy (measured: `rm -rf node_modules` alone leaves the
//                                artifact intact and the script unrun).
//   - `prop:jail-enforced`     — that same script tried to read a canary outside every
//                                grant and was REFUSED. Proves it ran CONFINED, so a probe
//                                that accidentally unjailed itself is not mistaken for a
//                                passing fix.
// Both are read back from the marker FILE the child wrote, never from a status the child
// reported about itself.
//
// Usage: node probe.mjs <path-to-nub-binary>

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

// ABSOLUTE, always. The install runs with `cwd` set to the fixture, and Node resolves a
// relative command against that NEW cwd — so a relative `target/fast/nub` silently fails
// to spawn, surfacing as `status: null` and an empty log rather than as an error.
const nub = process.argv[2] ? resolve(process.argv[2]) : null;
if (!nub) {
  console.error("usage: node probe.mjs <path-to-nub-binary>");
  process.exit(2);
}
if (!existsSync(nub)) {
  console.error(`no nub binary at ${nub}`);
  process.exit(2);
}

// NOT under the system temp dir: the jail grants a private tmp, so a canary placed there
// would be legitimately readable and `jail-enforced` would fail for a reason that has
// nothing to do with enforcement. The pid also keys the store entry (a `file:` dep hashes
// its path), which is what forces a genuine rebuild rather than a cached artifact.
const work = join(homedir(), `.nub-jail-native-probe-${process.pid}`);
rmSync(work, { recursive: true, force: true });

const addon = join(work, "addon");
const consumer = join(work, "consumer");
const canary = join(work, "canary-secret.txt");
mkdirSync(addon, { recursive: true });
mkdirSync(consumer, { recursive: true });
writeFileSync(canary, "the-confined-build-must-not-read-this\n");

writeFileSync(
  join(addon, "package.json"),
  JSON.stringify(
    {
      name: "nub-jail-probe-addon",
      version: "1.0.0",
      // `preflight.cjs` runs FIRST and unconditionally, so both controls are recorded
      // whether or not the compile that follows succeeds — which is the whole point: in
      // the poisoned arm the compile must fail while the controls still hold.
      scripts: { install: "node preflight.cjs && node-gyp rebuild" },
    },
    null,
    2,
  ),
);

// No `include_dirs`: the addon is plain N-API C against `node_api.h`, which node-gyp
// already supplies from `<nodedir>/include/node` — the exact header tree whose absence on
// Windows this probe exists to detect. Pulling in node-addon-api would put a registry
// fetch on the critical path and test the wrong thing.
writeFileSync(
  join(addon, "binding.gyp"),
  JSON.stringify({ targets: [{ target_name: "nub_jail_probe", sources: ["addon.cc"] }] }, null, 2),
);

writeFileSync(
  join(addon, "addon.cc"),
  `#include <node_api.h>

napi_value Probe(napi_env env, napi_callback_info info) {
  napi_value out;
  napi_create_string_utf8(env, "nub-jail-probe-ok", NAPI_AUTO_LENGTH, &out);
  return out;
}

napi_value Init(napi_env env, napi_value exports) {
  napi_value fn;
  napi_create_function(env, "probe", NAPI_AUTO_LENGTH, Probe, NULL, &fn);
  napi_set_named_property(env, exports, "probe", fn);
  return exports;
}

NAPI_MODULE(NODE_GYP_MODULE_NAME, Init)
`,
);

// The canary path is BAKED IN as a literal, not passed through the environment. Measured:
// an env var set on the `nub install` process does NOT reach the confined child (the jail
// replaces the env axis with the engine's constructed lifecycle env), so an env-carried
// path arrived `undefined`, `readFileSync(undefined)` threw ERR_INVALID_ARG_TYPE, and a
// naive "did it throw?" test read that as a refusal. That is a control that confirms
// enforcement without testing it — the worst kind. The marker now records the path and the
// error CODE, and the assertion demands the code a real denial produces.
writeFileSync(
  join(addon, "preflight.cjs"),
  `// Runs INSIDE the jail, before the compile. Writes both controls to a marker file in the
// package dir — the one location the jail grants write.
const fs = require("fs");
const path = require("path");

const CANARY = ${JSON.stringify(canary)};
let canaryRead;
try {
  canaryRead = "allowed:" + fs.readFileSync(CANARY, "utf8").trim().length;
} catch (err) {
  canaryRead = "refused:" + ((err && err.code) || "unknown");
}

fs.writeFileSync(
  path.join(__dirname, "probe-marker.json"),
  JSON.stringify({ canary: CANARY, canaryRead, nodedir: process.env.npm_config_nodedir || "" }, null, 2),
);
`,
);

writeFileSync(
  join(consumer, "package.json"),
  JSON.stringify(
    {
      name: "nub-jail-probe-consumer",
      version: "1.0.0",
      private: true,
      dependencies: { "nub-jail-probe-addon": `file:${addon.replace(/\\/g, "/")}` },
    },
    null,
    2,
  ),
);

const run = (label, args) => {
  const r = spawnSync(nub, args, { cwd: consumer, encoding: "utf8" });
  console.log(
    `${label} rc=${r.status} signal=${r.signal ?? "none"} error=${r.error ? r.error.message : "none"}`,
  );
  console.log(`---- ${label} stdout ----\n${r.stdout ?? ""}`);
  console.log(`---- ${label} stderr ----\n${r.stderr ?? ""}`);
  return r;
};

// The default-trust floor is a curated allowlist and a probe fixture is on no curated
// list, so the first install DECLINES to run the build script and names it. Approving is
// a second, explicit step by design; `--all` is itself the consent, so it needs no TTY.
// Going through `approve-builds` rather than hand-writing an `allowBuilds` entry lets nub
// derive the source-backed approval key itself — a `file:` dep is authorized by its source
// spec, whose spelling differs between platforms.
run("install", ["install", "--no-frozen-lockfile"]);
run("approve-builds", ["approve-builds", "--all"]);

const installed = join(consumer, "node_modules", "nub-jail-probe-addon");
const marker = join(installed, "probe-marker.json");
const artifact = join(installed, "build", "Release", "nub_jail_probe.node");

const results = [];
const record = (name, ok, detail) => {
  results.push({ name, ok });
  console.log(`prop:${name}=${ok ? "PASS" : "FAIL"} ${detail}`);
};

let markerData = null;
if (existsSync(marker)) {
  try {
    markerData = JSON.parse(readFileSync(marker, "utf8"));
  } catch (err) {
    console.log(`marker unparseable: ${err.message}`);
  }
}
record("jailed-child-ran", markerData !== null, `marker=${marker}`);

// A REAL denial only. `ERR_INVALID_ARG_TYPE` means the path never arrived and nothing was
// tested; `ENOENT` means the canary is missing rather than withheld. Both are probe bugs,
// and both must read FAIL rather than quietly confirming enforcement.
const denials = ["EPERM", "EACCES"];
const canaryRead = markerData?.canaryRead ?? "<no marker>";
record(
  "jail-enforced",
  denials.some((code) => canaryRead === `refused:${code}`),
  `canaryRead=${canaryRead} (a pass needs exactly one of ${denials.map((c) => `refused:${c}`).join(", ")})`,
);
console.log(`observed npm_config_nodedir=${markerData?.nodedir ?? "<no marker>"}`);

const artifactSize = existsSync(artifact) ? statSync(artifact).size : -1;
record("addon-artifact", artifactSize > 0, `path=${artifact} size=${artifactSize}`);

const load = spawnSync(
  process.execPath,
  ["-e", `process.stdout.write(require(${JSON.stringify(artifact)}).probe())`],
  { encoding: "utf8" },
);
record(
  "addon-loads",
  load.status === 0 && (load.stdout ?? "").includes("nub-jail-probe-ok"),
  `rc=${load.status} out=${JSON.stringify((load.stdout ?? "").slice(0, 120))} err=${JSON.stringify((load.stderr ?? "").slice(0, 400))}`,
);

rmSync(work, { recursive: true, force: true });

const failed = results.filter((r) => !r.ok).map((r) => r.name);
console.log(`probe complete: ${results.length - failed.length}/${results.length} properties passed`);
// The exit code reports the PROBE's own verdict; the workflow decides what each arm expects.
process.exit(failed.length === 0 ? 0 : 1);
