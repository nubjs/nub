#!/usr/bin/env node
// Fail the build when nub's oxc consumers drift apart.
//
// oxc is pre-1.0, so `^` never unifies two different minors: any consumer left
// behind compiles a SECOND full oxc stack into the same binary (long builds, two
// parsers with different semantics on the same source). The consumers are spread
// across three Cargo workspaces plus npm, none of which cargo can reconcile on
// its own:
//
//   Cargo.toml                          [workspace.dependencies] — the canonical pin
//   crates/nub-native/Cargo.toml        the transpiler addon (its OWN workspace)
//   crates/nub-phantom-core/Cargo.toml  the lean parser subset the CLI links
//   package.json                        @oxc-project/runtime — the emit helpers,
//                                       the JS half of the same oxc release
//
// Two independent checks:
//   1. PINS — every manifest above names the same oxc version, and the npm
//      helper-runtime pin is that exact version (no range).
//   2. LOCKS — no Cargo.lock resolves two versions of any oxc crate. This is the
//      one that actually proves a single stack; the pin check alone can't see a
//      duplicate introduced by a transitive dependency (e.g. rolldown).
//
// Usage: node scripts/check-oxc-lockstep.mjs   (exit 1 on any drift)
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (rel) => readFileSync(join(repoRoot, rel), "utf8");

// Crates whose version number IS the oxc release number. The satellites
// (oxc_sourcemap, oxc_resolver, oxc-miette, oxc-browserslist) version
// independently and are deliberately excluded from the pin comparison — the lock
// check below still holds them to one version each.
const RELEASE_VERSIONED = /^oxc(_(?!sourcemap|resolver|index)[a-z_]+)?$/;

const errors = [];

/** Every `<name> = "=X.Y.Z"` / `<name> = { version = "=X.Y.Z" ... }` pin in a manifest. */
function pins(manifest, file) {
  const found = new Map();
  for (const line of manifest.split("\n")) {
    const m = line.match(/^([a-z0-9_-]+)\s*=\s*(?:"=([\d.]+)"|\{[^}]*?version\s*=\s*"=([\d.]+)")/);
    if (!m) continue;
    const [, name, bare, table] = m;
    if (!name.startsWith("oxc")) continue;
    found.set(name, { version: bare ?? table, file });
  }
  return found;
}

const rootPins = pins(read("Cargo.toml"), "Cargo.toml");
const canonical = rootPins.get("oxc")?.version;
if (!canonical) {
  errors.push('Cargo.toml: no `oxc = { version = "=X.Y.Z" }` pin found — it is the canonical oxc version');
}

for (const file of [
  "Cargo.toml",
  "crates/nub-native/Cargo.toml",
  "crates/nub-phantom-core/Cargo.toml",
]) {
  for (const [name, { version }] of pins(read(file), file)) {
    if (!RELEASE_VERSIONED.test(name)) continue;
    if (canonical && version !== canonical) {
      errors.push(`${file}: ${name} pinned at ${version}, expected ${canonical} (the Cargo.toml oxc pin)`);
    }
  }
}

// oxc_sourcemap ships on its own version line, but the addon's pin and the
// nub-native workspace pin must still agree with each other.
const smRoot = rootPins.get("oxc_sourcemap")?.version;
const smNative = pins(read("crates/nub-native/Cargo.toml"), "x").get("oxc_sourcemap")?.version;
if (smRoot && smNative && smRoot !== smNative) {
  errors.push(`crates/nub-native/Cargo.toml: oxc_sourcemap pinned at ${smNative}, expected ${smRoot} (the Cargo.toml pin)`);
}

// The emit helpers (@oxc-project/runtime) are the JS half of the same oxc
// release as the transformer compiled into nub-native — a floating range here
// would let the helpers drift from the emit that imports them, and the pin
// doubles as the A12 transpile-cache-key proxy.
const rt = (JSON.parse(read("package.json")).dependencies ?? {})["@oxc-project/runtime"];
if (!rt) {
  errors.push("package.json: @oxc-project/runtime missing from dependencies");
} else if (!/^\d+\.\d+\.\d+$/.test(rt)) {
  errors.push(`package.json: @oxc-project/runtime must be an EXACT version, got "${rt}"`);
} else if (canonical && rt !== canonical) {
  errors.push(`package.json: @oxc-project/runtime is ${rt}, expected ${canonical} (the Cargo.toml oxc pin)`);
}

// The repo carries an npm and a bun lockfile (cross-PM dogfooding), and only
// whichever one the installer happens to use gets regenerated on a bump — so
// each is checked for a stale helper-runtime pin.
if (canonical) {
  const staleIn = (lock, found) => {
    const stale = [...new Set(found)].filter((v) => v !== canonical);
    if (!found.length) errors.push(`${lock}: no @oxc-project/runtime entry found`);
    else if (stale.length) {
      errors.push(`${lock}: @oxc-project/runtime at ${stale.join(", ")}, expected ${canonical} — re-run the installer`);
    }
  };
  // npm's lock spells the version in a structured field and again in the tarball
  // URL; a hand-edit that misses one is exactly the drift being guarded.
  const npmLock = JSON.parse(read("package-lock.json"));
  staleIn(
    "package-lock.json",
    Object.entries(npmLock.packages ?? {})
      .filter(([p]) => p.endsWith("node_modules/@oxc-project/runtime"))
      .flatMap(([, e]) => [e.version, e.resolved?.match(/runtime-([\d.]+)\.tgz/)?.[1]])
      .filter(Boolean),
  );
  // bun keys the package as `@oxc-project/runtime@X.Y.Z`.
  for (const lock of ["bun.lock"]) {
    staleIn(lock, [...read(lock).matchAll(/@oxc-project\/runtime@(\d+\.\d+\.\d+)/g)].map((m) => m[1]));
  }
}

// One resolved version per oxc crate, per lockfile. A second version means a
// duplicate stack got in — the failure this whole guard exists to prevent.
for (const lock of [
  "Cargo.lock",
  "crates/nub-native/Cargo.lock",
  "crates/nub-phantom/Cargo.lock",
]) {
  const versions = new Map();
  const text = read(lock);
  const re = /^name = "(oxc[\w-]*)"\nversion = "([\d.]+)"$/gm;
  for (const [, name, version] of text.matchAll(re)) {
    if (!versions.has(name)) versions.set(name, new Set());
    versions.get(name).add(version);
  }
  for (const [name, set] of versions) {
    if (set.size > 1) {
      errors.push(`${lock}: ${name} resolves to ${[...set].sort().join(" AND ")} — duplicate oxc stack`);
    }
  }
}

if (errors.length) {
  console.error("oxc lockstep check FAILED:\n  " + errors.join("\n  "));
  console.error(
    "\nEvery oxc consumer moves together. Bump the pins in Cargo.toml,\n" +
      "crates/nub-native/Cargo.toml, crates/nub-phantom-core/Cargo.toml and the\n" +
      "@oxc-project/runtime dependency in package.json to one version, then\n" +
      "refresh each Cargo.lock (`cargo update -p oxc ...`) so no lock carries two.",
  );
  process.exit(1);
}

console.log(`✓ oxc lockstep: every consumer on ${canonical}; no lockfile carries a duplicate oxc stack`);
