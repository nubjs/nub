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
  // ⛔⛔ NO CHILD PROCESS AT ALL, AND THAT IS THE POINT. This first shelled out to `npm view`, which broke
  // on Windows twice for two different reasons: Node refuses to spawn a `.cmd` through execFile without a
  // shell, and with a shell it still could not resolve `npm.cmd` from a Git-Bash PATH (MSYS converts PATH
  // for a native child, and the result did not reach cmd.exe intact). Each failure was swallowed into an
  // empty answer, which the sweep filed as "the band admits no version" — hiding the three most
  // download-prone win rows behind an innocent-looking skip.
  //
  // The registry's own HTTP API needs no npm, no shell and no PATH, so it behaves identically on all three
  // platforms. That removes the whole class of failure rather than patching this instance of it.
  const url = `https://registry.npmjs.org/${name.startsWith("@") ? name.replace("/", "%2F") : name}`;
  let versions = [];
  let lookupError = null;
  try {
    const res = await fetch(url, { headers: { accept: "application/json" }, signal: AbortSignal.timeout(90_000) });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    versions = Object.keys((await res.json()).versions ?? {});
    if (versions.length === 0) throw new Error("no versions in packument");
  } catch (e) {
    lookupError = e;
  }
  if (lookupError) {
    // Distinguishable on purpose: the caller must not file this as "the band admits no version".
    console.error(`pickversion: registry lookup FAILED for ${name} (${lookupError.message})`);
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
