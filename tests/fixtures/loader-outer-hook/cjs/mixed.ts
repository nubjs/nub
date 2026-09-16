import { readFileSync } from 'node:fs';
const path = require('node:path');
const n: number = 1;
console.log('mixed-ok', typeof readFileSync, typeof path.join, n);
