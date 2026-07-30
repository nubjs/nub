// Host-side fidelity check for `local-pipe-shim.mjs`. Answers the half of the question that is
// NOT a Windows fact: does the emulated channel behave like `child_process`'s own?
//
// It is a DIFFERENTIAL by construction — every case runs twice from one process, once with the
// preload and once without, and the two transcripts must be identical. A case that passes only in
// the shimmed arm proves nothing; a case that passes in both proves the emulation is transparent.
// The Windows namespace question is measured separately, on CI, by `child-objects.js`.
//
//   node tests/win-bypass-traverse/local-pipe-selftest.mjs

import cp from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const shim = path.join(here, "local-pipe-shim.mjs");
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nub-ipc-selftest-"));

// Each case is a self-contained parent script. It prints one `>` line per observation; the two
// arms' `>` lines are compared verbatim, so anything nondeterministic (a pid, a timing) must not
// be printed.
const cases = {
  "roundtrip-and-exit": `
    const cp = require('node:child_process');
    const fs = require('node:fs');
    const forkee = require('path').join(${JSON.stringify(dir)}, 'f-roundtrip.cjs');
    fs.writeFileSync(forkee, \`
      process.on('message', (m) => {
        if (m.q === 'ping') process.send({ a: 'pong', echo: m.n });
        else if (m.q === 'bye') process.disconnect();
      });
    \`);
    const child = cp.fork(forkee, [], { stdio: 'inherit' });
    console.log('> connected=' + child.connected);
    console.log('> channel=' + (child.channel ? 'present' : 'absent'));
    console.log('> send=' + child.send({ q: 'ping', n: 41 }));
    child.on('message', (m) => {
      console.log('> message=' + JSON.stringify(m));
      child.send({ q: 'bye' });
    });
    child.on('disconnect', () => console.log('> parent-saw-disconnect connected=' + child.connected));
    child.on('exit', (code) => console.log('> exit=' + code));
    child.on('close', () => console.log('> close'));
  `,

  "child-initiated-and-process-connected": `
    const cp = require('node:child_process');
    const fs = require('node:fs');
    const forkee = require('path').join(${JSON.stringify(dir)}, 'f-initiated.cjs');
    fs.writeFileSync(forkee, \`
      process.send({ hello: 'from-child', connected: process.connected, hasSend: typeof process.send });
      process.on('message', () => process.exit(7));
    \`);
    const child = cp.fork(forkee, [], { stdio: 'inherit' });
    child.on('message', (m) => { console.log('> message=' + JSON.stringify(m)); child.send({ go: 1 }); });
    child.on('exit', (code) => console.log('> exit=' + code));
  `,

  "send-after-disconnect-throws": `
    const cp = require('node:child_process');
    const fs = require('node:fs');
    const forkee = require('path').join(${JSON.stringify(dir)}, 'f-closed.cjs');
    fs.writeFileSync(forkee, "setTimeout(() => {}, 50);");
    const child = cp.fork(forkee, [], { stdio: 'inherit' });
    child.disconnect();
    child.on('disconnect', () => {
      try { child.send({ x: 1 }); console.log('> threw=no'); }
      catch (e) { console.log('> threw=' + e.code); }
      console.log('> connected=' + child.connected);
    });
    child.on('exit', (code) => console.log('> exit=' + code));
  `,

  "ordered-burst": `
    const cp = require('node:child_process');
    const fs = require('node:fs');
    const forkee = require('path').join(${JSON.stringify(dir)}, 'f-burst.cjs');
    fs.writeFileSync(forkee, \`
      const seen = [];
      process.on('message', (m) => {
        if (m.i !== undefined) { seen.push(m.i); return; }
        process.send({ seen: seen.join(',') });
      });
    \`);
    const child = cp.fork(forkee, [], { stdio: 'inherit' });
    for (let i = 0; i < 200; i++) child.send({ i });
    child.send({ done: 1 });
    child.on('message', (m) => { console.log('> seen-count=' + m.seen.split(',').length); console.log('> ordered=' + (m.seen === Array.from({length:200},(_,i)=>i).join(','))); child.kill(); });
  `,

  // A `fork` that rebuilds `env` from scratch, or empties `execArgv`, strips the preload — the
  // exact hazard the net-gate shim documented. Both must still get a channel, or the fix covers
  // only the polite half of the ecosystem.
  "forkee-with-execargv-emptied": `
    const cp = require('node:child_process');
    const fs = require('node:fs');
    const forkee = require('path').join(${JSON.stringify(dir)}, 'f-noargv.cjs');
    fs.writeFileSync(forkee, "process.send({ ok: 1, send: typeof process.send });");
    const child = cp.fork(forkee, [], { stdio: 'inherit', execArgv: [] });
    child.on('message', (m) => { console.log('> message=' + JSON.stringify(m)); child.kill(); });
    child.on('exit', () => console.log('> exited'));
  `,

  "forkee-with-env-rebuilt": `
    const cp = require('node:child_process');
    const fs = require('node:fs');
    const forkee = require('path').join(${JSON.stringify(dir)}, 'f-newenv.cjs');
    fs.writeFileSync(forkee, "process.send({ ok: 1 });");
    const child = cp.fork(forkee, [], { stdio: 'inherit', env: { PATH: process.env.PATH } });
    child.on('message', (m) => { console.log('> message=' + JSON.stringify(m)); child.kill(); });
    child.on('exit', () => console.log('> exited'));
  `,
};

