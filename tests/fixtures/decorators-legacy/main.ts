// Legacy decorators are opt-in via `experimentalDecorators: true` (in
// tsconfig.json here), matching tsc. A method decorator that uppercases the
// return value is observable proof the decorator ran with legacy semantics.
// Without the flag, decorator syntax is Stage 3 → error (see
// vanilla-ts/stage3-decorators.ts).
function upper(_target: any, _key: string, desc: PropertyDescriptor): PropertyDescriptor {
  const orig = desc.value;
  desc.value = function (...args: any[]) {
    return String(orig.apply(this, args)).toUpperCase();
  };
  return desc;
}

class Greeter {
  @upper
  greet(name: string): string {
    return "hi " + name;
  }
}

console.log("legacy-decorator:" + new Greeter().greet("world"));
