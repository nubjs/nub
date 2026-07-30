// The in-child operations table. Runs INSIDE the launched process (AppContainer or plain,
// depending on the arm) and prints one `op:<name>=OK|ERR …` line per operation.
//
// The child is deliberately DUMB about expectations: it reports what happened, never whether
// that was supposed to happen. Every verdict is asserted by the harness (`probe.ps1`), which
// knows the arm — so a cell that changes meaning between arms cannot be hidden inside a
// self-grading child.
//
// Every op is wrapped: an unhandled throw would truncate the table at the first denial, and a
// truncated table reads exactly like a launch that never ran.

'use strict';
const fs = require('fs');
const path = require('path');
const os = require('os');

const ARM = process.env.BT_ARM || '(unset)';
const DEEP = process.env.BT_DEEP;
const DEEPDIR = process.env.BT_DEEPDIR;
const DATA = process.env.BT_DATA;
const RUNTIME = process.env.BT_RUNTIME;
const SIB_INSIDE = process.env.BT_SIB_INSIDE;
const SIB_PROFILE = process.env.BT_SIB_PROFILE;

function one(line) {
  // Written synchronously: stdout is an inherited FILE handle, so ordering is stable, but a
  // process that dies mid-table must still have flushed everything before it.
  process.stdout.write(line.replace(/[\r\n]+/g, ' ') + '\n');
}
function ok(name, detail) {
  one('op:' + name + '=OK ' + (detail === undefined ? '' : String(detail)));
}
function err(name, e) {
  const code = (e && (e.code || e.errno)) || 'throw';
  one('op:' + name + '=ERR ' + code + ' ' + String((e && e.message) || e).slice(0, 200));
}
function run(name, fn) {
  try {
    ok(name, fn());
  } catch (e) {
    err(name, e);
  }
}
async function runAsync(name, fn) {
  try {
    ok(name, await fn());
  } catch (e) {
    err(name, e);
  }
}

one('child:start arm=' + ARM + ' node=' + process.versions.node + ' exec=' + process.execPath);
one('child:cwd-at-entry ' + (function () {
  try {
    return process.cwd();
  } catch (e) {
    return 'ERR ' + ((e && e.code) || e);
  }
})());

// ── the decisive cell and its immediate neighbours ──────────────────────────────────────
// A deep leaf under %USERPROFILE% whose only ACE is on an ANCESTOR SUBTREE well below
// C:\Users. Reaching it at all requires the traverse check on C:\ and C:\Users to be skipped.
run('read-deep-granted', () => fs.readFileSync(DEEP, 'utf8').length + 'B');
run('require-deep-granted', () => JSON.stringify(require(DEEP)));
run('realpath-deep-granted', () => fs.realpathSync(DEEP));
run('stat-deep-granted', () => 'size=' + fs.statSync(DEEP).size);
run('readdir-deepdir', () => fs.readdirSync(DEEPDIR).join(','));
run('write-into-granted', () => {
  const p = path.join(DATA, 'wrote-from-child-' + ARM + '.txt');
  fs.writeFileSync(p, 'hello');
  return p + ' ' + fs.readFileSync(p, 'utf8');
});

// `process.chdir` into the deep granted dir, then `process.cwd()` — the uv_cwd EPERM this
// effort saw before. Restored afterwards so later relative-path ops are not silently rebased.
const cwdBefore = (() => {
  try {
    return process.cwd();
  } catch (e) {
    return null;
  }
})();
run('chdir-to-deepdir', () => {
  process.chdir(DEEPDIR);
  return 'chdir ok';
});
run('cwd-after-chdir', () => process.cwd());
run('read-relative-after-chdir', () => fs.readFileSync('./index.js', 'utf8').length + 'B');
if (cwdBefore) {
  try {
    process.chdir(cwdBefore);
  } catch (e) {
    one('op:chdir-restore=ERR ' + ((e && e.code) || e));
  }
}

// ── the two un-ACE'able roots: expected DENIED, and that is FINE if the deep read passed ──
run('stat-c-root', () => 'mode=' + fs.statSync('C:\\').mode);
run('readdir-c-root', () => fs.readdirSync('C:\\').length + ' entries');
run('stat-c-users', () => 'mode=' + fs.statSync('C:\\Users').mode);
run('readdir-c-users', () => fs.readdirSync('C:\\Users').length + ' entries');
run('stat-userprofile', () => 'mode=' + fs.statSync(os.homedir()).mode);
run('readdir-userprofile', () => fs.readdirSync(os.homedir()).length + ' entries');

