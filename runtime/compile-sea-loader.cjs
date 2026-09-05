// The second half of a compiled artifact's single-executable main.
//
// `nub compile` concatenates compile-bootstrap.cjs and this file, substitutes the
// `__NUB_SEA_*__` placeholders, and stores the result as the SEA blob's `main`.
// Node runs it as CommonJS through its embedder loader, before any ESM in the
// process, with the bootstrap's frozen builtin accessors already published.
//
// It looks like the inline (`no-extract`) loader and is a different design in the
// one place that decides the artifact's start time. The inline shape has no
// choice: with nothing on disk and no hook API on its floor, each chunk has to
// become a `data:` URL. Inside a SEA there IS a choice, and taking the same one
// costs 8.3 ms on an 60 KB chunk — a base64 encode of the source, a `data:` URL
// parse, and a base64 decode back, none of which any cache covers. So the chunks
// are served through `module.registerHooks` at ordinary `file:` URLs instead, and
// three things follow, measured on Linux against plain `node` running the same
// source (baseline spread 1.49 ms):
//
//   * `data:` URL loader                            +12.14 ms
//   * hook-served `file:` URLs, `getRawAsset`        +3.81 ms
//   * ... plus `enableCompileCache`                  +0.53 ms
//
// The last row is parity: a plain SEA carrying no nub code at all measures
// -0.70 ms on the same instrument. `getRawAsset` is worth 1.0 ms of that on its
// own — module hooks accept an ArrayBuffer as `source`, so the blob is decoded
// where it is mapped and never copied into a JavaScript string.
//
// A real URL also buys capability the inline shape cannot have: cross-chunk
// relative specifiers resolve against it, so there is no specifier-substitution
// pass; a source map can attach to it; and a Worker can be pointed at one.
//
// The one thing a SEA takes away is the main's own `import()`, which is the
// EMBEDDER's and resolves builtins only — see the synthetic module at the bottom.

