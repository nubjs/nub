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
import { homedir, tmpdir } from "node:os";
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
    name: "install-registry-warm-nointegrity",
    fixture: "install-registry-warm-nointegrity",
    // Same warm dep, but the lockfile carries no integrity, so the warm read
    // key is the URL→sha512 binding the `none` run wrote into the host store.
    // The sandboxed cells must read that binding through from the store they
    // cannot write, or they re-fetch and hit the network deny.
    runs: [[NUB, "install"]],
  },
  {
    name: "install-phantom-eject",
    fixture: "install-phantom-eject",
    // `@firebase/database` imports `@firebase/app` without declaring it, so
    // the linker must EJECT it (real files in the project) instead of
    // symlinking it into the sealed store. A fresh, empty data home makes
    // every cell cold: a sandboxed cell that can reach the registry extracts
    // these packages into the project-local store, so the eject decision has
    // to read THAT store, not the global one. The probe reports which path was
    // materialized, not just the exit code. Cells with no registry access fail
    // at resolution; that is the pinned fact for them.
    freshDataHome: true,
    env: (ws) => ({ NUB_CACHE_DIR: join(ws, ".nub-cache") }),
    runs: [
      [NUB, "install"],
      [NUB, "probe.cjs"],
    ],
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
  srtSandbox("claude-srt", []),
  // Claude Code with the npm registry allowlisted — the usual "let the agent
  // install packages" configuration. Requests go through srt's local proxy.
  srtSandbox("claude-srt-registry", ["registry.npmjs.org"]),
];

function srtSandbox(name, allowedDomains) {
  return {
    name,
    wrap: (argv, ws) => [join(BIN, "srt"), "-s", join(ws, "srt-settings.json"), "--", ...argv],
    env: (ws) => {
      writeFileSync(
        join(ws, "srt-settings.json"),
        JSON.stringify(
          {
            network: { allowedDomains, deniedDomains: [] },
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
  };
}

// ── normalization ───────────────────────────────────────────────────
// Snapshots must be stable across machines, users, versions, and runs.

// miette wraps at 80 columns without a tty (COLUMNS is ignored), splitting long
// paths and URLs mid-token; rejoin its `│ ` continuation lines so the path
// placeholders can match. textwrap consumes the space at a word-boundary break,
// breaks after a hyphen with nothing consumed, and force-splits an over-long
// path/URL token exactly at the width — so the separator to put back is a
// space unless the wrapped line ended in a hyphen, or ran the full 80 columns
// while ending inside a path-shaped token.
const MIETTE_WIDTH = 80;
function dewrap(text) {
  const out = [];
  for (const line of text.split("\n")) {
    const m = /^\s*│ (.*)$/.exec(line);
    if (m && out.length > 0) {
      const prev = out[out.length - 1];
      const lastToken = prev.slice(prev.lastIndexOf(" ") + 1);
      // a force-split token is cut mid-word, so it never ends at closing punctuation
      const midToken = lastToken.includes("/") && !/[):;,]$/.test(lastToken);
      const glued = prev.endsWith("-") || (prev.length >= MIETTE_WIDTH && midToken);
      out[out.length - 1] = prev + (glued ? "" : " ") + m[1];
    } else {
      out.push(line);
    }
  }
  return out.join("\n");
}

function normalize(text, ws, dataHome) {
  const home = process.env.HOME ?? "";
  // ANSI first, so the de-wrap sees visible widths; the fresh data home (when the
  // flow has one) before HOME, since it lives under HOME
  const plain = dewrap(text.replace(/\x1b\[[0-9;]*[A-Za-z]/g, ""));
  return (
    (dataHome ? plain.replaceAll(dataHome, "<DATA>") : plain)
      .replaceAll(resolve(ws), "<WS>")
      .replaceAll(ws, "<WS>")
      .replace(/\/private<WS>/g, "<WS>")
      .replaceAll(home, "<HOME>")
      .replace(/\/private\/var\/folders\/[^\s"']+/g, "<TMP>")
      .replace(/\/var\/folders\/[^\s"']+/g, "<TMP>")
      .replace(/\/tmp\/claude\/[^\s"']+/g, "<TMP>") // srt points TMPDIR here
      .replace(/nub \d+\.\d+\.\d+(-[A-Z]+)?/g, "nub <VERSION>")
      .replace(/\bv\d+\.\d+\.\d+\b/g, "v<VERSION>")
      .replace(/in \d+(\.\d+)?(ms|s)\b/g, "in <T>")
      // tarballs fetch concurrently, so WHICH one a denied network fails on
      // first is arbitrary; keep the deny, drop the coordinate and its chain
      .replace(
        /failed to fetch \S+: network access denied: error sending request for url \(\S+\)/g,
        "failed to fetch <PKG>: network access denied: error sending request for url (<URL>)",
      )
      .replace(/\s*chain: \S+( > \S+)*/g, "")
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
          !l.includes("WARN_NUB_SLOW_METADATA") &&
          // the install summary line's shape (one line or two, with or without
          // resolve counts) depends on timing; the `+ pkg@version` lines carry the facts
          !/^nub <VERSION>( · ✓ installed .*)?$/.test(l) &&
          !l.startsWith("✓ resolved "),
      )
      .join("\n")
  );
}

// ── execution ───────────────────────────────────────────────────────
function runCell(sandbox, flow) {
  const ws = mkdtempSync(join(tmpdir(), `nub-asb-${sandbox.name}-${flow.name}-`));
  cpSync(join(HERE, "fixtures", flow.fixture), ws, { recursive: true });
  // A fresh data home lives under HOME, so it is empty AND — inside a
  // sandbox — unwritable, exactly like the real one on a cold machine.
  const dataHome = flow.freshDataHome ? mkdtempSync(join(homedir(), ".cache", "nub-asb-data-")) : undefined;
  const extraEnv = {
    ...sandbox.env(ws),
    ...(flow.env ? flow.env(ws) : {}),
    ...(dataHome ? { XDG_DATA_HOME: dataHome } : {}),
  };
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
    const out = normalize(`${r.stdout ?? ""}${r.stderr ?? ""}`, ws, dataHome);
    lines.push(`### run ${i + 1}: \`${argv.map((a) => (a === NUB ? "nub" : a)).join(" ")}\``);
    lines.push("", "```text", out || "(no output)", "```", "", `exit: ${r.status ?? `signal ${r.signal}`}`, "");
  }
  rmSync(ws, { recursive: true, force: true });
  if (dataHome) rmSync(dataHome, { recursive: true, force: true });
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
