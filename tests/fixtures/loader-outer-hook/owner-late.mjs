// Installs the `.ts` handler AFTER nub's compat-tier preload: an `--import` runs
// after every `--require`, which is where `--import tsx` (the shape mocha and
// vitest document) lands. Ownership nub decided at start-up does not cover it.
import { createRequire } from 'node:module';

createRequire(import.meta.url)('./owner-cjs.cjs');
