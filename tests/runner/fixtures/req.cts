// CommonJS require() of a CommonJS-content .cts with non-erasable TS.
const { twice, Mode } = require("./util-cjs.cts");
const v: number = 7;
console.log("cjs:", twice(v), Mode.A);
