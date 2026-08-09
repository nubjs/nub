// Exercises the transpile-cache eviction (A16) and nub's own V8 compile-cache
// eviction directly, with a controllable small cap so eviction actually triggers
// (the shipped cap is 512 MiB). Run by `nub` in an integration test; prints
// EVICT-OK on success.
import { sweepCache, sweepCompileCache } from "../../../runtime/cache-evict.mjs";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readdirSync,
  statSync,
  utimesSync,
  existsSync,
} from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

const hex = (i) => i.toString().padStart(64, "0"); // 64-char all-hex key name
const SIZE = 100;

// The sweep bounds REAL disk footprint, not logical length, so a 100-byte entry
// costs a whole allocation block (4 KiB on APFS/ext4). Derive the caps from what
// one entry actually occupies here, so the arithmetic below holds on any
// filesystem — and on Windows, where `blocks` is unreported and the sweep falls
// back to the logical size.
const unitOf = (p) => {
  const s = statSync(p);
  return typeof s.blocks === "number" && s.blocks > 0 ? s.blocks * 512 : s.size;
};

// ── transpile cache: flat dir, 64-hex entry names ───────────────────
const dir = mkdtempSync(join(tmpdir(), "nub-evict-"));
// 10 entries; index 0 is oldest (smallest mtime).
for (let i = 0; i < 10; i++) {
  const p = join(dir, hex(i));
  writeFileSync(p, "x".repeat(SIZE));
  utimesSync(p, 1000 + i, 1000 + i);
}
// Non-entries the sweep must ignore (never counted, never deleted).
writeFileSync(join(dir, ".sweep"), "");
writeFileSync(join(dir, hex(0) + ".999.0.tmp"), "y".repeat(SIZE));

const unit = unitOf(join(dir, hex(0)));
// 10 units total; cap 5 units, low-water 3 → evict oldest until ≤ 3: delete 7,
// keep the newest 3.
const res = sweepCache(dir, unit * 5, unit * 3);

const survivors = readdirSync(dir)
  .filter((n) => /^[0-9a-f]{64}$/.test(n))
  .sort();
const expected = [hex(7), hex(8), hex(9)].sort();

const flatOk =
  res.deleted === 7 &&
  survivors.length === 3 &&
  JSON.stringify(survivors) === JSON.stringify(expected) &&
  existsSync(join(dir, ".sweep")) &&
  existsSync(join(dir, hex(0) + ".999.0.tmp")) &&
  sweepCache(dir, unit * 5, unit * 3).deleted === 0; // under cap now → no-op

// ── V8 compile cache: Node's nested <version>/<entry> layout ────────
// Entry names are V8's (8-hex), and a whole subdirectory accumulates per Node
// build — so the sweep walks one level and prunes a version dir it empties.
const cdir = mkdtempSync(join(tmpdir(), "nub-ccevict-"));
const older = join(cdir, "v20.19.0-arm64-abcd1234-501");
const newer = join(cdir, "v26.5.0-arm64-abcd1234-501");
mkdirSync(older);
mkdirSync(newer);
for (let i = 0; i < 5; i++) {
  const a = join(older, `a${i}0000`);
  writeFileSync(a, "x".repeat(SIZE));
  utimesSync(a, 1000 + i, 1000 + i); // oldest cohort
  const b = join(newer, `b${i}0000`);
  writeFileSync(b, "x".repeat(SIZE));
  utimesSync(b, 2000 + i, 2000 + i); // newest cohort
}

// 10 units across two dirs; cap 5, low-water 3 → delete 7: all 5 of the older
// version (emptying and pruning its directory) plus the 2 oldest of the newer.
const cres = sweepCompileCache(cdir, unit * 5, unit * 3);
const nestedOk =
  cres.deleted === 7 &&
  !existsSync(older) && // emptied → pruned
  existsSync(newer) &&
  readdirSync(newer).length === 3 &&
  sweepCompileCache(cdir, unit * 5, unit * 3).deleted === 0; // under cap → no-op

console.log(
  flatOk && nestedOk
    ? "EVICT-OK"
    : `EVICT-FAIL flat=${flatOk} deleted=${res.deleted} survivors=${JSON.stringify(survivors)} ` +
        `nested=${nestedOk} cdeleted=${cres.deleted} olderPruned=${!existsSync(older)} ` +
        `newerLeft=${existsSync(newer) ? readdirSync(newer).length : "gone"}`,
);
