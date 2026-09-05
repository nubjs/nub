#!/usr/bin/env node
// The nub side of the capability-matched comparison: no polyfills, because a nub
// artifact supplies these globals through its preamble.
import { main } from "./cap-body.mjs";
main();
