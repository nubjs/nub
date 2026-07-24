// Stamp one version across every version surface: the 10 npm package.jsons
// (root optionalDependencies pinned exact), both workspace Cargo.tomls, and
// runtime/version.mjs (NUB_VERSION — the transpile-cache key, which must stay in
// lockstep with the binary version or a stale cache would serve stale output
// after an upgrade). Shared by `make version V=<v>` (committed release bumps —
// the Makefile also refreshes the Cargo.lock entries) and release.yml's canary
// stamp (an UNCOMMITTED prerelease set on the build runners, where `make` isn't
// dependable on Windows; the lockfiles self-heal on the next cargo invocation).
// Run from the repo root: node scripts/set-version.mjs <version>
import fs from "node:fs";

const v = process.argv[2];
if (!v) {
  console.error("Usage: node scripts/set-version.mjs <version>");
  process.exit(1);
}

const pkgs = [
  "npm/nub/package.json",
  "npm/nub-types/package.json",
  "npm/nub-darwin-arm64/package.json",
  "npm/nub-darwin-x64/package.json",
  "npm/nub-linux-x64/package.json",
  "npm/nub-linux-x64-musl/package.json",
  "npm/nub-linux-arm64/package.json",
  "npm/nub-linux-arm64-musl/package.json",
  "npm/nub-win32-x64/package.json",
  "npm/nub-win32-arm64/package.json",
];
for (const f of pkgs) {
  const p = JSON.parse(fs.readFileSync(f, "utf8"));
  p.version = v;
  if (p.optionalDependencies) {
    for (const k of Object.keys(p.optionalDependencies)) p.optionalDependencies[k] = v;
  }
  fs.writeFileSync(f, JSON.stringify(p, null, 2) + "\n");
}

const replaceOrDie = (file, re, replacement) => {
  const src = fs.readFileSync(file, "utf8");
  const next = src.replace(re, replacement);
  if (next === src) {
    console.error(`ERROR: version line not found in ${file}`);
    process.exit(1);
  }
  fs.writeFileSync(file, next);
};
replaceOrDie("Cargo.toml", /^version = .*/m, `version = "${v}"`);
replaceOrDie("crates/nub-native/Cargo.toml", /^version = .*/m, `version = "${v}"`);
replaceOrDie(
  "runtime/version.mjs",
  /export const NUB_VERSION = .*/,
  `export const NUB_VERSION = "${v}";`,
);

console.log(`✓ npm packages, both Cargo.tomls, and runtime/version.mjs set to ${v}`);
