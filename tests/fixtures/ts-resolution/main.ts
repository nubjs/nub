import { val as a } from "@lib/aliased"; // tsconfig path
import { val as b } from "./ext";        // extensionless .ts
import { val as c } from "./dotted.ext"; // dotted extensionless .ts
import { val as d } from "./swapped.js"; // .js -> .ts swap
console.log(`${a} ${b} ${c} ${d}`);
