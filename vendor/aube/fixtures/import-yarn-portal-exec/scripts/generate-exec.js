fs.writeFileSync(path.join(execEnv.buildDir, 'package.json'), JSON.stringify({
  name: 'exec-pkg',
  version: '2.0.0',
  main: 'index.js',
  dependencies: {
    'is-number': '7.0.0'
  }
}));

fs.writeFileSync(path.join(execEnv.buildDir, 'index.js'), "module.exports = require('is-number')(42) ? 'exec ok' : 'exec bad';\n");
