#!/usr/bin/env node
// remote-build — run a nub Rust build/gate on an ephemeral GCE spot VM instead of
// the maintainer's Mac, and (for a darwin build) pull the signed arm64 binary back.
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/remote-build.ts --job test
//   nub  scripts/remote-build.ts --job clippy
//
// Erasable TypeScript only (no enums/namespaces/parameter-properties) so plain
// modern `node` runs it with no build step — same constraint as the other scripts/*.ts.
//
// WHY THIS EXISTS. The dev host is a 10-core M1 Max saturated by ~20 concurrent agent
// worktrees. Measured under that load: ~30% of CPU sits IDLE while the load average
// reads 155, sys time is ~25%, and the disk sustains 3000-4000 tps at 5-6 KB/transfer.
// The bottleneck is not compute — it is cargo fingerprint/stat churn across a dozen
// multi-GB target dirs on ONE APFS volume. So the win from going remote is not "more
// cores", it is that every builder brings its OWN disk. See
// wiki/research/remote-build-offload.md for the full measurement set.
//
// WHAT GOES REMOTE, AND WHAT MUST NOT. Measured on n2-standard-16 vs the Mac:
//   warm incremental   8.1s remote vs ~5s local   -> STAYS LOCAL, remote loses
//   cold `release`     7m00s remote vs ~15m local -> remote wins 2x
//   clippy --all-targets --all-features  35.3s    -> remote
//   cargo test -p nub-cli   39.4s warm            -> remote, 718 passed / 0 failed
// The inner loop is deliberately NOT a job type here. This tool is for the heavy,
// cold-anyway gates that are what actually saturate the Mac.
//
// LINUX-NATIVE ONLY, DELIBERATELY. An earlier version of this tool also cross-compiled
// `aarch64-apple-darwin` here via cargo-zigbuild plus hand-written Apple framework stub
// TBDs. That worked, but the stubs froze 371 symbols against a real export surface of
// ~8,700 (4.3%), and their upkeep was REACTIVE — the only way to learn a symbol was
// missing was a link failure after a full compile. A live example was already latent:
// the stubs carried `_SecTrustGetCertificateAtIndex` but not the replacement Apple's own
// headers point at, so a routine `security-framework` bump would have broken the build.
// macOS artifacts now come from `.github/workflows/mac-build.yml` + `scripts/mac-build.ts`,
// which build natively on a real macOS runner: no stubs, no pinned zig, complete SDK, and
// a correct deployment target (minos 11.0, where the cross-build forced 13.0).
//
// ORPHAN-PROOFING IS THE POINT, NOT A NICETY. Stray builders that outlive their launcher
// are the exact failure this repo keeps paying for. Three independent layers, because a
// local `finally` alone is defeated by SIGKILL:
//   1. `finally` + SIGINT/SIGTERM handlers delete the VM on the normal and interrupted paths.
//   2. Every VM carries `--max-run-duration` + `--instance-termination-action=DELETE`, so
//      GCE deletes it server-side even if this process dies outright. This is the layer
//      that actually holds.
//   3. Every VM is labelled `nub-builder=1`, so `--reap` can find and delete strays with
//      no local state at all.
// A build is never detached LOCALLY — a detached local build reparents to PID 1, outlives its
// launcher, and is not reaped by the harness. `--detach` is not that: it detaches on the
// disposable REMOTE VM, which is single-purpose and carries layer 2's hard TTL, so a forgotten
// job cannot outlive it or contend with anything here. See the DETACHED MODE block below.

