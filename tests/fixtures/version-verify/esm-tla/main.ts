import { value } from "./mod.ts";

const resolved: number = await Promise.resolve(value + 1);
console.log(`tla: ${resolved}`);
