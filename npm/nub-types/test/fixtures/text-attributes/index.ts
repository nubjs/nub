// TEXT-ATTRIBUTES fixture (TypeScript 7.1+) — the documented `type: "text"` declaration
// types every text import as a string, and outranks @nubjs/types' `*.yaml` wildcard
// only when the attribute is present.
// Expected: tsc --noEmit exits 0.

import readme from "./README.md" with { type: "text" };
import rawYaml from "./config.yaml" with { type: "text" };
import parsedYaml from "./config.yaml";

type Equals<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;

const readmeIsString: Equals<typeof readme, string> = true;
const rawYamlIsString: Equals<typeof rawYaml, string> = true;
const parsedYamlIsRecord: Equals<typeof parsedYaml, Record<string, unknown>> = true;

export { readmeIsString, rawYamlIsString, parsedYamlIsRecord };
