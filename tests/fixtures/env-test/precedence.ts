// With NODE_ENV=development, load order is:
// 1. .env.development.local (doesn't exist)
// 2. .env.local             → LOCAL_VAR, SHARED=local-wins
// 3. .env.development       → DEV_VAR, SHARED=dev-wins (but .env.local already set SHARED)
// 4. .env                   → FOO
console.log("FOO=" + process.env.FOO);
console.log("LOCAL_VAR=" + process.env.LOCAL_VAR);
console.log("DEV_VAR=" + process.env.DEV_VAR);
console.log("SHARED=" + process.env.SHARED);
