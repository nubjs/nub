// The cases behind corpus.json: every import and export form, each printing what an
// importing module observes, mutations included. `record.mjs` runs them through
// TypeScript 5.9.3's CommonJS emit and stores the output beside them;
// crates/nub-cli/tests/ts_commonjs_corpus.rs runs them through Nub and compares.
//
// Every case gets show.cjs and an entry named main.ts. `tscRename` renames a
// binding in the sources TypeScript compiles, for the helper names it reserves and
// Nub must not: tsc rejects a file that declares one, so its observations come from
// the renamed file. `esm` replaces the rows where tsc leaves an export stale with
// what ES modules observe, which is what Nub does.

export const show = `"use strict";
function fmt(v, depth) {
  if (v === null) return "null";
  if (typeof v === "function") return "[function " + (v.name || "anonymous") + "]" + props(v, depth, false);
  if (typeof v === "string") return JSON.stringify(v);
  if (typeof v !== "object") return String(v);
  if (Array.isArray(v)) return "[" + v.map(function (x) { return fmt(x, depth + 1); }).join(", ") + "]";
  return props(v, depth, true);
}
function props(o, depth, always) {
  var keys = Object.keys(o);
  var marker = Object.prototype.hasOwnProperty.call(o, "__esModule") && keys.indexOf("__esModule") < 0;
  if (!always && keys.length === 0) return "";
  if (depth > 1) return "{...}";
  var parts = keys.map(function (k) { return k + ": " + fmt(o[k], depth + 1); });
  if (marker) parts.unshift("(__esModule)");
  return "{" + parts.join(", ") + "}";
}
module.exports = function show(label, v) { console.log(label + " = " + fmt(v, 0)); };
`;

const fnCjs = `module.exports = function fn() { return "fn"; };
module.exports.extra = "extra";
module.exports.self = function () {
  return this === module.exports ? "module" : this === globalThis ? "global" : typeof this;
};
`;

const esModuleCjs = `Object.defineProperty(exports, "__esModule", { value: true });
exports.default = "the default";
exports.named = "named";
exports.count = 0;
exports.bump = function () { exports.count++; };
`;

// Every kind of write to an exported binding. `decl` declares `x` and exports it.
const writes = (decl) => `${decl}
export function assign() { x = 1; }
export function compound() { x += 10; }
export function postfixStatement() { x++; }
export function postfixValue() { return x++; }
export function prefix() { return ++x; }
export function decrement() { return x--; }
export function logicalOr() { x = 0; x ||= 5; }
export function logicalAnd() { x &&= 6; }
export function nullish() { x = null; x ??= 7; }
export function arrayPattern() { [x] = [8]; }
export function objectShorthand() { ({ x } = { x: 9 }); }
export function objectRenamed() { ({ value: x } = { value: 10 }); }
export function arrayDefault() { [x = 11] = []; }
export function objectDefault() { ({ x = 12 } = {}); }
export function nested() { ({ deep: [x] } = { deep: [13] }); }
export function arrayRest() { [, ...x] = [0, 14]; }
export function objectRest() { ({ ...x } = { r: 15 }); }
export function forOf() { for (x of [16, 17]) {} }
export function forIn() { for (x in { 18: 1 }) {} }
export function forUpdate() { for (x = 0; x < 3; x++) {} }
export function closure() { const set = () => { x = 19; }; set(); }
export function swap() { let y = 20; [x, y] = [y, x]; return y; }
export function shadowed() { let x = 99; x = 100; return x; }
export function inExpression() { return [(x = 21), x + 1]; }
`;

const writeSteps = [
  "assign", "compound", "postfixStatement", "postfixValue", "prefix", "decrement",
  "logicalOr", "logicalAnd", "nullish", "arrayPattern", "objectShorthand",
  "objectRenamed", "arrayDefault", "objectDefault", "nested", "arrayRest",
  "objectRest", "forOf", "forIn", "forUpdate", "closure", "swap", "shadowed",
  "inExpression",
];

// tsc 5.9.3 does not copy a `for … in`/`of` head to `exports`, so after these
// writes its export (or its alias, for an `export let`) keeps the old value. In ESM
// every name reads the new one.
const staleInTsc = { forOf: "17", forIn: '"18"' };
const esmRows = (returned, width) =>
  Object.fromEntries(
    Object.entries(staleInTsc).map(([step, value]) => [
      step,
      `${step} = [${returned ? "undefined, " : ""}${Array(width).fill(value).join(", ")}]`,
    ]),
  );

