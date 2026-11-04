// Non-erasable TS (enum) + a type-only import: fails under plain node, must
// transpile under the loader.
import { greet, Color } from "./util.ts";
import type { Foo } from "./util.ts";
const x: Foo = { n: 41 };
console.log(greet("world"), Color.Red, x.n + 1);
