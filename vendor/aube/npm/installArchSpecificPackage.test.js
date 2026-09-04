var test = require('node:test');
var assert = require('node:assert/strict');

var childNpmEnv = require('./installArchSpecificPackage.js').childNpmEnv;

test('child npm env drops the outer allow-scripts policy', function() {
    var parentEnv = {
        npm_config_allow_scripts: '@endevco/aube',
        NPM_CONFIG_ALLOW_SCRIPTS: '@endevco/aube',
        npm_config_registry: 'https://registry.example.test',
        npm_config_global: 'true',
        NPM_CONFIG_GLOBAL: 'true',
    };

    var childEnv = childNpmEnv(parentEnv);

    assert.equal(childEnv.npm_config_allow_scripts, undefined);
    assert.equal(childEnv.NPM_CONFIG_ALLOW_SCRIPTS, undefined);
    assert.equal(childEnv.npm_config_global, 'false');
    assert.equal(childEnv.NPM_CONFIG_GLOBAL, undefined);
    assert.equal(childEnv.npm_config_registry, parentEnv.npm_config_registry);
    assert.equal(parentEnv.npm_config_allow_scripts, '@endevco/aube');
    assert.equal(parentEnv.npm_config_global, 'true');
    assert.equal(parentEnv.NPM_CONFIG_GLOBAL, 'true');
});
