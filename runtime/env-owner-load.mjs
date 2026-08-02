// The env-owner adapter — the ONLY file in nub that knows the external loader is
// varlock. Injected as an `--import` when a project carries a `.env.schema` and
// the loader package is importable; everything else in nub is written against the
// general "an external owner handles env" contract, so replacing this file with a
// native loader is the whole swap.
//
// Runs as an `--import` preload, which Node fully evaluates — top-level await
// included — before the entry module, so `process.env` is populated before any
// user code observes it.

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import path from "node:path";
import { isMainThread } from "node:worker_threads";

// Two DIFFERENT directories, and conflating them breaks monorepos:
//   root        — where .env.schema lives, i.e. what the loader must discover from
//   resolveFrom — the nearest project root, i.e. where its package is installed
// Under an isolated node_modules layout (nub's default linker) a member's
// dependencies sit in <member>/node_modules while the schema sits at the
// workspace root, so resolving from `root` would miss the package entirely.
const root = process.env.__NUB_ENV_OWNER_ROOT;
const resolveFrom = process.env.__NUB_ENV_OWNER_RESOLVE_FROM || root;

// A Worker needs the PROTECTIONS but not the resolution, and the two have to be
// separated because it cannot do the second.
//
// It inherits a copy of process.env, so the values are already present. It also
// cannot re-resolve correctly: `process.chdir` throws in a Worker, so the cwd hop
// is skipped, and a Worker started from a workspace member then resolves from a
// directory with no schema — an EMPTY graph that reports nothing, because
// __VARLOCK_ENV is still set and the verification pass sees a load.
//
// But skipping the module wholesale is not right either: the guards live in this
// process, so a Worker would print `@sensitive` values in the clear. Measured
// against `varlock run`, which redacts them in a Worker.
//
// So a Worker takes the loader's blob-consuming server entry instead — it installs
// the guards from the inherited __VARLOCK_ENV, resolves nothing, and spawns
// nothing (verified: 0 subprocesses, secret redacted).
if (root && !isMainThread) {
  // That entry does slightly more than install guards: it re-applies every
  // schema key to process.env from the blob first, because the redaction map is
  // built there. So a Worker constructed with an `env` override loses that
  // override for any schema-declared key — which does not happen under
  // `varlock run`, since the child there carries no loader at all.
  try {
    const req = createRequire(pathToFileURL(`${resolveFrom}/package.json`));
    await import(pathToFileURL(req.resolve("varlock/init-server")).href);
  } catch (err) {
    // A loader that simply has no such entry leaves the Worker with inherited
    // values and no guards — the state it would have had anyway, and not worth
    // killing a Worker over. ANY OTHER failure is different: it means the guards
    // did not install, so the Worker is printing @sensitive values in the clear,
    // which is the exact outcome this branch exists to prevent. Say so rather
    // than swallowing it. Warn instead of rethrowing: a redaction failure should
    // not take the Worker down with it.
    if (err?.code !== "MODULE_NOT_FOUND" && err?.code !== "ERR_MODULE_NOT_FOUND") {
      console.warn(
        `nub: could not install varlock's protections in this worker (${err?.message ?? err}).\n` +
          "      Sensitive values may print unredacted here.",
      );
    }
  }
} else if (root) {
  // Close BOTH routes back into nub before the loader is even imported.
  //
  // The loader normally reuses an already-populated __VARLOCK_ENV and never
  // spawns anything — but that fast path is conditional, and a user-facing knob
  // turns it off: with `_VARLOCK_FILTER` set, reuse is disabled and it shells out
  // to its CLI. That CLI is a `#!/usr/bin/env node` script, so with nub's shim on
  // PATH its shebang resolves `node` to nub, which re-detects this project and
  // runs the loader again, forever. Measured before this scrub: `_VARLOCK_FILTER=…`
  // hung with a recursive process chain.
  //
  // Rather than depend on the fast path holding, make the spawn HARMLESS: strip
  // nub's shim from PATH so any child reaches a real node, and drop this module's
  // own preload tokens so a child cannot re-run it. Both are consumed at startup,
  // so mutating them now affects only children, never this process.
  //
  // This must happen BEFORE importing the loader: it snapshots process.env at
  // module-load time (`originalProcessEnv`, captured by the first instance to
  // load) and hands THAT snapshot to its subprocess, so a later mutation is
  // invisible to it.
  const savedPath = process.env.PATH;
  const savedNodeOptions = process.env.NODE_OPTIONS;
  const drop = (value, matches) =>
    (value || "")
      .split(/\s+/)
      .filter((token) => token && !matches(token))
      .join(" ");
  if (savedPath !== undefined) {
    process.env.PATH = savedPath
      .split(path.delimiter)
      .filter((entry) => !path.basename(entry).startsWith("nub-node-shim-"))
      .join(path.delimiter);
  }
  if (savedNodeOptions !== undefined) {
    process.env.NODE_OPTIONS = drop(savedNodeOptions, (t) => t.includes("env-owner"));
  }

  try {
    // Resolve the loader from the USER'S PROJECT, not from here. This module
    // lives in nub's install dir, outside the project's node_modules, so a bare
    // `import "varlock"` resolves against nub and fails with ERR_MODULE_NOT_FOUND.
    const req = createRequire(pathToFileURL(`${resolveFrom}/package.json`));
    const loader = await import(pathToFileURL(req.resolve("varlock")).href);
    const autoLoad = pathToFileURL(req.resolve("varlock/auto-load")).href;

    // The loader discovers its schema from the CURRENT DIRECTORY only, with no
    // ancestor walk, so a workspace member would otherwise miss a root schema.
    // Hopping cwd is safe here and nowhere else: preloads run before any user
    // code, so nothing else can observe the process-global cwd mid-load.
    //
    // Except in a worker_threads Worker, where `process.chdir` EXISTS but throws
    // `TypeError: process.chdir() is not supported in workers` — so a `typeof`
    // check does not catch it. Workers inherit NODE_OPTIONS preloads and a copy of
    // process.env, so this module does re-run there, and in the monorepo case this
    // hop exists for it would throw out of a top-level await and kill the Worker.
    // A Worker also inherits the resolved values already, so skipping the hop
    // costs nothing.
    const cwd = process.cwd();
    let hopped = false;
    if (cwd !== root) {
      try {
        process.chdir(root);
        hopped = true;
      } catch {
        // Not chdir-able (a Worker). The loader will discover from cwd instead;
        // if that misses the schema, the verification pass reports it.
      }
    }
    try {
    // BOTH steps run inside the cwd hop, and that is load-bearing.
    //
    // `load()` resolves the graph. The second import hands off to the loader's OWN
    // unified entry, which installs the console / ServerResponse / Response guards,
    // applies `@encryptInjectedEnv`, and strips `@internal` keys — all gated on the
    // schema's settings. Calling those pieces individually meant nub deciding which
    // of them count, and it silently dropped the two it did not know about.
    //
    // nub cannot simply preload that entry on its own, because it resolves its
    // graph through a CLI SUBPROCESS whose `#!/usr/bin/env node` shebang re-enters
    // nub and recurses without bound (measured: 541 spawns from one plain-node
    // run). But it has a reuse fast-path: with `__VARLOCK_ENV` already populated it
    // skips the CLI entirely, which is exactly what the in-process `load()` above
    // sets up.
    //
    // That reuse check is evaluated against the CURRENT DIRECTORY. Restoring cwd
    // before this import made it reject the blob it had just been handed, re-resolve
    // from the workspace member — where there is no schema — and clobber the
    // correct values with an empty graph. Measured as a silent `null` on every
    // variable, with no error anywhere.
      await loader.load();
      await import(autoLoad);
    } finally {
      if (hopped) process.chdir(cwd);
    }
  } catch (err) {
    // A validation failure is the loader's own diagnostic and has already been
    // printed; let it terminate the run. Anything else is a nub-side wiring
    // problem the user cannot act on without being told which one it is.
    // BOTH spellings are needed. The lookup above is `createRequire().resolve()`,
    // the CommonJS resolver, which throws `MODULE_NOT_FOUND`; only the ESM
    // `import()` throws `ERR_MODULE_NOT_FOUND`. Matching just the ESM code left
    // this message dead for the case it names.
    if (err && (err.code === "MODULE_NOT_FOUND" || err.code === "ERR_MODULE_NOT_FOUND")) {
      console.error(
        `nub: found .env.schema but could not import varlock from ${resolveFrom}.\n` +
          `      Install it as a project dependency: nub add varlock`,
      );
      process.exit(1);
    }
    throw err;
  } finally {
    // Restore both for the USER's code. The scrub exists only to make the
    // loader's own subprocess safe; a `node` the application spawns afterwards
    // should still get nub's shim and preloads like any other child.
    if (savedPath !== undefined) process.env.PATH = savedPath;
    if (savedNodeOptions !== undefined) process.env.NODE_OPTIONS = savedNodeOptions;
  }
}
