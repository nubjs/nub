#!/usr/bin/env node
// Regenerate allowlist.txt from a full run.
//
// Usage: node gen-allowlist.mjs <junit.xml> <shim-invocations.log> <junit-baseline.xml> > allowlist.txt
//
// Inputs are what run.sh writes to its work directory: the report of the run
// against nub, the seam's per-invocation log (test, identity, cwd, argv), and
// the report of CONF_BASELINE=1, the same suite against pnpm itself.
//
// Each failing test takes the FIRST rule that accepts it, and the rule supplies
// the category and reason written beside it. A failure no rule accepts is
// printed to stderr and the script exits 1, so a new kind of failure needs a
// written reason before it can be allowlisted.
import { readFileSync } from "node:fs";
import { parseJunit } from "./junit.mjs";

// Verbs nub answers with its own command in every project (verb_routing.rs
// HOST_VERBS, plus nub's own namespaces).
const HOST_VERBS = new Set([
  "run", "run-script", "exec", "dlx", "create", "init", "env", "self-update",
  "test", "t", "start", "stop", "restart", "install-test", "it", "pm", "node", "watch", "upgrade",
]);
// Pre-verb options that take a separate value, so the verb is found after it.
const VALUE_OPTIONS = /^(--dir|-C|--filter|-F|--filter-prod|--prefix|--reporter|--loglevel|--workspace-concurrency|--resume-from|--workspace-packages|--shim)$/;
const verbOf = (argv) => {
  for (let i = 0; i < argv.length; i++) {
    if (!argv[i]) continue;
    if (argv[i].startsWith("-")) {
      if (VALUE_OPTIONS.test(argv[i])) i++;
      continue;
    }
    return argv[i];
  }
  return "";
};
// A shape nub's front door produces when a command line is not the engine's:
// its own runner's grammar, a pre-verb pnpm flag handed to node, or the
// bare-name fallback pnpm has and nub does not. assert_cmd's failure dump
// escapes quotes (`\'`), plain stderr does not.
const FRONT_DOOR = /is not a nub command|node: bad option|node: -r requires an argument|Usage: nub (run|exec|create|dlx|init)|not installed in node_modules\/\.bin|nub pm takes a subcommand|nub create: missing template name|missing script: "|refusing to overwrite existing files|Cannot find module \\?'/;

// Failures of engine verbs in a pnpm project, each read individually against
// the test source and its captured output.
const PNPM_PROJECT_ENGINE = {
  "deploy::shared_lockfile_deploy_refuses_a_linked_workspace_package_with_an_ambiguous_peer": [
    "bug/pnpm-project",
    "ERR_PNPM_DEPLOY_AMBIGUOUS_PEER is raised, but the diagnostic is hard-wrapped mid-sentence, so the asserted wording never appears on one line",
  ],
  "git_hosted_install::a_git_dependency_is_prepared_with_the_package_manager_it_pins": [
    "bug/pnpm-project",
    "preparing a git dependency that pins yarn@1.22.22 warns 'Cannot provide yarn@1.22.22' and runs the host's yarn, which is absent; pnpm provisions the pinned one",
  ],
  "peers::a_partial_upper_bound_peer_range_covers_the_omitted_component": [
    "bug/pnpm-project",
    "with strictPeerDependencies, `install --lockfile-only` reports @pnpm.e2e/foo@2.0.0 as an unmet peer of the range '>=1 <=2'",
  ],
  "pipeline_cargo_cache::cargo_state_is_shared_between_worktrees_and_survives_cache_deletion": [
    "bug/pnpm-project",
    "`pipeline --full` in a second git worktree rebuilds from scratch and never reports 'restored Cargo build state' from the shared cache",
  ],
  "licenses::licenses_reads_global_store_metadata_with_a_manifest_selected_runtime": [
    "bug/pnpm-project",
    "devEngines.runtime pins node '1' with onFail 'ignore', yet the lifecycle script's `node` tries to provision Node 1 and fails with ERR_NUB_NODE_PROVISION_FAILED",
  ],
  "store::empty_store_dir_override_restores_the_platform_default": [
    "identity/nub-project",
    "the test's first `store path` runs where no pnpm marker exists, so the default it compares against is nub's store",
  ],
};

