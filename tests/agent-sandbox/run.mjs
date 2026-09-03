#!/usr/bin/env node
// Agent-sandbox e2e harness: runs nub's main flows inside the DEFAULT Codex CLI
// and Claude Code (sandbox-runtime) sandboxes and snapshots the behavior, the way
// voidzero-dev/vite-task#561 does for `vt`. Snapshots record CURRENT behavior,
// including failures — a failing flow is a pinned fact, not a broken harness.
//
//   node tests/agent-sandbox/run.mjs            # regenerate snapshots/
//   node tests/agent-sandbox/run.mjs --check    # regenerate to a tmp dir and diff
//
// NUB_BIN selects the binary under test (default: `nub-dev` on PATH).
// macOS only: both sandboxes use Seatbelt here; Linux would need bubblewrap.

import { execFileSync, spawnSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const BIN = join(HERE, "node_modules", ".bin");
const CHECK = process.argv.includes("--check");
const SNAP_DIR = CHECK ? mkdtempSync(join(tmpdir(), "nub-asb-check-")) : join(HERE, "snapshots");

if (process.platform !== "darwin") {
  console.error("agent-sandbox harness: macOS only (Seatbelt sandboxes)");
  process.exit(2);
}

const NUB = execFileSync("/usr/bin/which", [process.env.NUB_BIN ?? "nub-dev"], { encoding: "utf8" }).trim();

// ── the flows ───────────────────────────────────────────────────────
// Each flow gets a fresh copy of its fixture as the sandbox workspace. `runs`
// is a list of argv to run in order (second runs exercise warm-cache paths,
// which is where a denied cache write would bite).
const FLOWS = [
  {
    name: "run-ts-file",
    fixture: "run-ts",
    runs: [
      [NUB, "main.ts"],
      [NUB, "main.ts"], // warm: transpile-cache reuse (or silent miss) under a read-only HOME
    ],
  },
  {
    name: "run-script",
    fixture: "run-script",
    runs: [[NUB, "run", "hello"]],
  },
  {
    name: "install-file-dep",
    fixture: "install-file-dep",
    runs: [[NUB, "install"]], // no registry needed; exercises store + linker under the sandbox
  },
  {
    name: "install-registry-warm",
    fixture: "install-registry-warm",
    // Lockfile-pinned registry dep. `none` runs first and warms the host store,
    // so the sandboxed cells measure the warm path: can an install link from a
    // store it may read but not write? Cell order is load-bearing.
    runs: [[NUB, "install"]],
  },
  {
    name: "nubx-registry",
    fixture: "nubx", // carries an .npmrc with fetch-retries=0 so the denied-network cell fails fast
    // A workspace-local cache keeps this cell cold and machine-independent:
    // the sandboxed runs always hit the network (denied) instead of silently
    // reusing whatever the host cache holds. Regeneration needs network for
    // the unsandboxed control.
    env: (ws) => ({ NUB_CACHE_DIR: join(ws, ".nub-cache") }),
    runs: [[NUB, "x", "cowsay@1.6.0", "hi"]],
  },
];

// ── the sandboxes ───────────────────────────────────────────────────
// "default profile, zero extra allowances": Codex's workspace-write mode with a
// hermetic CODEX_HOME, and an srt settings file shaped like Claude Code's
// documented defaults (workspace + temp writable, no network). `none` is the
// unsandboxed control: it separates "the flow is broken" from "the sandbox broke it".
const SANDBOXES = [
  {
    name: "none",
    wrap: (argv) => argv,
    env: () => ({}),
  },
  {
    name: "codex",
    wrap: (argv, ws) => [
      join(BIN, "codex"),
      "sandbox",
      "-c",
      'sandbox_mode="workspace-write"',
      "--",
      ...argv,
    ],
    env: (ws) => {
      const codexHome = join(ws, ".codex-home");
      mkdirSync(codexHome, { recursive: true });
      return { CODEX_HOME: codexHome };
    },
  },
  {
    name: "claude-srt",
    wrap: (argv, ws) => [join(BIN, "srt"), "-s", join(ws, "srt-settings.json"), "--", ...argv],
    env: (ws) => {
      writeFileSync(
        join(ws, "srt-settings.json"),
        JSON.stringify(
          {
            network: { allowedDomains: [], deniedDomains: [] },
            filesystem: {
              denyRead: [],
              allowRead: [],
              allowWrite: [".", "/tmp", process.env.TMPDIR ?? "/tmp"],
              denyWrite: [],
            },
          },
          null,
          2,
        ),
      );
      return {};
    },
  },
];

// ── normalization ───────────────────────────────────────────────────
// Snapshots must be stable across machines, users, versions, and runs.
function normalize(text, ws) {
  const home = process.env.HOME ?? "";
  return (
    text
      .replace(/\x1b\[[0-9;]*[A-Za-z]/g, "") // ANSI
      // miette wraps at 80 columns without a tty (COLUMNS is ignored), splitting
      // paths mid-token; rejoin its `│ ` continuation lines so the path
      // placeholders below can match
      .replace(/\n\s*│ /g, "")
      .replaceAll(resolve(ws), "<WS>")
      .replaceAll(ws, "<WS>")
      .replace(/\/private<WS>/g, "<WS>")
      .replaceAll(home, "<HOME>")
      .replace(/\/private\/var\/folders\/[^\s"']+/g, "<TMP>")
      .replace(/\/var\/folders\/[^\s"']+/g, "<TMP>")
      .replace(/nub \d+\.\d+\.\d+(-[A-Z]+)?/g, "nub <VERSION>")
      .replace(/\bv\d+\.\d+\.\d+\b/g, "v<VERSION>")
      .replace(/in \d+(\.\d+)?(ms|s)\b/g, "in <T>")
      .replace(/\b[0-9a-f]{16,64}\b/g, "<HASH>")
      .replace(/\.nub-cas-\S+/g, ".nub-cas-<RAND>")
      .replace(/~?\d+(\.\d+)? ?[kMG]?B\b/g, "<SIZE>")
      .replace(/(\r\n|\r)/g, "\n")
      // drop blank, spinner, and progress-bar lines so render variance can't drift the snapshot
      .split("\n")
      .filter(
        (l) =>
          l.trim() !== "" &&
          !/^[⠁-⣿\s]*$/.test(l) &&
          !l.includes("█") &&
          // pure pacing noise: how many print depends on wall-clock timing
          !l.includes("WARN_NUB_SLOW_METADATA"),
      )
      .join("\n")
  );
}

// ── execution ───────────────────────────────────────────────────────
function runCell(sandbox, flow) {
  const ws = mkdtempSync(join(tmpdir(), `nub-asb-${sandbox.name}-${flow.name}-`));
  cpSync(join(HERE, "fixtures", flow.fixture), ws, { recursive: true });
  const extraEnv = { ...sandbox.env(ws), ...(flow.env ? flow.env(ws) : {}) };
  const lines = [];
  for (const [i, argv] of flow.runs.entries()) {
    const wrapped = sandbox.wrap(argv, ws);
    const r = spawnSync(wrapped[0], wrapped.slice(1), {
      cwd: ws,
      // COLUMNS keeps miette from hard-wrapping error paths mid-token, which
      // would defeat the <HOME>/<WS> placeholders.
      env: { ...process.env, ...extraEnv, NO_COLOR: "1", FORCE_COLOR: "0", COLUMNS: "400" },
      encoding: "utf8",
      timeout: 120_000,
    });
    const out = normalize(`${r.stdout ?? ""}${r.stderr ?? ""}`, ws);
    lines.push(`### run ${i + 1}: \`${argv.map((a) => (a === NUB ? "nub" : a)).join(" ")}\``);
    lines.push("", "```text", out || "(no output)", "```", "", `exit: ${r.status ?? `signal ${r.signal}`}`, "");
  }
  rmSync(ws, { recursive: true, force: true });
  return lines.join("\n");
}

mkdirSync(SNAP_DIR, { recursive: true });
let failedCheck = false;
for (const sandbox of SANDBOXES) {
  const doc = [
    `# nub under \`${sandbox.name}\``,
    "",
    "Generated by `node tests/agent-sandbox/run.mjs` — do not edit by hand.",
    "",
  ];
  for (const flow of FLOWS) {
    console.error(`[${sandbox.name}] ${flow.name} …`);
    doc.push(`## ${flow.name}`, "", runCell(sandbox, flow));
  }
  const body = doc.join("\n");
  const file = join(SNAP_DIR, `${sandbox.name}.md`);
  if (CHECK) {
    const committed = readFileSync(join(HERE, "snapshots", `${sandbox.name}.md`), "utf8");
    if (committed !== body) {
      failedCheck = true;
      writeFileSync(file, body);
      console.error(`DRIFT: snapshots/${sandbox.name}.md differs; fresh copy at ${file}`);
    }
  } else {
    writeFileSync(file, body);
  }
}
if (CHECK) {
  console.error(failedCheck ? "agent-sandbox check: DRIFT (see above)" : "agent-sandbox check: clean");
  process.exit(failedCheck ? 1 : 0);
}