// Cases with no unshimmed equivalent, because the shimmed behaviour is a DELIBERATE divergence.
// They assert the shim's own contract instead of a diff, and each names why a diff is impossible.
const shimOnly = {
  // Handle passing needs `uv_write2`/SCM_RIGHTS, which no userland stream carries. The contract is
  // that it fails FAST and TYPED — the whole point of the exercise is not trading a hang for a hang.
  "handle-passing-refused-fast-and-typed": {
    source: `
      const cp = require('node:child_process');
      const fs = require('node:fs');
      const net = require('node:net');
      const forkee = require('path').join(${JSON.stringify(dir)}, 'f-handle.cjs');
      fs.writeFileSync(forkee, "setTimeout(() => {}, 100);");
      const child = cp.fork(forkee, [], { stdio: 'inherit' });
      const srv = net.createServer();
      srv.listen(0, () => {
        try { child.send({ s: 1 }, srv); console.log('> handle-send=accepted'); }
        catch (e) { console.log('> handle-send=' + e.code); }
        srv.close(); child.kill();
      });
    `,
    expect: ["> handle-send=ERR_NUB_SANDBOX_NO_IPC"],
  },
  "advanced-serialization-refused": {
    source: `
      const cp = require('node:child_process');
      const fs = require('node:fs');
      const forkee = require('path').join(${JSON.stringify(dir)}, 'f-adv.cjs');
      fs.writeFileSync(forkee, "setTimeout(() => {}, 50);");
      try { cp.fork(forkee, [], { stdio: 'inherit', serialization: 'advanced' }); console.log('> fork=accepted'); }
      catch (e) { console.log('> fork=' + e.code); }
    `,
    expect: ["> fork=ERR_NUB_SANDBOX_NO_IPC"],
  },
};

function runArm(source, shimmed) {
  const file = path.join(dir, `case-${Math.random().toString(36).slice(2)}.cjs`);
  fs.writeFileSync(file, source);
  const execArgv = shimmed ? ["--import", shim] : [];
  const r = cp.spawnSync(process.execPath, [...execArgv, file], {
    encoding: "utf8",
    timeout: 20000,
    env: { ...process.env, NODE_OPTIONS: "" },
  });
  const lines = String(r.stdout || "")
    .split(/\r?\n/)
    .filter((l) => l.startsWith("> "));
  return {
    lines,
    status: r.status,
    timedOut: r.signal === "SIGTERM" && r.status === null,
    stderr: String(r.stderr || "").trim().slice(0, 400),
  };
}

let failures = 0;
for (const [name, source] of Object.entries(cases)) {
  const plain = runArm(source, false);
  const shimd = runArm(source, true);
  // 'ordered-burst' with a real channel may coalesce differently; only the derived assertions are
  // printed, so the transcripts must still match exactly.
  const same =
    plain.lines.join("|") === shimd.lines.join("|") && plain.status === shimd.status && !shimd.timedOut && !plain.timedOut;
  console.log(`${same ? "PASS" : "FAIL"}  ${name}`);
  if (!same) {
    failures++;
    console.log(`   plain  status=${plain.status} timedOut=${plain.timedOut}`);
    for (const l of plain.lines) console.log(`     ${l}`);
    if (plain.stderr) console.log(`     stderr: ${plain.stderr}`);
    console.log(`   shimmed status=${shimd.status} timedOut=${shimd.timedOut}`);
    for (const l of shimd.lines) console.log(`     ${l}`);
    if (shimd.stderr) console.log(`     stderr: ${shimd.stderr}`);
  }
}

for (const [name, { source, expect }] of Object.entries(shimOnly)) {
  const r = runArm(source, true);
  const ok = r.lines.join("|") === expect.join("|") && !r.timedOut;
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}`);
  if (!ok) {
    failures++;
    console.log(`   want ${expect.join(" | ")}`);
    console.log(`   got  ${r.lines.join(" | ")}  (timedOut=${r.timedOut} status=${r.status})`);
    if (r.stderr) console.log(`     stderr: ${r.stderr}`);
  }
}

// The shim must never leave a pipe/socket behind, because a build jail run spawns hundreds of
// children.
const leaked = fs.readdirSync(os.tmpdir()).filter((f) => /^nub-ipc-\d+-/.test(f));
console.log(`${leaked.length === 0 ? "PASS" : "FAIL"}  no-channel-residue (${leaked.length} left)`);
if (leaked.length) failures++;

fs.rmSync(dir, { recursive: true, force: true });
console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
