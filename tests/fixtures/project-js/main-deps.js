// Requires two node_modules deps (a plain dep + a pnpm `.pnpm`-store dep). Both
// must load RAW — never transpiled — so their reformattable source survives and
// `using`-as-identifier stays a valid CJS binding. The marks prove the dep bodies
// ran unchanged.
const depy = require('depy')
const depz = require('depz')
console.log('DEPY:' + depy.mark + ':' + depy.shape.a + depy.shape.b)
console.log('DEPZ:' + depz.tag + ':' + depz.n.x)