const RULES = [
  {
    category: "environment",
    reason: "fails against pnpm 12.4.1 itself on the same runner (CONF_BASELINE=1)",
    match: (t) => t.failsInBaseline,
  },
  {
    category: "bug/pre-verb-reporter-ndjson",
    reason: "`nub --reporter=ndjson <verb>` is rejected by nub's global option parser (default|append-only|silent); pnpm accepts ndjson before the verb, and nub accepts it after",
    match: (t) => /invalid value \\?'ndjson\\?' for \\?'--reporter/.test(t.out),
  },
  {
    category: "seam/global-shims",
    reason: "pnpm writes context-aware shims by copying its own executable and dispatching on argv0; through the seam the copy is the shell shim and nub's binary dispatches its own argv0 names, so the harness cannot observe these",
    match: (t) => /^global_shims::|^global::global_shims_|^pnpx_alias::/.test(t.name),
  },
  {
    category: "divergence/version-flag",
    reason: "`--version` is answered by nub's front door with nub's own version, also in a pnpm project",
    match: (t) => t.calls.some((c) => c.argv.includes("--version") || c.argv.includes("-v")) && /v\d+\.\d+\.\d+/.test(t.out),
  },
  {
    category: "divergence/host-verb-in-pnpm-project",
    reason: "the command line reaches nub's own command (run/exec/dlx/create/init/env/self-update/test/start/stop/restart/install-test, a pre-verb pnpm flag, or pnpm's bare-name fallback) although the project is pnpm's",
    match: (t) => t.identities.has("pnpm") && (t.hostVerb || FRONT_DOOR.test(t.out)),
  },
  {
    category: "identity/nub-project",
    reason: "the fixture writes no pnpm marker, so nub applies its own project identity (its verbs, lockfile, configuration and ERR_NUB_* codes)",
    match: (t) => t.identities.size > 0 && !t.identities.has("pnpm"),
  },
  ...Object.entries(PNPM_PROJECT_ENGINE).map(([name, [category, reason]]) => ({
    category,
    reason,
    match: (t) => t.name === name,
  })),
];

const [junitPath, shimLogPath, baselinePath] = process.argv.slice(2);
if (!junitPath || !shimLogPath || !baselinePath) {
  console.error("usage: gen-allowlist.mjs <junit.xml> <shim-invocations.log> <junit-baseline.xml>");
  process.exit(2);
}

const calls = new Map();
for (const line of readFileSync(shimLogPath, "utf8").split("\n")) {
  if (!line) continue;
  const [test, identity, , args = ""] = line.split("\t");
  if (!calls.has(test)) calls.set(test, []);
  calls.get(test).push({ identity, argv: args.split(" ") });
}
const baselineFailures = new Set(
  parseJunit(readFileSync(baselinePath, "utf8")).filter((c) => c.failed).map((c) => c.name),
);

const failing = parseJunit(readFileSync(junitPath, "utf8")).filter((c) => c.failed);
const categories = [...new Set(RULES.map((r) => r.category))];
const sections = new Map(categories.map((c) => [c, []]));
const unmatched = [];
for (const c of failing) {
  const testCalls = calls.get(c.name) ?? [];
  const rule = RULES.find((r) =>
    r.match({
      name: c.name,
      out: c.message,
      calls: testCalls,
      identities: new Set(testCalls.map((x) => x.identity)),
      hostVerb: testCalls.some((x) => HOST_VERBS.has(verbOf(x.argv))),
      failsInBaseline: baselineFailures.has(c.name),
    }),
  );
  if (rule) sections.get(rule.category).push(`${c.name}  # ${rule.category}: ${rule.reason}`);
  else unmatched.push(c);
}

if (unmatched.length > 0) {
  for (const c of unmatched) {
    const seen = [...new Set((calls.get(c.name) ?? []).map((x) => x.identity))].join("+") || "no seam call";
    console.error(`UNCATEGORIZED ${c.name} [${seen}]\n${c.message.split("\n").slice(0, 12).join("\n")}\n`);
  }
  console.error(`${unmatched.length} failing test(s) match no rule; add a rule with a reason.`);
  process.exit(1);
}

const lines = [
  "# Known failures of pnpm 12.4.1's CLI suite (pnpm/crates/cli/tests/suite) under nub.",
  "# GENERATED by gen-allowlist.mjs from a full run; regenerate rather than edit.",
  "# Format: <module>::<test>  # <category>: <reason>",
];
for (const [category, entries] of sections) {
  if (entries.length === 0) continue;
  lines.push("", `# ${category} (${entries.length})`, ...entries.sort());
}
console.log(lines.join("\n"));
