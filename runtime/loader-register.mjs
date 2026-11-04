// `node --import <pkg>` — arms BOTH module systems (ESM hooks + CommonJS
// require() augmentation), tsx's default-entry shape.
import { arm } from "./loader-entry.mjs";

arm({ esm: true, cjs: true });
