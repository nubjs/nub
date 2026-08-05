// Legacy decorators plus emitDecoratorMetadata — the DI/ORM shape. The metadata
// only exists if the transform emitted design:* keys, so this fails loudly when
// the compiled path forgets them.
import "reflect-metadata";
function Keep(_t: unknown, _k?: string, d?: PropertyDescriptor) { return d; }
class Svc { @Keep m(a: string, b: number) { return a + b; } }
const types = (Reflect as { getMetadata?: (k: string, t: object, p: string) => unknown[] })
  .getMetadata?.("design:paramtypes", Svc.prototype, "m");
console.log("ok:" + (types ? types.map((c: { name: string }) => c.name).join(",") : "MISSING"));
