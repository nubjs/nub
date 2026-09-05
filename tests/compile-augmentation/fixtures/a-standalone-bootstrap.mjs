// Does a payload that reaches neither of the two builtins the bootstrap loads
// eagerly (the fork one, the thread one) start WITHOUT the bootstrap preload — and
// still carry the record every bundled reader expects?
//
// The printed line is world-independent on purpose. Under `nub <file>` there is no
// compiled record and no compiled bootstrap on execArgv, so a correct artifact
// prints exactly the same line; either regression — the preload creeping back onto
// execArgv, or a record with the wrong shape or a `requireArg` naming a file that
// is not there — changes it. Several names are spelled as concatenations because
// the compiler's usage scan is a substring match over the whole source, comments
// included (one of the record's own field names contains the thread global's
// name), and writing them out would keep the preload and test the other shape.
import { existsSync } from "node:fs";

const record = process[Symbol.for("nub.compile.bootstrap")];
const preloaded = process.execArgv.some((arg) => arg.endsWith("__nub_compile_bootstrap.cjs"));
let recordState = "ok";
if (record !== undefined) {
  const prefix = "--require=";
  const requirePath =
    typeof record.requireArg === "string" && record.requireArg.startsWith(prefix)
      ? record.requireArg.slice(prefix.length)
      : null;
  const wellFormed =
    Object.isFrozen(record) &&
    Reflect.ownKeys(record).length === 5 &&
    typeof record["create" + "Require"] === "function" &&
    typeof record.getBuiltin === "function" &&
    record.needsChildProcess === false &&
    record["needsW" + "orker"] === false &&
    // `--smol` plus a small payload is the INLINE shape: the bootstrap arrives as
    // Node's `-e`, so `__filename` is `[eval]`, `requireArg` is undefined by design,
    // and there is no file to look for. Every consumer already reads a missing value
    // as "do not prepend a preload", which is the truth there. The extracted shape
    // still has to name a file that exists.
    (record.requireArg === undefined || (requirePath !== null && existsSync(requirePath)));
  if (!wellFormed) recordState = `bad:${JSON.stringify(record.requireArg ?? null)}`;
}
// The lazy accessor must still deliver a constructor on computed access.
const lazy = typeof globalThis["Wor" + "ker"];
console.log(
  `preloaded=${preloaded} record=${recordState} lazy=${lazy} env=${"__NUB_COMPILED_BOOTSTRAP" in process.env}`,
);
