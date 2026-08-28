"use strict";
// Standalone-loader addon location: platform → `@nubjs/loader-<platform>` package
// selection, plus the resolver that turns the selected package into an absolute
// `nub-native.node` path. Mirrors npm/nub/platform.js (same musl detection, same
// platform matrix) but for the loader's per-platform addon packages, which carry
// the ~6 MB N-API addon instead of the full CLI binary. CommonJS so both the ESM
// side-effect module (loader-addon-env.mjs) and any CJS entry can share it.

const PLATFORMS = {
  "darwin-arm64": "@nubjs/loader-darwin-arm64",
  "darwin-x64": "@nubjs/loader-darwin-x64",
  "linux-x64": "@nubjs/loader-linux-x64",
  "linux-x64-musl": "@nubjs/loader-linux-x64-musl",
  "linux-arm64": "@nubjs/loader-linux-arm64",
  "linux-arm64-musl": "@nubjs/loader-linux-arm64-musl",
  "win32-x64": "@nubjs/loader-win32-x64",
  "win32-arm64": "@nubjs/loader-win32-arm64",
};

// True on a musl Linux (Alpine, etc.). Primary signal: Node's own diagnostic
// report — `header.glibcVersionRuntime` is present on glibc and absent on musl.
// Fallback: `ldd --version`, whose merged output contains "musl" there (the
// stderr-only read shipped wrong once in npm/nub — check the merged output).
function isMusl() {
  if (process.platform !== "linux") return false;
  try {
    const report = process.report.getReport();
    const header = (typeof report === "string" ? JSON.parse(report) : report).header;
    if (header && "glibcVersionRuntime" in header) {
      return !header.glibcVersionRuntime;
    }
  } catch {
    // process.report unavailable — fall through to ldd.
  }
  try {
    const out = require("child_process").execSync("ldd --version 2>&1", { encoding: "utf8" });
    return out.includes("musl");
  } catch (e) {
    const out = `${(e && e.stdout) || ""}${(e && e.stderr) || ""}`;
    return out.includes("musl");
  }
}

function platformKey() {
  const base = `${process.platform}-${process.arch}`;
  return isMusl() ? `${base}-musl` : base;
}

// Absolute path to this platform's `nub-native.node`, resolved from the loader
// package's own dependency tree via the caller-supplied `require` (created from a
// file inside the package, so the node_modules walk starts next to the platform
// packages regardless of hoisting). Returns null when the platform package is not
// installed — an unsupported platform, or optionalDependencies pruned.
function resolveAddonPath(requireFromPackage) {
  const pkg = PLATFORMS[platformKey()];
  if (!pkg) return null;
  try {
    return requireFromPackage.resolve(`${pkg}/nub-native.node`);
  } catch {
    return null;
  }
}

// Make the addon reachable for every transform-core instance in this process tree
// by setting the internal `__NUB_ADDON_PATH` plumbing var (probed LAST by
// transform-core, after its relative candidates, so a nested nub CLI always wins
// with its own bundled addon). Worker threads inherit process.env, which is what
// carries the path into the compat tier's loader worker. A sibling
// `addons/nub-native.node` (the dev tree, or a bundled layout) means the relative
// probe wins anyway and no env is needed. Idempotent; safe to call from both the
// ESM side-effect module and the CJS `--require` fallback.
function ensureAddonEnv(requireFromPackage) {
  if (process.env.__NUB_ADDON_PATH) return true;
  try {
    const { statSync } = require("node:fs");
    const sibling = require("node:path").join(__dirname, "addons", "nub-native.node");
    const s = statSync(sibling, { throwIfNoEntry: false });
    if (s !== undefined && s.isFile()) return true;
  } catch {
    // fall through to the platform-package probe
  }
  const resolved = resolveAddonPath(requireFromPackage);
  if (resolved) {
    process.env.__NUB_ADDON_PATH = resolved;
    return true;
  }
  process.stderr.write(
    `The Nub loader could not find its native addon for ${process.platform}-${process.arch}` +
      ` — the platform package may be missing (optionalDependencies pruned, or an` +
      ` unsupported platform). TypeScript transpilation is inactive.\n`,
  );
  return false;
}

module.exports = { PLATFORMS, platformKey, resolveAddonPath, ensureAddonEnv };
