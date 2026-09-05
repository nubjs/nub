// A native addon sitting beside the entry, in a package that declares
// `"type": "module"`.
//
// The compiler loads an embedded addon through a shim it GENERATES, and that shim
// is CommonJS — `module.exports = …`. The id it is served under is the `.node`
// file, whose owning manifest here is the application's, so the application's own
// `"type"` decides how the compiler's own module gets classified. An ESM package
// once made it read that CommonJS as ESM: `module` was left unbound and every
// artifact carrying an addon died on startup with ERR_AMBIGUOUS_MODULE_SYNTAX,
// while an addon under a package with no `"type"` kept working. Both halves are
// needed to reach it, which is why this fixture brings its own manifest.
//
// A `.cjs` entry rather than an ESM one so the `require` is Node's own. The ESM
// spelling of this is `require = createRequire(import.meta.url)`, an assignment
// the addon plugin BLANKS so the call it leaves behind is a real bundler-visible
// `require` — a `const` declaration is not that, and the specifier then resolves
// to nothing at build time.
const addon = require("./nub-native.node");

console.log("addon", addon.parseJson5("{answer:42}").answer);
