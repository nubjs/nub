// Does the collator attach each measurement to the grant its own version RESOLVES to?
//
// ⛔ THIS IS THE ONE PROPERTY A READER OF THE CATALOG CANNOT CHECK BY EYE. A band key is `<X`, and
// `semver` refuses a prerelease against such a bound (`version_scope::applies`), so a measured
// prerelease falls through every band to `default`. The generator did not know that: it folded a
// prerelease's grant into a band the band itself excludes, which granted a capability to release
// versions nobody measured AND withheld it from the one version that proved the need. The band's
// own note said `measured <that prerelease>`, so the fabrication read as evidence.
//
// The two failing assertions below are that defect. The two controls beside them exist because the
// cheapest wrong fix is to stop emitting bands, which would pass an evidence check by emitting
// nothing at all.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const HERE = path.dirname(fileURLToPath(import.meta.url));

/** One `MINIMUM` run record, trimmed to the fields the collator reads. */
function record(dir, pkg, version, grant, latest) {
  const d = path.join(dir, 'darwin-arm64', pkg, version);
  fs.mkdirSync(d, { recursive: true });
  fs.writeFileSync(path.join(d, 'results.json'), JSON.stringify({
    pkg, version, verdict: 'MINIMUM', grant,
    standing: { latestVersion: latest },
    provenance: { platform: 'darwin-arm64', harnessSha256: 'test' },
  }));
}

/** The prerelease rule, restated independently of the generator's own helper so the test cannot
 *  agree with a broken implementation by sharing it. A `<X` bound with X a plain release admits no
 *  prerelease at all, which is the only case these fixtures exercise. */
const isPrerelease = (v) => /^\d+\.\d+\.\d+-/.test(v);

function collate() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'collate-'));
  const runs = path.join(dir, 'runs');

  // THE DEFECT. A prerelease needs a whole-home write; every release measured needs less. Modelled
  // on `@tensorflow/tfjs-backend-wasm`, where the only two versions in the package declaring an
  // install hook are both prereleases, and the shipped catalog put their grant on a band covering
  // 65 hook-free releases while `default` — where those two actually resolve — granted nothing.
  record(runs, 'prerelease-pkg', '1.4.0-alpha2', { write: { userHome: true, deps: true, project: true } }, '4.22.0');
  record(runs, 'prerelease-pkg', '2.0.0', null, '4.22.0');
  record(runs, 'prerelease-pkg', '4.22.0', { network: true }, '4.22.0');

  // CONTROL: an ordinary release-measured band. Must still be emitted, or the fix is "stop banding".
  record(runs, 'release-band-pkg', '1.0.0', { write: 'disk' }, '3.0.0');
  record(runs, 'release-band-pkg', '3.0.0', null, '3.0.0');

  // CONTROL: latest is itself a prerelease, so the band's BOUND is one. `<1.1.15-patch.2` does
  // admit the release below it, so this band is evidence-backed and must survive. Modelled on
  // `@paloaltonetworks/postman-code-generators`, the shipped entry with this shape.
  record(runs, 'prerelease-latest-pkg', '1.1.9', { write: 'disk' }, '1.1.15-patch.2');
  record(runs, 'prerelease-latest-pkg', '1.1.15-patch.2', { network: true }, '1.1.15-patch.2');

  const out = path.join(dir, 'catalog.json');
  const r = spawnSync(process.execPath, [path.join(HERE, 'collate.mjs'), '--runs', runs, '--out', out],
    { encoding: 'utf8' });
  assert.equal(r.status, 0, `collate.mjs exited ${r.status}\n${r.stdout}\n${r.stderr}`);
  return JSON.parse(fs.readFileSync(out, 'utf8')).packages;
}

test('a measurement reaches the grant its own version resolves to', () => {
  const pkgs = collate();
  const e = pkgs['prerelease-pkg'];

  // `1.4.0-alpha2` matches no `<X` band, so it resolves to `default` — which must therefore carry
  // the write it measured. Before the fix `default` was `{network:true}` alone and the whole-home
  // write sat on a band that excluded the only version proving it was needed.
  assert.deepEqual(e.default.write, { userHome: true, deps: true, project: true },
    `default must carry the prerelease's measured write, since that is where the prerelease `
    + `resolves; got ${JSON.stringify(e.default)}`);

  // …and no band may be justified by evidence it excludes. Before the fix `<4.22.0` was emitted
  // with the note `measured 1.4.0-alpha2, 2.0.0`, covering 65 real releases on the strength of a
  // run none of them share.
  for (const [range, caps] of Object.entries(e.versions ?? {})) {
    const cited = /^measured ([^;]+);/.exec(caps.notes)?.[1].split(',').map((s) => s.trim()) ?? [];
    assert.deepEqual(cited.filter(isPrerelease), [],
      `band ${range} of prerelease-pkg cites a prerelease its own bound excludes: ${caps.notes}`);
  }
});

test('a band backed by a release measurement is still emitted', () => {
  const e = collate()['release-band-pkg'];
  assert.deepEqual(Object.keys(e.versions ?? {}), ['<3.0.0'],
    `the release-measured band must survive; got ${JSON.stringify(e)}`);
  assert.equal(e.versions['<3.0.0'].write, 'disk');
});

test('a prerelease BOUND survives when it admits the version that justifies it', () => {
  const e = collate()['prerelease-latest-pkg'];
  // `1.1.9` is a release below `1.1.15-patch.2`, so the bound admits it and the band is real.
  assert.deepEqual(Object.keys(e.versions ?? {}), ['<1.1.15-patch.2'],
    `a prerelease bound is not itself the defect — only unreachable evidence is; got ${JSON.stringify(e)}`);
  assert.equal(e.versions['<1.1.15-patch.2'].write, 'disk');
});
