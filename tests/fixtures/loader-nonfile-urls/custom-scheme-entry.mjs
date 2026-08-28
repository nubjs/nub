import { register } from "node:module";

register("./custom-scheme-loader.mjs", import.meta.url);

const mod = await import("custom://virtual/index.js");
console.log("LOADED", mod.default);
