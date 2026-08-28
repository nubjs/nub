// `using` lowering — exercises the @oxc-project/runtime helper resolution.
class R { [Symbol.dispose]() { console.log("disposed"); } }
{ using r = new R(); console.log("in scope"); }
