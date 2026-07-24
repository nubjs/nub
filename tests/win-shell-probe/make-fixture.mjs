// Builds the probe fixture: a realistic `node_modules/.bin` populated with the
// SAME shim triplet npm generates on Windows (`<tool>.cmd`, `<tool>` sh script,
// `<tool>.ps1`), plus two deliberately-asymmetric tools that isolate what a
// candidate shell can actually resolve:
//
//   mytool   — all three shim forms exist (what a real `npm install` leaves)
//   onlycmd  — ONLY `onlycmd.cmd` exists; no extensionless twin. This is the
//              headline test: resolving it REQUIRES PATHEXT-style extension
//              probing, which a naive POSIX `execvp` port does not do.
//   onlysh   — ONLY the extensionless `#!/bin/sh` script exists; resolving it
//              requires shebang interpretation, which cmd.exe cannot do.
//
// Shim bodies are copied from npm 10's generated output so the probe measures
// real-world shims, not a simplification.
import { mkdirSync, writeFileSync, rmSync, chmodSync } from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(process.argv[2] ?? "fixture");
rmSync(root, { recursive: true, force: true });

const bin = join(root, "node_modules", ".bin");
mkdirSync(bin, { recursive: true });

// One package dir per tool, mirroring npm's layout so the shims' `..` walk is real.
// `which` is deliberate: the npm package `which` really does ship a `which`
// binary, AND busybox has a built-in `which` applet. busybox prefers its own
// applets over anything on PATH, so this pair is the applet-shadowing test —
// a real `.bin` tool a busybox-backed `nub run` could silently hijack.
for (const tool of ["mytool", "onlycmd", "onlysh", "which"]) {
  const pkgBin = join(root, "node_modules", tool, "bin");
  mkdirSync(pkgBin, { recursive: true });
  writeFileSync(
    join(pkgBin, `${tool}.js`),
    `console.log("TOOL_OK name=${tool} argv=" + JSON.stringify(process.argv.slice(2)));\n`,
  );
}

const cmdShim = (tool) => `@ECHO off\r
GOTO start\r
:find_dp0\r
SET dp0=%~dp0\r
EXIT /b\r
:start\r
SETLOCAL\r
CALL :find_dp0\r
\r
IF EXIST "%dp0%\\node.exe" (\r
  SET "_prog=%dp0%\\node.exe"\r
) ELSE (\r
  SET "_prog=node"\r
  SET PATHEXT=%PATHEXT:;.JS;=;%\r
)\r
\r
endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\\..\\${tool}\\bin\\${tool}.js" %*\r
`;

const shShim = (tool) => `#!/bin/sh
basedir=$(dirname "$(echo "$0" | sed -e 's,\\\\,/,g')")

case \`uname\` in
    *CYGWIN*|*MINGW*|*MSYS*)
        if command -v cygpath > /dev/null 2>&1; then
            basedir=\`cygpath -w "$basedir"\`
        fi
    ;;
esac

if [ -x "$basedir/node" ]; then
  exec "$basedir/node"  "$basedir/../${tool}/bin/${tool}.js" "$@"
else
  exec node  "$basedir/../${tool}/bin/${tool}.js" "$@"
fi
`;

const ps1Shim = (tool) => `#!/usr/bin/env pwsh
$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent

$exe=""
if ($PSVersionTable.PSVersion -lt "6.0" -or $IsWindows) {
  $exe=".exe"
}
$ret=0
if (Test-Path "$basedir/node$exe") {
  & "$basedir/node$exe"  "$basedir/../${tool}/bin/${tool}.js" $args
  $ret=$LASTEXITCODE
} else {
  & "node$exe"  "$basedir/../${tool}/bin/${tool}.js" $args
  $ret=$LASTEXITCODE
}
exit $ret
`;

function writeShim(path, body, exec) {
  writeFileSync(path, body);
  if (exec) {
    try {
      chmodSync(path, 0o755);
    } catch {
      /* no-op on filesystems without the bit */
    }
  }
}

// mytool: the full npm triplet.
writeShim(join(bin, "mytool.cmd"), cmdShim("mytool"));
writeShim(join(bin, "mytool"), shShim("mytool"), true);
writeShim(join(bin, "mytool.ps1"), ps1Shim("mytool"));

// onlycmd: .cmd ONLY — extension probing required.
writeShim(join(bin, "onlycmd.cmd"), cmdShim("onlycmd"));

// onlysh: extensionless sh script ONLY — shebang interpretation required.
writeShim(join(bin, "onlysh"), shShim("onlysh"), true);

// which: collides with busybox's built-in `which` applet (see above).
writeShim(join(bin, "which.cmd"), cmdShim("which"));
writeShim(join(bin, "which"), shShim("which"), true);
writeShim(join(bin, "which.ps1"), ps1Shim("which"));

// Input files the POSIX-ism cases read.
writeFileSync(join(root, "a.txt"), "AAA\n");
writeFileSync(join(root, "dot.sh"), "echo dotted\n");
// A real script file (not `node -e`) so `--out=…`-shaped arguments land in
// process.argv rather than being swallowed by node's own option parser.
writeFileSync(join(root, "printargv.js"), 'console.log("ARGV=" + JSON.stringify(process.argv.slice(2)));\n');

process.stdout.write(`fixture built at ${root}\n`);
