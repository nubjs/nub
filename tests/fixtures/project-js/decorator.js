// A decorator in project `.js`. With no `experimentalDecorators` (Stage-3 mode,
// the default), nub must surface its branded diagnostic — proving the decorator
// trigger flags a `.js` for the transform path (NOT the verbatim skip), exactly as
// it does for `.ts`. (There is no `package.json` `type` here, so this resolves as
// CommonJS; the decorator detection runs before format matters.)
function log(target, key, desc) {
  return desc
}
class C {
  @log
  greet() {
    return "hi"
  }
}
console.log(new C().greet())
