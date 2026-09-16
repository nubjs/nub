// An enum is what Node's own type stripping cannot lower, so only a real
// transformer — nub's — can run this file.
enum Kind { One = 1 }
console.log('typed-ok', Kind.One);
