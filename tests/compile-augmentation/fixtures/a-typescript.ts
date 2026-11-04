enum Color { Red = 1, Blue }
namespace N { export const v = 2; }
class P { constructor(public n: number) {} }
const s = { a: 1 } satisfies Record<string, number>;
function first<const T extends readonly unknown[]>(t: T) { return t[0]; }
let disposed = "no";
class R { [Symbol.dispose]() { disposed = "yes"; } }
{ using _r = new R(); }
console.log("ok:" + Color.Blue + ":" + N.v + ":" + new P(4).n + ":" + s.a + ":" + first([7]) + ":" + disposed);
