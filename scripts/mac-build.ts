#!/usr/bin/env node
// mac-build — build nub on a REAL macOS runner and pull the binary back to this machine.
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/mac-build.ts --profile release
//   nub  scripts/mac-build.ts
//
// Erasable TypeScript only (no enums/namespaces/parameter-properties) — same constraint as
// the other scripts/*.ts.
//
// WHY THIS AND NOT scripts/remote-build.ts. Both offload the build off the contended dev Mac.
// The GCE path cross-compiles, which costs a hand-maintained set of Apple framework stubs:
// 371 frozen symbols against a real export surface of ~8,700, so a routine dependency bump
// breaks the LINK after a full compile. A real macOS runner needs none of it — native cargo,
// complete SDK, correct deployment target, and it can run the macOS-only code paths that
// Linux structurally cannot. Prefer this for a macOS artifact; keep remote-build.ts for the
// platform-agnostic gates (clippy, test) where Linux is cheaper and unconstrained.
//
// THE TRANSPORT IS GIT, NOT RSYNC, AND THAT IS THE REAL TRADE-OFF. GitHub Actions builds a
// REF, so your work must be pushed before it can be built. Uncommitted edits are invisible to
// it. That makes this a "build what I just pushed" tool, not an inner-loop tool — the ~5s warm
// local incremental remains the fastest way to iterate and should stay local.

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, copyFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const WORKFLOW = "mac-build.yml";
const POLL_MS = 10_000;

const HELP = `mac-build — build nub on a real macOS runner and pull the binary back

Usage:
  nub scripts/mac-build.ts [--profile release|fast] [--runner depot|github]
                           [--ref <branch>] [--no-push] [--out <dir>] [--timeout <min>]

Options:
  --profile <p>   Cargo profile for the CLI (default: release).
  --runner <r>    macOS runner to build on: depot | github (default: depot).
  --ref <branch>  Branch to build (default: the current branch).
  --no-push       Do not push first. The remote ref must already match what you want built.
  --out <dir>     Where to place artifacts (default: <repo>/target/mac).
  --timeout <min> Give up waiting after this many minutes (default: 45).
  --no-stage      Do not copy the addon into runtime/addons.
  -h, --help      Show this help.

The build runs on a REF, so local uncommitted work is not included — this builds what you push.
`;

export function parseArgs(argv: string[]) {
  const a = {
    profile: "release",
    runner: "depot",
    ref: "",
    push: true,
    out: "",
    timeoutMin: 45,
    stage: true,
  };
  for (let i = 0; i < argv.length; i++) {
    const v = argv[i];
    if (v === "-h" || v === "--help") {
      process.stdout.write(HELP);
      process.exit(0);
    } else if (v === "--profile") a.profile = argv[++i];
    else if (v === "--runner") a.runner = argv[++i];
    else if (v === "--ref") a.ref = argv[++i];
    else if (v === "--no-push") a.push = false;
    else if (v === "--no-stage") a.stage = false;
    else if (v === "--out") a.out = argv[++i];
    else if (v === "--timeout") a.timeoutMin = Number(argv[++i]);
    else {
      process.stderr.write(`mac-build: unknown argument ${v}\n\n${HELP}`);
      process.exit(2);
    }
  }
  if (!["release", "fast"].includes(a.profile)) {
    process.stderr.write("mac-build: --profile must be release|fast\n");
    process.exit(2);
  }
  if (!["depot", "github"].includes(a.runner)) {
    process.stderr.write("mac-build: --runner must be depot|github\n");
    process.exit(2);
  }
  return a;
}

function sh(cmd: string, args: string[], opts: Record<string, unknown> = {}) {
  return execFileSync(cmd, args, { encoding: "utf8", maxBuffer: 32 * 1024 * 1024, ...opts }).trim();
}

export function artifactName(profile: string) {
  return `macos-arm64-${profile}`;
}