// The find-up walk every npm tool does (find-up / pkg-dir / cosmiconfig / _nodeModulePaths).
// Reported per level so a walk that dies at C:\Users is distinguishable from one that never
// left the granted subtree.
run('findup-walk', () => {
  const parts = [];
  let dir = DEEPDIR;
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

// ── grant-scoping controls: these must FAIL in the AppContainer arms ───────────────────
run('read-ungranted-sibling-inside-root', () => fs.readFileSync(SIB_INSIDE, 'utf8'));
run('read-ungranted-sibling-under-profile', () => fs.readFileSync(SIB_PROFILE, 'utf8'));

// ── in-child proof the token really IS a LowBox one ────────────────────────────────────
// System32 carries an ALL APPLICATION PACKAGES ace, so an AppContainer reads it while being
// denied C:\. "System32 OK + C:\ denied" cannot be produced by a non-AppContainer token, and
// cannot be produced by a harness that is failing everything.
run('read-system32-hosts', () => fs.readFileSync('C:\\Windows\\System32\\drivers\\etc\\hosts', 'utf8').length + 'B');
run('readdir-system32', () => fs.readdirSync('C:\\Windows\\System32').length + ' entries');
run('whoami-groups', () => {
  const r = require('child_process').spawnSync('C:\\Windows\\System32\\whoami.exe', ['/groups'], {
    encoding: 'utf8',
  });
  if (r.error) throw r.error;
  const out = String(r.stdout || '') + String(r.stderr || '');
  const label = (out.match(/Mandatory Label\\[^\s]+/) || ['(no label)'])[0];
  const pkg = (out.match(/S-1-15-2-[0-9-]+/) || ['(no package sid)'])[0];
  const caps = (out.match(/S-1-15-3-[0-9-]+/g) || []).join(',') || '(no capability sids)';
  return 'rc=' + r.status + ' label=' + label + ' package=' + pkg + ' caps=' + caps;
});

// ── egress: must be DENIED in the AppContainer arms (internetClient withheld) ──────────
function connectOnce(name, host, port) {
  return new Promise((resolve) => {
    const net = require('net');
    let settled = false;
    let sock;
    const done = (line) => {
      if (settled) return;
      settled = true;
      one('op:' + name + '=' + line);
      try {
        sock.destroy();
      } catch (e) {}
      clearTimeout(guard);
      resolve();
    };
    // Belt-and-braces wall clock: a socket that neither connects nor errors nor times out
    // would otherwise hang the whole table, and a hung child is indistinguishable from a
    // launch that never happened.
    const guard = setTimeout(() => done('ERR HUNG no socket event within 8s'), 8000);
    try {
      sock = net.connect({ host, port });
    } catch (e) {
      done('ERR ' + ((e && e.code) || 'throw') + ' ' + String((e && e.message) || e).slice(0, 120));
      return;
    }
    sock.setTimeout(6000);
    sock.on('connect', () => done('OK connected ' + host + ':' + port));
    sock.on('timeout', () => done('ERR ETIMEDOUT no connect within 6s'));
    sock.on('error', (e) =>
      done('ERR ' + (e.code || e.errno || 'throw') + ' ' + String(e.message).slice(0, 120))
    );
    sock.on('close', () => done('ERR CLOSED closed without connect'));
  });
}

async function main() {
  await runAsync('dns-lookup-registry', () =>
    new Promise((resolve, reject) => {
      require('dns').lookup('registry.npmjs.org', (e, addr) => (e ? reject(e) : resolve(addr)));
    })
  );
  // A literal IP first: it isolates "egress blocked" from "name resolution blocked".
  await connectOnce('net-connect-ip', '1.1.1.1', 443);
  await connectOnce('net-connect-name', 'registry.npmjs.org', 443);
  await connectOnce('net-connect-loopback', '127.0.0.1', 135);
  one('child:done arm=' + ARM);
}

if (require.main === module) {
  main().then(
    () => process.exit(0),
    (e) => {
      one('child:fatal ' + String((e && e.stack) || e).replace(/[\r\n]+/g, ' '));
      process.exit(3);
    }
  );
}
