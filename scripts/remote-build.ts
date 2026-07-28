#!/usr/bin/env node
// remote-build — run a nub Rust build/gate on an ephemeral GCE spot VM instead of
// the maintainer's Mac, and (for a darwin build) pull the signed arm64 binary back.
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/remote-build.ts --job build
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
// THE DARWIN CROSS-BUILD. `cargo-zigbuild` + zig, no Apple SDK: zig ships its own
// libSystem.tbd, so nothing Apple-licensed is ever installed on the builder. ZIG_VERSION
// is PINNED and that pin is load-bearing — zig 0.14.1 and 0.15.2 SIGSEGV in the Mach-O
// linker, presenting as `error: linking ... exit status: 1` with an EMPTY `= note:` and a
// zero-byte output, i.e. no diagnostic at all. cargo-zigbuild's README claims "0.15+",
// which is not enough.
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
// Builds run in the ssh FOREGROUND and are never detached — a detached build reparents to
// PID 1, outlives its launcher, and is not reaped by the harness.

import { execFile, execFileSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const execFileAsync = promisify(execFile);

const PROJECT = "pullfrog";
const ZONE = "us-central1-a";
const IMAGE_FAMILY = "nub-builder";
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

const HELP = `remote-build — run a nub build/gate on an ephemeral GCE spot VM

Usage:
  nub scripts/remote-build.ts [--job build|clippy|test] [options]
  nub scripts/remote-build.ts --fanout <n>        # n concurrent builders + local-load sampling
  nub scripts/remote-build.ts --build-image       # bake the golden image (once, ~10 min)
  nub scripts/remote-build.ts --reap              # delete stray builder VMs

Jobs:
  build    cross-compile aarch64-apple-darwin, pull the signed binary back (default)
  clippy   cargo clippy --all-targets --all-features -- -D warnings (native Linux)
  test     cargo test -p nub-cli (native Linux)

Options:
  --job <j>          Job to run (default: build).
  --profile <p>      Cargo profile for --job build: fast | release (default: fast).
  --fanout <n>       Run n builders concurrently; samples local CPU/load throughout.
  --out <dir>        Where to place pulled artifacts (default: <repo>/target/remote).
  --source <dir>     Worktree to build (default: the git root of the cwd).
  --machine <type>   GCE machine type (default: ${MACHINE_TYPE}).
  --on-demand        Use on-demand rather than spot provisioning.
  --keep             Do not delete the VM on exit (debugging; it still self-deletes at ${MAX_RUN}).
  --build-image      Bake the golden image, then exit.
  --reap             Delete every VM labelled ${LABEL}=1, then exit.
  -h, --help         Show this help.

Cost: spot c3-standard-8 is a few cents per build. A stray cannot outlive ${MAX_RUN}.
`;

export function parseArgs(argv: string[]) {
  const a = {
    job: "build",
    profile: "fast",
    fanout: 1,
    out: "",
    source: "",
    machine: MACHINE_TYPE,
    onDemand: false,
    keep: false,
    buildImage: false,
    reap: false,
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
    else {
      process.stderr.write(`remote-build: unknown argument ${v}\n\n${HELP}`);
      process.exit(2);
    }
  }
  if (!["build", "clippy", "test"].includes(a.job)) {
    process.stderr.write(`remote-build: --job must be build|clippy|test\n`);
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

async function gcloud(args: string[], opts: { quiet?: boolean } = {}) {
  const full = [...args, "--project", PROJECT];
  const { stdout } = await execFileAsync("gcloud", full, {
    env: gcloudEnv(),
    maxBuffer: 64 * 1024 * 1024,
  });
  if (!opts.quiet) process.stderr.write("");
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

async function instanceIp(name: string) {
  return await gcloud([
    "compute", "instances", "describe", name, "--zone", ZONE,
    "--format=value(networkInterfaces[0].accessConfigs[0].natIP)",
  ]);
}

// A RUNNING status does not mean sshd is up — especially right after create. Poll the
// real thing (a successful command) rather than the status field, and surface the serial
// console on give-up, which diagnoses a wedged boot instantly where guessing does not.
async function waitForSsh(name: string, ip: string, timeoutMs: number) {
  const deadline = Date.now() + timeoutMs;
  let lastErr = "";
  while (Date.now() < deadline) {
    try {
      await execFileAsync("ssh", ["-i", SSH_KEY, ...SSH_OPTS, `${SSH_USER}@${ip}`, "true"]);
      return;
    } catch (e: any) {
      lastErr = String(e?.stderr || e?.message || e);
      await new Promise((r) => setTimeout(r, 5000));
    }
  }
  let serial = "";
  try {
    serial = await gcloud(["compute", "instances", "get-serial-port-output", name, "--zone", ZONE]);
    serial = serial.split("\n").slice(-25).join("\n");
  } catch {
    serial = "(serial console unavailable)";
  }
  throw new Error(`remote-build: ssh to ${name} (${ip}) never came up.\nlast: ${lastErr}\nserial tail:\n${serial}`);
}

export function instanceCreateArgs(
  name: string,
  pubKey: string,
  opts: { machine: string; onDemand: boolean; fromImage: boolean; selfDestruct: boolean },
) {
  const args = [
    "compute", "instances", "create", name,
    "--zone", ZONE,
    "--machine-type", opts.machine,
    "--boot-disk-size", `${DISK_GB}GB`,
    "--boot-disk-type", "pd-balanced",
    "--labels", `${LABEL}=1`,
    "--metadata", `ssh-keys=${SSH_USER}:${pubKey}`,
  ];
  // Layer 2 of the orphan-proofing: GCE deletes the VM server-side at MAX_RUN no matter
  // what happens to this process. Deliberately NOT applied to the image bake — that flow
  // stops the instance and images its disk, and a mid-bake server-side DELETE would take
  // the disk with it. The bake is supervised and short-lived; it relies on the `finally`
  // and on carrying the label so `--reap` still finds it.
  if (opts.selfDestruct) {
    args.push("--max-run-duration", MAX_RUN, "--instance-termination-action", "DELETE");
  }
  if (opts.fromImage) args.push("--image-family", IMAGE_FAMILY, "--image-project", PROJECT);
  else args.push("--image-family", BASE_IMAGE_FAMILY, "--image-project", BASE_IMAGE_PROJECT);
  if (!opts.onDemand) args.push("--provisioning-model", "SPOT");
  return args;
}

async function createInstance(
  name: string,
  opts: { machine: string; onDemand: boolean; fromImage: boolean; selfDestruct: boolean },
) {
  const pubKey = sh("cat", [`${SSH_KEY}.pub`]).trim();
  await gcloud(instanceCreateArgs(name, pubKey, opts));
}

async function deleteInstance(name: string) {
  try {
    await gcloud(["compute", "instances", "delete", name, "--zone", ZONE, "--quiet"]);
  } catch (e: any) {
    process.stderr.write(`remote-build: WARNING could not delete ${name}: ${e?.message || e}\n`);
  }
}

// rsync MUST be driven by an ALLOWLIST built from `git ls-files`. An --exclude blocklist
// makes rsync walk the entire ~99 GB working tree (target/, node_modules/, .repos/) just to
// decide what to skip, and it times out. The allowlist never touches those paths at all:
// measured 2.2s for the full ~107 MB payload, 0.8s for a few-file delta.
// tests/node-suite is a populated submodule (~795 MB) and is dropped explicitly.
export function filterSourceFiles(lsFilesOutput: string) {
  return lsFilesOutput.split("\n").filter((f) => f && !f.startsWith("tests/node-suite"));
}

function syncSource(source: string, ip: string) {
  const files = filterSourceFiles(sh("git", ["ls-files"], { cwd: source }));
  const listFile = join(tmpdir(), `remote-build-files-${process.pid}-${Math.random().toString(36).slice(2)}.txt`);
  writeFileSync(listFile, files.join("\n") + "\n");
  try {
    sh("rsync", [
      "-az", "--delete", "--delete-missing-args",
      `--files-from=${listFile}`,
      "-e", `ssh -i ${SSH_KEY} ${SSH_OPTS.join(" ")}`,
      `${source}/`,
      `${SSH_USER}@${ip}:~/src/`,
    ]);
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
const PREPARE = `set -euo pipefail
cd ~/src
command -v node >/dev/null || { echo "remote-build: node missing on builder (would silently degrade the primer)" >&2; exit 3; }
mkdir -p runtime/addons
[ -s runtime/addons/nub-native.node ] || printf 'placeholder' > runtime/addons/nub-native.node
[ -d node_modules ] || npm install --no-audit --no-fund --loglevel=error
`;

export function jobScript(job: string, profile: string) {
  if (job === "clippy") {
    return `${PREPARE}cargo clippy --all-targets --all-features -- -D warnings`;
  }
  if (job === "test") {
    return `${PREPARE}cargo test -p nub-cli`;
  }
  return `${PREPARE}cargo zigbuild --target aarch64-apple-darwin -p nub-cli --profile ${profile}
ls -la target/aarch64-apple-darwin/${profile}/nub`;
}

async function runJob(ip: string, script: string, onLine: (s: string) => void) {
  return await new Promise<void>((resolve, reject) => {
    const child = execFile(
      "ssh",
      ["-i", SSH_KEY, ...SSH_OPTS, `${SSH_USER}@${ip}`, "bash -s"],
      { maxBuffer: 64 * 1024 * 1024 },
      (err, _stdout, stderr) => {
        if (err) reject(new Error(`remote job failed: ${stderr || err.message}`));
        else resolve();
      },
    );
    child.stdout?.on("data", (d) => String(d).split("\n").forEach((l) => l && onLine(l)));
    child.stderr?.on("data", (d) => String(d).split("\n").forEach((l) => l && onLine(l)));
    child.stdin?.end(script);
  });
}

// arm64 macOS SIGKILLs any binary without at least an ad-hoc signature, so an unsigned
// artifact looks exactly like a build failure when it is nothing of the sort. zig emits a
// valid ad-hoc signature itself; this verifies rather than assumes, because a silently
// unsigned artifact is the single most confusing way for this pipeline to fail.
function verifyArtifact(path: string) {
  const fileOut = sh("file", [path]).trim();
  if (!/Mach-O 64-bit executable arm64/.test(fileOut)) {
    throw new Error(`remote-build: pulled artifact is not an arm64 Mach-O executable:\n  ${fileOut}`);
  }
  let signed = true;
  try {
    sh("codesign", ["--verify", path], { stdio: "pipe" });
  } catch {
    signed = false;
  }
  return { fileOut, signed };
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
  try {
    log(`creating ${name} (${a.machine}${a.onDemand ? "" : ", spot"})`);
    // Registered BEFORE create so a Ctrl-C during the create call still reaps it — the
    // instance can exist server-side before gcloud returns.
    live.add(name);
    await createInstance(name, { machine: a.machine, onDemand: a.onDemand, fromImage: true, selfDestruct: true });
    created = true;
    const ip = await instanceIp(name);
    await waitForSsh(name, ip, 240_000);
    log(`ssh up at ${ip} (+${((Date.now() - t0) / 1000).toFixed(0)}s)`);

    syncSource(source, ip);
    log(`source synced (+${((Date.now() - t0) / 1000).toFixed(0)}s)`);

    await runJob(ip, jobScript(a.job, a.profile), (l) => log(l));

    let artifact = "";
    let verified: { fileOut: string; signed: boolean } | null = null;
    if (a.job === "build") {
      mkdirSync(outDir, { recursive: true });
      artifact = join(outDir, a.fanout > 1 ? `nub-${idx}` : "nub");
      sh("rsync", [
        "-az",
        "-e", `ssh -i ${SSH_KEY} ${SSH_OPTS.join(" ")}`,
        `${SSH_USER}@${ip}:~/src/target/aarch64-apple-darwin/${a.profile}/nub`,
        artifact,
      ]);
      verified = verifyArtifact(artifact);
      log(`pulled ${artifact} — ${verified.fileOut.replace(/^.*?: /, "")}, signature ${verified.signed ? "valid" : "MISSING"}`);
    }
    const secs = (Date.now() - t0) / 1000;
    log(`done in ${secs.toFixed(0)}s`);
    return { idx, ok: true, secs, artifact, signed: verified?.signed ?? null, error: "" };
  } catch (e: any) {
    const secs = (Date.now() - t0) / 1000;
    log(`FAILED after ${secs.toFixed(0)}s: ${e?.message || e}`);
    return { idx, ok: false, secs, artifact: "", signed: null, error: String(e?.message || e) };
  } finally {
    if (created && !a.keep) await deleteInstance(name);
    live.delete(name);
  }
}

// Samples the LOCAL host while remote builds run. The whole claim this tool makes is
// "heavy builds stop touching your Mac", and that claim is only worth anything with a
// number attached. `top -l 2` is used because the first sample of `top` is a since-boot
// average, not an instantaneous reading — only the second sample is meaningful.
function sampleLocal() {
  const out = sh("top", ["-l", "2", "-n", "0", "-s", "1"]);
  const cpuLines = out.split("\n").filter((l) => l.startsWith("CPU usage"));
  const loadLines = out.split("\n").filter((l) => l.startsWith("Load Avg"));
  const cpu = cpuLines[cpuLines.length - 1] || "";
  const load = loadLines[loadLines.length - 1] || "";
  const idle = Number((cpu.match(/([\d.]+)% idle/) || [])[1] ?? NaN);
  const one = Number((load.match(/Load Avg:\s*([\d.]+)/) || [])[1] ?? NaN);
  return { idle, load: one };
}

async function reap() {
  const list = await gcloud([
    "compute", "instances", "list",
    "--filter", `labels.${LABEL}=1`,
    "--format=value(name,zone.basename())",
  ]);
  const rows = list.split("\n").filter(Boolean);
  if (!rows.length) {
    process.stdout.write("remote-build: no stray builder VMs.\n");
    return 0;
  }
  for (const row of rows) {
    const [name] = row.split(/\s+/);
    process.stdout.write(`remote-build: deleting stray ${name}\n`);
    await deleteInstance(name);
  }
  return rows.length;
}

// The golden image is what makes this a go-to tool rather than a ceremony: a bare Ubuntu
// needs rustup + the darwin target + zig + cargo-zigbuild + Node before it can do anything,
// and cargo-zigbuild itself is a multi-minute source build. Baking that once turns per-build
// setup into boot time. The registry warm-up additionally pre-fetches every crate so a cold
// builder does not re-download the index on every run.
async function buildImage(a: ReturnType<typeof parseArgs>, source: string) {
  const name = `nub-image-bake-${Date.now().toString(36)}`;
  process.stdout.write(`remote-build: baking image family ${IMAGE_FAMILY} on ${name}\n`);
  // On-demand (a preemption mid-bake wastes the whole run) and NO self-destruct (the
  // bake stops the instance and images its disk; a server-side DELETE would destroy it).
  await createInstance(name, { machine: a.machine, onDemand: true, fromImage: false, selfDestruct: false });
  try {
    const ip = await instanceIp(name);
    await waitForSsh(name, ip, 300_000);
    const provision = `set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
# cmake is MANDATORY, not optional: libz-ng-sys fails the build ~35s in without it.
sudo apt-get install -y -qq build-essential pkg-config cmake curl git rsync xz-utils python3-pip
curl -fsSL https://deb.nodesource.com/setup_${NODE_MAJOR}.x | sudo -E bash -
sudo apt-get install -y -qq nodejs
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
. "$HOME/.cargo/env"
rustup target add aarch64-apple-darwin
rustup component add clippy
# PIN zig 0.16.0. 0.14.1 and 0.15.2 SIGSEGV in the Mach-O linker with an EMPTY error note
# and a zero-byte output — the least debuggable failure in this pipeline.
curl -fsSL https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz -o /tmp/zig.tar.xz
sudo mkdir -p /opt/zig && sudo tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1
sudo ln -sf /opt/zig/zig /usr/local/bin/zig
zig version
cargo install cargo-zigbuild --locked
echo '. "$HOME/.cargo/env"' >> ~/.bashrc
`;
    await runJob(ip, provision, (l) => process.stdout.write(`  ${l}\n`));

    // Warm the crate registry + dependency artifacts from a real checkout so a booted
    // builder starts with the expensive dependency graph already compiled.
    syncSource(source, ip);
    const warm = `set -euo pipefail
. "$HOME/.cargo/env"
cd ~/src
mkdir -p runtime/addons && printf 'placeholder' > runtime/addons/nub-native.node
npm install --no-audit --no-fund --loglevel=error
cargo fetch
cargo zigbuild --target aarch64-apple-darwin -p nub-cli --profile fast || true
cargo build -p nub-cli --profile fast || true
rm -rf ~/src
`;
    await runJob(ip, warm, (l) => process.stdout.write(`  ${l}\n`));

    process.stdout.write("remote-build: stopping instance for imaging\n");
    await gcloud(["compute", "instances", "stop", name, "--zone", ZONE]);
    const image = `${IMAGE_FAMILY}-${Date.now().toString(36)}`;
    await gcloud([
      "compute", "images", "create", image,
      "--source-disk", name, "--source-disk-zone", ZONE,
      "--family", IMAGE_FAMILY,
    ]);
    process.stdout.write(`remote-build: image ${image} created in family ${IMAGE_FAMILY}\n`);
  } finally {
    await deleteInstance(name);
  }
}

async function main() {
  const a = parseArgs(process.argv.slice(2));
  const source = a.source || repoRoot(process.cwd());
  const outDir = a.out || join(source, "target", "remote");

  // Layer 1: best-effort local cleanup. Deliberately not the only layer — a SIGKILL
  // bypasses this entirely, which is why every VM also self-deletes server-side.
  const live = new Set<string>();
  const onSignal = () => {
    for (const n of live) {
      try {
        execFileSync("gcloud", ["compute", "instances", "delete", n, "--zone", ZONE, "--project", PROJECT, "--quiet"], { env: gcloudEnv() });
      } catch {}
    }
    process.exit(130);
  };
  process.on("SIGINT", onSignal);
  process.on("SIGTERM", onSignal);

  if (a.reap) {
    const n = await reap();
    process.exit(n > 0 ? 0 : 0);
  }
  if (a.buildImage) {
    await buildImage(a, source);
    process.exit(0);
  }

  const before = sampleLocal();
  process.stdout.write(
    `remote-build: job=${a.job} fanout=${a.fanout} source=${source}\n` +
      `remote-build: local BEFORE — idle ${before.idle}%, load ${before.load}\n`,
  );

  // Sample the local host every 15s for the duration, so the contention claim carries a
  // measurement rather than an assertion.
  const samples: Array<{ idle: number; load: number }> = [];
  const sampler = setInterval(() => {
    try {
      samples.push(sampleLocal());
    } catch {}
  }, 15_000);

  const t0 = Date.now();
  const results = await Promise.all(
    Array.from({ length: a.fanout }, (_, i) => oneBuild(i + 1, a, source, outDir, live)),
  );
  clearInterval(sampler);
  const wall = (Date.now() - t0) / 1000;
  const after = sampleLocal();

  const ok = results.filter((r) => r.ok);
  const failed = results.filter((r) => !r.ok);
  process.stdout.write(`\nremote-build: ${ok.length}/${results.length} succeeded in ${wall.toFixed(0)}s wall\n`);
  for (const r of results) {
    process.stdout.write(
      `  [${r.idx}] ${r.ok ? "ok" : "FAIL"} ${r.secs.toFixed(0)}s` +
        (r.artifact ? ` -> ${r.artifact}${r.signed === false ? " (UNSIGNED)" : ""}` : "") +
        (r.error ? ` — ${r.error.split("\n")[0]}` : "") + "\n",
    );
  }
  if (samples.length) {
    const idles = samples.map((s) => s.idle).filter((n) => !Number.isNaN(n));
    const loads = samples.map((s) => s.load).filter((n) => !Number.isNaN(n));
    const avg = (xs: number[]) => xs.reduce((p, c) => p + c, 0) / (xs.length || 1);
    process.stdout.write(
      `remote-build: local DURING (${samples.length} samples) — idle min ${Math.min(...idles).toFixed(1)}% ` +
        `avg ${avg(idles).toFixed(1)}%, load avg ${avg(loads).toFixed(1)}\n`,
    );
  }
  process.stdout.write(`remote-build: local AFTER — idle ${after.idle}%, load ${after.load}\n`);

  process.exit(failed.length ? 1 : 0);
}

// Same main-gate as scripts/ci-watch.ts: importing this module (tests) must not run it.
const isMain = process.argv[1] !== undefined && fileURLToPath(import.meta.url) === process.argv[1];
if (isMain) {
  main().catch((e) => {
    process.stderr.write(`remote-build: ${e?.message || e}\n`);
    process.exit(1);
  });
}
