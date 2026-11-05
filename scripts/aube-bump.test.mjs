// Guards the aube-bump mechanics script: the UPSTREAM marker parses, the
// venue path is derived (never a hardcoded user path), and the gate never
// pins a toolchain. Run: node --test scripts/aube-bump.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const SCRIPT = new URL("./aube-bump.ts", import.meta.url).pathname;
const src = readFileSync(SCRIPT, "utf8");

test("UPSTREAM marker exposes commit and tag", () => {
  const marker = readFileSync(
    new URL("../vendor/aube/UPSTREAM", import.meta.url),
    "utf8",
  );
  assert.match(marker, /^commit = \S+/m);
  assert.match(marker, /^tag\s*=/m);
});

test("no hardcoded user paths anywhere in the script", () => {
  assert.ok(!src.includes("/Users/"), "venue/tool paths must derive from env");
  assert.ok(!src.includes("/private/tmp/"), "venue dir must come from TMPDIR");
});

test("gate never pins a toolchain path", () => {
  assert.ok(
    !src.includes("rustup/toolchains"),
    "the ambient PATH decides the toolchain; the script must not",
  );
});

test("venue and gate are the only stateful steps", () => {
  for (const step of ["plan()", "venue(", "gate()", "land()"]) {
    assert.ok(src.includes(step), `missing step: ${step}`);
  }
});

test("venue stages only rerere-resolved paths", () => {
  assert.ok(
    src.includes("rerere remaining"),
    "markerless unmerged paths must be confirmed by rerere before staging",
  );
  assert.ok(
    !src.includes("<<<<<<<"),
    "conflict markers are not proof of a rerere replay",
  );
});

test("land writes the marker into both trees before the diff", () => {
  const land = src.slice(src.indexOf("function land()"));
  const marker = land.indexOf("writeFileSync(join(NUB_ROOT, UPSTREAM_FILE)");
  const venue = land.indexOf('writeFileSync(join(VENUE_DIR, "UPSTREAM")');
  const diff = land.indexOf("diff -rq");
  assert.ok(venue > 0, "the venue marker write is missing");
  assert.ok(marker < venue && venue < diff, "both writes must precede the diff");
});
