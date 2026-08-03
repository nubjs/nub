import { pathToFileURL } from 'node:url';

// The canonical grant state space for the build-jail catalog search.
//
// THE SEARCH METHODOLOGY, and why it is an ordered walk rather than a descent:
// enumerate every representable state in ASCENDING COST order and stop at the
// first pass. That state is the true minimum BY CONSTRUCTION — every cheaper
// state was already tested and failed. A greedy descent from the top finds only
// *a* minimal set, and which one depends on the order capabilities are dropped;
// that is a heuristic, not a methodology. Worst case here is all 54 states.
//
// Correctness does NOT rest on monotonicity. Only the step-2 shortcut below
// (top fails => nothing passes) assumes it, and that shortcut is an optimisation
// we can drop without affecting the answer.

/** Cost per capability atom.
 *
 *  HARD CONSTRAINT: a strictly wider state must cost strictly more, or the
 *  ascending walk can reach a superset before the subset it contains and return
 *  a non-minimal answer. `assertCostConsistency()` proves this over all 54 pairs
 *  rather than leaving it to review. That is why `read.disk` is 4 (it dominates
 *  both narrow reads at 2) and `write.disk` is 20 (it dominates everything else
 *  combined, at 19).
 *
 *  Within that constraint the numbers are a JUDGEMENT about what we least want
 *  to hand out, and they decide which minimum we land on when two states are
 *  incomparable. `network` is the contentious one: priced below `write.project`
 *  it says we would rather a package phone home than write the user's source.
 *  For a supply-chain threat model that is arguably backwards — egress is the
 *  exfiltration path and how a small dropper fetches a large payload. Raising it
 *  above `write.project` flips which minimum the ~200 egress-needing packages
 *  land on. Tune here; nothing else needs to change. */
export const COST = {
  'read.project': 1,
  'read.userHome': 1,
  'read.disk': 4,
  'write.deps': 3,
  'write.project': 5,
  'write.userHome': 7,
  'write.disk': 20,
  network: 3,
};

const READS = [[], ['project'], ['userHome'], ['project', 'userHome'], 'disk'];
const WRITES = [
  [], ['deps'], ['project'], ['userHome'],
  ['deps', 'project'], ['deps', 'userHome'], ['project', 'userHome'],
  ['deps', 'project', 'userHome'], 'disk',
];

/** The atoms a state grants, as a flat set — the representation containment is
 *  computed over. `write.disk` implies `read.disk`, and `write.<scope>` implies
 *  `read.<scope>`; those implications are materialised here so that `contains()`
 *  is plain set inclusion and never has to know the semantics. */
function atomsOf(read, write, network) {
  const a = new Set();
  if (write === 'disk') {
    a.add('write.disk');
    a.add('read.disk');
  } else {
    for (const s of write) {
      a.add(`write.${s}`);
      a.add(`read.${s}`);
    }
  }
  if (read === 'disk') a.add('read.disk');
  else for (const s of read) a.add(`read.${s}`);
  if (network) a.add('network');
  return a;
}

/** Atoms the AUTHOR wrote, i.e. cost-bearing ones — implied reads are free
 *  because the write already paid for them. */
function costAtomsOf(read, write, network) {
  const a = [];
  if (write === 'disk') a.push('write.disk');
  else for (const s of write) a.push(`write.${s}`);
  if (read === 'disk') a.push('read.disk');
  else for (const s of read) a.push(`read.${s}`);
  if (network) a.push('network');
  return a;
}

/** A state is REPRESENTABLE unless its `read` names a scope the `write` already
 *  covers. Those pairs are not merely redundant — they are two spellings of one
 *  state, and admitting both would let the walk test the same grant twice. */
function representable(read, write) {
  if (write === 'disk') return Array.isArray(read) && read.length === 0;
  if (read === 'disk') return true;
  return !read.some((s) => write.includes(s));
}

function build() {
  const out = [];
  for (const read of READS) {
    for (const write of WRITES) {
      if (!representable(read, write)) continue;
      for (const network of [false, true]) {
        const costAtoms = costAtomsOf(read, write, network);
        out.push({
          read,
          write,
          network,
          atoms: atomsOf(read, write, network),
          label: costAtoms.join(' + ') || '(nothing)',
          cost: costAtoms.reduce((n, k) => n + COST[k], 0),
        });
      }
    }
  }
  // Ties broken deterministically so a re-run walks the identical order and two
  // platforms' results stay comparable index-for-index.
  out.sort((a, b) => a.cost - b.cost || a.label.localeCompare(b.label));
  return out;
}

