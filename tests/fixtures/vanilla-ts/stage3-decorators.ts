// TC39 Stage 3 decorators — the default when experimentalDecorators is not set
// (this fixture has no tsconfig), matching tsc.
// KNOWN GAP: V8 does not support decorator syntax natively, and oxc does not
// lower Stage 3 decorators. This file verifies the error is clear, not silent
// corruption — and that the file is not miscompiled as a legacy decorator.
//
// Legacy decorators (experimentalDecorators: true) work — tested in
// decorators-legacy/main.ts. This file tests the Stage 3 path specifically.

function log<T extends (...args: any[]) => any>(target: T, context: ClassMethodDecoratorContext) {
  return function(this: any, ...args: any[]) {
    console.log("calling:" + String(context.name));
    return target.apply(this, args);
  } as unknown as T;
}

class Greeter {
  @log
  greet(name: string): string { return "hello " + name; }
}

console.log(new Greeter().greet("world"));
