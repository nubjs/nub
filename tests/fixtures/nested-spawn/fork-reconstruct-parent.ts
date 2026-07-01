// Regression guard for the dual-channel preload doubling: on the `nub <file>`
// path nub must inject its preload `--require` into NODE_OPTIONS ONLY, never also
// into argv. A tool that rebuilds a fork's Node flags by MERGING process.execArgv
// + NODE_OPTIONS (Next `next build`'s getParsedNodeOptions -> formatNodeOptions,
// jest-worker) would otherwise collect the SAME preload path from both channels
// and space-join the duplicate into one quoted `--require "a b"`, killing the
// fork with `Cannot find module 'a b'`. This fixture reproduces that exact
// reconstruction and asserts the collected `--require` set holds the preload
// EXACTLY ONCE, then forks a .ts child to confirm the single flag still augments.
import { fork } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

// getParsedNodeOptions(): merge execArgv (as separate tokens) + NODE_OPTIONS
// (space-split) and collect every `--require`/`-r` value.
function collectRequires(): string[] {
  const tokens: string[] = [...process.execArgv];
  for (const t of (process.env.NODE_OPTIONS || "").split(" ").filter(Boolean)) tokens.push(t);
  const requires: string[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i];
    if (t === "--require" || t === "-r") requires.push(tokens[++i]);
    else if (t.startsWith("--require=")) requires.push(t.slice("--require=".length));
    else if (t.startsWith("-r=")) requires.push(t.slice(3));
  }
  return requires;
}

const requires = collectRequires();
const preloadRequires = requires.filter((r) => /preload\.(c?js)$/.test(r));
console.log("preload-require-count:" + preloadRequires.length);

// formatNodeOptions(): re-emit as a single NODE_OPTIONS, quoting the space-joined
// value the way Next does so a spacey install path survives the tokenizer. With
// the fix this is one path (loads fine); pre-fix it was two (Cannot find module).
const childNodeOptions = requires.length ? `--require "${requires.join(" ")}"` : "";
const child = fork(join(here, "fork-reconstruct-child.ts"), [], {
  execArgv: [], // Next clears execArgv and routes everything through NODE_OPTIONS
  env: { ...process.env, NODE_OPTIONS: childNodeOptions },
});
child.on("exit", (code) => console.log("child-exit:" + code));
