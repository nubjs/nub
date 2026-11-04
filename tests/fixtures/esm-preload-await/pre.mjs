// The await settles on a timer rather than a microtask: a macrotask turn is what
// lets a pass armed too early in nub's own preload reach the entry ahead of this
// file. One line each side of the await, so where the entry landed is visible.
console.log("preload:start");
await new Promise((resolve) => setTimeout(resolve, 30));
console.log("preload:done");
