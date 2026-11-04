// Node's harness announces a skip as a zero-test TAP plan, `1..0 # Skipped:
// <reason>`, and exits 0 — so by exit code a skip IS a pass (Node's own
// convention, kept for every runtime; Node's `tools/test.py` likewise reads the
// marker off stdout). The same line is also what `common.printSkipMessage()`
// prints for a skipped SUBCASE, after which the file keeps running its
// assertions, so the marker alone cannot say whether the file ran. The test's
// own source can: a reason that matches a `printSkipMessage(...)` argument in
// it was a subcase; anything else came through the terminating `common.skip()`
// (directly, or via a helper in `common/` or a sibling the test requires).

const MARKER = /^1\.\.0 # Skipped: (.*)$/m;

/** `null` when the output carries no skip plan, `"file"` for a whole-file
 *  skip, `"partial"` when the file announced a skipped subcase and kept going. */
export function classifySkip(out, source) {
  const m = MARKER.exec(out);
  if (!m) return null;
  const reason = m[1].trim();
  for (const re of printSkipPatterns(source)) if (re.test(reason)) return "partial";
  return "file";
}

// One regex per `printSkipMessage(` call: the string pieces of its argument in
// order (a concatenation reads as one string), with a template's `${…}` holes
// matching anything.
function printSkipPatterns(source) {
  const out = [];
  const call = /printSkipMessage\s*\(([\s\S]*?)\)\s*;/g;
  for (let m; (m = call.exec(source)); ) {
    let pat = "";
    const piece = /'((?:[^'\\]|\\.)*)'|"((?:[^"\\]|\\.)*)"|`((?:[^`\\]|\\.)*)`/g;
    for (let p; (p = piece.exec(m[1])); ) {
      if (p[3] !== undefined) pat += p[3].split(/\$\{[^}]*\}/).map(escapeRe).join(".*");
      else pat += escapeRe(p[1] ?? p[2]);
    }
    if (pat) out.push(new RegExp(`^${pat}$`));
  }
  return out;
}

function escapeRe(s) {
  return s.replace(/\\(['"`])/g, "$1").replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
