const isOdd = require('is-odd');
const isNumber = require('is-number');
const localDir = require('local-dir');

exports.describe = (value) => ({
  odd: isOdd(value),
  number: isNumber(value),
  local: localDir.name,
});
