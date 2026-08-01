// Runs INSIDE the confined child. Reads an op list from `WC_OPS` (a JSON file the unconfined
// parent wrote) and prints one `op:<id>=<result>` line per op.
//
// WHY THE OPS COME FROM A FILE: every arm must run the byte-identical program over the
// byte-identical op list, or a difference between arms is a difference between programs. The only
// variable across arms is the token and the ACEs the parent installed.
//
// NO `child_process` ANYWHERE. A piped `spawnSync` never returns under an AppContainer
// (MECHANISM-FACTS §5h, reproduced across three runs) and swallows every op after it, which
// presents as a timed-out arm rather than as a failed op.
//
// NO `fs.realpath*`, and no bare-specifier `require`. Every realpath surface Node exposes is
// refused under an AppContainer (§5h's API table), and `Module._findPath` realpaths every
// non-main resolution — so this file uses only `node:`-prefixed builtins, which resolve without
// touching the filesystem, and absolute paths.

'use strict';
const fs = require('node:fs');
const net = require('node:net');
const dns = require('node:dns');

// Written IMMEDIATELY, never buffered to the end: a child that dies mid-matrix must still leave
// every op it completed. Buffering turns a partial run into an empty log, which reads as "the
// mechanism is unavailable" rather than "op 7 killed it". `fd 1` is the log handle the parent
// opened UNCONFINED and passed in already-open.
function say(k, v) {
  fs.writeSync(1, `${k}=${v}\n`);
}
function err(e) {
  // The CODE is the finding: EPERM/EACCES is the jail, ENOENT is a broken fixture, and reporting
  // only "failed" makes those indistinguishable.
  return `ERR:${(e && e.code) || (e && e.message) || 'unknown'}`;
}

const ops = JSON.parse(fs.readFileSync(process.env.WC_OPS, 'utf8'));

say('child:start', 'ok');
say('child:pid', process.pid);
say('child:cwd', (() => {
  try {
    return process.cwd();
  } catch (e) {
    return err(e);
  }
})());

for (const op of ops) {
  let res;
  try {
    switch (op.kind) {
      case 'read':
        res = `OK:${fs.readFileSync(op.path).length}`;
        break;
      case 'list':
        res = `OK:${fs.readdirSync(op.path).length}`;
        break;
      case 'stat':
        res = `OK:${fs.statSync(op.path).size}`;
        break;
      // WRITE into an EXISTING file. This is the cell the whole probe turns on: an existing file
      // needs the ACE on ITSELF, which is what makes a broad grant a tree walk rather than one
      // descriptor write.
      case 'write':
        fs.writeFileSync(op.path, `wc ${Date.now()}\n`);
        res = `OK:${fs.statSync(op.path).size}`;
        break;
      // CREATE a new file in an existing directory. Separate from `write` because a new file
      // inherits its parent's inheritable ACEs AT CREATION, so this can pass where `write` fails —
      // and conflating them would read as "the grant works" when only new content is reachable.
      case 'create':
        fs.writeFileSync(op.path, 'wc\n');
        res = `OK:${fs.statSync(op.path).size}`;
        break;
      case 'mkdir':
        fs.mkdirSync(op.path, { recursive: false });
        res = 'OK';
        break;
      case 'unlink':
        fs.unlinkSync(op.path);
        res = 'OK';
        break;
      default:
        res = 'ERR:unknown-kind';
    }
  } catch (e) {
    res = err(e);
  }
  say(`op:${op.id}`, res);
}

// The NET axis, last, and in the same child as the write matrix — that adjacency IS the
// separability question. A broad filesystem grant is a DACL fact; egress denial is a capability
// fact; if they are independent, both hold at once in one token.
function netProbe() {
  return new Promise((resolve) => {
    const done = [];
    let pending = 2;
    const finish = () => {
      if (--pending === 0) resolve(done);
    };
    // A literal IP, so a refusal is the capability and not name resolution.
    const s = net.connect({ host: '1.1.1.1', port: 443 });
    const t = setTimeout(() => {
      s.destroy();
      done.push(['net:connect-1.1.1.1', 'ERR:TIMEOUT']);
      finish();
    }, 8000);
    s.on('connect', () => {
      clearTimeout(t);
      s.destroy();
      done.push(['net:connect-1.1.1.1', 'OK']);
      finish();
    });
    s.on('error', (e) => {
      clearTimeout(t);
      done.push(['net:connect-1.1.1.1', err(e)]);
      finish();
    });
    dns.lookup('registry.npmjs.org', (e, addr) => {
      done.push(['net:dns', e ? err(e) : `OK:${addr}`]);
      finish();
    });
  });
}

netProbe().then((rows) => {
  for (const [k, v] of rows) say(k, v);
  // The completion marker. An arm whose log lacks it was truncated or hung, and its ops must not
  // be read as denials — the failure mode §5h lost a run to.
  say('child:done', 'ok');
});
