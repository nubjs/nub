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
  // `errno` and `syscall` ride along because the `.native` battery below is ATTRIBUTED by them:
  // libuv maps ERROR_ACCESS_DENIED to EPERM and ERROR_SHARING_VIOLATION to EBUSY
  // (`deps/uv/src/win/error.c:158` and `:86`), so which one comes back says which Win32 call
  // refused. A bare `code` collapses exactly the distinction those cells exist to make.
  const detail =
    'errno=' +
    ((e && e.errno) === undefined ? '?' : e.errno) +
    ' syscall=' +
    ((e && e.syscall) || '?');
  one('op:' + name + '=ERR ' + code + ' ' + detail + ' ' + String((e && e.message) || e).slice(0, 160));
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

// ── THE `.native` BATTERY: is `GetFinalPathNameByHandleW` really refused, and by WHICH call? ──
// A prior lane recorded `.native` as refused and closed the search on the strength of it. The
// attribution is what these cells re-open: the ONLY per-component access sensitivity Microsoft
// documents for `GetFinalPathNameByHandleW` is scoped to SMB, so on LOCAL NTFS the normalized
// name should come back in one query. libuv's own call shape has two other suspects —
// `CreateFileW(path, /*access*/0, /*share*/0, …)` and `VOLUME_NAME_DOS`'s Mount-Manager step
// (`deps/uv/src/win/fs.c` `fs__realpath` / `fs__realpath_handle`).
//
// `open-deep-granted` bounds the whole battery: if the leaf cannot even be OPENED, a `.native`
// refusal says nothing about realpath. `native-deep-granted-held` is the one-variable
// discriminator — an already-open handle makes libuv's `dwShareMode=0` open a SHARING violation,
// which libuv maps to EBUSY. EPERM here and EBUSY there means `CreateFileW` succeeds when
// unheld, so the refusal belongs to `GetFinalPathNameByHandleW`; EPERM in both means the access
// check on the open denies first.
run('open-deep-granted', () => {
  const fd = fs.openSync(DEEP, 'r');
  fs.closeSync(fd);
  return 'opened';
});
run('native-deep-granted', () => fs.realpathSync.native(DEEP));
run('native-deep-granted-held', () => {
  const fd = fs.openSync(DEEP, 'r');
  try {
    return fs.realpathSync.native(DEEP);
  } finally {
    fs.closeSync(fd);
  }
});
run('native-deepdir-granted', () => fs.realpathSync.native(DEEPDIR));
run('native-runtime-granted', () => fs.realpathSync.native(RUNTIME));
// System32 carries an ALL APPLICATION PACKAGES ace and so does `C:\Windows`; only `C:\` does
// not. If per-component sensitivity were the mechanism this still fails, and if the leaf's own
// DACL were the mechanism it succeeds — the two hypotheses disagree here and nowhere else.
run('native-system32-hosts', () => fs.realpathSync.native('C:\\Windows\\System32\\drivers\\etc\\hosts'));
// `\\?\` suppresses Win32 path parsing/normalisation before the open. If that alone flips the
// answer, the refusal is in name resolution rather than in the security check.
run('native-longpath-granted', () => fs.realpathSync.native('\\\\?\\' + DEEP));
run('native-c-root', () => fs.realpathSync.native('C:\\'));

// ── THE REALPATH SHIM, and the control that decides whether it is allowed to ship ──
// `realpath-shim-installed` separates "the repair works" from "the preload never arrived": a
// `data:` `--import` that failed to evaluate would otherwise read as a repair that did nothing.
run('realpath-shim-installed', () => {
  if (!globalThis.__nubJailRealpathShim) throw new Error('shim absent');
  return 'installed';
});
// THE CORRECTNESS CONTROL THAT MATTERS MOST, on the real kernel rather than in simulation.
// `node_modules/foo` is a store-cell symlink whose OWN private dependency is `bar@2.0.0`; an
// unrelated `bar@1.0.0` sits at the layout's top level, which is where a link-path walk lands.
// `--preserve-symlinks` silently answers `bar@1.0.0` here and exits 0 — the hazard that
// disqualified the flag. A repair that resolves symlinks for real must answer `bar@2.0.0`.
run('isolated-layout-version', () => 'bar@' + require(process.env.BT_ISO_FOO).barVersion);
run('isolated-layout-resolved-main', () => require.resolve(process.env.BT_ISO_FOO));
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
// `lstat` and `realpath` on the volume root are singled out because they are what Node's own
// `realpathSync` does, and bypass-traverse exempts INTERMEDIATE components only — an ancestor
// opened as a TARGET is still access-checked. This pair is the measured cause of the
// `EPERM lstat 'C:\'` that kills an unflagged confined `node` before user code exists.
run('lstat-c-root', () => 'mode=' + fs.lstatSync('C:\\').mode);
run('realpath-c-root', () => fs.realpathSync('C:\\'));
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

// THE PROPERTY THE WHOLE EXERCISE EXISTS TO GET. On Linux (Landlock) and macOS (Seatbelt) a
// confined lifecycle script cannot reach $HOME secrets; on Windows's current restricted-token
// design it can, because the token keeps the user's sid and every DACL granting the user applies.
// An AppContainer is deny-by-default, so these should be refused by construction — CONFIRMED here
// rather than assumed, since it is the single cell that distinguishes the two designs.
run('read-ssh-private-key', () => 'LEAKED ' + fs.readFileSync(process.env.BT_SSH_KEY, 'utf8').slice(0, 24));
run('readdir-dot-ssh', () => fs.readdirSync(path.dirname(process.env.BT_SSH_KEY)).join(','));
run('stat-ssh-private-key', () => 'size=' + fs.statSync(process.env.BT_SSH_KEY).size);
run('read-npmrc', () => 'LEAKED ' + fs.readFileSync(process.env.BT_NPMRC, 'utf8').slice(0, 24));

// ── in-child proof the token really IS a LowBox one ────────────────────────────────────
// System32 carries an ALL APPLICATION PACKAGES ace, so an AppContainer reads it while being
// denied C:\. "System32 OK + C:\ denied" cannot be produced by a non-AppContainer token, and
// cannot be produced by a harness that is failing everything.
run('read-system32-hosts', () => fs.readFileSync('C:\\Windows\\System32\\drivers\\etc\\hosts', 'utf8').length + 'B');
run('readdir-system32', () => fs.readdirSync('C:\\Windows\\System32').length + ' entries');

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
  // `child:done` is emitted BEFORE the piped spawn below, deliberately: run 30506477831 measured
  // that spawn HANGING INDEFINITELY under the AppContainer (libuv's named-pipe setup blocks before
  // Node's own `timeout` can arm), which took every op after it — the whole egress table — down
  // with it. So the marker that says "the table completed" must precede the op that can hang.
  one('child:done arm=' + ARM);
  // Kept as the LAST op, and named for what it measures: a piped child_process spawn is what every
  // npm lifecycle script does, and an indefinite hang is a worse failure mode than a refusal.
  run('spawn-piped-whoami', () => {
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
  one('child:spawn-op-returned arm=' + ARM);
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
