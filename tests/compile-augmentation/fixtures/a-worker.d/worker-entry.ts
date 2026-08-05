// `enum` proves a transpiler ran; the two globals prove the preamble reached the
// thread rather than only the main context.
enum E { Answer = 7 }
const has = (n: string) => ((globalThis as Record<string, unknown>)[n] !== undefined ? "Y" : "n");
console.log(`ok:enum=${E.Answer}:Temporal=${has("Temporal")}:reportError=${has("reportError")}`);
