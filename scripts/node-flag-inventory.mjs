#!/usr/bin/env node
// Dump the running Node's --experimental-* CLI-flag inventory as deterministic JSON.
// Each flag maps to [OptionType, envVarSettings], read from Node's own option table via
// internalBinding('options') — the same introspection Node's parser uses, so it is the
// ground truth for "does this binary accept this flag, and how".
//
// MUST be invoked with --expose-internals (internal/test/binding is otherwise unreachable):
//   node --no-warnings --expose-internals scripts/node-flag-inventory.mjs
//
// This is the capture half of the flag-drift guard (scripts/check-node-flag-drift.mjs
// diffs a fresh capture against the committed snapshot). It is BUILD/CI tooling only —
// nub's runtime never probes a Node; the feature matrix is hand-vetted (see
// crates/nub-core/src/node/feature_matrix.rs).
import { createRequire } from "node:module";

// OptionType (src/node_options.h). Only kNoOp/kBoolean flags are bare (value-less);
// the rest take a value. Recorded so a human vetting a drift sees the shape at a glance.
const OPTION_TYPE = {
  0: "kNoOp",
  1: "kV8Option",
  2: "kBoolean",
  3: "kInteger",
  4: "kUInteger",
  5: "kString",
  6: "kHostPort",
  7: "kStringList",
};
// envVarSettings: whether the flag is legal inside NODE_OPTIONS.
const ENV_VAR_SETTINGS = { 0: "kAllowedInEnvvar", 1: "kDisallowedInEnvvar" };

function readInventory() {
  const require = createRequire(import.meta.url);
  const { internalBinding } = require("internal/test/binding");
  const options = internalBinding("options");
  // Accessor name drifted across releases: getCLIOptionsInfo() is the newer shape,
  // getCLIOptions() the older one. Both return { options: Map, aliases: Map }.
  const info = options.getCLIOptionsInfo
    ? options.getCLIOptionsInfo()
    : options.getCLIOptions();
  const flags = {};
  for (const [name, meta] of info.options) {
    if (
      typeof name === "string" &&
      name.startsWith("--experimental-") &&
      typeof meta.type === "number" &&
      typeof meta.envVarSettings === "number"
    ) {
      flags[name] = [meta.type, meta.envVarSettings];
    }
  }
  return flags;
}

let flags;
try {
  flags = readInventory();
} catch (err) {
  process.stderr.write(
    `node-flag-inventory: introspection unavailable on ${process.version} ` +
      `(need --expose-internals; internalBinding('options') threw): ${err?.message ?? err}\n`,
  );
  process.exit(2);
}

// Sort keys so the serialized inventory is byte-stable across runs and platforms —
// the whole diff depends on this being deterministic.
const sorted = Object.fromEntries(Object.keys(flags).sort().map((k) => [k, flags[k]]));
const out = {
  nodeVersion: process.version,
  optionTypeLegend: OPTION_TYPE,
  envVarSettingsLegend: ENV_VAR_SETTINGS,
  flags: sorted,
};
process.stdout.write(JSON.stringify(out, null, 2) + "\n");