// `gh workflow run` prints nothing useful, so the dispatched run has to be FOUND. Matching on
// "newest run for this ref" races against any run already in flight for the same branch, so
// the createdAt floor is load-bearing: only accept a run created at/after the moment we
// dispatched. A second of slack absorbs clock skew between here and GitHub.
export function pickDispatchedRun(
  runs: Array<{ databaseId: number; createdAt: string; headBranch: string }>,
  ref: string,
  dispatchedAtMs: number,
) {
  const floor = dispatchedAtMs - 1000;
  const candidates = runs
    .filter((r) => r.headBranch === ref && Date.parse(r.createdAt) >= floor)
    .sort((a, b) => Date.parse(a.createdAt) - Date.parse(b.createdAt));
  return candidates.length ? candidates[0].databaseId : null;
}

function currentBranch(root: string) {
  return sh("git", ["rev-parse", "--abbrev-ref", "HEAD"], { cwd: root });
}

function repoSlug(root: string) {
  return sh("gh", ["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"], { cwd: root });
}

async function main() {
  const a = parseArgs(process.argv.slice(2));
  const root = sh("git", ["rev-parse", "--show-toplevel"]);
  const ref = a.ref || currentBranch(root);
  const outDir = a.out || join(root, "target", "mac");
  const slug = repoSlug(root);

  if (a.push) {
    process.stdout.write(`mac-build: pushing ${ref}…\n`);
    // The runner builds the REMOTE ref, so an un-pushed commit would silently build stale
    // code — the failure mode being guarded against is a "successful" build of the wrong tree.
    sh("git", ["push", "origin", `HEAD:${ref}`], { cwd: root, stdio: "inherit" });
  }
  const localSha = sh("git", ["rev-parse", "HEAD"], { cwd: root });
  const remoteSha = sh("git", ["rev-parse", `origin/${ref}`], { cwd: root });
  if (localSha !== remoteSha) {
    process.stderr.write(
      `mac-build: local HEAD (${localSha.slice(0, 9)}) != origin/${ref} (${remoteSha.slice(0, 9)}).\n` +
        `The runner builds the remote ref, so this would build something other than your working tree.\n` +
        `Push, or pass --ref to name the branch you actually want built.\n`,
    );
    process.exit(1);
  }

  const dispatchedAt = Date.now();
  process.stdout.write(`mac-build: dispatching ${WORKFLOW} on ${ref} (profile=${a.profile}, runner=${a.runner})\n`);
  try {
    sh("gh", [
      "workflow", "run", WORKFLOW,
      "--ref", ref,
      "-f", `profile=${a.profile}`,
      "-f", `runner=${a.runner}`,
    ], { cwd: root });
  } catch (e: any) {
    const msg = String(e?.stderr || e?.message || e);
    // The single most likely first-run failure, and the least self-explanatory: GitHub only
    // registers a workflow_dispatch trigger once the file exists on the DEFAULT branch.
    if (/no workflows found|could not find any workflows/i.test(msg)) {
      process.stderr.write(
        `mac-build: GitHub has not registered ${WORKFLOW} for dispatch.\n` +
          `workflow_dispatch is inert until the workflow file is on the DEFAULT branch — land\n` +
          `${WORKFLOW} on main first (it is CI config, so it goes direct to main), or push to a\n` +
          `branch the workflow's \`push:\` trigger lists.\n`,
      );
      process.exit(1);
    }
    throw e;
  }

  process.stdout.write("mac-build: locating the dispatched run…\n");
  let runId: number | null = null;
  const findDeadline = Date.now() + 120_000;
  while (!runId && Date.now() < findDeadline) {
    const json = sh("gh", [
      "run", "list", "--workflow", WORKFLOW, "--branch", ref, "--limit", "20",
      "--json", "databaseId,createdAt,headBranch",
    ], { cwd: root });
    runId = pickDispatchedRun(JSON.parse(json), ref, dispatchedAt);
    if (!runId) await new Promise((r) => setTimeout(r, 3000));
  }
  if (!runId) {
    process.stderr.write("mac-build: dispatched run never appeared. Check `gh run list`.\n");
    process.exit(1);
  }
  process.stdout.write(`mac-build: run https://github.com/${slug}/actions/runs/${runId}\n`);

  const deadline = Date.now() + a.timeoutMin * 60_000;
  let last = "";
  for (;;) {
    if (Date.now() > deadline) {
      process.stderr.write(`mac-build: timed out after ${a.timeoutMin}m. Run is still at ${last}.\n`);
      process.exit(1);
    }
    const j = JSON.parse(sh("gh", ["run", "view", String(runId), "--json", "status,conclusion"], { cwd: root }));
    if (j.status !== last) {
      last = j.status;
      process.stdout.write(`mac-build: ${j.status}\n`);
    }
    if (j.status === "completed") {
      if (j.conclusion !== "success") {
        process.stderr.write(
          `mac-build: run ${j.conclusion}. Logs: gh run view ${runId} --log-failed\n` +
            `If the jobs sat QUEUED, the Depot GitHub App is probably not installed on this org —\n` +
            `retry with --runner github to use GitHub-hosted macOS instead.\n`,
        );
        process.exit(1);
      }
      break;
    }
    await new Promise((r) => setTimeout(r, POLL_MS));
  }

  mkdirSync(outDir, { recursive: true });
  const name = artifactName(a.profile);
  process.stdout.write(`mac-build: downloading ${name} → ${outDir}\n`);
  sh("gh", ["run", "download", String(runId), "-n", name, "-D", outDir], { cwd: root });

  const bin = join(outDir, "nub");
  const addon = join(outDir, "nub-native.node");
  // `gh run download` unpacks a zip, which does not preserve the executable bit.
  sh("chmod", ["+x", bin]);

  // The runner recorded checksums; verifying them here is what makes this a TRANSFER rather
  // than a leap of faith — it catches a truncated download or a mismatched artifact before the
  // binary is staged and run.
  const sums = join(outDir, "SHA256SUMS");
  if (existsSync(sums)) {
    try {
      sh("shasum", ["-a", "256", "-c", "SHA256SUMS"], { cwd: outDir, stdio: "pipe" });
      process.stdout.write("mac-build: checksums match the runner\n");
    } catch (e: any) {
      process.stderr.write(`mac-build: CHECKSUM MISMATCH — refusing to stage.\n${e?.stdout || ""}\n`);
      process.exit(1);
    }
  }

  const fileOut = sh("file", [bin]);
  if (!/Mach-O 64-bit executable arm64/.test(fileOut)) {
    process.stderr.write(`mac-build: pulled artifact is not an arm64 Mach-O:\n  ${fileOut}\n`);
    process.exit(1);
  }
  // A zip round-trip can invalidate a signature, and arm64 macOS SIGKILLs an unsigned binary —
  // which reads as a build bug rather than a packaging one. Re-sign ad-hoc if needed.
  try {
    sh("codesign", ["--verify", bin], { stdio: "pipe" });
  } catch {
    process.stdout.write("mac-build: signature did not survive the artifact round-trip; re-signing ad-hoc\n");
    sh("codesign", ["-s", "-", "-f", bin]);
  }

  if (a.stage && existsSync(addon)) {
    const staged = join(root, "runtime", "addons");
    mkdirSync(staged, { recursive: true });
    copyFileSync(addon, join(staged, "nub-native.node"));
    process.stdout.write(`mac-build: staged addon → ${join(staged, "nub-native.node")}\n`);
  }

  process.stdout.write(`\nmac-build: ${sh(bin, ["--version"]).split("\n")[0]}\n`);
  process.stdout.write(`mac-build: ${bin}\n`);
}

const isMain = process.argv[1] !== undefined && fileURLToPath(import.meta.url) === process.argv[1];
if (isMain) {
  main().catch((e) => {
    process.stderr.write(`mac-build: ${e?.message || e}\n`);
    process.exit(1);
  });
}