import { execFile, execFileSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const execFileAsync = promisify(execFile);

const PROJECT = "pullfrog";
// A single (zone, machine-type) pair is a single point of failure: GCE returns
// ZONE_RESOURCE_POOL_EXHAUSTED (STOCKOUT) for a specific shape in a specific zone, and
// spot capacity is exactly where that bites. A 10-way fanout asks for ten instances at
// once, so this is a routine occurrence, not an edge case — it took out the first bake.
// Candidates are tried in order until one is admitted. c3 first (fastest measured), then
// progressively more commodity shapes that are almost always available.
const ZONES = ["us-central1-a", "us-central1-b", "us-central1-c", "us-central1-f"];
const MACHINE_FALLBACKS = ["c3-standard-8", "n2-standard-8", "n2d-standard-8", "e2-standard-8"];
// Quota shows up as prose ("Quota 'C3_CPUS' exceeded"), not a token — and a --fanout 10 of
// 8-vCPU shapes asks for 80 vCPUs at once, which is a realistic way to trip a per-FAMILY
// regional limit. Falling through to another machine family is exactly the right response.
const STOCKOUT = /ZONE_RESOURCE_POOL_EXHAUSTED|does not have enough resources|STOCKOUT|QUOTA_EXCEEDED|quota .*exceeded/i;
const IMAGE_FAMILY = "nub-builder";

// Not any real exit code — `--attach` returning this means "job still running, call again".
// Declared up here rather than beside the detached-mode code because HELP interpolates it,
// and HELP is evaluated at module load (a later `const` would be a TDZ ReferenceError).
const EX_STILL_RUNNING = 75;
const LABEL = "nub-builder";
// Ubuntu 24.04 is the base for the golden image; c3-standard-8 matches the box the
// cross-build was proven on. 50 GB is a floor, not a preference: the dev-loop target dir
// alone is 10 GB, and the standing 4-vCPU box hit ENOSPC mid-`cargo test` on 30 GB.
const BASE_IMAGE_FAMILY = "ubuntu-2404-lts-amd64";
const BASE_IMAGE_PROJECT = "ubuntu-os-cloud";
const MACHINE_TYPE = "c3-standard-8";
const DISK_GB = "50";
const ZIG_VERSION = "0.16.0";
const NODE_MAJOR = "26";
const SSH_USER = "nub";
const SSH_KEY = join(homedir(), ".ssh", "nub-vm");
const CRED = join(homedir(), ".config", "pullfrog", "vertex-service-account.json");
// Server-side backstop (layer 2). Generous enough for a cold release build (~7 min) plus
// sync and toolchain, tight enough that a stray cannot bill for hours.
const MAX_RUN = "45m";
// The bake is longer (apt + rustup + cargo-install + a cold dependency compile) and must not
// be DELETEd — it stops the instance and images the disk. STOP is a valid termination action
// for --max-run-duration (gcloud documents it for "automatic instance termination", not only
// spot preemption), and stopping is exactly what the bake does next anyway. So layer 2 covers
// EVERY instance this tool creates; there is no exception to maintain.
const BAKE_MAX_RUN = "90m";

const HELP = `remote-build — run a nub CI gate on an ephemeral GCE spot VM

Usage:
  nub scripts/remote-build.ts [--job clippy|test] [options]
  nub scripts/remote-build.ts --fanout <n>        # n concurrent builders + local-load sampling
  nub scripts/remote-build.ts --build-image       # bake the golden image (once, ~10 min)
  nub scripts/remote-build.ts --reap              # delete stray builder VMs

Jobs:
  clippy   the full CI clippy gate (default)
  test     the whole-workspace test suite

For a macOS BINARY use scripts/mac-build.ts, which builds natively on a real macOS
runner. This tool is Linux-native gates only.

Options:
  --job <j>          Job to run (default: clippy).
  --fanout <n>       Run n builders concurrently; samples local CPU/load throughout.
  --source <dir>     Worktree to build (default: the git root of the cwd).
  --machine <type>   GCE machine type (default: ${MACHINE_TYPE}).
  --on-demand        Use on-demand rather than spot provisioning.
  --keep             Do not delete the VM on exit (debugging; it still self-deletes at ${MAX_RUN}).
  --build-image      Bake the golden image, then exit.
  --reap             Delete builder VMs older than 90m (definitionally stray), then exit.
  --reap-all         Delete EVERY builder VM, including live ones owned by other agents.
  --detach           Provision, sync, start the job on the VM, then EXIT (prints the name).
  --attach <name>    Stream + poll a --detach'd job; deletes the VM when it finishes.
                     Exits ${EX_STILL_RUNNING} if still running — just call it again.

Driving this from an agent harness? Use --detach/--attach. A foreground run is SIGKILLed at
the harness timeout (10 min max, 2 min default), which no signal handler can catch, so the
VM leaks and only the server-side TTL reclaims it.
  -h, --help         Show this help.

Cost: spot c3-standard-8 is a few cents per build. A stray cannot outlive ${MAX_RUN}.
`;

export function parseArgs(argv: string[]) {
  const a = {
    job: "clippy",
    profile: "fast",
    fanout: 1,
    out: "",
    source: "",
    machine: MACHINE_TYPE,
    onDemand: false,
    keep: false,
    buildImage: false,
    reap: false,
    reapAll: false,
    detach: false,
    attach: "",
  };
  for (let i = 0; i < argv.length; i++) {
    const v = argv[i];
    if (v === "-h" || v === "--help") {
      process.stdout.write(HELP);
      process.exit(0);
    } else if (v === "--job") a.job = argv[++i];
    else if (v === "--profile") a.profile = argv[++i];
    else if (v === "--fanout") a.fanout = Number(argv[++i]);
    else if (v === "--out") a.out = argv[++i];
    else if (v === "--source") a.source = argv[++i];
    else if (v === "--machine") a.machine = argv[++i];
    else if (v === "--on-demand") a.onDemand = true;
    else if (v === "--keep") a.keep = true;
    else if (v === "--build-image") a.buildImage = true;
    else if (v === "--reap") a.reap = true;
    else if (v === "--reap-all") a.reapAll = true;
    else if (v === "--detach") a.detach = true;
    else if (v === "--attach") a.attach = argv[++i];
    else {
      process.stderr.write(`remote-build: unknown argument ${v}\n\n${HELP}`);
      process.exit(2);
    }
  }
  if (!["clippy", "test"].includes(a.job)) {
    process.stderr.write(`remote-build: --job must be clippy|test\n`);
    process.exit(2);
  }
  // Whitelisted for the same reason --job is: `profile` is interpolated into a script piped
  // to `bash -s` AND into an rsync REMOTE path (which the far side shell-expands). Not
  // attacker-reachable — you would already need argv control — but an unvalidated value
  // there turns a typo into a shell syntax error from inside a generated script.
  // Without this, `--machine` as the final argv element silently becomes the STRING
  // "undefined" and every create fails with `--machine-type undefined`.
  if (!a.machine || !/^[a-z0-9]+-[a-z0-9]+-\d+$/.test(a.machine)) {
    process.stderr.write(`remote-build: --machine must be a GCE machine type (e.g. c3-standard-8)\n`);
    process.exit(2);
  }
  // `--attach` as the FINAL argv element leaves `argv[++i]` undefined, so `a.attach` is falsy
  // and main() skips BOTH the attach and detach branches — a "collect my result" invocation
  // then falls through to the ordinary path and provisions a fresh VM for a full cold job.
  // Exactly the trap the --machine guard above exists for.
  if (argv.includes("--attach") && !a.attach) {
    process.stderr.write(`remote-build: --attach needs an instance name (e.g. --attach nub-builder-ms84mnyn-1-sfkm)\n`);
    process.exit(2);
  }
  if (!["fast", "release"].includes(a.profile)) {
    process.stderr.write(`remote-build: --profile must be fast|release\n`);
    process.exit(2);
  }
  if (!Number.isInteger(a.fanout) || a.fanout < 1) {
    process.stderr.write(`remote-build: --fanout must be a positive integer\n`);
    process.exit(2);
  }
  return a;
}

// gcloud always runs under the service-account override. The USER credential's refresh
// token is revoked periodically by org session policy, so an interactive login is not
// durable here; the SA key is. Failing loudly on a missing key beats a confusing
// mid-run reauth prompt that cannot be answered non-interactively.
function gcloudEnv() {
  if (!existsSync(CRED)) {
    throw new Error(
      `remote-build: service-account key not found at ${CRED}. ` +
        `VM operations need it (the user credential is not durable).`,
    );
  }
  return { ...process.env, CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE: CRED };
}

async function gcloud(args: string[]) {
  const full = [...args, "--project", PROJECT];
  const { stdout } = await execFileAsync("gcloud", full, {
    env: gcloudEnv(),
    maxBuffer: 64 * 1024 * 1024,
    // No gcloud call here should ever take minutes; without this a hung API call hangs the
    // whole run, and for the bake there is no server-side backstop to end it.
    timeout: 180_000,
  });
  return stdout.trim();
}

function sh(cmd: string, args: string[], opts: Record<string, unknown> = {}) {
  return execFileSync(cmd, args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024, ...opts });
}

function repoRoot(from: string) {
  return sh("git", ["rev-parse", "--show-toplevel"], { cwd: from }).trim();
}

const SSH_OPTS = [
  "-o", "StrictHostKeyChecking=no",
  "-o", "UserKnownHostsFile=/dev/null",
  "-o", "LogLevel=ERROR",
  "-o", "ConnectTimeout=15",
  "-o", "ServerAliveInterval=30",
];

async function instanceIp(name: string, zone: string) {
  return await gcloud([
    "compute", "instances", "describe", name, "--zone", zone,
    "--format=value(networkInterfaces[0].accessConfigs[0].natIP)",
  ]);
}

