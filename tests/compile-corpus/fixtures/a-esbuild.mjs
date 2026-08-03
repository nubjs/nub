// A package whose native payload is an EXECUTABLE it spawns, not an addon it
// dlopens. Distinct from every other fixture here: nothing loads a `.node`, so
// what has to survive the payload round-trip is the file's executable bit.
import * as esbuild from "esbuild";
const out = await esbuild.transform("const x: number = 1; export default x;", { loader: "ts" });
console.log("ok:" + (out.code.includes("1") ? 1 : 0));
