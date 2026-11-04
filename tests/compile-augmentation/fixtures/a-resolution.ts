// Resolution is where a compiled artifact can be silently WRONG rather than
// broken: a different module answers, with no error. So every import asserts the
// identity of the file that answered, not merely that something did.
import { who as alias } from "@app/alias";           // tsconfig paths
import { who as tilde } from "~lib";                 // paths → an exact file
import { who as dir } from "./src/deep";             // directory → index
import { who as ext } from "./src/ext";              // extensionless
// The `.js → .ts` rewrite is a FALLBACK for a file that does not exist, so both
// directions are pinned: with no `only.js` on disk the specifier must reach the
// `.ts`, and where a real `rewrite.js` DOES sit beside `rewrite.ts` the real file
// must win — a resolver that rewrote unconditionally would shadow it.
import { who as rewritten } from "./src/only.js";
import { who as realJs } from "./src/rewrite.js";
console.log("ok:" + [alias, tilde, dir, ext, rewritten, realJs].join("|"));
