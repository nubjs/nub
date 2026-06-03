import type { Shape, Color } from "./type-provider.js";

const describeShape = (name: string, c: Color): string => `${name} is ${c}`;
console.log("type-only:" + describeShape("square", "red"));
console.log("type-only:no-side-effect");
