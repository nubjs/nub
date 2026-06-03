// Exercises the transpile-cache eviction (A16) directly, with a controllable
// small cap so eviction actually triggers (the shipped cap is 512 MiB). Run by
// `nub` in an integration test; prints EVICT-OK on success.
import { sweepCache } from "../../../runtime/cache-evict.mjs";
import { mkdtempSync, writeFileSync, readdirSync, utimesSync, existsSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

const dir = mkdtempSync(join(tmpdir(), "nub-evict-"));
const hex = (i) => i.toString().padStart(64, "0"); // 64-char all-hex key name
const SIZE = 100;

// 10 entries (1000 bytes total); index 0 is oldest (smallest mtime).
for (let i = 0; i < 10; i++) {
  const p = join(dir, hex(i));
  writeFileSync(p, "x".repeat(SIZE));
  utimesSync(p, 1000 + i, 1000 + i);
}
// Non-entries the sweep must ignore (never counted, never deleted).
writeFileSync(join(dir, ".sweep"), "");
writeFileSync(join(dir, hex(0) + ".999.0.tmp"), "y".repeat(SIZE));

// cap 500, low-water 300 → evict oldest until ≤ 300: delete 7, keep newest 3.
const res = sweepCache(dir, 500, 300);

const survivors = readdirSync(dir)
  .filter((n) => /^[0-9a-f]{64}$/.test(n))
  .sort();
const expected = [hex(7), hex(8), hex(9)].sort();

const ok =
  res.deleted === 7 &&
  survivors.length === 3 &&
  JSON.stringify(survivors) === JSON.stringify(expected) &&
  existsSync(join(dir, ".sweep")) &&
  existsSync(join(dir, hex(0) + ".999.0.tmp")) &&
  sweepCache(dir, 500, 300).deleted === 0; // under cap now → no-op

console.log(
  ok
    ? "EVICT-OK"
    : `EVICT-FAIL deleted=${res.deleted} survivors=${JSON.stringify(survivors)}`,
);
