// The loader must NOT clobber polyfill packages the way the CLI runtime does:
// it installs no globals, so a user's real installed package has to load — on
// BOTH tiers (the compat tier's loader worker holds its own transform-core
// instance; the clear is carried there via module.register data).
import { Temporal } from "@js-temporal/polyfill";
console.log(typeof Temporal?.Now?.instant?.().epochMilliseconds);
