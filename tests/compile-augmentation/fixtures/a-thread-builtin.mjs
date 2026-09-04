// Does a compiled artifact skip a builtin its sealed graph cannot reach?
//
// The preload hides nub's ARGV-only V8 flags from `process.execArgv`, and reads
// the thread-scoped channel to do it — which loaded the whole threading subgraph
// (8 internal modules, ~1.4 ms) on every run of every artifact. That cost sits in
// the preamble's MODULE EVALUATION, so no call-gating reaches it: a static import
// evaluates whether or not anything calls into it. A payload the compiler proved
// cannot start a second thread has nothing to read from that channel and nobody to
// publish to, so the load is skipped and only the env-var route remains.
//
// The `nub <file>` reference column is the positive control, and the reason this
// fixture carries a `.differs`: an ordinary run makes no such proof and loads the
// subgraph. If this row ever matched the reference, the gate stopped firing.
//
// The name is spelled in two pieces on purpose. The build-time scan reads raw
// source text, comments included, and treats the contiguous name as "this payload
// might start a thread" — so a fixture that writes it out sets the flag and then
// measures the ungated path while looking green.
const loaded = process.moduleLoadList.some((m) => m.endsWith("_threads"));
console.log(`ok:threadbuiltin=${loaded ? "loaded" : "deferred"}`);