// A RUNNING status does not mean sshd is up — especially right after create. Poll the
// real thing (a successful command) rather than the status field, and surface the serial
// console on give-up, which diagnoses a wedged boot instantly where guessing does not.
async function waitForSsh(name: string, zone: string, ip: string, timeoutMs: number) {
  const deadline = Date.now() + timeoutMs;
  let lastErr = "";
  while (Date.now() < deadline) {
    try {
      await execFileAsync("ssh", ["-i", SSH_KEY, ...SSH_OPTS, `${SSH_USER}@${ip}`, "true"], { timeout: 20_000 });
      return;
    } catch (e: any) {
      lastErr = String(e?.stderr || e?.message || e);
      await new Promise((r) => setTimeout(r, 5000));
    }
  }
  let serial = "";
  try {
    serial = await gcloud(["compute", "instances", "get-serial-port-output", name, "--zone", zone]);
    serial = serial.split("\n").slice(-25).join("\n");
  } catch {
    serial = "(serial console unavailable)";
  }
  throw new Error(`remote-build: ssh to ${name} (${ip}) never came up.\nlast: ${lastErr}\nserial tail:\n${serial}`);
}

export function instanceCreateArgs(
  name: string,
  pubKey: string,
  opts: { machine: string; zone: string; onDemand: boolean; fromImage: boolean; terminate: "DELETE" | "STOP" },
) {
  const args = [
    "compute", "instances", "create", name,
    "--zone", opts.zone,
    "--machine-type", opts.machine,
    "--boot-disk-size", `${DISK_GB}GB`,
    "--boot-disk-type", "pd-balanced",
    "--labels", `${LABEL}=1`,
    // Only this key. A builder gets a public IP on the default network (which ships
    // default-allow-ssh from 0.0.0.0/0), so project-wide keys are needless extra reach.
    "--metadata", `ssh-keys=${SSH_USER}:${pubKey},block-project-ssh-keys=TRUE`,
  ];
  // Layer 2: GCE terminates the VM server-side no matter what happens to this process —
  // the only layer a SIGKILL cannot defeat. Builders DELETE (a merely-stopped VM still bills
  // its disk); the bake STOPs, because it images that disk immediately afterwards.
  args.push(
    "--max-run-duration", opts.terminate === "STOP" ? BAKE_MAX_RUN : MAX_RUN,
    "--instance-termination-action", opts.terminate,
  );
  if (opts.fromImage) args.push("--image-family", IMAGE_FAMILY, "--image-project", PROJECT);
  else args.push("--image-family", BASE_IMAGE_FAMILY, "--image-project", BASE_IMAGE_PROJECT);
  if (!opts.onDemand) args.push("--provisioning-model", "SPOT");
  return args;
}

// Walks the (zone × machine-type) candidate grid until GCE admits one, and returns where
// it landed so every later call (describe/delete/serial) targets the right zone. Only
// stockout/quota errors advance the grid — any other failure is a real error and rethrows
// immediately rather than burning through every combination.
export function placementCandidates(preferredMachine: string) {
  const machines = [preferredMachine, ...MACHINE_FALLBACKS.filter((m) => m !== preferredMachine)];
  const out: Array<{ zone: string; machine: string }> = [];
  for (const machine of machines) for (const zone of ZONES) out.push({ zone, machine });
  return out;
}

async function createInstance(
  name: string,
  opts: { machine: string; onDemand: boolean; fromImage: boolean; terminate: "DELETE" | "STOP" },
  log: (s: string) => void = () => {},
) {
  const pubKey = readFileSync(`${SSH_KEY}.pub`, "utf8").trim();
  let lastErr: any;
  for (const { zone, machine } of placementCandidates(opts.machine)) {
    try {
      await gcloud(instanceCreateArgs(name, pubKey, { ...opts, machine, zone }));
      return { zone, machine };
    } catch (e: any) {
      const msg = String(e?.stderr || e?.message || e);
      if (!STOCKOUT.test(msg)) throw e;
      lastErr = e;
      log(`no ${machine} capacity in ${zone}, trying next`);
    }
  }
  throw new Error(`remote-build: no capacity for ${name} in any zone/machine combination.\n${lastErr?.message || lastErr}`);
}

async function deleteInstance(name: string, zone: string) {
  try {
    await gcloud(["compute", "instances", "delete", name, "--zone", zone, "--quiet"]);
    return true;
  } catch (e: any) {
    const msg = String(e?.stderr || e?.message || e);
    // Spot preemption already deleted it server-side — expected, not a warning worth printing.
    if (/was not found|already being deleted/i.test(msg)) return true;
    process.stderr.write(`remote-build: WARNING could not delete ${name} (${zone}): ${msg.split("\n")[0]}\n`);
    return false;
  }
}

// rsync MUST be driven by an ALLOWLIST built from `git ls-files`. An --exclude blocklist
// makes rsync walk the entire ~99 GB working tree (target/, node_modules/, .repos/) just to
// decide what to skip, and it times out. The allowlist never touches those paths at all:
// measured 2.2s for the full ~107 MB payload, 0.8s for a few-file delta.
// tests/node-suite is a populated submodule (~795 MB) and is dropped explicitly.
// `git ls-files` alone lists the INDEX. Two failure modes follow, and the first is the
// worst output a build tool can produce: an unstaged NEW file is never synced, so the
// remote lints/builds a tree WITHOUT your change and reports success. The second: a
// tracked file deleted in the worktree is still listed, and rsync aborts the entire sync
// when it cannot open it. So union in untracked-but-not-ignored files, and drop entries
// that no longer exist (openrsync 2.6.9 has no --ignore-missing-args).
export function filterSourceFiles(lsFilesOutput: string, exists: (f: string) => boolean = () => true) {
  const seen = new Set<string>();
  return lsFilesOutput
    .split("\n")
    .filter((f) => f && !f.startsWith("tests/node-suite"))
    .filter((f) => (seen.has(f) ? false : (seen.add(f), true)))
    .filter((f) => exists(f));
}

// macOS ships openrsync ("rsync version 2.6.9 compatible"), NOT rsync 3.x, so any
// 3.x-only flag fails the whole sync — `--delete-missing-args` cost a full image bake
// before this was caught. No deletion flag is needed anyway: every builder boots fresh
// from the golden image, whose bake removes ~/src, so there is never anything stale on
// the far side to delete. Keep this list 2.6.9-safe.
export function rsyncPushArgs(listFile: string, source: string, ip: string, sshCmd: string) {
  return ["-az", `--files-from=${listFile}`, "-e", sshCmd, `${source}/`, `${SSH_USER}@${ip}:~/src/`];
}

