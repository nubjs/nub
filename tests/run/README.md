# Standalone runner harness

End-to-end checks for the standalone runner package (`npm/run`): the packed tarballs are installed into a throwaway project and each fixture runs three ways — under `node --import <pkg>`, under `--require <pkg>` where the Node has `require(esm)`, and through the package's own `nubr` command — with stdout compared to `fixtures/expected.txt`. Install-from-tarball is the point — the addon discovery, the `@oxc-project/runtime` dependency, the flat package layout, and the `node_modules/.bin` shim are only exercised through a real install, never from the dev tree (where a sibling `runtime/addons/` masks addon-resolution bugs; one shipped that way in the first cut).

```sh
make addon-fast                                         # or any built runtime/addons/nub-native.node
tests/run/run-matrix.sh                              # host node
NODE_VERSIONS="18.19.0 22.14.0 22.15.0 26.7.0" tests/run/run-matrix.sh   # nvm-installed versions
TSX=1 tests/run/run-matrix.sh                        # differential: same fixtures under tsx
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
| `greet` | `nubr` column only: a `package.json` script whose body is `nubr main.ts`, so it also proves the command is on a script's own `PATH` |

The fixture project is `"type": "module"`, so `.ts` files with `import`/`export` are ES modules; `.cts` content must be CommonJS (`module.exports`) because the loader transpiles syntax without converting module formats.

## The `nubr` column

Four extra assertions run once per Node version, after the fixture sweep, because they compare exact strings rather than fixture stdout: `args-literal` (forwarded arguments survive the shell as literals), `opt-split` and `opt-equals` (a leading Node option in either `--x y` or `--x=y` form reaches Node instead of being mistaken for the target), and `lifecycle-env` (a script sees the manifest-derived `npm_package_*` values).

That column is what catches an entry-dispatch regression: because the fixture project is `"type": "module"`, a `.ts` entry is an ES module, and routing it through `Module.runMain` would fail on Node below 22.15 while passing everywhere else.

`nubr-args.test.mjs` beside this file covers the same argument fidelity WITHOUT an install or an addon, which is what lets CI run it on Windows and macOS as well as Linux (`node --test tests/run/nubr-args.test.mjs`). The cmd.exe escape path runs nowhere else.

## Tiers

The `--import` column is expected green on every supported Node (18.19+): 22.15+ arms sync `module.registerHooks`, older versions the `module.register` loader worker (`preload-async-hooks.mjs`). The `--require` delivery loads the arming logic through `require(esm)` and is skipped below 20.19 / 22.12.

Known, inherited from the CLI: `require()` of an ESM-syntax `.ts` from a `.cts` crashes on 22.15–22.17 inside Node's translator (`cjsCache.get(...)`, fixed upstream in Node #60380); the CLI fails identically, so the fixtures avoid that shape.
