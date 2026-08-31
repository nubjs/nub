# Standalone loader harness

End-to-end checks for the standalone loader package (`npm/loader`): the packed tarballs are installed into a throwaway project and each fixture runs under `node --import <pkg>` (plus `--require <pkg>` where the Node has `require(esm)`), with stdout compared to `fixtures/expected.txt`. Install-from-tarball is the point — the loader's addon discovery, its `@oxc-project/runtime` dependency, and the flat package layout are only exercised through a real install, never from the dev tree (where a sibling `runtime/addons/` masks addon-resolution bugs; one shipped that way in the first cut).

```sh
make addon-fast                                         # or any built runtime/addons/nub-native.node
tests/loader/run-matrix.sh                              # host node
NODE_VERSIONS="18.19.0 22.14.0 22.15.0 26.7.0" tests/loader/run-matrix.sh   # nvm-installed versions
TSX=1 tests/loader/run-matrix.sh                        # differential: same fixtures under tsx
```

## Fixtures

| Fixture | Exercises |
| --- | --- |
| `main.ts` | non-erasable TS (`enum`) and a type-only import — fails under plain `node`, so a pass proves the loader transpiled |
| `paths.ts` | tsconfig `paths`, an extensionless import, a YAML data import |
| `req.cts` | CommonJS `require()` of a CommonJS-content `.cts` with an `enum` |
| `using.ts` | `using` lowering — resolves the `@oxc-project/runtime` helpers from the package's real dependency |
| `worker-main.ts` | a worker thread inheriting the preload and transpiling its own `.ts` entry |
| `clobber.ts` | a real installed `@js-temporal/polyfill` must load, not the CLI's synthetic global re-export — covers the clear on both tiers |

The fixture project is `"type": "module"`, so `.ts` files with `import`/`export` are ES modules; `.cts` content must be CommonJS (`module.exports`) because the loader transpiles syntax without converting module formats.

## Tiers

The `--import` column is expected green on every supported Node (18.19+): 22.15+ arms sync `module.registerHooks`, older versions the `module.register` loader worker (`preload-async-hooks.mjs`). The `--require` delivery loads the arming logic through `require(esm)` and is skipped below 20.19 / 22.12.

Known, inherited from the CLI: `require()` of an ESM-syntax `.ts` from a `.cts` crashes on 22.15–22.17 inside Node's translator (`cjsCache.get(...)`, fixed upstream in Node #60380); the CLI fails identically, so the fixtures avoid that shape.