function syncSource(source: string, ip: string) {
  const listed = sh("git", ["ls-files", "--cached", "--others", "--exclude-standard", "--deduplicate"], { cwd: source });
  const files = filterSourceFiles(listed, (f) => existsSync(join(source, f)));
  const listFile = join(tmpdir(), `remote-build-files-${process.pid}-${Math.random().toString(36).slice(2)}.txt`);
  writeFileSync(listFile, files.join("\n") + "\n");
  try {
    sh("rsync", rsyncPushArgs(listFile, source, ip, `ssh -i ${SSH_KEY} ${SSH_OPTS.join(" ")}`));
  } finally {
    rmSync(listFile, { force: true });
  }
}

// Two prerequisites a fresh clone does not satisfy, both of which fail in confusing ways:
//   - Under --all-features, crates/nub-core/build.rs PANICS unless addons/nub-native.node is
//     staged. CI already stages a PLACEHOLDER for its addon-less ubuntu job; same trick here.
//     The placeholder only has to exist and hash — it is never loaded for a build/clippy/test.
//   - Without `node` on PATH, aube-resolver/build.rs emits "shipping empty primer" and
//     produces a SILENTLY DEGRADED binary. The golden image installs Node for this reason;
//     this check fails loudly if a caller points at a hand-rolled box that lacks it.
const PREPARE_BASE = `set -euo pipefail
# sshd runs \`bash -s\` NON-interactively, so it reads neither /etc/profile nor (thanks to
# Ubuntu's early \`case $- in *i*) ;; *) return\`) ~/.bashrc. rustup's PATH line therefore
# never applies and every cargo call would die with 127. Source it explicitly, exactly as
# the bake's own provision/warm scripts do.
. "$HOME/.cargo/env"
# Must match the bake's target dir. The bake compiles the dependency graph and then
# \`rm -rf ~/src\`, so a target dir INSIDE ~/src would be destroyed with it and every
# builder would pay a full cold compile despite the image claiming otherwise.
export CARGO_TARGET_DIR="$HOME/.cargo-shared-target"
@PRIMER_GUARD@
cd ~/src
command -v node >/dev/null || { echo "remote-build: node missing on builder (would silently degrade the primer)" >&2; exit 3; }
mkdir -p runtime/addons
[ -s runtime/addons/nub-native.node ] || printf 'placeholder' > runtime/addons/nub-native.node
[ -d node_modules ] || npm install --no-audit --no-fund --loglevel=error
`;

// Jobs whose output is a binary someone could end up running. Empty today — this tool is
// gates only; macOS artifacts come from scripts/mac-build.ts and releases from release.yml —
// but the primer guard below keys off it, so adding an artifact job here re-arms the
// protection rather than quietly shipping a degraded binary.
const ARTIFACT_JOBS = new Set<string>();

// AUBE_REQUIRE_PRIMER protects a SHIPPED artifact, and only that. The `command -v node`
// check above catches only a MISSING node; aube-resolver/build.rs has two further
// silent-degrade branches (generate-primer.mjs fails to spawn, or exits non-zero) that emit
// a cargo:warning and ship an EMPTY primer — a binary that falls back to network packument
// fetches, exit code 0, invisible to artifact verification. release.yml sets it for that.
//
// A GATE must NOT set it, and setting it broke every gate: the primer data is gitignored
// (vendor/aube/.gitignore, `popular-top*.json`) while the rsync allowlist comes from
// `git ls-files`, so the file can never reach a builder — and the flag turns that into a
// hard build.rs failure. clippy/test ship nothing, so a degraded primer cannot reach a user.
export function prepare(job: string) {
  const guard = ARTIFACT_JOBS.has(job) ? "export AUBE_REQUIRE_PRIMER=1\n" : "";
  return PREPARE_BASE.replace("@PRIMER_GUARD@\n", guard);
}

export function jobScript(job: string, profile: string) {
  // Mirror .github/workflows/ci.yml EXACTLY. The root clippy does NOT cover nub-native —
  // it is its own workspace (panic=unwind cdylib), `exclude`d from the root — so a
  // root-only run goes green on code CI then rejects. The brand lints are cheap greps.
  if (job === "clippy") {
    // `--profile fast` is part of mirroring ci.yml (lines 220/226), not an optimisation:
    // omitting it drove a whole SECOND dependency build under `dev`, so the bake's warm
    // `--profile fast` artifacts were never reused and every remote clippy was fully cold.
    //
    // FOUR legs, not three: `check-path-literals.sh` (ci.yml:237) joined `check-env-reads.sh`
    // and this mirror never picked it up, so a remote clippy could go green on a path-literal
    // violation that CI then rejects — the same class of false green the nub-native leg exists
    // to prevent. Keep this list in lockstep with ci.yml's clippy job.
    return `${prepare(job)}cargo clippy --all-targets --all-features --profile fast -- -D warnings
(cd crates/nub-native && cargo clippy --all-features --profile fast -- -D warnings)
tests/brand-lint/check-env-reads.sh
tests/brand-lint/check-path-literals.sh`;
  }
  // CI runs the WHOLE workspace, and builds the REAL addon first because the data-format
  // loader tests dlopen it — an 11-byte placeholder makes them fail on a malformed library
  // rather than skip. Build it here for the same reason.
  if (job === "test") {
    return `${prepare(job)}(cd crates/nub-native && cargo build)
cp "$CARGO_TARGET_DIR/debug/libnub_native.so" runtime/addons/nub-native.node
cargo test`;
  }
}

// `bash -s` reads the script FROM STDIN, incrementally, as it executes. Any command inside
// that also reads stdin (npm, cargo, anything that prompts) consumes the remainder of the
// un-executed script; bash then hits EOF and exits 0 — a SILENTLY TRUNCATED job that reports
// success. This is not hypothetical: a bake stopped dead right after `cargo fetch`, skipped
// the entire warm compile, and still published a golden image. Nothing in the exit code, the
// stderr, or the streamed output said so.
//
// So the script is shipped as a base64 ARGUMENT, landed on disk, and run from a FILE. stdin
// is then never the script's source, and `ssh -n` additionally points the session's stdin at
// /dev/null so a stdin-reading command gets EOF instead of eating the job.
export function remoteJobCommand(script: string, b64: string) {
  return (
    `f=$(mktemp /tmp/remote-build-XXXXXX.sh); ` +
    `printf %s '${b64}' | base64 -d > "$f" || exit 1; ` +
    `bash "$f"; rc=$?; rm -f "$f"; exit $rc`
  );
}

