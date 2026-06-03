import { fileURLToPath } from "node:url";

const greet = (name: string): string => `hello, ${name}`;
console.log(greet("nub"));
console.log("file:", fileURLToPath(import.meta.url));
