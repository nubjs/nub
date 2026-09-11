// A user handler for `.cjs`, which Node itself never registers: its presence
// widens extensionless resolution to `sub.cjs` under plain Node, and nub — which
// wraps the handler to lower project files — must not treat the key as one it
// introduced when it retries a dependency lookup.
const { readFileSync } = require('node:fs');

require('node:module')._extensions['.cjs'] = (mod, filename) => {
  mod._compile(readFileSync(filename, 'utf8'), filename);
};