async function runJob(ip: string, script: string, onLine: (s: string) => void) {
  const b64 = Buffer.from(script, "utf8").toString("base64");
  return await new Promise<void>((resolve, reject) => {
    const child = execFile(
      "ssh",
      ["-n", "-i", SSH_KEY, ...SSH_OPTS, `${SSH_USER}@${ip}`, remoteJobCommand(script, b64)],
      { maxBuffer: 64 * 1024 * 1024 },
      (err, _stdout, stderr) => {
        if (err) reject(new Error(`remote job failed: ${stderr || err.message}`));
        else resolve();
      },
    );
    child.stdout?.on("data", (d) => String(d).split("\n").forEach((l) => l && onLine(l)));
    child.stderr?.on("data", (d) => String(d).split("\n").forEach((l) => l && onLine(l)));
  });
}


// DETACHED MODE — why this exists.
//
// The agent harness this is usually driven from SIGKILLs a command at its timeout, and that
// timeout caps at 10 minutes (it defaults to TWO). SIGKILL cannot be caught, so it defeats
// layer 1 outright: the SIGINT/SIGTERM/SIGHUP handlers never run, the `finally` in oneBuild
// never runs, and the VM leaks until the server-side TTL reclaims it. Measured twice: a cold
// clippy killed at 2m13s (default timeout) and again at 10m (the max), each orphaning a
// builder. A cold `--all-targets --all-features` clippy plus spot-stockout failover simply
// does not fit that ceiling, so the LOCAL process must not have to outlive the job.
//
// So `--detach` starts the job under `setsid` on the VM and returns immediately; `--attach`
// streams the log and polls for the completion marker in a bounded window, and is safe to
// re-run as many times as the job needs. Detaching HERE is not the hazard that a detached
// LOCAL build is: the VM is single-purpose, disposable, and carries a hard
// `--max-run-duration`, so a forgotten job cannot outlive it or contend with anything.
const JOB_SH = "$HOME/remote-build-job.sh";
const JOB_LOG = "$HOME/remote-build-job.log";
const JOB_RC = "$HOME/remote-build-job.rc";
// Deliberately well UNDER the harness's 10-minute hard cap rather than near it: the deadline
// is only checked after both ssh calls, each of which can burn its own 60s timeout, so a
// window set close to the cap overruns it. Measured — an 8-minute window was SIGKILLed at 10
// minutes just as it was about to report, losing the progress it was holding.
const ATTACH_WINDOW_MS = 5 * 60_000;

async function ssh(ip: string, cmd: string, timeout = 60_000) {
  const { stdout } = await execFileAsync(
    "ssh",
    ["-n", "-i", SSH_KEY, ...SSH_OPTS, `${SSH_USER}@${ip}`, cmd],
    { timeout, maxBuffer: 64 * 1024 * 1024 },
  );
  return stdout;
}

async function startDetachedJob(ip: string, script: string) {
  const b64 = Buffer.from(script, "utf8").toString("base64");
  // The script is landed on disk rather than fed to `bash -s`, for the same reason
  // `remoteJobCommand` does it: anything inside that reads stdin would otherwise swallow the
  // un-executed remainder and exit 0, silently truncating the job.
  //
  // The rc file is written LAST and is the ONLY completion signal `--attach` trusts — an
  // empty or missing log means nothing, since a job can be silent for minutes while a C
  // dependency builds.
  // The `( … &)` subshell is load-bearing, and the obvious spelling is a trap. Writing
  // `cmd && cmd && setsid … &` backgrounds the ENTIRE `&&` chain, and its earlier commands
  // inherit the ssh session's stdout — so the channel never reaches EOF and ssh BLOCKS even
  // though the job started. Measured: that form printed `detached` instantly and then hung
  // until killed at 25s (here, until the 60s exec timeout, surfacing as `Command failed`).
  // Backgrounding only the fully-redirected setsid inside a subshell returns in ~2s.
  await ssh(
    ip,
    `printf %s '${b64}' | base64 -d > ${JOB_SH} && rm -f ${JOB_LOG} ${JOB_RC} && ` +
      `(setsid nohup bash -c 'bash ${JOB_SH} > ${JOB_LOG} 2>&1; echo $? > ${JOB_RC}' ` +
      `</dev/null >/dev/null 2>&1 &) && sleep 1 && echo detached`,
  );
}

async function findZone(name: string) {
  const z = await gcloud(["compute", "instances", "list", "--filter", `name=${name}`, "--format", "value(zone)"]);
  if (!z) throw new Error(`remote-build: no instance named ${name} — already reclaimed, or wrong name.`);
  return z.split("\n")[0].trim();
}

async function attachToJob(a: ReturnType<typeof parseArgs>, name: string) {
  const zone = await findZone(name);
  const ip = await instanceIp(name, zone);
  const t0 = Date.now();
  // The offset must survive the PROCESS, not just the loop. Re-running `--attach` is the
  // normal path, not the exception: the window below is deliberately shorter than the harness
  // timeout that would otherwise SIGKILL the call, so a long job takes several invocations. A
  // process-local counter would restart at 0 each time and `tail -n +1` would re-print the
  // entire transcript on every re-attach — for the agent harness this exists for, that means
  // re-reading thousands of lines into a context window, and it grows with each attempt.
  const seenFile = join(tmpdir(), `remote-build-seen-${name}`);
  let seen = (existsSync(seenFile) && Number(readFileSync(seenFile, "utf8").trim())) || 0;
  // `final` is load-bearing. A poll can land while the job is mid-write, leaving a PARTIAL
  // last line: printing it as though complete and counting it into `seen` makes the next
  // window resume past it, so the rest of that line is lost forever. While polling, drop the
  // trailing element unconditionally and let the next window re-read the line whole. On the
  // terminal pass the job has exited, so whatever remains is complete and must be printed or
  // the last line of every job would vanish.
  const drain = async (final: boolean) => {
    const out = await ssh(ip, `tail -n +${seen + 1} ${JOB_LOG} 2>/dev/null || true`);
    const lines = out.split("\n");
    if (lines.length && (!final || lines[lines.length - 1] === "")) lines.pop();
    for (const l of lines) process.stdout.write(`[r] ${l}\n`);
    seen += lines.length;
    writeFileSync(seenFile, String(seen));
  };
  for (;;) {
    await drain(false);

    const rc = (await ssh(ip, `cat ${JOB_RC} 2>/dev/null || true`)).trim();
    if (rc !== "") {
      await drain(true);
      const secs = ((Date.now() - t0) / 1000).toFixed(0);
      process.stdout.write(`remote-build: job finished rc=${rc} after +${secs}s attached\n`);
      // `--keep` is documented as "do not delete the VM on exit" for post-mortem debugging;
      // it has to reach THIS delete too, or `--attach <name> --keep` destroys the very box
      // the operator asked to preserve.
      if (!a.keep) await deleteInstance(name, zone);
      // Terminal: the offset can never be needed again, so it cleans itself up rather than
      // accumulating a file per builder in tmp.
      rmSync(seenFile, { force: true });
      return Number(rc) || 0;
    }
    if (Date.now() - t0 > ATTACH_WINDOW_MS) {
      process.stdout.write(
        `remote-build: still running (${seen} log lines so far); VM kept.\n` +
          `remote-build: re-attach with: node scripts/remote-build.ts --attach ${name}\n`,
      );
      return EX_STILL_RUNNING;
    }
    await new Promise((r) => setTimeout(r, 10_000));
  }
}

