// Branch-scoped Windows probe for the shell that runs DEPENDENCY LIFECYCLE
// scripts (`preinstall`/`install`/`postinstall`), which the engine used to
// hardcode to cmd.exe while `nub run` already defaulted to the bundled
// busybox-w32 `sh`. Sibling of tests/busybox-run-probe/, which covers `nub run`.
//
// This is a DIFFERENTIAL, not a green check: one nub binary runs each case
// twice and the ONLY variable is whether `busybox.exe` sits next to `nub.exe`.
//
//   arm `cmd`     — sidecar absent. `apply_lifecycle_script_shell` warns and
//                   leaves the engine on `cmd.exe /d /s /c`, byte-identical to
//                   the pre-change behavior. This is the control.
//   arm `busybox` — sidecar staged as the win32 package lays it out, so the
//                   engine resolves it and spawns `busybox.exe sh -c <body>`.
//
// Each body is POSIX and chosen to FAIL under cmd.exe, so the control arm must
// go red. A case that passes in both arms is not a discriminator and proves
// nothing, so the probe asserts the split in BOTH directions.
//
// Each case gets its OWN fixture and its own install. Sharing one fixture is
// wrong: a lifecycle failure aborts the JoinSet driving the parallel pass,
// which kills sibling scripts mid-write — that silently truncated one case's
// marker and suppressed another's entirely.
//
// Usage: node run-probe.mjs <path-to-nub.exe> <work-dir> <path-to-busybox.exe>
// Exit 0 iff every case shows the expected cmd-fails / busybox-passes split.

import { spawnSync } from "node:child_process";
import {
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  copyFileSync,
  existsSync,
  chmodSync,
} from "node:fs";
import { join, resolve, dirname } from "node:path";

const nubBin = resolve(process.argv[2] ?? "");
const workRoot = resolve(process.argv[3] ?? "work");
const busyboxSrc = resolve(process.argv[4] ?? "");
if (!process.argv[2] || !process.argv[4]) {
  process.stderr.write("usage: run-probe.mjs <nub.exe> <work-dir> <busybox.exe>\n");
  process.exit(2);
}
// The sidecar nub resolves at run time is `busybox.exe` beside the binary.
// Staging and un-staging THIS path is the probe's single independent variable.
const sidecar = join(dirname(nubBin), `busybox${process.platform === "win32" ? ".exe" : ""}`);

// `want` is what the body must leave in `<pkg>/out.txt` under a POSIX shell.
// `exec` files get the exec bit (meaningless on Windows, required on Unix).
const cases = [
  {
    // Braced parameter expansion with a default, plus the `test` utility cmd
    // has no equivalent for. cmd leaves `${…}` literal and errors on `test`.
    id: "param_expansion_and_test",
    script: 'echo "MARK=${LIFECYCLE_PROBE:-posix}" > out.txt && test -d . && echo dirok >> out.txt',
    files: {},
    want: "MARK=posix\ndirok",
  },
  {
    // The shape that makes this a FIX rather than a swap: real packages
    // (`detox-recorder`, `svf-lib`) invoke a shipped shell script from their
    // postinstall, which cmd.exe cannot execute at all.
    id: "invokes_shipped_sh_script",
    script: "./scripts/build.sh",
    files: { "scripts/build.sh": "#!/bin/sh\nprintf 'SH_RAN\\n' > out.txt\n" },
    exec: ["scripts/build.sh"],
    want: "SH_RAN",
  },
  {
    // Glob expansion + append redirection inside a loop — the shell, not the
    // applet, has to expand the pattern.
    id: "glob_loop_and_redirect",
    script: "for f in src/*.txt; do cat $f >> out.txt; done",
    files: { "src/a.txt": "A\n", "src/b.txt": "B\n" },
    want: "A\nB",
  },
];

// Every case gets a DISTINCT package name. Also load-bearing, and also a
// broken-control bug this probe already hit: with one shared name the
// content-addressed store served the first case's built package directory to
// all of them, so all three reported the first case's marker and the control
// "confirmed" the wrong thing three times.
const pkgName = (c) => `probedep-${c.id.replace(/_/g, "-")}`;

