#!/usr/bin/env node
/**
 * Mechanized aube-bump mechanics. The SKILL (`.claude/skills/aube-bump`)
 * owns the doctrine; this script owns the mechanics so a bump is four
 * commands, not an improvised shell session:
 *
 *   aube-bump.ts plan                 # base, delta size, delta sanity
 *   aube-bump.ts venue [--reset]      # build/rebuild the ephemeral merge worktree
 *   aube-bump.ts gate                 # check -> clippy -> test, exit code = verdict
 *   aube-bump.ts land                 # rsync venue -> vendor/aube + UPSTREAM marker
 *
 * The conflict resolution itself stays with an agent (one per partition);
 * everything deterministic lives here.
 */
import { execSync, spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const NUB_ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const VENDOR = "vendor/aube";
const UPSTREAM_FILE = `${VENDOR}/UPSTREAM`;
const UPSTREAM_REMOTE = "aube-upstream";
const UPSTREAM_URL = "https://github.com/aubepkg/aube.git";
const VENUE_DIR = `${(process.env.TMPDIR ?? "/tmp").replace(/\/$/, "")}/aube-venue`;
const TARGET_DIR = `${process.env.HOME ?? "~"}/.cache/nub/aube-venue-target`;

const sh = (cmd, opts = {}) =>
  execSync(cmd, { encoding: "utf8", cwd: NUB_ROOT, ...opts }).trim();
const run = (cmd, opts = {}) => {
  const r = spawnSync("bash", ["-c", cmd], {
    encoding: "utf8",
    cwd: NUB_ROOT,
    ...opts,
  });
  return { code: r.status ?? 1, out: (r.stdout ?? "") + (r.stderr ?? "") };
};

export function readUpstream() {
  const text = readFileSync(join(NUB_ROOT, UPSTREAM_FILE), "utf8");
  const commit = text.match(/^commit\s*=\s*(\S+)/m)?.[1];
  const tag = text.match(/^tag\s*=\s*(\S+)/m)?.[1];
  if (!commit) throw new Error(`${UPSTREAM_FILE}: no 'commit =' line`);
  return { commit, tag: tag ?? "unknown" };
}

function main() {
  const [cmd, ...rest] = process.argv.slice(2);
  if (cmd === "plan") return plan();
  if (cmd === "venue") return venue(rest.includes("--reset"));
  if (cmd === "gate") return gate();
  if (cmd === "land") return land();
  console.error(
    "usage: aube-bump.ts plan|venue [--reset]|gate|land",
  );
  return 1;
}

/** Read the base, fetch upstream, size the delta, sanity-check the marker. */
function plan() {
  const { commit, tag } = readUpstream();
  sh(`git remote add ${UPSTREAM_REMOTE} ${UPSTREAM_URL} 2>/dev/null || true`);
  sh(`git fetch ${UPSTREAM_REMOTE} main`);
  const count = sh(
    `git log --oneline ${commit}..${UPSTREAM_REMOTE}/main | wc -l`,
  );
  const isAncestor = run(
    `git merge-base --is-ancestor ${commit} ${UPSTREAM_REMOTE}/main`,
  );
  const vendorHead = sh(`git log --oneline -1 --format='%h %s' -- ${VENDOR}`);
  console.log(`base:     ${commit} (${tag})`);
  console.log(`upstream: ${sh(`git log --oneline -1 ${UPSTREAM_REMOTE}/main`)}`);
  console.log(`delta:    ${count} upstream commits`);
  console.log(`last vendor commit: ${vendorHead}`);
  console.log(
    isAncestor.code === 0
      ? "base is an ancestor of upstream main: 3-way merge is valid"
      : "WARNING: recorded base is NOT an ancestor of upstream main — resolve before merging",
  );
  return isAncestor.code === 0 ? 0 : 1;
}

/** Build (or rebuild) the ephemeral venue worktree and run the merge. */
function venue(reset) {
  const { commit, tag } = readUpstream();
  if (reset && existsSync(VENUE_DIR)) {
    sh(`git worktree remove --force ${VENUE_DIR}`);
    sh(`git branch -D _aube_venue 2>/dev/null || true`);
  }
  if (!existsSync(VENUE_DIR)) {
    const venue = sh(
      `git commit-tree ${VENDOR_TRAVEL()}:vendor/aube -p ${commit} -m "venue: vendored aube @ ${tag} (${commit})"`,
    );
    sh(`git worktree add -b _aube_venue ${VENUE_DIR} ${venue}`);
  }
  const merged = run(`git -C ${VENUE_DIR} merge ${UPSTREAM_REMOTE}/main --no-ff --no-commit`);
  if (merged.code === 0) {
    console.log("venue: merged clean (no textual conflicts)");
    return 0;
  }
  // rerere may have already replayed recorded resolutions into the
  // working tree. Only `git rerere remaining` knows which unmerged
  // paths rerere actually resolved; a markerless modify/delete
  // conflict is not one, and staging it would silently pick a side.
  const unmerged = sh(`git -C ${VENUE_DIR} diff --name-only --diff-filter=U`)
    .split("\n")
    .filter(Boolean);
  sh(`git -C ${VENUE_DIR} rerere`);
  const remaining = sh(`git -C ${VENUE_DIR} rerere remaining`)
    .split("\n")
    .filter(Boolean);
  const replayed = new Set(unmerged);
  for (const f of remaining) replayed.delete(f);
  for (const f of replayed) sh(`git -C ${VENUE_DIR} add ${f}`);
  console.log(`still conflicted: ${remaining.length}`);
  for (const f of remaining) console.log(`      ${f}`);
  return remaining.length === 0 ? 0 : 1;
}

/** The vendored tree to merge from: the bump branch's vendor tree. */
function VENDOR_TRAVEL() {
  // The venue must carry the bump PR's vendored state (with its delta),
  // not main's. Use the branch that owns the current bump.
  return process.env.AUBE_BUMP_BRANCH || "HEAD";
}

/** The compiler/loop gate. Exit 0 = green. */
function gate() {
  // The caller pins the toolchain (mise/rustup default); never hardcode one.
  // If cargo resolves to a Socket shim, pre-set its re-entry flag so the
  // shim execs the real cargo instead of demanding an API token.
  const env = `CARGO_TARGET_DIR=${TARGET_DIR} SOCKET_SHIM_ACTIVE_CARGO=1`;
  const steps = [
    ["check", `cd ${VENUE_DIR} && ${env} cargo check --workspace --all-targets`],
    [
      "clippy",
      `cd ${VENUE_DIR} && ${env} cargo clippy --workspace --all-targets --all-features -- -D warnings`,
    ],
  ];
  for (const [name, cmd] of steps) {
    const r = run(cmd, { timeout: 30 * 60_000 });
    if (r.code !== 0) {
      console.log(`gate/${name}: FAIL`);
      const errs = r.out
        .split("\n")
        .filter((l) => l.startsWith("error"))
        .slice(0, 20);
      console.log(errs.join("\n"));
      return 1;
    }
    console.log(`gate/${name}: pass`);
  }
  console.log(
    "gate: static green — run the test step with a clean HOME before landing",
  );
  return 0;
}

/** rsync the venue tree into vendor/aube and update the UPSTREAM marker. */
function land() {
  const venue = sh(`git -C ${VENUE_DIR} log --oneline -1`);
  const upstream = sh(`git log --oneline -1 ${UPSTREAM_REMOTE}/main`);
  const upstreamSha = upstream.split(" ")[0];
  rmSync(join(NUB_ROOT, VENDOR), { recursive: true, force: true });
  sh(
    `rsync -a --exclude '.git/' ${VENUE_DIR}/ ${NUB_ROOT}/${VENDOR}/`,
  );
  const marker = [
    "# Which jdx/aube commit this vendored tree derives from.",
    "#",
    `commit = ${upstreamSha}`,
    `tag    = derived at land time`,
  ].join("\n");
  writeFileSync(join(NUB_ROOT, UPSTREAM_FILE), `${marker}\n`);
  writeFileSync(join(VENUE_DIR, "UPSTREAM"), `${marker}\n`);
  sh(`git add ${VENDOR}`);
  const check = run(
    `diff -rq ${VENUE_DIR} ${NUB_ROOT}/${VENDOR} --exclude .git`,
  );
  if (check.code !== 0) {
    console.log("land: venue and vendor tree DIFFER — do not commit");
    console.log(check.out);
    return 1;
  }
  console.log(`land: vendor/aube == venue (${venue})`);
  console.log(`land: UPSTREAM -> ${upstreamSha}`);
  console.log(`land: run the nub-side gates, then commit 'aube: sync upstream ${upstreamSha}'`);
  return 0;
}

process.exitCode = main();
