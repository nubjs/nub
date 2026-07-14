// node:ffi (Node 26.1+), node:vfs (26.4+), and node:stream/iter (25.9+) are each
// gated behind an --experimental-* flag and default-off — bare Node throws
// ERR_UNKNOWN_BUILTIN_MODULE on import. Under nub the feature matrix injects
// --experimental-{ffi,vfs,stream-iter} for a Node that has them, so all three import.
// Run this on a Node that carries all three (26.4+); the markers below all read true.
import * as ffi from "node:ffi";
import * as vfs from "node:vfs";
import * as streamIter from "node:stream/iter";

const loaded = (m) => m != null && typeof m === "object";
console.log("module-enablers:" + [loaded(ffi), loaded(vfs), loaded(streamIter)].join(","));