(() => {
  const boot = process[Symbol.for("nub.compile.bootstrap")];
  const sea = boot.getBuiltin("node:sea");
  const Module = boot.getBuiltin("node:module");
  const fs = boot.getBuiltin("node:fs");

  const ENTRY = "__NUB_SEA_ENTRY__";
  // Every payload file the module loader may be asked for, by payload name.
  const FILES = __NUB_SEA_FILES__;
  // Whether this Node's `localStorage` getter throws without a storage file, and
  // so has to be neutralized by the preamble. The launcher sets the same signal in
  // the child's environment; here the artifact sets it on itself, because a SEA
  // has no parent process to be handed anything by.
  const NEUTRALIZE_LOCALSTORAGE = __NUB_SEA_NEUTRALIZE_LOCALSTORAGE__;
  // This artifact's subdirectory under nub's cache root, or "" to leave the
  // compile cache off. The same mechanism the extracted shape drives through
  // `NODE_COMPILE_CACHE`, and worth 3.3 ms: without it every chunk is compiled
  // from source on every start.
  const COMPILE_CACHE_KEY = "__NUB_SEA_COMPILE_CACHE__";
  // The virtual root every chunk reports as its own location — identical to the
  // inline shape's, and for the same reason. See `compile::inline::VIRTUAL_ROOT`
  // for why it carries a drive letter.
  const ROOT = "file:///N:/$nub/";

  // Release CI's embedded-notice gate, on the same private environment channel the
  // launcher uses. It rides an environment variable rather than a reserved
  // argument spelling so a compiled app keeps its whole argv surface — no flag a
  // publisher might already use is intercepted, at any argument count. A SEA's
  // argv with no application arguments is [execPath, execPath].
  if (process.env.__NUB_COMPILED_LAUNCHER_MODE === "licenses" && process.argv.length === 2) {
    process.stdout.write(Buffer.from(sea.getRawAsset("__nub_node_license")));
    return;
  }

  // A SEA's argv is already [execPath, ...userArgs]: Node repeats argv[0] at
  // position 1 in place of the missing entry path (`FixupArgsForSEA`), which puts
  // the artifact exactly where a program expects its own path. The inline shape
  // has to splice it in; here there is nothing to do, and `process.execArgv`
  // holds the real flags because Node parsed them out of the blob.

  // The build could only ask whether this Node's `localStorage` throws WITHOUT a
  // storage file, because a blob is written once and the user's `NODE_OPTIONS`
  // does not exist yet. The launcher recomputes it per run from the real
  // environment; here the run itself answers, which is better than either — the
  // getter is the property, so reading it cannot drift from Node's own accepted
  // spellings of `--localstorage-file`. Without a file it throws and the preamble
  // must remove it; with one it returns a working Storage and must not. Measured:
  // an artifact on Node 22.20 deleted a `localStorage` that
  // `NODE_OPTIONS=--localstorage-file=…` had made work.
  let neutralize = NEUTRALIZE_LOCALSTORAGE;
  if (neutralize) {
    try {
      void globalThis.localStorage;
      neutralize = false;
    } catch {
      // Still the throwing getter, so the baked answer stands.
    }
  }
  if (neutralize) process.env.__NUB_NEUTRALIZE_LOCALSTORAGE = "1";
  // Set or REMOVED, never inherited: a sealed artifact launched from an armed nub
  // process must not take its parent's runtime-V8 signal. Every SEA payload is
  // sealed — an unsealed one stays on the launcher — so the removal is
  // unconditional here, where the launcher has to compute the set first.
  delete process.env.__NUB_RUNTIME_V8_FLAGS;
  delete process.env.__NUB_ARGV_ONLY_FLAGS;

  if (COMPILE_CACHE_KEY) {
    try {
      // nub's cache root, as `node::discovery::cache_dir` resolves it: the XDG
      // variable when set, else `~/.cache/nub` — which is also what the Windows
      // branch uses for an ordinary user profile. Deliberately NOT a full mirror
      // of that function: its remaining branch is the Windows SYSTEM-account
      // fallback, and reproducing it here would put a second copy of a rule that
      // only exists to pick a WRITABLE directory. Passing `undefined` when the
      // root cannot be determined hands Node its own default under `os.tmpdir()`,
      // and an unwritable directory throws into the catch below — so every way
      // this can be wrong costs milliseconds and nothing else. Forward slashes
      // are accepted on Windows, so one join serves both.
      const root = process.env.XDG_CACHE_HOME
        ? `${process.env.XDG_CACHE_HOME}/nub`
        : process.env.HOME || process.env.USERPROFILE
          ? `${process.env.HOME || process.env.USERPROFILE}/.cache/nub`
          : undefined;
      Module.enableCompileCache(root === undefined ? undefined : `${root}/${COMPILE_CACHE_KEY}`);
    } catch {
      // An unwritable cache directory is not a reason to refuse to start: every
      // chunk still compiles from source, exactly as a first run does.
    }
  }

  const files = new Set(FILES);
  // An absolute `ROOT` URL names a payload file and nothing else, so it needs no
  // parent to disambiguate. A bare `./name` does: these hooks are process-global,
  // and a module loaded from DISK whose own relative import happens to spell a
  // payload name would otherwise be handed the embedded file instead. Nothing can
  // reach that today — every route to a disk module (an `--external` package, a
  // retained computed `import()`, a computed `require`) also refuses this
  // container — but the scoping is what makes that safety local rather than a
  // consequence of an unrelated eligibility rule.
  const nameOf = (specifier, parentURL) => {
    if (specifier.startsWith(ROOT)) return specifier.slice(ROOT.length);
    if (specifier.startsWith("./") && (parentURL ?? "").startsWith(ROOT)) {
      return specifier.slice(2);
    }
    return null;
  };
  // `.cjs` is the extension every generated CommonJS support file carries, and
  // the payload names are nub's own, so the extension is a reliable format tag
  // here in a way it would not be for arbitrary user files.
  const formatOf = (name) => (name.endsWith(".cjs") ? "commonjs" : "module");

  Module.registerHooks({
    resolve(specifier, context, next) {
      const name = nameOf(specifier, context.parentURL);
      if (name !== null && files.has(name)) {
        return { url: ROOT + name, format: formatOf(name), shortCircuit: true };
      }
      return next(specifier, context);
    },
    load(url, context, next) {
      // A load URL is always the absolute one `resolve` returned, so the parent is
      // not consulted and cannot be.
      const name = nameOf(url, ROOT);
      if (name !== null && files.has(name)) {
        // The ArrayBuffer straight out of the mapped blob. Node decodes it in
        // C++; handing it a string instead costs a copy of the whole chunk.
        return { source: sea.getRawAsset(name), format: formatOf(name), shortCircuit: true };
      }
      return next(url, context);
    },
  });

  // The backstop for the one thing the build-time scan cannot promise.
  //
  // Which payloads reach `child_process`/`cluster` is decided by reading the
  // emitted chunks for what they RESOLVE, and that decision keeps such a payload
  // on the launcher, which has a real Node to hand a fork. The scan is syntactic,
  // so it is a heuristic and not a proof: it follows a require through a rename
  // and through an alias binding, and it cannot follow one passed as an argument,
  // stored on an object, or picked out of an array. Completing it would take
  // interprocedural dataflow over the whole bundle.
  //
  // What matters is therefore not that the scan is complete but what happens when
  // it is wrong, and without this the answer was the worst available. A fork here
  // spawns `process.execPath`, which is the artifact; Node discards a
  // single-executable's `argv[1]` (`FixupArgsForSEA`), so the child re-runs the
  // whole application and forks again. Measured: a two-line fixture printed its
  // first line until it was killed. One line of guard turns that into an error
  // naming the cause, at the call, with a stack.
  //
  // Installed HERE rather than in the bootstrap because it is true of this
  // container alone, and before the entry because Node's own
  // `internal/cluster/primary` destructures `fork` into a module-local const the
  // first time cluster is required — which is inside the application's graph, so
  // a patch applied later would never reach it.
  //
  // It is a runtime patch and therefore revocable, which bounds what it can be:
  // a `NODE_OPTIONS` preload runs BEFORE this main and can keep the original
  // function or hand it back afterwards. That limit is not closable — it is true
  // of every runtime patch in every JavaScript program — and the alternative of
  // keeping any payload that MIGHT fork on the launcher is the same undecidable
  // question the scan already answers as well as it can. What is closable is the
  // one route a preload takes without meaning to, and the branch below takes it.
  {
    const childProcess = boot.getBuiltin("node:child_process");
    const refuse = (what) => (modulePath) => {
      throw new Error(
        `${what}(${JSON.stringify(String(modulePath))}) cannot run in this executable: the ` +
          "child would re-run the whole application instead of that module. The build is " +
          "meant to detect a program that forks and produce a different kind of executable, " +
          "so this is a bug worth reporting.",
      );
    };
    childProcess.fork = refuse("child_process.fork");

    // The one case where replacing `child_process.fork` is already too late. A
    // preload that loaded `node:cluster` gave `internal/cluster/primary` its
    // module-local `fork` before this ran, and nothing reaches that const —
    // but every use of it goes through `cluster.fork`, which is still ours to
    // take. Conditional because reading the module would otherwise LOAD it,
    // which costs every artifact a builtin it does not use and would capture
    // the original itself.
    if (process.moduleLoadList.some((entry) => entry.endsWith("internal/cluster/primary"))) {
      boot.getBuiltin("node:cluster").fork = refuse("cluster.fork");
    }
  }

  // `nub compile`'s build-time self-check, the counterpart to the launcher's
  // probe mode and deliberately the LAST thing before the app would start: every
  // line above has already run, so reaching here proves Node accepted the blob,
  // found the main, and got the whole loader installed. Reading the entry asset
  // proves the chunks are in the blob and reachable through the official
  // `getRawAsset` — which is what a drift in Node's blob layout would break, and
  // what nothing static can check, because the layout is Node's rather than ours.
  //
  // Placed after the hooks rather than beside the licenses gate for exactly that
  // reason: an early return would prove only that Node ran SOMETHING.
  if (process.env.__NUB_COMPILED_LAUNCHER_MODE === "probe" && process.argv.length === 2) {
    const bytes = sea.getRawAsset(ENTRY);
    process.stdout.write(`nub-probe ok ${ENTRY} ${bytes.byteLength}\n`);
    return;
  }

  // The one line a SEA needs that no other shape does. `import()` from here is the
  // embedder's and throws ERR_UNKNOWN_BUILTIN_MODULE for anything that is not a
  // builtin — the hooks above are irrelevant to it, because the rejection happens
  // before resolution. A module compiled through the REAL CommonJS loader gets the
  // ordinary dynamic-import callback, which goes through them.
  const shim = new Module(ROOT + "__nub_sea_entry", null);
  shim.filename = ROOT + "__nub_sea_entry";
  shim.paths = [];
  shim._compile(`module.exports = import(${JSON.stringify(ROOT + ENTRY)});`, shim.filename);

  // The promise has to be OBSERVED, because Node decides two things by watching
  // its own entry module's evaluation and this import is not that. Measured
  // against `nub app.mjs` on the same file: an entry ending in an unsettled
  // top-level await exited 0 rather than 13, and a throwing entry under
  // `--unhandled-rejections=warn` exited 0 rather than 1.
  // Both the exit code and the diagnostic are raised from `exit`, because that is
  // the ONLY point at which an entry can be called unsettled. `beforeExit` runs
  // again every time a listener schedules work, so no number of rounds is a proof
  // — and this loader's listener would necessarily run before the application's,
  // which is where an entry awaiting something resolved from `beforeExit` gets
  // settled. Measured, both shapes: an entry resolved on the first such round and
  // one resolved on the second each exit 0 in silence.
  //
  // The line is therefore written to fd 2 rather than emitted. `process.emitWarning`
  // queues its write behind a tick, so one emitted from an `exit` listener is
  // composed and dropped; a synchronous write is also exactly what Node does for
  // this same diagnostic in `ModuleWrap::…`.
  //
  // Node gates its copy on `env()->options()->warnings`, which is the parsed
  // `--no-warnings` and nothing else — `process.noProcessWarnings` is a read-only
  // alias of exactly that option (`pre_execution.js::addReadOnlyProcessAlias`),
  // present since Node 10 and so below every version either shape supports.
  //
  // Reading the option rather than anything downstream of it is what makes this
  // right in the two places a proxy is not. `NODE_NO_WARNINGS=1` removes Node's
  // own `warning` listener but does NOT suppress this diagnostic — measured, the
  // control still prints it — so listener state answers a different question; and
  // a `--require` preload can add a listener of its own before either loader runs.
  const warningsEnabled = !process.noProcessWarnings;

  let settled = false;
  const unsettled = () => {
    if (settled || process.exitCode !== undefined) return;
    process.exitCode = 13;
    if (warningsEnabled) {
      fs.writeSync(2, `Warning: Detected unsettled top-level await at ${ROOT}${ENTRY}\n`);
    }
  };
  const done = () => {
    settled = true;
    process.off("exit", unsettled);
  };
  process.on("exit", unsettled);
  shim.exports.then(
    () => {
      done();
    },
    (error) => {
      done();
      // Rethrown rather than reported, because a failed ESM ENTRY is an uncaught
      // exception in Node and not an unhandled rejection — so it must fail the
      // process whatever `--unhandled-rejections` says, and it must still reach an
      // `uncaughtException` handler the application installed.
      process.nextTick(() => {
        throw error;
      });
    },
  );
})();
