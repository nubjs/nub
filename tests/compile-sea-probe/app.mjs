// The fixture the probe compiles. Every line it prints is something the probe
// compares against the same file run on plain node, so a wrong answer shows up as
// a diff rather than as a crash.
//
// `typeof Worker` is the load-bearing one: it is `function` only because nub's
// augmentation is live. A plain Node single-executable application of this same
// file prints `undefined` there, so it separates "the container works" from "the
// container works AND still carries the augmentation".
import { join } from "node:path";

const parts = [
  `argv:${process.argv.slice(2).join(",")}`,
  `worker:${typeof Worker}`,
  `join:${join("a", "b")}`,
  `env:${process.env.NUB_SEA_PROBE ?? "unset"}`,
  `platform:${process.platform}-${process.arch}`,
];
console.log(parts.join(" "));