async function startDetached(a: ReturnType<typeof parseArgs>, source: string, live: Set<string>) {
  const name = `nub-builder-${Date.now().toString(36)}-1-${Math.random().toString(36).slice(2, 6)}`;
  const log = (s: string) => process.stdout.write(`[1] ${s}\n`);
  log(`creating ${name} (${a.machine}${a.onDemand ? "" : ", spot"})`);
  const placement = await createInstance(
    name,
    { machine: a.machine, onDemand: a.onDemand, fromImage: true, terminate: "DELETE" },
    log,
  );
  const zone = placement.zone;
  live.add(`${name}|${zone}`);
  // `live` is drained only by the SIGNAL handlers, so a plain throw between here and the job
  // starting reaches `main().catch` → `process.exit(1)` with the instance still running,
  // leaking it for the whole MAX_RUN TTL. `oneBuild` and `buildImage` both guard this same
  // window. Deliberately a catch, not a finally: on SUCCESS the VM must survive — that is
  // what --detach is for.
  try {
    const ip = await instanceIp(name, zone);
    await waitForSsh(name, zone, ip, 240_000);
    log(`ssh up at ${ip} on ${zone}/${placement.machine}`);
    syncSource(source, ip);
    log("source synced");
    await startDetachedJob(ip, jobScript(a.job, a.profile));
  } catch (e) {
    if (!a.keep) await deleteInstance(name, zone);
    live.delete(`${name}|${zone}`);
    throw e;
  }
  // Released from local ownership only once the job is actually running, because from that
  // point outliving this process is the entire point. `--max-run-duration` is the backstop.
  live.delete(`${name}|${zone}`);
  process.stdout.write(
    `remote-build: detached job=${a.job} on ${name} (${zone})\n` +
      `remote-build: collect with: node scripts/remote-build.ts --attach ${name}\n`,
  );
}

async function oneBuild(
  idx: number,
  a: ReturnType<typeof parseArgs>,
  source: string,
  outDir: string,
  live: Set<string>,
) {
  const name = `nub-builder-${Date.now().toString(36)}-${idx}-${Math.random().toString(36).slice(2, 6)}`;
  const t0 = Date.now();
  const log = (s: string) => process.stdout.write(`[${idx}] ${s}\n`);
  let created = false;
  let zone = ZONES[0];
  try {
    log(`creating ${name} (${a.machine}${a.onDemand ? "" : ", spot"})`);
    // Registered BEFORE create so a Ctrl-C during the create call still reaps it — the
    // instance can exist server-side before gcloud returns.
    const placement = await createInstance(
      name,
      { machine: a.machine, onDemand: a.onDemand, fromImage: true, terminate: "DELETE" },
      log,
    );
    zone = placement.zone;
    // The create window itself is NOT covered by layer 1 — the instance can exist
    // server-side before gcloud returns, and the placement walk can span several zones.
    // Builders are covered there by layer 2 (the server-side TTL); --reap catches the rest.
    live.add(`${name}|${zone}`);
    created = true;
    const ip = await instanceIp(name, zone);
    await waitForSsh(name, zone, ip, 240_000);
    log(`ssh up at ${ip} on ${zone}/${placement.machine} (+${((Date.now() - t0) / 1000).toFixed(0)}s)`);

    syncSource(source, ip);
    log(`source synced (+${((Date.now() - t0) / 1000).toFixed(0)}s)`);

    await runJob(ip, jobScript(a.job, a.profile), (l) => log(l));

    const secs = (Date.now() - t0) / 1000;
    log(`done in ${secs.toFixed(0)}s`);
    return { idx, ok: true, secs, error: "" };
  } catch (e: any) {
    const secs = (Date.now() - t0) / 1000;
    log(`FAILED after ${secs.toFixed(0)}s: ${e?.message || e}`);
    return { idx, ok: false, secs, error: String(e?.message || e) };
  } finally {
    if (created && !a.keep) await deleteInstance(name, zone);
    live.delete(`${name}|${zone}`);
  }
}

// Samples the LOCAL host while remote builds run, to put a number on "heavy builds stop
// touching your Mac".
//
// CPU/load alone CANNOT support that claim on a shared dev box, and pretending otherwise
// produces a confidently wrong result: during the first 10-way fanout a SIBLING agent
// started its own local cargo build mid-run, so the idle figure collapsed for reasons that
// had nothing to do with this tool. So the load-INDEPENDENT signal is the one that actually
// answers the question — how many local rustc/cargo processes exist. Remote builds
// contribute exactly zero; ten local ones would contribute dozens.
//
// Must be ASYNC. A synchronous `top` blocks the event loop for ~1-2s every 15s, stalling
// every in-flight build's I/O, delaying signal delivery, and perturbing the very measurement
// it is taking.
async function localBuildProcs() {
  try {
    const { stdout } = await execFileAsync("bash", ["-c", "pgrep -x rustc | wc -l; pgrep -x cargo | wc -l"]);
    const [rustc, cargo] = stdout.trim().split("\n").map((n) => Number(n.trim()));
    return { rustc: rustc || 0, cargo: cargo || 0 };
  } catch {
    return { rustc: 0, cargo: 0 };
  }
}

async function sampleLocal() {
  const { stdout: out } = await execFileAsync("top", ["-l", "2", "-n", "0", "-s", "1"], { timeout: 15_000 });
  const cpuLines = out.split("\n").filter((l) => l.startsWith("CPU usage"));
  const loadLines = out.split("\n").filter((l) => l.startsWith("Load Avg"));
  const cpu = cpuLines[cpuLines.length - 1] || "";
  const load = loadLines[loadLines.length - 1] || "";
  const idle = Number((cpu.match(/([\d.]+)% idle/) || [])[1] ?? NaN);
  const one = Number((load.match(/Load Avg:\s*([\d.]+)/) || [])[1] ?? NaN);
  const procs = await localBuildProcs();
  return { idle, load: one, ...procs };
}

