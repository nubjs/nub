// The operations table that killed the AppContainer route, plus the controls
// that make a failure column meaningful. Run under the jail AND unjailed.
//
//   node ops.js <grantedProjectDir>
//
// Expected verdicts are asserted inline so a table row that flips is visible
// without cross-referencing a separate baseline run.
const fs = require('fs');
const path = require('path');
const os = require('os');

const proj = process.argv[2];
const profile = process.env.USERPROFILE || os.homedir();
let pass = 0, fail = 0, unexpected = 0;

function t(expect, name, fn) {
  let got, detail;
  try { const v = fn(); got = 'PASS'; detail = String(v === undefined ? '' : v).slice(0, 110); }
  catch (e) { got = 'FAIL'; detail = (e.code || '') + ' ' + String(e.message).split('\n')[0].slice(0, 110); }
  const ok = got === expect;
  if (got === 'PASS') pass++; else fail++;
  if (!ok) unexpected++;
  console.log(`${ok ? ' ' : '!'} ${got.padEnd(4)} (want ${expect}) ${name.padEnd(42)} ${detail}`);
}

console.log('== node ' + process.version + '  cwd=' + process.cwd() + '  proj=' + proj);

// --- CONTROLS: if these do not behave, the whole table is meaningless -------
t('PASS', 'CONTROL readFileSync granted leaf', () =>
  fs.readFileSync(path.join(proj, 'package.json'), 'utf8').length + 'B');
t('PASS', 'CONTROL writeFileSync granted dir', () => {
  const p = path.join(proj, 'jail-write-probe.txt');
  fs.writeFileSync(p, 'hello'); return fs.readFileSync(p, 'utf8');
});
t('FAIL', 'NEGCONTROL write into %USERPROFILE%', () => {
  const p = path.join(profile, 'jail-escape-probe.txt');
  fs.writeFileSync(p, 'escaped'); return 'WROTE ' + p;
});
t('FAIL', 'NEGCONTROL write into C:\\', () => {
  fs.writeFileSync('C:\\jail-escape-probe.txt', 'escaped'); return 'WROTE C:\\';
});

// --- the AppContainer-killing read operations ------------------------------
t('PASS', 'process.cwd()', () => process.cwd());
t('PASS', 'realpathSync granted leaf', () => fs.realpathSync(path.join(proj, 'package.json')));
t('PASS', 'realpathSync granted dir', () => fs.realpathSync(proj));
t('PASS', 'statSync C:\\', () => fs.statSync('C:\\').isDirectory());
t('PASS', 'statSync C:\\Users', () => fs.statSync('C:\\Users').isDirectory());
t('PASS', 'statSync %USERPROFILE%', () => fs.statSync(profile).isDirectory());
t('PASS', 'readdirSync %USERPROFILE%', () => fs.readdirSync(profile).length + ' entries');
t('PASS', 'readdirSync C:\\', () => fs.readdirSync('C:\\').length + ' entries');

t('PASS', 'find-up walk to filesystem root', () => {
  // The pkg-dir / find-up / cosmiconfig shape: existsSync at every ancestor.
  let dir = path.join(proj, 'node_modules', 'nested-pkg', 'a', 'b', 'c');
  const hits = [];
  for (;;) {
    if (fs.existsSync(path.join(dir, 'package.json'))) hits.push(dir);
    const up = path.dirname(dir);
    if (up === dir) break;
    dir = up;
  }
  return 'reached root, ' + hits.length + ' package.json ancestors';
});

t('PASS', 'bare require from nested dir (_nodeModulePaths)', () => {
  // Runs a child module that lives deep inside node_modules and does
  // require('leftpad-stub') with no relative path, so Node probes every
  // ancestor node_modules on the way up.
  return require(path.join(proj, 'node_modules', 'nested-pkg', 'a', 'b', 'c', 'probe.js'));
});

t('PASS', 'require.resolve walks ancestors', () =>
  require.resolve('leftpad-stub', { paths: [path.join(proj, 'node_modules', 'nested-pkg', 'a', 'b', 'c')] }));

// --- broad reads are explicitly sanctioned; record what is visible ---------
t('PASS', 'read %USERPROFILE%\\.npmrc-ish (recon, sanctioned)', () => {
  const p = path.join(profile, 'secret-probe.txt');
  return fs.readFileSync(p, 'utf8').trim();
});

console.log(`\n== ops summary pass=${pass} fail=${fail} unexpected=${unexpected}`);
process.exitCode = unexpected === 0 ? 0 : 9;
