// Enums: string, numeric with reverse mapping, const, computed
enum Color { Red = "red", Green = "green" }
enum Direction { Up, Down, Left, Right }
const enum Status { Active = 1, Inactive = 2 }
enum Flags { Read = 1 << 0, Write = 1 << 1, All = (1 << 0) | (1 << 1) }

// Namespace
namespace Utils { export function double(x: number) { return x * 2; } }

// Nested namespace (A.B.C)
namespace Outer { export namespace Middle { export namespace Inner { export const value = "deeply-nested"; } } }

// Namespace merging with a class
class Validator { validate(s: string): boolean { return s.length > 0; } }
namespace Validator { export function isEmail(s: string): boolean { return s.includes("@"); } export const VERSION = "1.0"; }

// Parameter properties
class Person { constructor(public name: string, private age: number = 30) {} info() { return `${this.name}:${this.age}`; } }

console.log("enum:" + Color.Green);
console.log("reverse:" + Direction[0]);
console.log("const-enum:" + Status.Active);
console.log("computed:" + Flags.All);
console.log("namespace:" + Utils.double(21));
console.log("nested-ns:" + Outer.Middle.Inner.value);
console.log("merge-class:" + new Validator().validate("hello"));
console.log("merge-fn:" + Validator.isEmail("a@b.c"));
console.log("merge-const:" + Validator.VERSION);
console.log("param-prop:" + new Person("Alice").info());
