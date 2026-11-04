# @nubjs/types

TypeScript ambient declarations for code authored against the Nub runtime, including its global Worker, Temporal, proposal polyfills, data-format imports, and hot-reload metadata.

## Usage

```
npm i -D @nubjs/types @types/node
```

Then in `tsconfig.json`:

```json
{ "compilerOptions": { "types": ["node", "@nubjs/types"] } }
```

Use `@types/node@26`, or `@types/node@25.9.3` at minimum. TypeScript 6 and newer use the compiler's official Temporal declarations; TypeScript 5.9 uses the matching declarations bundled with this package.
