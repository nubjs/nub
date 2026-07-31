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

import { execFileSync, spawnSync } from "node:child_process";
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
const canary = join(work, "canary-secret.txt");
mkdirSync(addon, { recursive: true });
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
      files: ["addon.cc", "binding.gyp", "preflight.cjs"],
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
// enforcement without testing it — the worst kind. The marker records the error CODE, and
// the assertion demands the code a real denial produces.
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

// The MSVC trio is recorded from INSIDE the jail because that is the only place the
// question is meaningful: nub resolving Visual Studio in the parent proves nothing if the
// env allowlist then drops the answer on the way in. A build that succeeds without these
// would mean node-gyp found MSVC some other way and the injection is not what fixed it.
//
// \`npm_config_python\` is here for exactly the same reason, and it is the DELIVERY half of
// the answer: nub's own diagnostic says whether the grant resolved, this says whether the
// value survived the env allowlist. Only both together tell an unresolved grant apart from
// a resolved one that was scrubbed on the way in — a silent-drop shape this jail has hit
// before (\`NODE_EXECUTABLE\`).

// ROUTE 2, REPRODUCED WHERE IT ACTUALLY FAILS. node-gyp's second discovery route is a bare
// name resolved against the CHILD's PATH, and under the jail it reports \`executable path
// is ""\` for \`python3\`, \`python\` and \`py\` alike. Every prior elimination checked nub's
// resolution logic in the parent, which is fine; none checked the shape of the environment
// block the child receives. Win32 environments are case-insensitive and hold each name
// once, so a block carrying both \`Path\` and \`PATH\` is not a defined shape — and an empty
// or truncated PATH inside the child would produce exactly the observed symptom while
// leaving every parent-side check correct.
//
// So this records the child's PATH SHAPE (which spellings, how long, how many entries) and
// then walks it for the three bare names, keeping the error CODES. An empty \`hits\` with a
// well-formed PATH and \`EPERM\` codes is a denial; an empty \`hits\` with a short PATH is a
// delivery defect. The two are indistinguishable from node-gyp's own message.
const PATH_KEYS = Object.keys(process.env).filter(function (k) {
  return k.toLowerCase() === "path";
});
const RAW_PATH = process.env.PATH || "";
const SEP = process.platform === "win32" ? ";" : ":";
const EXTS = process.platform === "win32" ? [".exe", ""] : [""];
const PATH_DIRS = RAW_PATH.split(SEP).filter(Boolean);
const bareLookup = {};
["python3", "python", "py"].forEach(function (name) {
  const hits = [];
  const codes = {};
  PATH_DIRS.forEach(function (dir) {
    EXTS.forEach(function (ext) {
      const cand = path.join(dir, name + ext);
      try {
        if (fs.statSync(cand).isFile()) hits.push(cand);
      } catch (err) {
        const code = (err && err.code) || "unknown";
        codes[code] = (codes[code] || 0) + 1;
      }
    });
  });
  bareLookup[name] = { hits: hits.slice(0, 3), codes: codes };
});