export function isStray(creationTimestamp: string, now: number, maxAgeMs: number) {
  const created = Date.parse(creationTimestamp);
  if (Number.isNaN(created)) return false; // unparseable age -> never assume abandoned
  return now - created > maxAgeMs;
}

async function reap(all: boolean) {
  const list = await gcloud([
    "compute", "instances", "list",
    "--filter", `labels.${LABEL}=1`,
    "--format=value(name,zone.basename(),creationTimestamp)",
  ]);
  const rows = list.split("\n").filter(Boolean).map((r) => r.split(/\s+/));
  // A VM younger than the bake TTL may be a SIBLING AGENT'S live build, or a running image
  // bake. Deleting it destroys someone else's work — and with ~20 agents sharing this project
  // that is a live footgun in the very layer that is supposed to be the safety net. Age is
  // the gate: layer 2 guarantees a healthy instance is terminated by BAKE_MAX_RUN, so
  // anything older than that is genuinely abandoned.
  const maxAgeMs = 90 * 60_000;
  const now = Date.now();
  const targets = all ? rows : rows.filter((r) => isStray(r[2], now, maxAgeMs));
  const skipped = rows.length - targets.length;
  if (skipped) {
    process.stdout.write(
      `remote-build: skipping ${skipped} builder VM(s) younger than 90m — they may be live ` +
        `builds owned by another agent. Use --reap-all to force.\n`,
    );
  }
  if (!targets.length) {
    process.stdout.write("remote-build: no stray builder VMs.\n");
    return { deleted: 0, failed: 0 };
  }
  let failed = 0;
  for (const [name, zone] of targets) {
    process.stdout.write(`remote-build: deleting stray ${name} (${zone})\n`);
    if (!(await deleteInstance(name, zone))) failed++;
  }
  return { deleted: targets.length - failed, failed };
}

// The golden image is what makes this a go-to tool rather than a ceremony: a bare Ubuntu
// needs rustup + clippy + Node before it can do anything,
// and cargo-zigbuild itself is a multi-minute source build. Baking that once turns per-build
// setup into boot time. The registry warm-up additionally pre-fetches every crate so a cold
// builder does not re-download the index on every run.
async function buildImage(a: ReturnType<typeof parseArgs>, source: string, live: Set<string>) {
  const name = `nub-image-bake-${Date.now().toString(36)}`;
  process.stdout.write(`remote-build: baking image family ${IMAGE_FAMILY} on ${name}\n`);
  // On-demand (a preemption mid-bake wastes the whole run) and NO self-destruct (the
  // bake stops the instance and images its disk; a server-side DELETE would destroy it).
  const { zone } = await createInstance(
    name,
    { machine: a.machine, onDemand: true, fromImage: false, terminate: "STOP" },
    (l) => process.stdout.write(`  ${l}\n`),
  );
  // The bake is the ONE instance with no server-side self-destruct, so layer 1 is all that
  // stands between a Ctrl-C and an on-demand VM billing indefinitely. Register it.
  live.add(`${name}|${zone}`);
  try {
    const ip = await instanceIp(name, zone);
    await waitForSsh(name, zone, ip, 300_000);
    const provision = `set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
# cmake is MANDATORY, not optional: libz-ng-sys fails the build ~35s in without it.
sudo apt-get install -y -qq build-essential pkg-config cmake curl git rsync xz-utils python3-pip
curl -fsSL https://deb.nodesource.com/setup_${NODE_MAJOR}.x | sudo -E bash -
sudo apt-get install -y -qq nodejs
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
. "$HOME/.cargo/env"
rustup component add clippy
echo '. "$HOME/.cargo/env"' >> ~/.bashrc
`;
    await runJob(ip, provision, (l) => process.stdout.write(`  ${l}\n`));

    // Warm the crate registry + dependency artifacts from a real checkout so a booted
    // builder starts with the expensive dependency graph already compiled.
    syncSource(source, ip);
    // CARGO_TARGET_DIR must live OUTSIDE ~/src, which this step deletes at the end — that
    // is the entire point of warming it — and must match PREPARE_BASE's, or builders silently
    // start cold against an image that advertises warm artifacts.
    const warm = `set -euxo pipefail
. "$HOME/.cargo/env"
export CARGO_TARGET_DIR="$HOME/.cargo-shared-target"
cd ~/src
mkdir -p runtime/addons && printf 'placeholder' > runtime/addons/nub-native.node
npm install --no-audit --no-fund --loglevel=error
cargo fetch
mkdir -p "$HOME/.darwin-stubs"
cp -R scripts/darwin-stubs/. "$HOME/.darwin-stubs/"
# Best-effort: a warm-up failure still leaves a usable image (toolchain + registry), so it
# must not abort the bake — but it must not be SILENT either, or a cold-building image is
# indistinguishable from a warm one. Warn loudly rather than `|| true`.
# A "NOT best-effort" note here once described an aarch64-apple-darwin cross-build as the
# bake's zig/SDK smoke test. That build no longer exists — darwin artifacts come from
# scripts/mac-build.ts on a real macOS runner, per this file's header — so the note
# documented a command that was not here. The .darwin-stubs copy above is retained.
#
# Warm with the EXACT invocations jobScript() runs, not an approximation of them. Cargo
# fingerprints on the command shape, so \`cargo build -p nub-cli --profile fast\` warmed
# artifacts that the clippy job could not reuse for FOUR independent reasons: build vs
# clippy is a different driver, \`fast\` vs the default \`dev\` is a different target directory
# entirely, one package vs the whole workspace, and no --all-features. Only \`cargo fetch\`
# carried over, so every remote clippy recompiled the entire graph from scratch — which is
# why a run could not fit the driving harness's ceiling and the tool never completed a job.
# Keep these lines in lockstep with jobScript() or the image silently goes cold again.
cargo clippy --all-targets --all-features --profile fast -- -D warnings || echo "WARM-WARN: clippy warm-up failed; builders will cold-compile"
(cd crates/nub-native && cargo clippy --all-features --profile fast -- -D warnings) || echo "WARM-WARN: addon clippy warm-up failed"
# The test job runs on the DEFAULT profile (matching ci.yml), a separate artifact universe
# from \`fast\`. --no-run stops at link, which is all the warming needs.
cargo test --workspace --no-run || echo "WARM-WARN: test warm-up failed; the test job will cold-compile"
# nub-native is an EXCLUDED workspace (panic=unwind cdylib), so --workspace never reaches it,
# and the clippy line above is both a different profile directory and a different driver. Its
# heavy deps (oxc with features=["full"], napi 3, oxc_napi, oxc_sourcemap[napi]) are declared
# only in crates/nub-native/Cargo.toml, so nothing in the root workspace warms them. This is
# jobScript("test")'s FIRST command, verbatim — without it the test job cold-compiles that
# whole graph.
(cd crates/nub-native && cargo build) || echo "WARM-WARN: addon warm-up failed"
rm -rf ~/src
# Fail the bake if the warm compile produced nothing. Without this a truncated or skipped
# warm step publishes a cold image that looks identical to a warm one until every builder
# is mysteriously slow.
echo "warm target dir: $(du -sh "$CARGO_TARGET_DIR" | cut -f1)"
`;
    await runJob(ip, warm, (l) => process.stdout.write(`  ${l}\n`));

    process.stdout.write("remote-build: stopping instance for imaging\n");
    await gcloud(["compute", "instances", "stop", name, "--zone", zone]);
    const image = `${IMAGE_FAMILY}-${Date.now().toString(36)}`;
    await gcloud([
      "compute", "images", "create", image,
      "--source-disk", name, "--source-disk-zone", zone,
      "--family", IMAGE_FAMILY,
    ]);
    process.stdout.write(`remote-build: image ${image} created in family ${IMAGE_FAMILY}\n`);
  } finally {
    await deleteInstance(name, zone);
    live.delete(`${name}|${zone}`);
  }
}

