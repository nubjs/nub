// Operations table for the UNIQUE-RESTRICTING-SID arm. Same shape as ops.js but
// the expectations are INVERTED: this mechanism is supposed to be
// deny-by-default, so `%USERPROFILE%` secrets must FAIL and only ACE'd paths
// may pass. Run the identical file in the baseline arm, where every FAIL row is
// expected to flip to PASS -- a row that fails in both arms is a harness or
// fixture problem, not confinement.
//
//   node ops-usid.js <grantedProjectDir> <ungrantedSiblingDir>
const fs = require('fs');
const path = require('path');
const os = require('os');

const proj = process.argv[2];
const sibling = process.argv[3];
// Optional start delay. The parent-driven two-token startup can only strip the
// child's main-thread impersonation AFTER loader init, so a probe that reads
// immediately would race the revert and measure the pre-revert (unconfined)
// state. Delaying the reads past the revert makes the measurement unambiguous.
const delayMs = Number(process.argv[4] || 0);
const profile = process.env.USERPROFILE || os.homedir();
let pass = 0, fail = 0;

function t(name, fn) {
  let got, detail;
  try { const v = fn(); got = 'PASS'; detail = String(v === undefined ? '' : v).slice(0, 110); }
  catch (e) { got = 'FAIL'; detail = (e.code || '') + ' ' + String(e.message).split('\n')[0].slice(0, 110); }
  if (got === 'PASS') pass++; else fail++;
  console.log(`  ${got.padEnd(4)} ${name.padEnd(46)} ${detail}`);
}

function main() {
console.log('== node ' + process.version + '  delayMs=' + delayMs + '  proj=' + proj);

// --- the grant half: these must PASS or the mechanism is unusable ----------
t('GRANTED readFileSync leaf', () => fs.readFileSync(path.join(proj, 'package.json'), 'utf8').length + 'B');
t('GRANTED readdirSync project', () => fs.readdirSync(proj).length + ' entries');
t('GRANTED realpathSync leaf', () => fs.realpathSync(path.join(proj, 'package.json')));
t('GRANTED process.cwd()', () => process.cwd());
t('GRANTED require() deep file', () => require(path.join(proj, 'node_modules', 'nested-pkg', 'a', 'b', 'c', 'probe.js')));
t('GRANTED bare require from nested dir', () =>
  require.resolve('leftpad-stub', { paths: [path.join(proj, 'node_modules', 'nested-pkg', 'a', 'b', 'c')] }));
t('GRANTED find-up walk to root', () => {
  let dir = path.join(proj, 'node_modules', 'nested-pkg', 'a', 'b', 'c');
  const hits = [];
  for (;;) {
    try { if (fs.existsSync(path.join(dir, 'package.json'))) hits.push(dir); } catch (e) { /* record below */ }
    const up = path.dirname(dir);
    if (up === dir) break;
    dir = up;
  }
  return 'reached root, ' + hits.length + ' package.json ancestors';
});
t('GRANTED write into project (label)', () => {
  const p = path.join(proj, 'usid-write-probe.txt');
  fs.writeFileSync(p, 'hello'); return fs.readFileSync(p, 'utf8');
});

// --- the deny half: the whole point. Every row here must FAIL in the jailed
//     arm and PASS in the baseline arm.
t('DENY-WANTED read ~/.ssh/id_rsa', () => fs.readFileSync(path.join(profile, '.ssh', 'id_rsa'), 'utf8').trim().slice(0, 40));
t('DENY-WANTED read ~/.npmrc', () => fs.readFileSync(path.join(profile, '.npmrc'), 'utf8').trim().slice(0, 40));
t('DENY-WANTED readdirSync %USERPROFILE%', () => fs.readdirSync(profile).length + ' entries');
t('DENY-WANTED statSync %USERPROFILE%', () => fs.statSync(profile).isDirectory());
t('DENY-WANTED readdirSync C:\\Users', () => fs.readdirSync('C:\\Users').length + ' entries');
t('DENY-WANTED readdirSync C:\\', () => fs.readdirSync('C:\\').length + ' entries');
t('DENY-WANTED read ungranted sibling', () => fs.readFileSync(path.join(sibling, 'sibling.txt'), 'utf8').trim());
t('DENY-WANTED readdirSync ungranted sibling', () => fs.readdirSync(sibling).length + ' entries');

console.log(`\n== ops summary pass=${pass} fail=${fail}`);
}
if (delayMs > 0) setTimeout(main, delayMs); else main();
