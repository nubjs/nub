// Requiring a TypeScript sibling is the proof that nub's hooks are already live:
// the annotation only strips through the load hook nub installs in its own
// preload. One line per execution, so a duplicate run is visible in the output.
const { tag } = require("./probe.ts");
console.log(`preload:${tag}:${process.versions.nub ? "marked" : "unmarked"}`);