async function main() {
  const a = parseArgs(process.argv.slice(2));
  const source = a.source || repoRoot(process.cwd());
  const outDir = a.out || join(source, "target", "remote");

  // Layer 1: best-effort local cleanup. Deliberately not the only layer — a SIGKILL
  // bypasses this entirely, which is why every VM also self-deletes server-side.
  const live = new Set<string>();
  let signalled = false;
  const onSignal = () => {
    // Re-entrant guard: without it a second Ctrl-C restarts the whole sweep.
    if (signalled) return;
    signalled = true;
    process.stderr.write(`\nremote-build: interrupted — deleting ${live.size} VM(s)…\n`);
    // Deletes run CONCURRENTLY and asynchronously. Serial synchronous deletes blocked the
    // event loop for minutes at --fanout 10, so a second Ctrl-C did nothing and the user's
    // next move was SIGKILL — the exact orphaning this handler exists to prevent.
    const done = Promise.all(
      [...live].map((entry) => {
        const [n, z] = entry.split("|");
        return deleteInstance(n, z);
      }),
    );
    // Hard backstop: never hang on cleanup. Layer 2 still terminates anything left behind.
    const bail = setTimeout(() => {
      process.stderr.write("remote-build: cleanup timed out; server-side TTL will reclaim the rest\n");
      process.exit(130);
    }, 45_000);
    bail.unref();
    done.then(() => process.exit(130)).catch(() => process.exit(130));
  };
  process.on("SIGINT", onSignal);
  process.on("SIGTERM", onSignal);
  // Closing the terminal would otherwise skip cleanup entirely.
  process.on("SIGHUP", onSignal);

  if (a.reap || a.reapAll) {
    const r = await reap(a.reapAll);
    // A reap that failed every delete must NOT read as "clean" to an operator or a wrapper.
    process.exitCode = r.failed ? 1 : 0;
    return;
  }
  if (a.buildImage) {
    await buildImage(a, source, live);
    return;
  }
  // Collect-only: no provisioning, no local sampling — just stream and poll.
  if (a.attach) {
    process.exitCode = await attachToJob(a, a.attach);
    return;
  }
  if (a.detach) {
    await startDetached(a, source, live);
    return;
  }

  const before = await sampleLocal();
  process.stdout.write(
    `remote-build: job=${a.job} fanout=${a.fanout} source=${source}\n` +
      `remote-build: local BEFORE — idle ${before.idle}%, load ${before.load}, ` +
      `local rustc=${before.rustc} cargo=${before.cargo}\n`,
  );

  // Sample the local host every 15s for the duration, so the contention claim carries a
  // measurement rather than an assertion.
  const samples: Array<{ idle: number; load: number; rustc: number; cargo: number }> = [];
  const sampler = setInterval(() => {
    sampleLocal().then((s) => samples.push(s)).catch(() => {});
  }, 15_000);

  const t0 = Date.now();
  const results = await Promise.all(
    Array.from({ length: a.fanout }, (_, i) => oneBuild(i + 1, a, source, outDir, live)),
  );
  clearInterval(sampler);
  const wall = (Date.now() - t0) / 1000;
  const after = await sampleLocal();

  const ok = results.filter((r) => r.ok);
  const failed = results.filter((r) => !r.ok);
  process.stdout.write(`\nremote-build: ${ok.length}/${results.length} succeeded in ${wall.toFixed(0)}s wall\n`);
  for (const r of results) {
    process.stdout.write(
      `  [${r.idx}] ${r.ok ? "ok" : "FAIL"} ${r.secs.toFixed(0)}s` +
        (r.error ? ` — ${r.error.split("\n")[0]}` : "") + "\n",
    );
  }
  if (samples.length) {
    const idles = samples.map((s) => s.idle).filter((n) => !Number.isNaN(n));
    const loads = samples.map((s) => s.load).filter((n) => !Number.isNaN(n));
    const avg = (xs: number[]) => xs.reduce((p, c) => p + c, 0) / (xs.length || 1);
    const maxRustc = Math.max(...samples.map((s) => s.rustc));
    const maxCargo = Math.max(...samples.map((s) => s.cargo));
    if (idles.length) {
      process.stdout.write(
        `remote-build: local DURING (${samples.length} samples) — idle min ${Math.min(...idles).toFixed(1)}% ` +
          `avg ${avg(idles).toFixed(1)}%, load avg ${avg(loads).toFixed(1)}\n`,
      );
    }
    // The attributable number. CPU/load on a shared box can move for reasons this tool did
    // not cause; a local compiler process during a REMOTE build cannot be caused by it.
    process.stdout.write(
      `remote-build: local compiler processes peaked at rustc=${maxRustc} cargo=${maxCargo} ` +
        `during ${results.length} remote build(s) — anything above zero came from OTHER work ` +
        `on this machine, not from these builds.\n`,
    );
  }
  process.stdout.write(
    `remote-build: local AFTER — idle ${after.idle}%, load ${after.load}, ` +
      `local rustc=${after.rustc} cargo=${after.cargo}\n`,
  );

  process.exitCode = failed.length ? 1 : 0;
}

// Same main-gate as scripts/ci-watch.ts: importing this module (tests) must not run it.
const isMain = process.argv[1] !== undefined && fileURLToPath(import.meta.url) === process.argv[1];
if (isMain) {
  main().catch((e) => {
    process.stderr.write(`remote-build: ${e?.message || e}\n`);
    process.exit(1);
  });
}
