// Verify that an external env owner actually loaded the environment.
//
// nub stands down from its own `.env*` cascade the moment it sees a `.env.schema`,
// so a loader that never ran leaves the process with NO environment at all. Every
// way that happens is otherwise silent:
//
//   - the loader is installed but was never wired into the run;
//   - nub found the schema at the project root, but the loader — which searches
//     only the current directory — was invoked somewhere it could not see it;
//   - the loader's own preload failed early.
//
// ORDERING IS THE WHOLE DESIGN HERE. Node runs `--require` preloads, then each
// `--import` in argv order, then the entry module. nub's own preload is the
// fast-tier `--require`, so a check placed there would run BEFORE the loader and
// warn every single time. Deferring from there does not help either: a
// `process.nextTick` still fires before `--import` modules, and a `setImmediate`
// fires after the entry module has already run. So nub appends this as its own
// `--import`, LAST among the preload tokens — after any loader adapter, still
// before user code.
//
// Deliberately loader-agnostic: it asks "did anything load the environment?", not
// "did varlock run?". `__VARLOCK_ENV` is recognized as one signal among others so
// that a future native loader keeps this check working unchanged.

const owner = process.env.__NUB_ENV_OWNER;

// `--silent` means silent. This warning is important enough to be on by default —
// it reports a run with NO environment at all — but a user who asked for quiet
// output should not get it on stderr anyway.
const quiet = process.env.__NUB_ENV_OWNER_QUIET === "1";

if (owner && !quiet) {
  const loaded =
    // nub resolved the graph itself, out of process, via the loader CLI.
    process.env.__NUB_ENV_OWNER_LOADED === "1" ||
    // A loader ran in-process, or an outer `varlock run --` wrapped this process.
    process.env.__VARLOCK_ENV !== undefined;

  if (!loaded) {
    console.warn(
      "nub: found .env.schema, so nub did not load .env files — but " +
        `${owner} never loaded the environment, so no variables were applied.\n` +
        "      Check that it is installed, and that you are running from a " +
        "directory where it can find the schema.",
    );
  }
}
