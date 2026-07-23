// nub#460 regression guard — nub must select the hook tier that COMPOSES with a foreign
// async ESM loader riding in this process's own startup flags (`--import`/`--loader`).
//
// A synthetic pass-through loader can't reproduce the real crash (nub's stub-recovery and
// builtin short-circuits absorb it; only tsx's query-suffixed internal graph defeats them),
// so this guards the FIX'S MECHANISM directly: on the broken-compose band (Node 22.15–24.11)
// nub must switch OFF its sync `module.registerHooks` fast tier onto the async loader-worker
// tier, or a real foreign loader (tsx) reaches the `resolveSync`/`loadSync` stub and crashes
// with ERR_METHOD_NOT_IMPLEMENTED. Outside the band the sync fast tier composes natively and
// must stay (it is faster). The end-to-end crash path is covered by manual real-tsx runs
// across the Node matrix (see the PR); this is the self-contained CI guard.
//
// Tier is observable: the fast tier wraps `module.registerHooks` (sets `__nubWrapped`); the
// async tier registers via `module.register` and leaves `registerHooks` unwrapped.
import nodeModule from "node:module";
import { writeSync } from "node:fs";

// SELF-GUARD: process.versions.nub is published in BOTH tiers, so a missing marker means
// nub's preload never loaded and the tier is not under test — fail loud, don't pass vacuously.
if (!process.versions.nub) {
  writeSync(2, "TIER_FAIL: nub augmentation not active (process.versions.nub unset) — preload did not load; not under test\n");
  process.exit(3);
}

const [maj, min, pat] = process.versions.node.split(".").map((n) => parseInt(n, 10));
const brokenBand =
  (maj > 22 || (maj === 22 && min >= 15)) &&
  (maj < 24 || (maj === 24 && (min < 11 || (min === 11 && pat === 0))));
// The fast tier stamps `__nubWrapped` on registerHooks (installUserHookDetector, in
// runtime/preload-common.cjs) — the same marker production relies on. Keep this in sync if
// that sentinel is ever renamed, or this guard silently stops discriminating the tiers.
const fastTier =
  typeof nodeModule.registerHooks === "function" && nodeModule.registerHooks.__nubWrapped === true;

if (brokenBand && fastTier) {
  writeSync(
    2,
    `TIER_FAIL: Node ${process.versions.node} is on the broken-compose band and a foreign async ` +
      `loader flag is present, but nub stayed on the SYNC fast tier — a real tsx loader would crash ` +
      `here on the resolveSync stub (nub#460 regressed)\n`,
  );
  process.exit(1);
}

writeSync(1, `TIER_OK band=${brokenBand ? "broken" : "ok"} tier=${fastTier ? "fast" : "async"}\n`);
