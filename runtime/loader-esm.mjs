// `node --import <pkg>/esm` — arms the ESM hook surface only (tsx/esm's shape).
// `import` of TS/JSX/data formats works; a bare `require()` of the same files is
// left to Node.
import { arm } from "./loader-entry.mjs";

arm({ esm: true, cjs: false });
