// The in-child operations table for both probes in this directory, driven entirely by env so one
// child serves every arm of both.
//
// `AJ_TARGETS` is `name|kind|path` triples separated by `;`, kind in {read, stat, readdir, walk}.
// Each produces exactly one `op:<name>=OK|ERR …` line. Two cells are FIXED and are the in-child
// controls every arm needs:
//
//   stat-c-root            must be ERR in an AppContainer arm. GRANTED would mean the token is not
//                          actually confined, and every pass in the table would be vacuous.
//   read-system32-hosts    must be OK in an AppContainer arm. `C:\Windows\System32` carries an
//                          ALL APPLICATION PACKAGES ace, so this proves the gate is PASSABLE — a
//                          column of ERRs is then a statement about DACLs and not about a child
//                          that fails at everything.
//
// The child is deliberately DUMB about expectations: it reports what happened, never whether that
// was supposed to happen. Every verdict is asserted by the harness, which knows the arm.
//
// NO PIPED SPAWN AND NO NETWORK IO HERE. `spawnSync` with piped stdio hangs INDEFINITELY under an
// AppContainer (MECHANISM-FACTS §5h, reproduced across three runs) and has already taken out a
// whole run by swallowing every op after it. Nothing in this child can block.
//
// The `--preserve-symlinks-main --preserve-symlinks` pair is passed by the harness on every arm:
// without it a confined `node` dies in `resolveMainPath` at `EPERM lstat 'C:\'` before a single
// user statement runs (§5h). That is a KNOWN precondition for reaching this table at all, not a
// variable under test here, so it is held constant across every arm of both probes.

'use strict';
const fs = require('fs');
const path = require('path');

const ARM = process.env.AJ_ARM || '(unset)';

function one(line) {
  // Synchronous: stdout is an inherited FILE handle, so a process that dies mid-table has still
  // flushed everything before it.
  process.stdout.write(line.replace(/[\r\n]+/g, ' ') + '\n');
}
function run(name, fn) {
  try {
    const v = fn();
    one('op:' + name + '=OK ' + (v === undefined ? '' : String(v)));
  } catch (e) {
    one(
      'op:' +
        name +
        '=ERR ' +
        ((e && (e.code || e.errno)) || 'throw') +
        ' ' +
        String((e && e.message) || e).slice(0, 180)
    );
  }
}

one('child:start arm=' + ARM + ' node=' + process.versions.node + ' exec=' + process.execPath);
run('cwd-at-entry', () => process.cwd());

for (const spec of (process.env.AJ_TARGETS || '').split(';')) {
  if (!spec.trim()) continue;
  const bar = spec.split('|');
  if (bar.length < 3) {
    one('op:malformed-target=ERR ' + spec);
    continue;
  }
  const [name, kind, target] = [bar[0], bar[1], bar.slice(2).join('|')];
  if (kind === 'read') run(name, () => fs.readFileSync(target, 'utf8').length + 'B');
  else if (kind === 'stat') run(name, () => 'size=' + fs.statSync(target).size);
  else if (kind === 'readdir') run(name, () => fs.readdirSync(target).length + ' entries');
  else if (kind === 'require') run(name, () => JSON.stringify(require(target)));
  // A find-up walk reported PER LEVEL, so a walk that dies at the volume root is distinguishable
  // from one that never left the granted subtree. This is the cell that shows the traverse skip's
  // exact shape: intermediate components carried, the same components refused AS TARGETS.
  else if (kind === 'walk')
    run(name, () => {
      const parts = [];
      let dir = target;
      for (let i = 0; i < 12; i++) {
        let cell;
        try {
          fs.statSync(dir);
          cell = 'OK';
        } catch (e) {
          cell = 'ERR:' + ((e && e.code) || 'throw');
        }
        parts.push(dir + '=' + cell);
        const up = path.dirname(dir);
        if (up === dir) break;
        dir = up;
      }
      return parts.join(' | ');
    });
  else one('op:' + name + '=ERR unknown-kind ' + kind);
}

run('stat-c-root', () => 'mode=' + fs.statSync('C:\\').mode);
run('read-system32-hosts', () => fs.readFileSync('C:\\Windows\\System32\\drivers\\etc\\hosts', 'utf8').length + 'B');
one('child:done arm=' + ARM);
