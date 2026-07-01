// Regression guard for the dual-channel preload doubling. On the `nub <file>`
// path nub must inject its preload flag (fast tier: `--require <cjs>`; compat
// tier: `--import <url>`) into NODE_OPTIONS ONLY, never also into argv. A tool
// that rebuilds a fork's Node flags by MERGING process.execArgv + NODE_OPTIONS
// (Next `next build`'s getParsedNodeOptions -> formatNodeOptions, jest-worker)
// would otherwise collect the SAME preload path from BOTH channels; when it then
// space-joins and quotes the duplicate (`--require "a b"`), the fork dies with
// `Cannot find module 'a b'`. The channel-level invariant that kills the bug on
// EVERY tier: the preload token is in NODE_OPTIONS but ABSENT from execArgv, so a
// merge collects it exactly once.
import { execArgv, env } from "node:process";

// nub's preload token in either channel — `--require=<...>/preload.cjs` (fast) or
// `--import=<...>/preload.mjs` (compat). execArgv splits `--require <path>` into
// two tokens, so join it before matching.
const PRELOAD = /(--require|--import)[= ]\S*[/\\]preload\.(c?js|mjs)/;

const inArgv = PRELOAD.test(execArgv.join(" "));
const inNodeOptions = PRELOAD.test(env.NODE_OPTIONS || "");
console.log("preload-in-argv:" + inArgv);
console.log("preload-in-node-options:" + inNodeOptions);

// A fork-reconstruction that merges both channels must see the preload once. Count
// distinct preload tokens across the merged token stream (pre-fix: 2; fixed: 1).
const tokens = [...execArgv, ...(env.NODE_OPTIONS || "").split(" ")].join(" ");
const preloadHits = tokens.match(new RegExp(PRELOAD, "g")) || [];
console.log("preload-merged-count:" + preloadHits.length);
