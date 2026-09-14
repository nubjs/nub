// Shared readers for the nextest JUnit report and the allowlist.

const ENTITIES = { lt: "<", gt: ">", amp: "&", quot: '"', apos: "'" };
const decode = (s) =>
  s.replace(/&(#x[0-9a-f]+|#\d+|\w+);/gi, (m, e) =>
    e[0] === "#"
      ? String.fromCodePoint(e[1].toLowerCase() === "x" ? parseInt(e.slice(2), 16) : Number(e.slice(1)))
      : (ENTITIES[e] ?? m),
  );

// nextest writes one <testcase> per test; a failure carries a <failure> or
// <error> child plus <system-out>/<system-err> with the captured output.
export function parseJunit(xml) {
  const cases = [];
  const re = /<testcase\b([^>]*?)(?:\/>|>([\s\S]*?)<\/testcase>)/g;
  for (const m of xml.matchAll(re)) {
    const attrs = Object.fromEntries([...m[1].matchAll(/(\w+)="([^"]*)"/g)].map((a) => [a[1], decode(a[2])]));
    const body = m[2] ?? "";
    const failed = /<(failure|error)\b/.test(body);
    const text = [...body.matchAll(/<(failure|error|system-out|system-err)\b([^>]*?)(?:\/>|>([\s\S]*?)<\/\1>)/g)]
      .map((p) => {
        const msg = /message="([^"]*)"/.exec(p[2] ?? "");
        return [msg ? decode(msg[1]) : "", decode((p[3] ?? "").replace(/^<!\[CDATA\[|\]\]>$/g, ""))].join("\n");
      })
      .join("\n");
    cases.push({ name: attrs.name, failed, skipped: /<skipped\b/.test(body), message: text.trim() });
  }
  return cases;
}

export function readAllowlist(text) {
  const entries = new Map();
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const hash = line.indexOf("  #");
    const name = (hash === -1 ? line : line.slice(0, hash)).trim();
    const note = hash === -1 ? "" : line.slice(hash + 3).trim();
    const colon = note.indexOf(":");
    entries.set(name, {
      category: colon === -1 ? note : note.slice(0, colon).trim(),
      reason: colon === -1 ? "" : note.slice(colon + 1).trim(),
    });
  }
  return entries;
}
