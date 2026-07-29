// Can a dependency lifecycle script RUN under nub's build jail on Windows? — an
// end-to-end probe.
//
// WHY IT EXISTS. aube builds the `cmd.exe` line with `CommandExt::raw_arg` on purpose:
// cmd.exe does not implement the `CommandLineToArgvW` rules Rust's argv encoder targets,
// so a script carrying interior double quotes (`node -e "require('is-odd')(3)"`) arrives
// mangled if anything re-encodes it. A JAILED spawn never goes through `spawn_shell` —
// nub rebuilds it as a `CommandSpec` and the Windows launcher runs it through its own
// `CreateProcessW` encoder — and that second encoding turned the raw tail into a first
// token of `\""`. Every dependency lifecycle script under the jail died there, before any
// of the header/toolchain work downstream of it could even be measured.
//
// SO THE VERDICT IS A MARKER FILE THE CHILD WROTE, never an exit code: a lifecycle script
// can exit 0 having done nothing, and this repo has measured exactly that.
//
// THREE FIXTURES, one variable each:
//   jailed-quoted    — confined, `node -e "require('…')('…')"`. THE headline: the shape
//                      aube's own doc comment names as the one that mangles.
//   jailed-plain     — confined, a script with NO quoting at all. It fails too when the
//                      bug is present, because the corruption is in the `/d /s /c "`
//                      PREFIX rather than in the script — which is why an unquoted script
//                      cannot serve as the both-arms control.
//   unjailed-quoted  — the identical quoted script with `dependenciesMeta.<pkg>.sandbox:
//                      false`, so aube spawns it through its ORDINARY unconfined path.
//                      THIS is the both-arms control: it exercises the whole harness —
//                      pack, install, approve, script, marker — while toggling exactly the
//                      one thing under test, so the probe cannot go green by being
//                      permissive and a red jailed verdict cannot be a broken harness.
//
// Every path the child touches is BAKED IN AS AN ABSOLUTE LITERAL. Measured on a sibling
// lane: a canary path passed through the environment never reaches the confined child (the
// jail replaces the env axis with a constructed lifecycle env), so `readFileSync(undefined)`
// threw `ERR_INVALID_ARG_TYPE` and a naive "did it throw?" read that as a refusal — a
// control that confirms enforcement without testing it. The marker records the error CODE
// and the assertion demands the code a real denial produces.
//
// Usage: node probe.mjs <path-to-nub-binary>

import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

// ABSOLUTE, always. The install runs with `cwd` set to the consumer, and Node resolves a
// relative command against that NEW cwd — so a relative `target/fast/nub` silently fails
// to spawn, surfacing as `status: null` and an empty log rather than as an error.
const nub = process.argv[2] ? resolve(process.argv[2]) : null;
if (!nub || !existsSync(nub)) {
  console.error(`usage: node probe.mjs <path-to-nub-binary> (got ${nub})`);
  process.exit(2);
}

// NOT under the system temp dir: the jail grants a private tmp, so a canary placed there
// would be legitimately readable and the enforcement control would fail for a reason that
// has nothing to do with enforcement. The pid also keys every store entry (a `file:` dep
// hashes its path and content), which is what forces a genuine re-run rather than a
// previously-built copy the store would otherwise serve — `rm -rf node_modules` alone does
// NOT invalidate it.
const work = join(homedir(), `.nub-cmdline-probe-${process.pid}`);

// The scripts below embed absolute paths WITHOUT quoting them, deliberately: a quoted path
// would make every fixture a quoting test and destroy the plain/quoted distinction the
// probe rests on. A space in the work dir would break that silently, so refuse up front.
if (/\s/.test(work)) {
  console.error(`work dir contains whitespace, which would confound the plain fixture: ${work}`);
  process.exit(2);
}

rmSync(work, { recursive: true, force: true });
mkdirSync(work, { recursive: true });