const writesMain = (aliases) => `import * as lib from "./lib";
import { x${aliases.map((a) => `, ${a}`).join("")} } from "./lib";
const show = require("./show.cjs");
show("start", [x${aliases.map((a) => `, ${a}`).join("")}]);
for (const step of ${JSON.stringify(writeSteps)}) {
  const returned = (lib as any)[step]();
  show(step, [returned, x, lib.x${aliases.map((a) => `, ${a}, (lib as any).${a}`).join("")}]);
}
`;

export const cases = [
  {
    name: "default import of module.exports",
    files: {
      "fn.cjs": fnCjs,
      "main.ts": `import fn from "./fn.cjs";
const show = require("./show.cjs");
show("fn", fn);
show("call", typeof fn === "function" ? fn() : "not callable");
`,
    },
  },
  {
    name: "default imported by name",
    files: {
      "fn.cjs": fnCjs,
      "dep.cjs": esModuleCjs,
      "main.ts": `import { default as fn } from "./fn.cjs";
import { default as fn2, extra } from "./fn.cjs";
import fn3, { default as fn4 } from "./fn.cjs";
import { default as d, named } from "./dep.cjs";
const show = require("./show.cjs");
show("fn", fn);
show("fn2", [fn2, extra]);
show("fn3", [fn3, fn4]);
show("dep", [d, named]);
`,
    },
  },
  {
    name: "namespace import",
    files: {
      "fn.cjs": fnCjs,
      "dep.cjs": esModuleCjs,
      "main.ts": `import * as ns from "./fn.cjs";
import * as dep from "./dep.cjs";
const show = require("./show.cjs");
show("typeof ns", typeof ns);
show("ns", ns);
show("ns.default", (ns as any).default);
show("dep", dep);
`,
    },
  },
  {
    name: "default with named and with namespace",
    files: {
      "fn.cjs": fnCjs,
      "main.ts": `import fn, { extra, self } from "./fn.cjs";
import fn2, * as ns from "./fn.cjs";
const show = require("./show.cjs");
show("fn", [fn, extra]);
show("unbound call", self());
show("fn2", fn2);
show("ns", ns);
`,
    },
  },
  {
    name: "imports from a module with __esModule",
    files: {
      "dep.cjs": esModuleCjs,
      "main.ts": `import d, { named, count, bump } from "./dep.cjs";
import d2 from "./dep.cjs";
import * as ns from "./dep.cjs";
const show = require("./show.cjs");
show("default", [d, d2, (ns as any).default]);
show("named", named);
show("count", [count, ns.count]);
bump();
show("count after bump", [count, ns.count]);
`,
    },
  },
  {
    name: "imports from a compiled TypeScript module",
    files: {
      "lib.ts": `export default function greet() { return "hi"; }
export const named = "named";
export let counter = 0;
export function inc() { counter++; }
`,
      "main.ts": `import greet, { named, counter, inc } from "./lib";
import greet2 from "./lib";
import * as ns from "./lib";
const show = require("./show.cjs");
show("default", [greet, greet2, greet()]);
show("named", named);
show("ns", ns);
inc();
show("after inc", [counter, ns.counter]);
`,
    },
  },
  {
    name: "named imports read live and unbound",
    files: {
      "dep.cjs": `exports.count = 0;
exports.bump = function () { exports.count++; return this === exports ? "module" : typeof this; };
exports.tag = function () { return this === exports ? "module" : typeof this; };
`,
      "main.ts": `import { count, bump, tag } from "./dep.cjs";
import * as ns from "./dep.cjs";
const show = require("./show.cjs");
show("count", count);
show("bump this", bump());
show("tag this", tag\`x\`);
show("count after", [count, ns.count]);
show("namespace call this", ns.bump());
`,
    },
  },
  {
    name: "import order and side effects",
    files: {
      "a.cjs": `console.log("a runs");\n`,
      "b.cjs": `console.log("b runs");\nexports.b = "b";\n`,
      "main.ts": `console.log("main starts");
import "./a.cjs";
import { b } from "./b.cjs";
console.log("main reads", b);
`,
    },
  },
  {
    name: "unused and type-only imports",
    files: {
      "a.cjs": `console.log("a runs");\nexports.unused = 1;\n`,
      "b.cjs": `console.log("b runs");\n`,
      "c.cjs": `console.log("c runs");\n`,
      "d.cjs": `console.log("d runs");\n`,
      "e.cjs": `console.log("e runs");\n`,
      "main.ts": `import { unused } from "./a.cjs";
import type { T } from "./b.cjs";
import { type U } from "./c.cjs";
import {} from "./d.cjs";
import "./e.cjs";
console.log("main runs");
`,
    },
  },
  {
    name: "import equals and export import",
    files: {
      "fn.cjs": fnCjs,
      "lib.ts": `export import again = require("./fn.cjs");
export const own = "own";
`,
      "main.ts": `import fn = require("./fn.cjs");
import path = require("node:path");
import * as lib from "./lib";
const show = require("./show.cjs");
show("fn", fn);
show("sep", path.sep);
show("lib", lib);
`,
    },
  },
  {
    name: "export equals",
    files: {
      "obj.ts": `const value = { a: 1, b: "b" };
export = value;
`,
      "func.ts": `function named() { return 2; }
namespace named { export const prop = "prop"; }
export = named;
`,
      "main.ts": `import obj = require("./obj");
import func = require("./func");
import d from "./func";
import * as ns from "./obj";
import { a } from "./obj";
const show = require("./show.cjs");
show("obj", obj);
show("func", [func, func.prop]);
show("default import", d);
show("namespace", ns);
show("named", a);
`,
    },
  },
  {
    name: "json imports",
    files: {
      "data.json": `{ "a": 1, "list": [2, 3] }\n`,
      "main.ts": `import data from "./data.json";
import * as ns from "./data.json";
import { a } from "./data.json";
const show = require("./show.cjs");
show("default", data);
show("namespace", ns);
show("named", a);
`,
    },
  },
  {
    name: "dynamic import",
    files: {
      "fn.cjs": fnCjs,
      "dep.cjs": esModuleCjs,
      "lib.ts": `export default "lib default";
export const named = "named";
`,
      "data.json": `{ "a": 1 }\n`,
      "main.ts": `const show = require("./show.cjs");
async function main() {
  show("json", await import("./data.json"));
  show("module.exports function", await import("./fn.cjs"));
  show("__esModule", await import("./dep.cjs"));
  show("compiled TypeScript", await import("./lib"));
  const path = await import("node:path");
  show("builtin", [typeof path.join, typeof (path as any).default]);
  const specifier = "./fn.cjs";
  show("computed specifier", await import(specifier));
  show("same object", (await import("./dep.cjs")) === (await import("./dep.cjs")));
}
main();
`,
    },
  },
  {
    name: "dynamic import of an ES module",
    files: {
      "plain.mjs": `export const named = "named";\nexport default "plain default";\n`,
      "marked.mjs": `export const named = "named";\nexport default { __esModule: true, value: "value" };\n`,
      "main.ts": `const show = require("./show.cjs");
async function main() {
  const plain = await import("./plain.mjs");
  show("plain", [plain.named, plain.default]);
  const marked = await import("./marked.mjs");
  show("marked", [marked.named, (marked.default as any).value]);
}
main();
`,
    },
  },
  {
    name: "export declarations",
    files: {
      "lib.ts": `export const c = "c";
export let l = "l";
export var v = "v";
export function f() { return "f"; }
export function* g() { yield 1; }
export async function af() { return 1; }
export class C { static s = "s"; }
export let uninit: string;
export function setAll() { l = "l2"; v = "v2"; uninit = "set"; }
`,
      "main.ts": `import * as lib from "./lib";
import { l, v, uninit, setAll, C } from "./lib";
const show = require("./show.cjs");
show("keys", Object.keys(lib));
show("lib", lib);
show("class", [C.s, new C() instanceof C]);
setAll();
show("after", [l, v, uninit, lib.l, lib.v, lib.uninit]);
`,
    },
  },
  {
    name: "writes to export let",
    files: { "lib.ts": writes("export let x: any = 0;"), "main.ts": writesMain([]) },
  },
  {
    name: "writes to export var",
    files: { "lib.ts": writes("export var x: any = 0;"), "main.ts": writesMain([]) },
  },
  {
    name: "writes to a local in an export list",
    esm: esmRows(true, 2),
    files: { "lib.ts": writes("let x: any = 0;\nexport { x };"), "main.ts": writesMain([]) },
  },
  {
    name: "writes to export let with an alias",
    esm: esmRows(true, 4),
    files: {
      "lib.ts": writes("export let x: any = 0;\nexport { x as alias };"),
      "main.ts": writesMain(["alias"]),
    },
  },
  {
    name: "writes to an aliased local",
    esm: esmRows(true, 4),
    files: {
      "lib.ts": writes("let x: any = 0;\nexport { x, x as alias };"),
      "main.ts": writesMain(["alias"]),
    },
  },
  {
    name: "writes to export let aliased before its declaration",
    esm: esmRows(true, 4),
    files: {
      "lib.ts": writes("export { x as alias };\nexport let x: any = 0;"),
      "main.ts": writesMain(["alias"]),
    },
  },
  {
    name: "writes to a local exported as default",
    esm: esmRows(false, 3),
    files: {
      "lib.ts": writes("let x: any = 0;\nexport { x, x as default };"),
      "main.ts": `import * as lib from "./lib";
import d, { x } from "./lib";
const show = require("./show.cjs");
show("start", [d, x]);
for (const step of ${JSON.stringify(writeSteps)}) {
  (lib as any)[step]();
  show(step, [d, x, lib.default]);
}
`,
    },
  },
  {
    name: "reassigned exported function and class",
    files: {
      "lib.ts": `export function f() { return 1; }
export class C { static id = "C"; }
export function swap() {
  // @ts-ignore
  f = function g() { return 2; };
  // @ts-ignore
  C = class D { static id = "D"; };
}
`,
      "main.ts": `import { f, C, swap } from "./lib";
import * as lib from "./lib";
const show = require("./show.cjs");
show("before", [f(), f.name, C.id]);
swap();
show("after", [f(), f.name, C.id, lib.f.name, lib.C.id]);
`,
    },
  },
  {
    name: "default exports",
    files: {
      "d1.ts": `export default function named() { return "named"; }\n`,
      "d2.ts": `export default function () { return "anonymous"; }\n`,
      "d3.ts": `export default class Named { static id = "Named"; }\n`,
      "d4.ts": `export default class { static id = "anonymous"; }\n`,
      "d5.ts": `let value = 1;\nexport default value;\nexport function bump() { value++; }\n`,
      "d6.ts": `let value = 1;\nexport { value as default };\nexport function bump() { value++; }\n`,
      "d7.ts": `export default () => "arrow";\n`,
      "d8.ts": `export default 40 + 2;\n`,
      "d9.ts": `const obj = { a: 1 };\nexport default obj;\n`,
      "main.ts": `import d1 from "./d1";
import d2 from "./d2";
import d3 from "./d3";
import d4 from "./d4";
import d5, { bump as bump5 } from "./d5";
import d6, { bump as bump6 } from "./d6";
import d7 from "./d7";
import d8 from "./d8";
import d9 from "./d9";
import * as ns2 from "./d2";
const show = require("./show.cjs");
show("functions", [d1, d1(), d2, d2(), d7, d7()]);
show("classes", [d3, d3.id, d4, d4.id]);
show("values", [d8, d9]);
show("namespace", ns2);
bump5();
bump6();
show("snapshot and live", [d5, d6]);
`,
    },
  },
  {
    name: "enums and namespaces",
    files: {
      "lib.ts": `export enum Color { Red, Green = "g" }
export namespace NS { export const inner = "inner"; export function fn() { return inner; } }
export declare const ambient: number;
export type T = number;
export interface I { a: 1 }
enum Local { A }
export { Local };
namespace Merged { export const a = 1; }
namespace Merged { export const b = 2; }
export { Merged };
`,
      "main.ts": `import * as lib from "./lib";
import { Color, NS } from "./lib";
const show = require("./show.cjs");
show("keys", Object.keys(lib));
show("Color", [Color, Color.Red, Color[0]]);
show("NS", [NS, NS.fn()]);
show("Local", lib.Local);
show("Merged", lib.Merged);
`,
    },
  },
  {
    name: "destructuring export declarations",
    files: {
      "lib.ts": `const source = { a: 1, b: [2, 3, 4], c: { d: 5 } };
export const { a, b: [first, ...restB], c: { d } } = source;
export let { a: mutable } = source;
export const [p = "p default", q] = [undefined, "q"];
export function change() { mutable = 99; [mutable] = [100]; }
`,
      "main.ts": `import * as lib from "./lib";
import { mutable, change } from "./lib";
const show = require("./show.cjs");
show("lib", lib);
change();
show("after change", [mutable, lib.mutable]);
`,
    },
  },
  {
    name: "export star",
    files: {
      "a.ts": `export const shared = "from a";
export const onlyA = "onlyA";
export let counter = 0;
export function inc() { counter++; }
export default "a default";
`,
      "b.cjs": `exports.shared = "from b";\nexports.onlyB = "onlyB";\nexports.default = "b default";\n`,
      "fn.cjs": fnCjs,
      "lib.ts": `export * from "./a";
export * from "./b.cjs";
export * from "./fn.cjs";
export const shared = "local";
`,
      "main.ts": `import * as lib from "./lib";
const show = require("./show.cjs");
show("lib", lib);
lib.inc();
show("counter through star", lib.counter);
`,
    },
  },
  {
    name: "named re-exports from a module",
    files: {
      "a.ts": `export let value = 1;
export function inc() { value++; }
export default "a default";
`,
      "fn.cjs": fnCjs,
      "lib.ts": `export { value, inc, value as renamed } from "./a";
export { default } from "./fn.cjs";
export { default as aDefault } from "./a";
export { value as valueAgain } from "./a";
export * as ns from "./a";
export * as fnNs from "./fn.cjs";
`,
      "main.ts": `import * as lib from "./lib";
import d from "./lib";
const show = require("./show.cjs");
show("keys", Object.keys(lib));
show("default", d);
show("values", [lib.value, lib.renamed, lib.valueAgain, lib.aDefault]);
show("ns", lib.ns);
show("fnNs", lib.fnNs);
lib.inc();
show("after inc", [lib.value, lib.renamed, lib.valueAgain, lib.ns.value]);
`,
    },
  },
  {
    name: "re-exported imports",
    files: {
      "fn.cjs": fnCjs,
      "dep.cjs": esModuleCjs,
      "before.ts": `export { fn, ns, named, renamedNamed as alias, fn as fnAgain, count };
import fn from "./fn.cjs";
import * as ns from "./fn.cjs";
import { named, named as renamedNamed, count } from "./dep.cjs";
`,
      "after.ts": `import fn from "./fn.cjs";
import * as ns from "./fn.cjs";
import d, { count } from "./dep.cjs";
export { fn, ns, d, count };
`,
      "main.ts": `import * as before from "./before";
import * as after from "./after";
import { bump } from "./dep.cjs";
const show = require("./show.cjs");
show("before", before);
show("after", after);
bump();
show("count after bump", [before.count, after.count]);
`,
    },
  },
  {
    name: "circular imports",
    files: {
      "a.ts": `import { seen } from "./b";
export const constA = "constA";
export function fnA() { return "fnA"; }
export default function defA() { return "defA"; }
export class ClassA {}
export let letA = "letA";
const show = require("./show.cjs");
show("b saw", seen);
`,
      "b.ts": `import * as a from "./a";
import defA, { fnA } from "./a";
export const seen = {
  keys: Object.keys(a),
  constA: typeof a.constA,
  fnA: typeof fnA,
  defA: typeof defA,
  ClassA: typeof a.ClassA,
  letA: typeof a.letA,
};
`,
      "main.ts": `import { constA } from "./a";
console.log("main", constA);
`,
    },
  },
  {
    name: "top-level this",
    files: {
      "dep.cjs": `exports.x = "x";\n`,
      "main.ts": `import { x } from "./dep.cjs";
const show = require("./show.cjs");
show("typeof this", typeof this);
const arrowThis = () => this;
show("arrow this", typeof arrowThis());
function inner(this: any) { return this; }
show("function this", typeof inner());
show("x", x);
`,
    },
  },
  {
    name: "references in every position",
    files: {
      "fn.cjs": fnCjs,
      "lib.ts": `export let x = "x";\nexport function setX() { x = "x2"; }\n`,
      "main.ts": `import fn, { extra } from "./fn.cjs";
import { x, setX } from "./lib";
export const exported = "e";
export let mutable = "m";
const show = require("./show.cjs");
show("shorthand", { fn, extra, x, exported, mutable });
show("template", \`\${extra} \${x}\`);
show("typeof", [typeof fn, typeof extra, typeof x]);
class K { m() { return extra; } static s = extra; [extra] = 1; }
show("class", [new K().m(), K.s, Object.keys(new K())]);
function param(extra: string) { return extra; }
show("parameter shadows", param("param"));
show("default parameter", ((a = extra) => a)());
const getX = () => x;
setX();
show("live in closure", [getX(), { x }]);
mutable = "m2";
show("export let shorthand after write", { mutable });
`,
    },
  },
  {
    name: "module object shape",
    files: {
      "a.ts": `export const fromA = "fromA";\n`,
      "lib.ts": `export const c = 1;
export let l = 2;
export function f() {}
export class K {}
export default "d";
export { fromA } from "./a";
export * from "./a";
let local = 3;
export { local, local as alias };
`,
      "main.ts": `const lib = require("./lib");
const show = require("./show.cjs");
show("keys", Object.keys(lib));
show("__esModule", lib.__esModule);
show("kinds", Object.keys(lib).map((k: string) => k + ":" + ("get" in Object.getOwnPropertyDescriptor(lib, k)! ? "get" : "value")));
import "./lib";
`,
    },
  },
  {
    name: "empty export list",
    files: {
      "lib.ts": `export {};\nconsole.log("lib runs");\n`,
      "main.ts": `import * as lib from "./lib";
const show = require("./show.cjs");
show("lib", lib);
show("__esModule", (lib as any).__esModule);
`,
    },
  },
  {
    name: "string export names",
    files: {
      "lib.ts": `const value = 1;
let live = "before";
export function change() { live = "after"; }
export { value as "a-b", live as "with space" };
`,
      "main.ts": `import { "a-b" as ab, "with space" as spaced, change } from "./lib";
import * as lib from "./lib";
const show = require("./show.cjs");
show("named", [ab, spaced]);
change();
show("after change", [spaced, lib["with space"]]);
show("lib", lib);
`,
    },
  },
  {
    name: "helper name collisions",
    tscRename: {
      __importDefault: "user__importDefault",
      __importStar: "user__importStar",
      __createBinding: "user__createBinding",
      __setModuleDefault: "user__setModuleDefault",
      __exportStar: "user__exportStar",
    },
    files: {
      "fn.cjs": fnCjs,
      "b.cjs": `exports.fromB = "fromB";\n`,
      "main.ts": `import fn from "./fn.cjs";
import fn2, { extra } from "./fn.cjs";
import * as ns from "./fn.cjs";
export * from "./b.cjs";
const __importDefault = "user importDefault";
function __importStar() { return "user importStar"; }
let __createBinding = "user createBinding", __setModuleDefault = "user setModuleDefault";
var __exportStar = "user exportStar";
const __importDynamic = "user importDynamic";
const show = require("./show.cjs");
show("imports", [fn, fn2, extra, typeof ns]);
show("user bindings", [__importDefault, __importStar(), __createBinding, __setModuleDefault, __exportStar, __importDynamic]);
show("export star", require("./main").fromB);
import("./fn.cjs").then((m) => show("dynamic", typeof m.default));
`,
    },
  },
  {
    name: "generated name collisions",
    files: {
      "fn.cjs": fnCjs,
      "lib.ts": `const _a = "user _a";
const default_1 = "user default_1";
export let count = 0;
export function next() { return [count++, _a, default_1]; }
export default function () { return "anonymous"; }
`,
      "main.ts": `import fn from "./fn.cjs";
import d, { next, count } from "./lib";
const fn_cjs_1 = "user fn_cjs_1";
const lib_1 = "user lib_1";
const show = require("./show.cjs");
show("imports", [fn, d, d()]);
show("next", next());
show("count", count);
show("user bindings", [fn_cjs_1, lib_1]);
`,
    },
  },
];
