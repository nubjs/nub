#!/usr/bin/env node
// pending.mjs — the list of packages whose builds the install left ignored.
//
// This is the input to the per-package lifecycle windows. `nub approve-builds`
// rejects a `name@version` argument ("not in the ignored-builds set") and accepts
// only the BARE name, so the version is stripped here rather than at the call site.
// A bare name that has two pending versions approves BOTH in one window — that is
// aube's behaviour, not a harness choice, and `windows.json` records the resulting
// job count so the scheduling gate can still rule on it.
//
// Parsed from `WARN_NUB_IGNORED_BUILD_SCRIPTS`, which nub emits with a machine-
// readable `packages=[…]` tail. The warning is printed more than once per install,
// so the names are unioned; order is the warning's own (alphabetical), which makes
// a run's window sequence deterministic and therefore comparable across arms.
import fs from 'node:fs';

const log = fs.readFileSync(process.argv[2], 'utf8');
const seen = new Set();
const out = [];
for (const m of log.matchAll(/packages=\[([^\]]*)\]/g)) {
  for (const raw of m[1].matchAll(/"([^"]+)"/g)) {
    // Scoped names carry a second `@`, so strip only the LAST one.
    const bare = raw[1].replace(/@[^@]+$/, '');
    if (!bare || seen.has(bare)) continue;
    seen.add(bare);
    out.push(bare);
  }
}
process.stdout.write(out.join('\n') + (out.length ? '\n' : ''));