const canary = join(work, "canary-secret.txt");
writeFileSync(canary, "the-confined-script-must-not-read-this\n");

const posix = (p) => p.replace(/\\/g, "/");

// Written by the SHELL itself, so its presence means the command line parsed.
const SHELL_MARKER = "cmdline-ok";
// Echoed rather than written — see the diagnostic note where the script is built.
const SHELL_ECHO = "cmdline-reached-the-shell";

/** What the child writes into its own package dir — the one place the jail grants write. */
const jailprobeSource = (variant) => `// Runs as the dependency's \`install\` script. Writes every observation to a marker file
// in the package dir; nothing is reported back through an exit code or a status bit.
const fs = require("fs");
const path = require("path");

const CANARY = ${JSON.stringify(canary)};

function record(tag) {
  let canaryRead;
  try {
    canaryRead = "allowed:" + fs.readFileSync(CANARY, "utf8").trim().length;
  } catch (err) {
    canaryRead = "refused:" + ((err && err.code) || "unknown");
  }
  fs.writeFileSync(
    path.join(__dirname, "marker.json"),
    JSON.stringify(
      {
        variant: ${JSON.stringify(variant)},
        tag,
        cwd: process.cwd(),
        canaryRead,
        // The raw script body as the child's own env reports it, so the log carries what
        // cmd.exe was actually asked to run rather than what the fixture intended.
        lifecycleScript: process.env.npm_lifecycle_script || "",
      },
      null,
      2,
    ),
  );
}

module.exports = record;

// The plain fixture invokes this file directly, with no quoting anywhere on the line.
if (require.main === module) record(process.argv[2] || "direct");
`;

/**
 * Build one fixture package, pack it, and install it into its own consumer.
 *
 * Packed PER VARIANT rather than once: each embeds the absolute path of its own installed
 * copy, so the three tarballs differ in content as well as in path and cannot collide on a
 * store key.
 */
function buildVariant({ variant, pkg, jailed, quoted }) {
  const src = join(work, `src-${variant}`);
  const consumer = join(work, `consumer-${variant}`);
  const installed = join(consumer, "node_modules", pkg);
  mkdirSync(src, { recursive: true });
  mkdirSync(consumer, { recursive: true });

  // The path the SCRIPT names is the installed copy's, not the source's — resolved here so
  // the script never depends on its own cwd being right. That keeps the cwd observation in
  // the marker independent evidence rather than a precondition of the whole fixture.
  const entry = posix(join(installed, "jailprobe.cjs"));

  // THE SHAPE UNDER TEST. `node -e "require('…')('…')"` is verbatim the example aube's own
  // `spawn_shell_with_settings` doc comment names as the case that arrives mangled: an
  // interior double-quoted `-e` argument wrapping single-quoted strings.
  //
  // It is PREFIXED with a shell-builtin redirect, which is the property that isolates the
  // hop this branch fixes. Writing that marker needs only that cmd.exe (or sh) PARSED the
  // command line and began executing it — no Node, no interpreter grant, no toolchain. So
  // it stays a valid verdict on the command line even while something further downstream
  // stops the script's real body, which is exactly the state Windows is in.
  const shellMarkerName = "cmdline-marker.txt";
  const body = quoted
    ? `node -e "require('${entry}')('${variant}')"`
    : `node ${entry} ${variant}`;
  // The bare `echo` before the redirect is a DIAGNOSTIC, not the verdict: it needs no write
  // grant, so if the file marker goes missing it tells apart "cmd.exe never parsed the line"
  // from "it parsed but could not write into the package dir". Its own token is weaker
  // evidence — the string also occurs in the script text — which is why the gated property
  // stays the file.
  const script = `echo ${SHELL_ECHO} && echo ${SHELL_MARKER}> ${shellMarkerName} && ${body}`;

  writeFileSync(join(src, "jailprobe.cjs"), jailprobeSource(variant));
  writeFileSync(
    join(src, "package.json"),
    JSON.stringify(
      {
        name: pkg,
        version: "1.0.0",
        scripts: { install: script },
        files: ["jailprobe.cjs"],
      },
      null,
      2,
    ),
  );

  const packed = execFileSync("npm", ["pack", src, "--pack-destination", work, "--silent"], {
    encoding: "utf8",
    shell: process.platform === "win32",
  })
    .trim()
    .split(/\r?\n/)
    .pop();

  writeFileSync(
    join(consumer, "package.json"),
    JSON.stringify(
      {
        name: `nub-cmdline-probe-consumer-${variant}`,
        version: "1.0.0",
        private: true,
        dependencies: { [pkg]: `file:${posix(join(work, packed))}` },
        // The ONE toggle that separates the control from the subjects. Read only from the
        // CONSUMER's own manifest, which is why it is written here and not in the fixture.
        ...(jailed ? {} : { dependenciesMeta: { [pkg]: { sandbox: false } } }),
      },
      null,
      2,
    ),
  );

  return { variant, pkg, jailed, quoted, script, consumer, installed, shellMarkerName };
}

