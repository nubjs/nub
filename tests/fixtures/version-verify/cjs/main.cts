const { greet } = require("./lib.cts") as { greet: (name: string) => string };
console.log(greet("cjs"));
module.exports = { entry: true };
