import { val as a } from "@lib/aliased"; // tsconfig path
import { val as b } from "./ext";        // extensionless .ts
import { val as c } from "./swapped.js"; // .js -> .ts swap
console.log(`${a} ${b} ${c}`);