const results = [];
const record = (name, ok, detail) => {
  results.push({ name, ok });
  console.log(`prop:${name}=${ok ? "PASS" : "FAIL"} ${detail}`);
};

const variants = [
  { variant: "jailed-quoted", pkg: "nub-cmdline-probe-jq", jailed: true, quoted: true },
  { variant: "jailed-plain", pkg: "nub-cmdline-probe-jp", jailed: true, quoted: false },
  { variant: "unjailed-quoted", pkg: "nub-cmdline-probe-uq", jailed: false, quoted: true },
];

for (const spec of variants) {
  const f = buildVariant(spec);
  console.log(`\n======== ${f.variant} (jailed=${f.jailed} quoted=${f.quoted}) ========`);
  console.log(`script: ${f.script}`);

  const run = (label, args) => {
    const r = spawnSync(nub, args, { cwd: f.consumer, encoding: "utf8" });
    const out = `${r.stdout ?? ""}${r.stderr ?? ""}`;
    console.log(
      `${f.variant}/${label} rc=${r.status} signal=${r.signal ?? "none"} error=${r.error ? r.error.message : "none"}`,
    );
    console.log(`---- ${label} stdout ----\n${r.stdout ?? ""}`);
    console.log(`---- ${label} stderr ----\n${r.stderr ?? ""}`);
    return { rc: r.status, out };
  };

  // The default-trust floor is a curated allowlist and a probe fixture is on no curated
  // list, so the first install DECLINES to run the build script and names it. Approving is
  // a second, explicit step by design; `--all` is itself the consent, so it needs no TTY.
  const first = run("install", ["install", "--no-frozen-lockfile"]);
  const approve = run("approve-builds", ["approve-builds", "--all"]);
  const combined = `${first.out}\n${approve.out}`;

  // "THE EXPERIMENT ACTUALLY RAN." A CI run on this epic once reported success with
  // rc=127 on every arm — nothing had executed. The dependency being materialized on disk
  // is the cheapest fact that cannot be true unless nub really ran an install, and it must
  // hold in EVERY arm of EVERY variant regardless of what the lifecycle script did.
  record(
    `${f.variant}/dependency-materialized`,
    existsSync(join(f.installed, "package.json")),
    `installed=${f.installed} install-rc=${first.rc} approve-rc=${approve.rc}`,
  );

  // THE PROPERTY THIS BRANCH IS ABOUT. cmd.exe reached the first command on the line, so
  // the command line it was handed was well-formed. Independent of everything the script
  // then goes on to need.
  const shellMarker = join(f.installed, f.shellMarkerName);
  const shellMarkerText = existsSync(shellMarker) ? readFileSync(shellMarker, "utf8").trim() : "";
  record(
    `${f.variant}/shell-ran-the-line`,
    shellMarkerText === SHELL_MARKER,
    `path=${shellMarker} content=${JSON.stringify(shellMarkerText)} want=${JSON.stringify(SHELL_MARKER)}`,
  );
  record(
    `${f.variant}/shell-echoed`,
    combined.includes(SHELL_ECHO),
    `diagnostic only — distinguishes an unparsed command line from an ungranted write`,
  );

  const markerPath = join(f.installed, "marker.json");
  let marker = null;
  if (existsSync(markerPath)) {
    try {
      marker = JSON.parse(readFileSync(markerPath, "utf8"));
    } catch (err) {
      console.log(`marker unparseable: ${err.message}`);
    }
  }
  record(`${f.variant}/script-ran`, marker !== null, `marker=${markerPath}`);
  record(
    `${f.variant}/tag-round-trip`,
    marker?.tag === f.variant,
    `tag=${JSON.stringify(marker?.tag ?? null)} want=${JSON.stringify(f.variant)}`,
  );

  // Ran CONFINED (or provably not). A jailed script must be REFUSED the canary with a real
  // denial code; the unjailed control must be ALLOWED it. Demanding the specific code in
  // each direction is what stops an incidental failure from reading as enforcement.
  const canaryRead = marker?.canaryRead ?? "<no marker>";
  const denied = canaryRead === "refused:EPERM" || canaryRead === "refused:EACCES";
  const allowed = canaryRead.startsWith("allowed:");
  record(
    `${f.variant}/confinement-as-configured`,
    f.jailed ? denied : allowed,
    `canaryRead=${canaryRead} (jailed=${f.jailed} wants ${f.jailed ? "refused:EPERM|EACCES" : "allowed:<n>"})`,
  );

  // `cwd` is threaded through nub's own `strip_verbatim_prefix`: `std::fs::canonicalize`
  // hands back an extended-length `\\?\C:\…` path, cmd.exe refuses one and silently runs in
  // the Windows directory instead. That fix had no behavioural coverage because this
  // blocker stopped every jailed script before it could report anything.
  //
  // Compared against the REALPATH: `node_modules/<pkg>` is a link into the store, and the
  // package dir the child actually runs in is the store copy behind it.
  const realInstalled = existsSync(f.installed) ? realpathSync(f.installed) : f.installed;
  const cwdOk =
    typeof marker?.cwd === "string" &&
    posix(realpathSync(marker.cwd)).toLowerCase() === posix(realInstalled).toLowerCase();
  record(
    `${f.variant}/cwd-is-package-dir`,
    cwdOk,
    `cwd=${JSON.stringify(marker?.cwd ?? null)} want=${JSON.stringify(realInstalled)}`,
  );

  // The failure SIGNATURE, asserted by literal rather than inferred from a non-zero exit —
  // a script can fail for a hundred reasons and only this one is the bug under test.
  // cmd.exe's two ways of saying "the line I was handed does not parse": it either took a
  // mangled fragment for a command name, or took one for a path. Matching both rather than
  // one literal token, because the exact fragment depends on where the re-quoting landed —
  // while either message means precisely this defect and nothing else.
  const mangleSignatures = [
    "is not recognized as an internal or external command",
    "The system cannot find the path specified",
  ];
  record(
    `${f.variant}/mangled-signature`,
    mangleSignatures.some((sig) => combined.includes(sig)),
    `cmd.exe could not parse the command line; expected ABSENT with the fix, PRESENT without it`,
  );

  console.log(`observed ${f.variant} npm_lifecycle_script=${JSON.stringify(marker?.lifecycleScript ?? null)}`);
}

rmSync(work, { recursive: true, force: true });

const failed = results.filter((r) => !r.ok).map((r) => r.name);
console.log(`\nprobe complete: ${results.length - failed.length}/${results.length} properties recorded PASS`);
// The exit code reports the PROBE's own tally; the workflow decides what each arm expects,
// because several properties are expected to read FAIL in the poisoned arm.
process.exit(failed.length === 0 ? 0 : 1);
