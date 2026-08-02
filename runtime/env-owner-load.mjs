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
      await loader.load();
    } finally {
      if (hopped) process.chdir(cwd);
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

    // `load()` always writes the serialized graph to __VARLOCK_ENV in PLAINTEXT.
    // The loader's own `auto-load` entry encrypts it when the schema asks for it,
    // but that entry reaches its graph through a CLI subprocess whose shebang
    // re-enters nub, so nub cannot use it. Rather than reimplement the loader's
    // encryption — which would be nub building exactly the thing it defers on —
    // say plainly that this one setting is not honoured here, since the blob is
    // inherited by every child process.
    try {
      const settings = JSON.parse(process.env.__VARLOCK_ENV || "{}").settings;
      if (settings?.encryptInjectedEnv) {
        console.warn(
          "nub: this schema sets @encryptInjectedEnv, which nub does not apply — " +
            "__VARLOCK_ENV is passed to child processes unencrypted.\n" +
            "      Use `varlock run -- nub …` if you need the encrypted blob.",
        );
      }
    } catch {
      // A blob we cannot parse is not worth failing a run over; the loader owns
      // its own format and has already validated it.
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
  }
}
