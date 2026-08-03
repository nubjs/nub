// Windows-only, build-free: which `.cmd` shim shape actually survives cmd.exe?
//
// The shipped shim captures a path with
//   for /f "usebackq delims=" %%i in (`"%EXE%" __node-gyp-bootstrap "%DIR%"`) do ...
// and on a real runner that fails with "The filename, directory name, or volume
// label syntax is incorrect." Theory: `for /f` runs the command through cmd /c,
// which strips the outer quote pair when the string both starts and ends with a
// quote — leaving `C:\...\tool.exe" arg "C:\dir` and a broken program name.
//
// Rather than guess-and-push at ~25 minutes per CI loop, this tests every
// candidate in ONE run against a stand-in that mimics the real shape: a quoted
// executable path plus a quoted argument. Candidate A is the shipped form and
// is expected to FAIL — it is the control. A run where A passes means the
// reproduction is wrong and no conclusion should be drawn from the others.
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

if (process.platform !== "win32") {
  console.log("skip: windows-only probe");
  process.exit(0);
}

const root = mkdtempSync(join(tmpdir(), "cmdquote-"));
// Stand-in for `nub.exe __node-gyp-bootstrap <dir>`: prints a path on stdout.
// A .cmd, so the quoting shape matches without needing a compiled binary.
const tool = join(root, "stand in", "tool.cmd");
mkdirSync(join(root, "stand in"));
writeFileSync(tool, "@echo off\r\necho RESOLVED:%~2\r\n");

// A second stand-in on a space-free path — the real CI failure happened with no
// spaces at all, so the bug is the quote stripping rather than the spaces.
const plain = join(root, "toolplain.cmd");
writeFileSync(plain, "@echo off\r\necho RESOLVED:%~2\r\n");

const candidates = {
  // A — the shipped form. CONTROL: expected to fail.
  A: (exe) => `for /f "usebackq delims=" %%i in (\`"${exe}" boot "%TARGET%"\`) do set "OUT=%%i"`,
  // B — wrap the whole command in one more quote pair, so cmd /c's strip leaves
  // exactly the intended string.
  B: (exe) => `for /f "usebackq delims=" %%i in (\`""${exe}" boot "%TARGET%""\`) do set "OUT=%%i"`,
  // C — sidestep cmd /c entirely: run the tool directly, redirect to a file and
  // read it back. Needs a unique temp name because six builds can race.
  C: (exe) =>
    [
      `set "TMPOUT=%TEMP%\\nubgyp-%RANDOM%%RANDOM%.txt"`,
      `"${exe}" boot "%TARGET%" > "%TMPOUT%"`,
      `set /p OUT=<"%TMPOUT%"`,
      `del "%TMPOUT%" 2>nul`,
    ].join("\r\n"),
};

let failures = 0;
const winners = new Set();
for (const [name, build] of Object.entries(candidates)) {
  for (const [label, exe] of [["spaced", tool], ["plain", plain]]) {
    const script = join(root, `cand-${name}-${label}.cmd`);
    writeFileSync(
      script,
      ["@echo off", "setlocal", 'if not defined TARGET set "TARGET=%CD%"', build(exe), 'if not defined OUT exit /b 1', "echo GOT:%OUT%"].join("\r\n") + "\r\n",
    );
    const r = spawnSync("cmd.exe", ["/c", script], { encoding: "utf8", timeout: 30_000, cwd: root });
    const out = `${r.stdout}${r.stderr}`.trim();
    const ok = r.status === 0 && /GOT:RESOLVED:/.test(out);
    const expected = name === "A" ? "(control — expected FAIL)" : "";
    console.log(`${ok ? "OK  " : "BAD "} ${name}/${label} status=${r.status} ${expected}\n     ${out.replace(/\r?\n/g, " | ")}`);
    if (name === "A") {
      if (ok) {
        console.log("     !! control PASSED — the reproduction is wrong, ignore every other row");
        failures++;
      }
    } else if (ok) {
      winners.add(name);
    }
  }
}

// A losing ALTERNATIVE is a result, not a failure — counting it as one would
// leave this job permanently red over candidate C and teach readers to skip it.
// Only two things are real failures: the control passing (the reproduction is
// invalid, so no row means anything), or no alternative working at all (there
// is nothing to build the shim out of).
console.log(`\nworking candidates: ${winners.size ? [...winners].join(", ") : "NONE"}`);
if (!winners.size) {
  console.log("!! no candidate survived cmd.exe — the shim cannot be fixed by requoting alone");
  failures++;
}
process.exit(failures === 0 ? 0 : 1);
