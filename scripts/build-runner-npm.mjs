// Stage the standalone-runner npm packages from the runtime/ sources.
//
// The `@nubjs/runner` package ships a curated slice of runtime/ verbatim — the
// `nubr` command and the preload entrypoints, plus the shared resolve/transpile
// machinery (transform-core and friends) — laid out FLAT at the package root so
// every relative require works
// unchanged. This script copies that slice into npm/runner/ and, when a built
// addon is present (or --addon points at one), places it in the current
// platform's npm/runner-<platform>/ package. Release CI runs it once per
// platform leg; locally it stages whatever the dev tree has built.
//
//   node scripts/build-runner-npm.mjs [--addon <path-to-nub-native.node>] [--pack]
//
// --pack additionally runs `npm pack` in each staged package dir, leaving
// versioned tarballs in place (the local e2e installs from these).
import { cpSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// The exact file closure of the runner entrypoints. Additions to the entries'
// relative-import graph must be mirrored here; the existence check below turns a
// forgotten file into a hard failure rather than a broken tarball.
const RUNTIME_FILES = [
  "nubr.mjs",
  "nubr-escape.mjs",
  "loader-register.mjs",
  "loader-register.cjs",
  "loader-esm.mjs",
  "loader-entry.mjs",
  "loader-addon-env.mjs",
  "loader-platform.cjs",
  "transform-core.mjs",
  "preload-common.cjs",
  "preload-async-hooks.mjs",
  "pnp-util.cjs",
  "floor-builtin.mjs",
  "cache-evict.mjs",
];

const args = process.argv.slice(2);
const pack = args.includes("--pack");
const addonFlag = args.indexOf("--addon");
const addonPath =
  addonFlag !== -1 ? resolve(args[addonFlag + 1]) : join(repo, "runtime", "addons", "nub-native.node");

const runnerDir = join(repo, "npm", "runner");
for (const f of RUNTIME_FILES) {
  const src = join(repo, "runtime", f);
  if (!existsSync(src)) {
    console.error(`missing runtime file: ${src}`);
    process.exit(1);
  }
  cpSync(src, join(runnerDir, f));
}
cpSync(join(repo, "LICENSE"), join(runnerDir, "LICENSE"));
console.log(`staged ${RUNTIME_FILES.length} runtime files into npm/runner/`);

// Verify the staged file list matches the manifest's `files` allowlist, so a file
// added to RUNTIME_FILES but not to package.json (or vice versa) fails here.
const manifest = JSON.parse(readFileSync(join(runnerDir, "package.json"), "utf8"));
const missing = RUNTIME_FILES.filter((f) => !manifest.files.includes(f));
if (missing.length) {
  console.error(`package.json files[] is missing: ${missing.join(", ")}`);
  process.exit(1);
}

const packed = [runnerDir];

// The current platform's addon package. Cross-platform staging is CI's job (one
// leg per platform); locally only the host's package can be staged.
if (existsSync(addonPath)) {
  // file: URL, not a bare path — Windows absolute paths are not importable.
  const { platformKey } = await import(
    pathToFileURL(join(repo, "runtime", "loader-platform.cjs")).href
  ).then((m) => m.default ?? m);
  const platDir = join(repo, "npm", `runner-${platformKey()}`);
  mkdirSync(platDir, { recursive: true });
  cpSync(addonPath, join(platDir, "nub-native.node"));
  console.log(`staged addon → npm/runner-${platformKey()}/nub-native.node`);
  packed.push(platDir);
} else {
  console.log(`no addon at ${addonPath} — platform package not staged`);
}

if (pack) {
  for (const dir of packed) {
    execFileSync("npm", ["pack"], { cwd: dir, stdio: "inherit" });
  }
}
