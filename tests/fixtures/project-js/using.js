// Project-source plain `.js` using ES2026 explicit resource management. Raw
// `using` is a SyntaxError on every supported Node's V8 (it is not yet shipped),
// so this file proves nub down-levels project `.js` through the same pipeline as
// `.ts` — the transformableSyntax skip-gate must flag it.
class Handle {
  constructor(name) {
    this.name = name;
  }
  [Symbol.dispose]() {
    console.log("close:" + this.name);
  }
}

function run() {
  using a = new Handle("a");
  using b = new Handle("b");
  console.log("open:" + a.name + "," + b.name);
}

run();
console.log("using:done");
