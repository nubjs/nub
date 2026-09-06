const { register } = require('node:module');
const { pathToFileURL } = require('node:url');
register('./foreign-hooks.mjs', pathToFileURL(__filename));
