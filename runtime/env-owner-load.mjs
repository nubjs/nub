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

// Two DIFFERENT directories, and conflating them breaks monorepos:
//   root        — where .env.schema lives, i.e. what the loader must discover from
//   resolveFrom — the nearest project root, i.e. where its package is installed
// Under an isolated node_modules layout (nub's default linker) a member's
// dependencies sit in <member>/node_modules while the schema sits at the
// workspace root, so resolving from `root` would miss the package entirely.
const root = process.env.__NUB_ENV_OWNER_ROOT;
const resolveFrom = process.env.__NUB_ENV_OWNER_RESOLVE_FROM || root;
if (root) {
  try {
    // Resolve the loader from the USER'S PROJECT, not from here. This module
    // lives in nub's install dir, outside the project's node_modules, so a bare
    // `import "varlock"` resolves against nub and fails with ERR_MODULE_NOT_FOUND.
    const req = createRequire(pathToFileURL(`${resolveFrom}/package.json`));
    const loader = await import(pathToFileURL(req.resolve("varlock")).href);

    // The loader discovers its schema from the CURRENT DIRECTORY only, with no
    // ancestor walk, so a workspace member would otherwise miss a root schema.
    // Hopping cwd is safe here and nowhere else: preloads run before any user
    // code, so nothing else can observe the process-global cwd mid-load.
    const cwd = process.cwd();
    const hop = cwd !== root;
    if (hop) process.chdir(root);
    try {
      await loader.load();
    } finally {
      if (hop) process.chdir(cwd);
    }

    // Install exactly the protections the loader installs for itself — its own
    // `auto-load` entry calls these three, in this order, and each is internally
    // gated on the schema's own settings (`@redactLogs`, `@preventLeaks`), so a
    // project that turned one off still gets what it asked for. nub adds no
    // redaction of its own and makes no judgement about which of these to run.
    //
    // Optional-called: an older or newer loader may not export all three, and a
    // missing protection should not abort the run.
    //
    // This covers only THIS process. Stream-level redaction — raw
    // `process.stdout.write`, and anything a subprocess prints — is what
    // `varlock run --` adds by piping the child's stdio, and is out of scope
    // here by design.
    loader.patchGlobalConsole?.();
    loader.patchGlobalServerResponse?.();
    loader.patchGlobalResponse?.();
  } catch (err) {
    // A validation failure is the loader's own diagnostic and has already been
    // printed; let it terminate the run. Anything else is a nub-side wiring
    // problem the user cannot act on without being told which one it is.
    if (err && err.code === "ERR_MODULE_NOT_FOUND") {
      console.error(
        `nub: found .env.schema but could not import varlock from ${resolveFrom}.\n` +
          `      Install it as a project dependency: nub add varlock`,
      );
      process.exit(1);
    }
    throw err;
  }
}
