const { registerHooks } = require('node:module');
const calls = [];
registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier.endsWith('/foreign-target.cjs')) {
      calls.push({ kind: 'resolve', specifier, conditions: context.conditions });
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    if (url.endsWith('/foreign-target.cjs')) {
      calls.push({ kind: 'load', url, conditions: context.conditions });
    }
    return nextLoad(url, context);
  },
});
process.on('exit', () => console.log(JSON.stringify(calls)));