function writeFixture(dir, c) {
  rmSync(dir, { recursive: true, force: true });
  const pkg = pkgName(c);
  const pkgDir = join(dir, pkg);
  mkdirSync(pkgDir, { recursive: true });
  writeFileSync(
    join(pkgDir, "package.json"),
    JSON.stringify({ name: pkg, version: "1.0.0", scripts: { postinstall: c.script } }, null, 2),
  );
  for (const [rel, body] of Object.entries(c.files)) {
    const p = join(pkgDir, rel);
    mkdirSync(dirname(p), { recursive: true });
    writeFileSync(p, body);
  }
  for (const rel of c.exec ?? []) chmodSync(join(pkgDir, rel), 0o755);
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify(
      {
        name: "lifecycle-shell-probe",
        version: "0.0.0",
        private: true,
        dependencies: { [pkg]: `file:./${pkg}` },
      },
      null,
      2,
    ),
  );
  // LOAD-BEARING for the differential, not hygiene. The side-effects cache is
  // on by default and keys on `(name, version, engine, input hash)` — which is
  // IDENTICAL across the two arms, since only the shell differs and the shell
  // is not part of the key. With it on, whichever arm ran second would hardlink
  // the first arm's built tree back and skip the script entirely, so the
  // control would "confirm" whatever the first arm did.
  writeFileSync(join(dir, ".npmrc"), "side-effects-cache=false\n");
}

function nub(cwd, args) {
  const r = spawnSync(nubBin, args, { cwd, encoding: "utf8", timeout: 300000 });
  return { out: (r.stdout ?? "") + (r.stderr ?? ""), code: r.status, flag: r.error?.code };
}

// Install once to surface the ignored build, approve it, then run the MEASURED
// install. `approve-builds --all` writes the allowlist key itself, so the probe
// never hardcodes a `file:` spec spelling that could normalize per platform.
function runCase(c, arm, withBusybox) {
  rmSync(sidecar, { force: true });
  if (withBusybox) copyFileSync(busyboxSrc, sidecar);
  const dir = join(workRoot, arm, c.id);
  writeFixture(dir, c);
  nub(dir, ["install", "--offline"]);
  const approve = nub(dir, ["approve-builds", "--all"]);
  const measured = nub(dir, ["install", "--offline"]);
  const out = join(dir, "node_modules", pkgName(c), "out.txt");
  return {
    approve,
    measured,
    marker: existsSync(out) ? readFileSync(out, "utf8").replace(/\r\n/g, "\n").trim() : null,
  };
}

// The control arm only means anything on Windows: `apply_lifecycle_script_shell`
// is gated to Windows, so off it BOTH arms run the platform `/bin/sh` and the
// "cmd" arm is not a cmd arm at all. A Unix run is therefore a harness smoke
// test — it proves the fixtures, the approve step, and every POSIX body are
// right — and the control checks report SKIP rather than a misleading pass.
const isWindows = process.platform === "win32";

let failures = 0;
function check(id, pass, detail) {
  if (!pass) failures++;
  process.stdout.write(`RESULT|${id}|${pass ? "PASS" : "FAIL"}|${detail}\n`);
}
function checkControl(id, pass, detail) {
  if (!isWindows) {
    process.stdout.write(`RESULT|${id}|SKIP|not Windows: both arms run /bin/sh\n`);
    return;
  }
  check(id, pass, detail);
}

for (const c of cases) {
  const cmd = runCase(c, "cmd", false);
  const bb = runCase(c, "busybox", true);
  process.stdout.write(
    `ARM|${c.id}|cmd:exit=${cmd.measured.code},marker=${JSON.stringify(cmd.marker)}` +
      `|busybox:exit=${bb.measured.code},marker=${JSON.stringify(bb.marker)}\n`,
  );

  // Guard the control itself: if approve never ran the build stays ignored, so
  // BOTH arms would produce no marker and the split would read as "cmd failed"
  // for a reason that has nothing to do with the shell.
  for (const [arm, r] of [
    ["cmd", cmd],
    ["busybox", bb],
  ]) {
    check(
      `${c.id}_${arm}_build_approved`,
      /Approved 1 package/.test(r.approve.out),
      JSON.stringify(r.approve.out.trim().slice(0, 200)),
    );
  }

  check(
    `${c.id}_busybox_install_succeeds`,
    bb.measured.code === 0,
    JSON.stringify(bb.measured.out.slice(-400)),
  );
  check(
    `${c.id}_busybox_marker_is_posix_result`,
    bb.marker === c.want,
    `want=${JSON.stringify(c.want)} got=${JSON.stringify(bb.marker)}`,
  );
  checkControl(
    `${c.id}_cmd_arm_differs`,
    cmd.measured.code !== 0 || cmd.marker !== c.want,
    `the control must NOT reproduce the POSIX result, else this case is not a ` +
      `discriminator: exit=${cmd.measured.code} marker=${JSON.stringify(cmd.marker)}`,
  );
}

process.stdout.write(`\nSUMMARY|${failures === 0 ? "PASS" : `${failures} FAILED`}\n`);
process.exit(failures === 0 ? 0 : 1);