fs.writeFileSync(
  path.join(__dirname, "probe-marker.json"),
  JSON.stringify({
    canary: CANARY,
    canaryRead,
    nodedir: process.env.npm_config_nodedir || "",
    python: process.env.npm_config_python || "",
    pathKeys: PATH_KEYS,
    pathLen: RAW_PATH.length,
    pathDirs: PATH_DIRS.length,
    bareLookup: bareLookup,
    vcInstallDir: process.env.VCINSTALLDIR || "",
    vscmdVer: process.env.VSCMD_VER || "",
    sdkVersion: process.env.WindowsSDKVersion || "",
  }, null, 2),
);
`,
);

const posix = (p) => p.replace(/\\/g, "/");

// TWO DELIVERY SHAPES for the identical fixture, because the shape is not neutral on
// Windows. A `file:` DIRECTORY dependency crashes `nub install` outright there — measured
// on this harness's first run: `thread 'main' has overflowed its stack`, rc 0xC00000FD,
// identically in the fixed and poisoned arms, so it is a pre-existing defect elsewhere and
// not something this branch introduced. A packed TARBALL takes the ordinary
// fetch-and-extract path instead. Both are reported so the crash stays visible rather
// than being quietly designed around; only `tarball` gates the verdict.
const shapes = [];
try {
  const packed = execFileSync("npm", ["pack", addon, "--pack-destination", work, "--silent"], {
    encoding: "utf8",
    shell: process.platform === "win32",
  })
    .trim()
    .split(/\r?\n/)
    .pop();
  shapes.push({ name: "tarball", spec: `file:${posix(join(work, packed))}` });
} catch (err) {
  console.log(`shape:tarball=UNAVAILABLE (npm pack failed: ${err.message})`);
}
shapes.push({ name: "dir", spec: `file:${posix(addon)}` });

const results = [];
const record = (name, ok, detail) => {
  results.push({ name, ok });
  console.log(`prop:${name}=${ok ? "PASS" : "FAIL"} ${detail}`);
};

for (const shape of shapes) {
  console.log(`\n======== shape: ${shape.name} (${shape.spec}) ========`);
  const consumer = join(work, `consumer-${shape.name}`);
  mkdirSync(consumer, { recursive: true });
  writeFileSync(
    join(consumer, "package.json"),
    JSON.stringify(
      {
        name: "nub-jail-probe-consumer",
        version: "1.0.0",
        private: true,
        dependencies: { "nub-jail-probe-addon": shape.spec },
      },
      null,
      2,
    ),
  );

  const run = (label, args) => {
    const r = spawnSync(nub, args, { cwd: consumer, encoding: "utf8" });
    console.log(
      `${shape.name}/${label} rc=${r.status} signal=${r.signal ?? "none"} error=${r.error ? r.error.message : "none"}`,
    );
    console.log(`---- ${label} stdout ----\n${r.stdout ?? ""}`);
    console.log(`---- ${label} stderr ----\n${r.stderr ?? ""}`);
    return r;
  };

  // The default-trust floor is a curated allowlist and a probe fixture is on no curated
  // list, so the first install DECLINES to run the build script and names it. Approving is
  // a second, explicit step by design; `--all` is itself the consent, so it needs no TTY.
  // Going through `approve-builds` rather than hand-writing an `allowBuilds` entry lets nub
  // derive the source-backed approval key itself, whose spelling differs between platforms.
  run("install", ["install", "--no-frozen-lockfile"]);
  run("approve-builds", ["approve-builds", "--all"]);

  const installed = join(consumer, "node_modules", "nub-jail-probe-addon");
  const marker = join(installed, "probe-marker.json");
  const artifact = join(installed, "build", "Release", "nub_jail_probe.node");

  let markerData = null;
  if (existsSync(marker)) {
    try {
      markerData = JSON.parse(readFileSync(marker, "utf8"));
    } catch (err) {
      console.log(`marker unparseable: ${err.message}`);
    }
  }
  record(`${shape.name}/jailed-child-ran`, markerData !== null, `marker=${marker}`);

  // A REAL denial only. `ERR_INVALID_ARG_TYPE` means the path never arrived and nothing was
  // tested; `ENOENT` means the canary is missing rather than withheld. Both are probe bugs,
  // and both must read FAIL rather than quietly confirming enforcement.
  const denials = ["EPERM", "EACCES"];
  const canaryRead = markerData?.canaryRead ?? "<no marker>";
  record(
    `${shape.name}/jail-enforced`,
    denials.some((code) => canaryRead === `refused:${code}`),
    `canaryRead=${canaryRead} (a pass needs exactly one of ${denials.map((c) => `refused:${c}`).join(", ")})`,
  );
  console.log(`observed ${shape.name} npm_config_nodedir=${markerData?.nodedir ?? "<no marker>"}`);
  console.log(
    `observed ${shape.name} npm_config_python=${JSON.stringify(markerData?.python ?? "<no marker>")} child-path-keys=${JSON.stringify(markerData?.pathKeys ?? "<no marker>")} child-path-len=${markerData?.pathLen ?? "?"} child-path-dirs=${markerData?.pathDirs ?? "?"}`,
  );
  console.log(
    `observed ${shape.name} child-bare-lookup=${JSON.stringify(markerData?.bareLookup ?? "<no marker>")}`,
  );

  // WHICH TOOLCHAIN, not merely whether something built. `config.gypi` is node-gyp's own
  // record of the installation it selected, written by `configure` before any compile — so
  // it survives a failed build and is the one place the answer is not inferred. A build that
  // succeeds against an unexpected compiler is a worse outcome than a clean failure, and
  // these lines are what make the two distinguishable.
  const gypi = join(installed, "build", "config.gypi");
  let selected = "<no config.gypi>";
  if (existsSync(gypi)) {
    const text = readFileSync(gypi, "utf8");
    const field = (key) => (text.match(new RegExp(`"${key}":\\s*"([^"]*)"`)) ?? [, ""])[1];
    selected = `msbuild=${field("msbuild_path")} toolset=${field("msbuild_toolset")} sdk=${field("msvs_windows_target_platform_version")}`;
  }
  console.log(`observed ${shape.name} toolchain-selected ${selected}`);
  console.log(
    `observed ${shape.name} child-msvc-env VCINSTALLDIR=${markerData?.vcInstallDir ?? "<no marker>"} VSCMD_VER=${markerData?.vscmdVer ?? "<no marker>"} WindowsSDKVersion=${markerData?.sdkVersion ?? "<no marker>"}`,
  );

  const artifactSize = existsSync(artifact) ? statSync(artifact).size : -1;
  record(`${shape.name}/addon-artifact`, artifactSize > 0, `path=${artifact} size=${artifactSize}`);

  const load = spawnSync(
    process.execPath,
    ["-e", `process.stdout.write(require(${JSON.stringify(artifact)}).probe())`],
    { encoding: "utf8" },
  );
  record(
    `${shape.name}/addon-loads`,
    load.status === 0 && (load.stdout ?? "").includes("nub-jail-probe-ok"),
    `rc=${load.status} out=${JSON.stringify((load.stdout ?? "").slice(0, 120))} err=${JSON.stringify((load.stderr ?? "").slice(0, 400))}`,
  );
}

rmSync(work, { recursive: true, force: true });

const failed = results.filter((r) => !r.ok).map((r) => r.name);
console.log(`probe complete: ${results.length - failed.length}/${results.length} properties passed`);
// The exit code reports the PROBE's own verdict; the workflow decides what each arm expects.
process.exit(failed.length === 0 ? 0 : 1);
