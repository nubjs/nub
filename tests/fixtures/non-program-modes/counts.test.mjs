// A test file that is also a plain script. Plain Node evaluates its body once when
// it runs as the program or under `--test` (in the runner's child), and never under
// `--check` or as an argument after `-e`. The line marks each evaluation.
import test from "node:test";

console.log("BODY-RAN");

test("noop", () => {});
