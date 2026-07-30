// The Low-IL hazard probe. srt runs its child at MEDIUM deliberately
// (token.rs:45-47: "so Schannel / LSA / registry edge cases that fire at Low IL
// don't apply"). Adopting Low inherits those. A jail that confines writes but
// breaks TLS is not shippable, so each of these is a ship/no-ship gate.
//
//   node edge.js
const https = require('https');
const { execFileSync, spawnSync } = require('child_process');
const net = require('net');
const os = require('os');
const fs = require('fs');
const path = require('path');

let bad = 0;
function line(name, ok, detail) {
  if (!ok) bad++;
  console.log(`${ok ? ' ' : '!'} ${ok ? 'OK  ' : 'FAIL'} ${name.padEnd(38)} ${String(detail).slice(0, 130)}`);
}
function sync(name, fn) {
  try { line(name, true, fn()); } catch (e) { line(name, false, (e.code || '') + ' ' + String(e.message).split('\n')[0]); }
}

// 1. Schannel: real TLS handshake to a real host.
function tls(host, pathname) {
  return new Promise((res) => {
    const req = https.get({ host, path: pathname, timeout: 25000 }, (r) => {
      // Read the TLS params on 'response', while the socket is still attached —
      // by 'end' r.socket is null and touching it throws inside the handler,
      // which masks the very result this probe exists to report.
      const proto = r.socket && r.socket.getProtocol ? r.socket.getProtocol() : '?';
      const cipher = r.socket && r.socket.getCipher ? (r.socket.getCipher() || {}).name : '?';
      let n = 0;
      r.on('data', (d) => { n += d.length; });
      r.on('end', () => res(['ok', `status=${r.statusCode} bytes=${n} proto=${proto} cipher=${cipher}`]));
    });
    req.on('timeout', () => { req.destroy(new Error('timeout')); });
    req.on('error', (e) => res(['err', (e.code || '') + ' ' + e.message]));
  });
}

// 2. Registry via reg.exe (spawns a child AND reads HKCU).
sync('spawn child process (node -e)', () =>
  execFileSync(process.execPath, ['-e', 'process.stdout.write("child-ran")'], { encoding: 'utf8' }));
sync('reg query HKCU\\Environment', () =>
  execFileSync('reg.exe', ['query', 'HKCU\\Environment'], { encoding: 'utf8' }).split('\n').length + ' lines');
sync('reg query HKLM\\...\\CurrentVersion', () =>
  execFileSync('reg.exe', ['query', 'HKLM\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion', '/v', 'ProductName'], { encoding: 'utf8' }).trim().split('\n').pop().trim());
sync('reg WRITE HKCU (low-IL write-up?)', () => {
  const r = spawnSync('reg.exe', ['add', 'HKCU\\Software\\NubJailProbe', '/v', 'x', '/d', '1', '/f'], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error('rc=' + r.status + ' ' + (r.stderr || '').trim());
  return 'wrote HKCU';
});
sync('cmd.exe /c echo', () => execFileSync('cmd.exe', ['/c', 'echo cmd-ran'], { encoding: 'utf8' }).trim());
sync('os.userInfo()', () => os.userInfo().username);
sync('os.tmpdir writable', () => {
  const p = path.join(os.tmpdir(), 'nubjail-tmp-probe.txt');
  fs.writeFileSync(p, 'x'); return p;
});
sync('whoami /groups (LSA lookup)', () =>
  execFileSync('whoami.exe', ['/groups'], { encoding: 'utf8' }).split('\n').length + ' lines');

// 3. Named pipe round trip (npm/node IPC, and node-gyp's msbuild uses pipes).
function pipe() {
  return new Promise((res) => {
    const name = '\\\\.\\pipe\\nubjail-' + process.pid;
    const srv = net.createServer((s) => { s.end('pong'); });
    srv.on('error', (e) => res(['err', 'server ' + (e.code || '') + ' ' + e.message]));
    srv.listen(name, () => {
      const c = net.connect(name);
      let got = '';
      c.on('data', (d) => { got += d; });
      c.on('end', () => { srv.close(); res(['ok', 'round-trip=' + got]); });
      c.on('error', (e) => { srv.close(); res(['err', 'client ' + (e.code || '') + ' ' + e.message]); });
    });
  });
}

// 4. DNS (uses the DNS client service over ALPC).
const dns = require('dns');
function resolve() {
  return new Promise((res) => dns.lookup('registry.npmjs.org', (e, a) =>
    res(e ? ['err', (e.code || '') + ' ' + e.message] : ['ok', a])));
}

(async () => {
  const [rk, rv] = await resolve(); line('dns.lookup', rk === 'ok', rv);
  for (const [h, p] of [['registry.npmjs.org', '/-/ping'], ['nodejs.org', '/dist/index.json'], ['github.com', '/']]) {
    const [k, v] = await tls(h, p);
    line('TLS https://' + h, k === 'ok', v);
  }
  const [pk, pv] = await pipe(); line('named pipe round trip', pk === 'ok', pv);
  console.log('\n== edge summary failures=' + bad);
  process.exitCode = bad === 0 ? 0 : 8;
})();
