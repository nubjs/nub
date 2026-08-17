// The two JSON transforms `cold-network-sweep.sh` needs, in Node rather than python.
//
// ⛔⛔ WHY NODE. The sweep's whole point is to judge a per-OS overlay on the platform it applies to, and
// the Windows box has NO python3 and no `py` — so a python-based harness can never judge the
// 9 win overlays, which is the exact "blocked on a Windows runner" excuse this effort has already paid for
// once. Node is present on every platform nub targets, because nub runs ON the user's Node. So the
// dependency goes away instead of being installed per box.
//
// Three subcommands:
//   worklist <catalog> <osKey> [onlyPkg]   -> TSV of `name\tband` for every overlay that withdraws a
//                                             network the outer band GRANTED
//   pickversion <name> <band>              -> a concrete version the band admits, or empty
//   tweak <catalog> <out> <pkg> <osKey> <restore0|1>
//                                          -> writes a candidate catalog, optionally with that package's
//                                             network withdrawal dropped
import { readFileSync, writeFileSync } from "node:fs";

// A band is either the `default` body or one entry of `versions`.
function bands(entry) {
  const out = [];
  if (entry.default && typeof entry.default === "object") out.push(["default", entry.default]);
  for (const [k, v] of Object.entries(entry.versions ?? {})) out.push([k, v]);
  return out;
}

// The shape under test: the overlay names `network` and sets it to null (which REMOVES), while the outer
// band granted it. `"network" in ov` matters — an overlay that simply omits the key inherits, and is not a
// withdrawal. Conflating the two would put ~every overlay on the worklist.
const withdrawsNetwork = (body, osKey) => {
  const ov = body?.[osKey];
  return ov && typeof ov === "object" && "network" in ov && ov.network === null && body.network === true;
};

const [cmd, ...rest] = process.argv.slice(2);

if (cmd === "worklist") {
  const [file, osKey, only] = rest;
  const cat = JSON.parse(readFileSync(file, "utf8"));
  const rows = new Set();
  for (const [name, entry] of Object.entries(cat.packages)) {
    if (only && name !== only) continue;
    for (const [band, body] of bands(entry)) if (withdrawsNetwork(body, osKey)) rows.add(`${name}\t${band}`);
  }
  process.stdout.write([...rows].sort().join("\n") + (rows.size ? "\n" : ""));
} else if (cmd === "tweak") {
  const [file, out, pkg, osKey, restore] = rest;
  const cat = JSON.parse(readFileSync(file, "utf8"));
  if (restore === "1") {
    for (const [, body] of bands(cat.packages[pkg])) {
      const ov = body?.[osKey];
      if (ov && typeof ov === "object" && ov.network === null) {
        delete ov.network;
        if (Object.keys(ov).length === 0) delete body[osKey];
      }
    }
  }
  // ⛔ THE STAMP LIVES UNDER `provenance`, NOT AT THE TOP LEVEL. Putting it top-level makes the reader
  // refuse the file ("no provenance.generatedAt, so it cannot be shown to be newer") and every row then
  // silently measures the COMPILED catalog while reporting as though the override were in force. That
  // happened, and only grepping for the reader's exact banner caught it.
  (cat.provenance ??= {}).generatedAt = "2099-01-01T00:00:00Z";
  writeFileSync(out, JSON.stringify(cat));
} else if (cmd === "pickversion") {
  // A version the band actually admits. `default` covers every version, so the registry's latest is fine;
  // a `<X` band needs the newest release strictly BELOW X, or the entry under test is not the one that
  // resolves and the row measures a different grant than it reports.
  const [name, band] = rest;
  if (band === "default") { console.log("latest"); process.exit(0); }
  if (!band.startsWith("<")) { console.log(""); process.exit(0); }
  const { execFileSync } = await import("node:child_process");
  // ⛔⛔ WINDOWS NEEDS `shell: true`, AND WITHOUT IT EVERY `<X` BAND SILENTLY SKIPPED. Node refuses to
  // spawn a `.cmd`/`.bat` through execFile without a shell (the ERR_CHILD_PROCESS / EINVAL hardening), so
  // `npm.cmd view` threw on every call, the catch below turned it into an empty answer, and the sweep
  // recorded `SKIPPED-no-version-in-band` — a verdict that reads like the registry had nothing to offer.
  // Measured: 3 of the 9 win overlays skipped that way, and they were `flow-bin`, `geckodriver` and
  // `ttf2woff2` — the three most download-prone packages in the set, i.e. exactly the rows most likely to
  // be artefacts. A swallowed lookup failure is indistinguishable from a real absence unless it says so.
  const win = process.platform === "win32";
  let versions = [];
  let lookupError = null;
  try {
    versions = JSON.parse(execFileSync(win ? "npm.cmd" : "npm", ["view", name, "versions", "--json"], {
      encoding: "utf8", timeout: 90_000, stdio: ["ignore", "pipe", "ignore"], shell: win,
    }));
    if (!Array.isArray(versions)) versions = [versions];
  } catch (e) {
    lookupError = e;
  }
  if (lookupError) {
    // Distinguishable on purpose: the caller must not file this as "the band admits no version".
    console.error(`pickversion: registry lookup FAILED for ${name} (${lookupError.code ?? lookupError.message})`);
    console.log("LOOKUP-FAILED");
    process.exit(0);
  }
  const key = (v) => v.split(".").slice(0, 3).map((x) => parseInt(x, 10) || 0);
  const below = (a, b) => { const [x, y] = [key(a), key(b)]; for (let i = 0; i < 3; i++) { if (x[i] !== y[i]) return x[i] < y[i]; } return false; };
  const cand = versions.filter((v) => !v.includes("-") && below(v, band.slice(1)));
  console.log(cand.length ? cand[cand.length - 1] : "");
} else {
  console.error("usage: catalog-tweak.mjs worklist <catalog> <os> [only] | tweak <catalog> <out> <pkg> <os> <0|1>");
  process.exit(2);
}
