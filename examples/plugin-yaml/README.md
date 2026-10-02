# @nubjs/plugin-yaml

Import YAML files as modules, with types.

> An example, not a published package. It shows the shape a Nub plugin takes; the specifiers below work from a `file:` install of this directory.

```ts
import config from "./config.yaml";
config.port; // number — the shape of the file, on TypeScript 7.1+
```

One package, three integrations, all through standard extension points:

| Surface | Entry | Mechanism |
| --- | --- | --- |
| Node / Nub runtime | `@nubjs/plugin-yaml/register` | `module.registerHooks` load hook (Node ≥ 22.15). `node --import @nubjs/plugin-yaml/register app.mjs`, or list it as a Nub preload. |
| TypeScript | `typescript.contentMapper` in `package.json` | A [content mapper](https://github.com/microsoft/typescript-go/pull/4712) (TypeScript ≥ 7.1). Each `.yaml` file becomes a typed module; parse errors are reported in the YAML file. |
| TypeScript < 7.1 | `register.d.ts` | Wildcard `declare module "*.yaml"` giving `Record<string, unknown>`, referenced from the same `/register` specifier. |
| Vite / Rollup | `@nubjs/plugin-yaml/vite` | A `transform` plugin producing the same `export default` module. |

## Setup

```jsonc title="tsconfig.json"
{
  "contentMappers": [
    { "package": "@nubjs/plugin-yaml", "extensions": [".yaml", ".yml"] }
  ]
}
```

```ts title="nub-env.d.ts"
/// <reference types="@nubjs/plugin-yaml/register" />
```

Run the checker with the flag content mappers require:

```sh
tsc --noEmit --runExternalCode
```

Without the flag, or on a compiler older than 7.1, the wildcard declaration applies and every value is `unknown`.

## Notes

- A YAML parse error is a mapper diagnostic, which `tsc` treats like a syntax error: semantic checking of the whole program is skipped until it is fixed, as with a syntax error in a `.ts` file.
- The emitted module is a plain object literal, so a value's type is `string` / `number` / `boolean`, not the literal. Arrays are unions of their element types.
- `parse()` throws at runtime on a malformed file. The type checker reports the same error without running anything.