/** All representable states, cheapest first. Index 0 is "no grant"; the last is
 *  the top. */
export const STATES = build();

export const BOTTOM = 0;
export const TOP = STATES.length - 1;

/** The v2 catalog grant a state corresponds to, or `null` for the empty state — the catalog spells
 *  "needs nothing" as NO ENTRY.
 *
 *  ⛔ ONE DEFINITION, because there are now two callers and they must never disagree: `search.mjs`
 *  writes it into a fresh record, and `collate.mjs` BACKFILLS it for the ~2,500 records written
 *  while `grantFor` was returning `undefined`. A second spelling of this mapping would let a
 *  re-collated record and a freshly measured one describe the same state differently, which is
 *  exactly the kind of divergence the catalog cannot detect.
 *
 *  Lives here rather than in `search.mjs` because it is a property of the STATE SPACE — a pure
 *  function of `atoms` — and this module owns that space. */
export function grantForState(state) {
  if (!state || state.atoms.size === 0) return null;
  const grant = {};

  if (state.atoms.has('write.disk')) {
    grant.write = 'disk';
  } else {
    const w = {};
    for (const s of ['deps', 'project', 'userHome']) if (state.atoms.has(`write.${s}`)) w[s] = true;
    if (Object.keys(w).length) grant.write = w;
  }

  // `read` carries only what `write` does not already imply at the same scope — the parser rejects
  // a redundant read alongside `write: "disk"`.
  if (grant.write !== 'disk') {
    if (state.atoms.has('read.disk')) {
      grant.read = 'disk';
    } else {
      const r = {};
      for (const s of ['project', 'userHome']) {
        if (state.atoms.has(`read.${s}`) && !(grant.write && grant.write[s])) r[s] = true;
      }
      if (Object.keys(r).length) grant.read = r;
    }
  }

  if (state.atoms.has('network')) grant.network = true;
  return grant;
}

const contains = (a, b) => [...b.atoms].every((x) => a.atoms.has(x));

/** Prove the ordering is sound: if state j is strictly wider than state i, then
 *  j must sort AFTER i. A violation would let the walk return a non-minimal
 *  grant, silently — so this runs as a test, not as a comment. */
export function assertCostConsistency() {
  const bad = [];
  for (let i = 0; i < STATES.length; i++) {
    for (let j = 0; j < STATES.length; j++) {
      if (i === j) continue;
      const wider = contains(STATES[j], STATES[i]) && !contains(STATES[i], STATES[j]);
      if (wider && j < i) bad.push(`"${STATES[j].label}" is wider than "${STATES[i].label}" but sorts earlier`);
    }
  }
  if (bad.length) throw new Error(`cost order is inconsistent with containment:\n  ${bad.join('\n  ')}`);
  return STATES.length;
}

// ⛔ `file://${process.argv[1]}` IS NOT A FILE URL ON WINDOWS, so this guard is silently FALSE
// there and the cost-consistency self-test below never runs — on the one platform whose path
// handling has broken this harness most. MEASURED: for argv[1] `D:\a\nub\states.mjs`,
// `import.meta.url` is `file:///D:/a/nub/states.mjs` while the template yields
// `file://D:\a\nub\states.mjs`; they never compare equal, and nothing reports it.
//
// Same family as the `new URL(…).pathname` drive-letter doubling fixed in 63082210: a POSIX path
// assumption that degrades to a no-op rather than an error. `pathToFileURL` is the only correct
// spelling and is byte-identical on POSIX.
// `argv[1]` is undefined under `node -e` / `node --eval`, where `pathToFileURL` THROWS rather than
// returning a non-match — so the presence check is load-bearing, not defensive noise.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  console.log('states:', assertCostConsistency(), '(cost order verified against containment)');
  STATES.forEach((s, i) => console.log(String(i).padStart(3), 'cost', String(s.cost).padStart(3), '', s.label));
}
