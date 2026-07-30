---
**Status:** v1, 2026-05-24. Write-once research doc.
**Question:** Should Nub support TypeScript's `emitDecoratorMetadata` (the legacy `experimentalDecorators` form that emits `Reflect.metadata("design:*", …)` calls into the transpile output) in v0.1? If yes, can we ship it on oxc today, or are we blocked on upstream?
**Headline answer:** Yes — ship it in v0.1. The non-erasable-syntax plan already commits to it (`../runtime/non-erasable-syntax.md` §"What we ship"), oxc-transformer has shipped the emission path since [`oxc-project/oxc#8614`](https://github.com/oxc-project/oxc/pull/8614) merged 2025-02-09 (refined through 2025-04 in [`#10632`](https://github.com/oxc-project/oxc/pull/10632)/[`#10633`](https://github.com/oxc-project/oxc/pull/10633)), and the alternative (silently break NestJS / TypeORM / class-validator / InversifyJS / Typegoose / Angular-JIT for any project Nub touches) writes Nub out of the entire server-side TypeScript ecosystem. `@nestjs/core` alone is ~8.4M downloads/week and is hard-locked to legacy decorators + `reflect-metadata` for the foreseeable future; Stage 3 decorator metadata (`Symbol.metadata`) is real but is **not** a drop-in for `design:type` / `design:paramtypes` — the TypeScript team explicitly rejected wiring the legacy type-reflection emit into the new metadata channel ([`microsoft/TypeScript#57533`](https://github.com/microsoft/TypeScript/issues/57533)). The load-bearing caveat is the oxc long-tail: types that require full type inference fall back to `Object` (per [oxc docs](https://oxc.rs/docs/guide/usage/transformer/typescript)), which matches `tsc`'s behavior for *external/imported* types but diverges on a small set of *intra-file* type-alias / mapped-type / conditional-type cases that `tsc` resolves and oxc doesn't. We accept that divergence, document it, and tell users to fall back to explicit `@Reflect.metadata("design:paramtypes", [...])` annotations when bitten. Same posture as oxc's own docs.
**Builds on:** [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md), [`tsgo-vs-oxc-for-transpile.md`](tsgo-vs-oxc-for-transpile.md), [`tsx-architecture.md`](tsx-architecture.md), [`node-swc-vs-oxc-choice.md`](node-swc-vs-oxc-choice.md), and [`AGENTS.md`](../../AGENTS.md) for the augmenter-not-fork and additivity rules.
---

# `emitDecoratorMetadata` support in Nub

## 1. TL;DR

- **Decision: support `emitDecoratorMetadata` in v0.1 (option A).** Already committed to in `non-erasable-syntax.md` §"What we ship"; this doc supplies the rationale and verifies oxc's shipped status.
- **oxc-transformer has shipped legacy-decorator + metadata emission.** PR [`#8614`](https://github.com/oxc-project/oxc/pull/8614) (merged 2025-02-09) added the legacy decorator transform; subsequent PRs ([`#10632`](https://github.com/oxc-project/oxc/pull/10632), [`#10633`](https://github.com/oxc-project/oxc/pull/10633), April 2025) fixed the type-reference fallback. The `decorator.legacy` + `decorator.emitDecoratorMetadata` options on `oxc-transform` are stable, documented, and used in production by Vite/Rolldown adopters.
- **One bounded divergence vs. `tsc`: oxc falls back to `Object` for types that require type inference.** Matches `tsc`'s behavior for externally-resolved type references; diverges on some intra-file cases. Users can pin via explicit `@Reflect.metadata("design:paramtypes", [...])`. Same divergence shape as `swc` with `decoratorMetadata: true` (see [`swc-project/swc#6824`](https://github.com/swc-project/swc/issues/6824) on the union-with-null edge case).
- **The runtime polyfill (`reflect-metadata` / `core-js/proposals/reflect-metadata`) stays user-owned.** Nub does **not** auto-inject — that would mutate `globalThis.Reflect` and violate the additivity policy (`../philosophy.md#additivity`). Same posture as `tsc`, `swc`, Bun, ts-node.
- **Stage 3 decorator metadata (`Symbol.metadata`, TC39 [proposal-decorator-metadata](https://github.com/tc39/proposal-decorator-metadata) Stage 3 since Nov 2023) is NOT a substitute.** It provides a per-class metadata bag for decorator authors to write into; it does not emit runtime type information from the type system. TypeScript explicitly declined to bridge the two ([`microsoft/TypeScript#57533`](https://github.com/microsoft/TypeScript/issues/57533)). NestJS 10/11 (current as of May 2026) still require legacy + `reflect-metadata`; no Stage-3-aligned NestJS major has shipped.

## 2. What `emitDecoratorMetadata` actually does

### 2.1 The three keys

When `experimentalDecorators: true` and `emitDecoratorMetadata: true` are both set in `tsconfig.json`, `tsc` (and any conforming transpiler) wraps each decorated class / method / property / parameter with calls to `Reflect.metadata(key, value)` inline at the decorator call site. The three keys it emits are:

| Key | Value | Where applied |
|-----|-------|---------------|
| `"design:type"` | The constructor function for the property's TypeScript type (`String`, `Number`, `Boolean`, the class itself, or `Object` if not resolvable) | Property and method decorators |
| `"design:paramtypes"` | An array of constructor functions for each parameter type | Method and class (constructor) decorators |
| `"design:returntype"` | The constructor function for the method's return type (or `void 0`) | Method decorators |

Source: [TypeScript: TSConfig Option emitDecoratorMetadata](https://www.typescriptlang.org/tsconfig/emitDecoratorMetadata.html).

### 2.2 Concrete emit shape

Given source:

```ts
function LogMethod(target: any, propertyKey: string, descriptor: PropertyDescriptor) {}

class Demo {
  @LogMethod
  foo(bar: number) {}
}
```

`tsc` with `emitDecoratorMetadata: true` emits (simplified):

```js
__decorate([
  LogMethod,
  __metadata("design:type", Function),
  __metadata("design:paramtypes", [Number]),
  __metadata("design:returntype", void 0)
], Demo.prototype, "foo", null);
```

where `__metadata` is a thin helper that calls `Reflect.metadata(k, v)` if `Reflect.metadata` is a function (i.e. if `reflect-metadata` is loaded), otherwise no-ops.

### 2.3 Runtime polyfill

`Reflect.metadata` is **not** a built-in JS surface. It existed in an early TC39 metadata proposal that did not advance; the surviving implementation is the npm package [`reflect-metadata`](https://www.npmjs.com/package/reflect-metadata) (Microsoft) or the equivalent under `core-js/proposals/reflect-metadata`. The user installs it and imports it once, at the top of their entry file, before any decorated class is loaded:

```ts
import "reflect-metadata";
import { AppModule } from "./app.module";
// …
```

Without that import, the emitted `__metadata` calls all no-op — silently — and frameworks that depend on the metadata (NestJS, TypeORM, class-validator, etc.) fail at runtime with errors like `ColumnTypeUndefinedError` or "Cannot resolve dependencies of …".

### 2.4 Why it's `experimentalDecorators`-only

`emitDecoratorMetadata` is bound to the legacy decorator semantics. Stage 3 decorators (TC39 [proposal-decorators](https://github.com/tc39/proposal-decorators), Stage 3, shipped in TypeScript 5.0) have a different decorator signature: instead of `(target, key, descriptor)` they get `(value, context)` where `context.metadata` is a plain object the decorator can write into. There is **no emission pathway** for `design:type` / `design:paramtypes` / `design:returntype` in the Stage 3 form. The TypeScript team's own announcement says: *"This new decorators proposal is not compatible with `--emitDecoratorMetadata`, and it does not allow decorating parameters. Future ECMAScript proposals may be able to help bridge that gap."* See §6 for the Stage 3 decorator-metadata alternative.

## 3. Transpiler support matrix (verified May 2026)

| Transpiler | `experimentalDecorators` | `emitDecoratorMetadata` | Notes / links |
|------------|--------------------------|--------------------------|---------------|
| **`tsc` (TypeScript 5.x / 6.x)** | ✓ | ✓ | Reference implementation. Status: full, stable. |
| **`tsgo` / `@typescript/native-preview` (TS 7.0 Beta nightly)** | ✓ | ✓ | Status table: "Emit (JS output): done." Stage 3 decorator transform added in PR [`#2926`](https://github.com/microsoft/typescript-go/pull/2926). Same code path as `tsc` by construction. Unusable for Nub's load hook regardless: programmatic API status is "not ready" (see [`tsgo-vs-oxc-for-transpile.md`](tsgo-vs-oxc-for-transpile.md)). |
| **`oxc-transformer` (oxc-project/oxc)** | ✓ since [`#8614`](https://github.com/oxc-project/oxc/pull/8614) (merged 2025-02-09) | ✓ via `decorator.emitDecoratorMetadata` option | Documented at [oxc.rs/docs/guide/usage/transformer/typescript](https://oxc.rs/docs/guide/usage/transformer/typescript). Type-inference long-tail: external/uninferrable types fall back to `Object` (matches `tsc` for external; diverges on some intra-file cases). Type-symbol fallback fixed in PR [`#10633`](https://github.com/oxc-project/oxc/pull/10633) (2025-04-27). Computed-key edge case [`#20418`](https://github.com/oxc-project/oxc/issues/20418) still open as of May 2026. `accessor` + legacy decorators [`#20133`](https://github.com/oxc-project/oxc/issues/20133) tracked, partial fix in PR [`#20348`](https://github.com/oxc-project/oxc/pull/20348). Stage 3 / TC39 standard decorators **deliberately not yet shipped** ([`#9170`](https://github.com/oxc-project/oxc/issues/9170)) pending [`tc39/test262#4103`](https://github.com/tc39/test262/issues/4103); Boshen reopened this in March 2026 to unblock Vite v8 adopters. |
| **`swc` / `@swc/core`** | ✓ via `jsc.parser.decorators: true` + `jsc.transform.legacyDecorator: true` | ✓ via `jsc.transform.decoratorMetadata: true` (since v1.2.13) | Documented at [swc.rs/docs/configuration/compilation](https://swc.rs/docs/configuration/compilation). Stage 3 via `jsc.transform.decoratorVersion: "2022-03"` since v1.3.47 (newer `"2023-11"` available; default is still `"2021-12"` legacy). Known edge cases: union-with-`null` metadata divergence ([`#6824`](https://github.com/swc-project/swc/issues/6824)), `target: esnext` still transforms decorators contra `tsc` ([`#11784`](https://github.com/swc-project/swc/issues/11784)). |
| **`esbuild`** | ✓ (legacy) | ✗ **intentional, not supported** | [`evanw/esbuild#257`](https://github.com/evanw/esbuild/issues/257), Evan Wallace: *"The `emitDecoratorMetadata` flag is intentionally not supported. It relies on running the TypeScript type checker… you're probably better off using another tool instead of esbuild if you need to do this."* Workaround is `esbuild-plugin-tsc` (delegate decorator files to tsc). Esbuild **does** support the newer TC39 Stage 3 decorator-metadata (`Symbol.metadata`) since v0.21.0 (commit [`5e7cf25`](https://github.com/evanw/esbuild/commit/5e7cf259752f500d75c5640b1d72fbf498be9dcd)) — different feature, addresses [`#3760`](https://github.com/evanw/esbuild/issues/3760). |
| **`@babel/plugin-transform-typescript`** | ✓ via `legacy: true` (on `@babel/plugin-proposal-decorators`) | ✓ via `onlyRemoveTypeImports: false` + `@babel/plugin-transform-typescript`'s metadata option | Standard documented path; predates oxc/swc/esbuild. Babel's Stage 3 path uses `@babel/plugin-proposal-decorators` with `version: '2023-05'`. |
| **Node `--strip-types` / `--experimental-strip-types` (amaro)** | ✗ rejected | ✗ rejected | Erasable-only by design. Decorators in source produce `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`. Per [`nodejs/amaro#200`](https://github.com/nodejs/amaro/issues/200) (Marco Ippolito 2025-05-26): the Node TS team committed to staying on SWC for amaro "for the foreseeable future." Even on the SWC backend, amaro only exposes the strip path, not transform. |
| **Bun (built-in runtime TS, `bun run script.ts`)** | partial / fragile (see §5) | ✓ when explicitly configured | Bun's runtime transpiler emits the legacy `__legacyDecorateClassTS` / `__legacyMetadataTS` helpers when `experimentalDecorators` + `emitDecoratorMetadata` are set in tsconfig — *but only when set in the file actually consumed*; tsconfig `extends`-chain merging was historically buggy ([`oven-sh/bun#6326`](https://github.com/oven-sh/bun/issues/6326), [`#30478`](https://github.com/oven-sh/bun/pull/30478) — November-2026 fix). `Bun.Transpiler` API and `bun build` had a regression in Bun 1.3.10 that **always emitted TC39 decorators regardless of `experimentalDecorators`** ([`#27575`](https://github.com/oven-sh/bun/issues/27575)); partial fix in [`#27582`](https://github.com/oven-sh/bun/pull/27582). Type-reference fallback bug ([`#7591`](https://github.com/oven-sh/bun/issues/7591)) emits `Object` where tsc emits `String`. Decorated-field-without-initializer removal ([`#20664`](https://github.com/oven-sh/bun/issues/20664)) regressed in v1.3 and was patched in [`#27266`](https://github.com/oven-sh/bun/pull/27266) (Feb 2026). **Net: NestJS / TypeORM on Bun works in May 2026 if you explicitly inline both flags in your immediate tsconfig and run on ≥1.3.11.** |
| **`tsx` (privatenumber/tsx, esbuild-based)** | ✓ | ✗ **does not support** | [`privatenumber/tsx#347`](https://github.com/privatenumber/tsx/issues/347) — closed as won't-fix because esbuild won't support it and pulling in tsc/swc would defeat the speed pitch. Standard recommendation in that issue: *"switch to ts-node for now."* This is the direct hole in Nub's comparable ecosystem that supporting `emitDecoratorMetadata` natively would close. |
| **`ts-node` (TypeStrong/ts-node) with default `tsc` backend** | ✓ | ✓ | Slow path (uses real tsc for transpile). Standard NestJS dev pattern pre-SWC era. |
| **`@swc-node/register` (formerly `ts-node`'s `--swc`)** | ✓ | ✓ "Respect the boolean value in tsconfig" — [npm/@swc-node/register](https://www.npmjs.com/package/@swc-node/register) | The modern recommended fast-NestJS dev loader. |

### 3.1 Quick read of the matrix

The only transpilers that **don't** emit decorator metadata are esbuild (deliberate) and tsx (downstream of esbuild). Every other transpiler in the v0.1-relevant set — tsc, tsgo, oxc, swc, Babel, Bun (with caveats), ts-node, @swc-node/register — does. Nub on oxc joins the majority; Nub on esbuild would join tsx in a hole the ecosystem actively works around via plugins or by switching to ts-node.

## 4. Ecosystem dependency audit

This is the load-bearing section for the recommendation. The question for each framework is: **does it hard-require `design:type` / `design:paramtypes`, or does it have a metadata-free code path?**

| Framework | What depends on metadata | Hard-required vs. migrate-able | Notes |
|-----------|--------------------------|--------------------------------|-------|
| **NestJS** (`@nestjs/core` ~8.4M/week, `@nestjs/common` ~8.7M/week) | DI container resolves constructor parameter types via `design:paramtypes`. `@Inject()`, `@Body()`, `@Query()`, pipe transformers, `ValidationPipe`, `Logger` injection — all read metadata to know what to construct. | **Hard-required for NestJS 10.x / 11.x.** Per [Stage 3 vs Legacy in NestJS](https://dev.to/gabrielanhaia/stage-3-vs-legacy-typescript-decorators-in-a-nestjs-app-p2f) and [Nest can't resolve dependencies — TypeScript World](https://typescriptworld.com/nest-cant-resolve-dependencies-at-index-0-after-disabling-emitdecoratormetadata-on-ts-5-5): "On `@nestjs/core` 10.3 or 10.4 with TypeScript 5.5, both `experimentalDecorators` and `emitDecoratorMetadata` must stay on for implicit injection to work… as of this writing, the framework has not shipped a stage-3-aligned major." Workaround for individual classes: explicit `@Inject(TOKEN)` for each parameter — viable for greenfield but a non-starter for migrating any existing app. | NestJS 12.x is in alpha (5 alpha releases by May 2026); no Stage 3 alignment announced. |
| **TypeORM** (`@nestjs/typeorm` ~2.4M/week; `typeorm` itself similar) | `@Column()` infers SQL column type from the TS property type via `design:type`. Without metadata, raises `ColumnTypeUndefinedError: Column type for X#y is not defined and cannot be guessed.` (verbatim from [`swc-project/swc#1920`](https://github.com/swc-project/swc/discussions/1920) and [`oven-sh/bun#20664`](https://github.com/oven-sh/bun/issues/20664)). Relations (`@OneToMany`, `@ManyToOne`) also use the type to derive the related entity in some shapes. | **Hard-required for ergonomic use.** Workaround: explicit type on every `@Column({ type: "varchar" })` — viable but defeats the appeal. | Same situation on Stage 3: TypeORM has not shipped a Stage-3 path. |
| **Angular (Ivy, AOT)** | Ivy's AOT compiler reads decorators at *build* time and synthesizes the equivalent metadata into the JS output (`ɵcmp`, `ɵdir` static members). AOT-built Angular **does not depend on `reflect-metadata` at runtime**. See [`angular/angular packages/compiler/design/architecture.md`](https://github.com/angular/angular/blob/main/packages/compiler/design/architecture.md). | **Mostly migrated away.** AOT is the default since Angular 9 (2020); JIT remains supported but is the slow-dev fallback. | JIT mode **still** needs `emitDecoratorMetadata` + `reflect-metadata` for runtime DI. Per [`oven-sh/bun#27575`](https://github.com/oven-sh/bun/issues/27575) (Angular 21.1.5): Bun emitting TC39 decorators in JIT-equivalent contexts breaks Angular runtime — AOT is "5-7× slower than JIT for dev/HMR." |
| **class-validator** (~5M/week) + **class-transformer** | `@IsString()`, `@IsNumber()`, `@IsBoolean()` etc. can infer expected types from `design:type` for `transform: true` with `enableImplicitConversion: true`. `@Type(() => Foo)` decorators on `class-transformer` need metadata to perform nested transformation. | **Soft-required.** Validators *can* operate with explicit type annotations (`@IsString({})` already names the validation), but `class-transformer`'s automatic nested transformation degrades. | Universally paired with NestJS; sharing fate with NestJS on the legacy-decorator question. |
| **InversifyJS** | DI container reads `design:paramtypes` for constructor injection via `@injectable()`. `@inject(TOKEN)` parameter decorators name explicit tokens but the *types* still come from metadata. | **Hard-required by default.** Explicit `@inject(TOKEN)` on every parameter is supported but the framework's docs and conventions assume metadata. | Inversify v7 introduced `@injectFromBase` and some Stage 3 explorations but the dominant API is still legacy. |
| **routing-controllers** / **TypeStack** | Parameter metadata for `@Body()`, `@Param()`, `@QueryParam()` — reads `design:paramtypes` to know what class to deserialize the request body into. | **Hard-required.** | Sister project to class-validator; same maintainer (typestack). |
| **typedi** | DI container — pure `design:paramtypes` reader. | **Hard-required by default.** Explicit `@Inject(token)` works. | |
| **MikroORM** (TypeORM alternative; ~150k/week) | Uses `reflect-metadata` *optionally* for entity decorators. **Has a metadata-free code path:** the MikroORM-CLI can scan entities and generate static metadata files via `ts-morph`, removing the runtime metadata dependency. Also offers explicit-config mode. | **Migrate-able.** MikroORM is the cleanest Stage-3-ready ORM in this list. | Documented in their `@Property()` decorator notes — metadata is one of several discovery mechanisms. |
| **Typegoose / @typegoose/typegoose** (Mongoose wrapper, ~250k/week) | `@prop()` reads `design:type` to derive the Mongoose schema field type. Without metadata, every `@prop({ type: String })` must name the type explicitly. | **Soft-required.** | Stage 3 not announced. |
| **TypeGraphQL** (~150k/week) | `@Field()` infers GraphQL field types from `design:type`. | **Soft-required;** explicit `@Field(() => String)` works. | |
| **Awilix / tsyringe** | tsyringe reads metadata for `@injectable()`. Awilix uses string-key DI and is metadata-free. | tsyringe: hard. Awilix: not affected. | |

### 4.1 The take-away

The frameworks that **hard-require** `emitDecoratorMetadata`, sorted by download weight:

1. **NestJS** (~8.4M/wk core) — the single biggest server-side TS framework on npm. No Stage 3 migration in sight.
2. **TypeORM via `@nestjs/typeorm`** (~2.4M/wk just the Nest binding) — column-type inference.
3. **class-validator / class-transformer** (~5M/wk combined) — paired with NestJS in 90%+ of deployments.
4. **InversifyJS, typedi, routing-controllers, Typegoose** — smaller, but each meaningful.

The frameworks that **don't** need it:

1. **Angular AOT** — default for production builds since 2020.
2. **MikroORM** with static-metadata mode.
3. **Awilix**, anything not based on decorators.

**The first list is roughly an order of magnitude more npm weight than the second list.** A TypeScript runtime that doesn't emit decorator metadata can't run any NestJS app, any TypeORM app paired with NestJS, or anything in the class-validator orbit, without forcing the user to either pre-compile via `tsc` (defeating the runtime pitch) or rewrite the app to use explicit annotations everywhere (defeating the framework's ergonomics).

## 5. Current Node.js ecosystem patterns

What people actually use to run `.ts` files with decorator metadata today:

| Setup | Decorator metadata works? | Notes |
|-------|---------------------------|-------|
| Plain `tsc --outDir` then `node dist/main.js` (compile-ahead) | ✓ | The traditional production pattern; correct but slow dev loop. |
| `ts-node script.ts` (default `tsc` backend) | ✓ | Slow (~1-3 s warmup); historically the NestJS dev default. |
| `ts-node` with `"ts-node": { "swc": true }` in tsconfig | ✓ via `@swc-node/register` | Fast (≈30× faster than tsc backend per ts-node docs). Common modern NestJS dev setup. |
| `@swc-node/register` directly (`node -r @swc-node/register script.ts`) | ✓ | Respects tsconfig `emitDecoratorMetadata`. |
| `@swc/register` with `.swcrc` setting `legacyDecorator: true` + `decoratorMetadata: true` | ✓ | Lower-level than `@swc-node/register`; explicit `.swcrc` config. |
| **`tsx script.ts`** | ✗ | **Silently fails** — emits no metadata, NestJS / TypeORM break at runtime. Documented in [`privatenumber/tsx#347`](https://github.com/privatenumber/tsx/issues/347). The standard workaround is "use ts-node instead." |
| `esbuild-register` | ✗ | Same hole as tsx, same root cause (esbuild). |
| `tsup --watch` for dev (esbuild) | ✗ unless paired with `esbuild-plugin-tsc` / `esbuild-plugin-emit-decorator-metadata` | Forces a per-file tsc fork for decorated files, eliminating most of esbuild's speed. |
| `bun run script.ts` (Bun ≥1.3.11) | ✓ with caveats | Works if `emitDecoratorMetadata` + `experimentalDecorators` are set in the **immediate** tsconfig (not via `extends`). Bug-fix history through 2026 has been substantial; production NestJS-on-Bun deployments exist but are fragile (see issues cited in §3). |
| **NestJS CLI's recommended dev path: `nest start -b swc --type-check`** | ✓ | Per official [docs.nestjs.com/recipes/swc](https://docs.nestjs.com/recipes/swc): use SWC builder for compilation (10× faster than tsc), use tsc in parallel for `--noEmit` type-checking. Requires `legacyDecorator: true` + `decoratorMetadata: true` in `.swcrc`. This is the canonical 2026 NestJS dev setup. |
| `nodemon` + `ts-node` (with or without swc) | ✓ | Pre-`--watch`-flag classic. |
| `node --watch --import tsx/esm script.ts` | ✗ same as tsx | |

### 5.1 The visible pattern

The Node ecosystem's TS-runtime tooling is sharply bimodal on `emitDecoratorMetadata`:

- **Tools that route through `tsc` or `swc` for transpilation** (ts-node, @swc-node/register, NestJS CLI's swc builder, Bun's runtime path) → support metadata.
- **Tools that route through `esbuild`** (tsx, esbuild-register) → do not support metadata.

Nub is in the architecturally privileged position of routing through `oxc`, which sits in the first camp by capability. Nub-on-oxc is in the strong half of this split with no extra engineering work, since the support has already shipped upstream.

### 5.2 What silently fails

A user who installs `tsx` and runs `tsx main.ts` against a NestJS app gets *no warning* — the build succeeds, the process starts, the very first `app.get(SomeService)` throws `Nest can't resolve dependencies of ... (?). Please make sure that the argument at index [0] is available...`. This is the most-complained-about failure mode in the tsx issue tracker. If Nub shipped without metadata, every NestJS user trying Nub would hit this within minutes, and Nub would be tagged in NestJS docs as "doesn't work, use ts-node."

## 6. Stage 3 decorators + decorator-metadata proposal

### 6.1 Two separate TC39 proposals

- [**tc39/proposal-decorators**](https://github.com/tc39/proposal-decorators) — Stage 3 since 2022 (the version that shipped in TS 5.0). Defines the `(value, context) => …` decorator shape. **No metadata emission.**
- [**tc39/proposal-decorator-metadata**](https://github.com/tc39/proposal-decorator-metadata) — Stage 3 since November 2023. Adds a `context.metadata` object to each decorator's context argument; after all decorators run, the metadata object is assigned to `Class[Symbol.metadata]`. **Metadata bag for decorator authors to write into — not a type-information emitter.**

### 6.2 What Stage 3 metadata gives you

```ts
function track(_, context) {
  (context.metadata.names ||= []).push(context.name);
}

class Foo {
  @track x;
  @track y;
}

Foo[Symbol.metadata].names; // ["x", "y"]
```

The metadata bag inherits from the parent class's metadata (prototype chain), enabling natural decorator-inherited state. It can also be used as a `WeakMap` key for private metadata. None of this requires a global polyfill.

### 6.3 What Stage 3 metadata does NOT give you

The Stage 3 proposal **does not** emit `design:type`, `design:paramtypes`, or `design:returntype`. There is no runtime-type-reflection story — the design intentionally avoids depending on TypeScript-specific semantics, because the proposal targets all JavaScript (where there are no static types to reflect).

A request to extend TypeScript so that `emitDecoratorMetadata: true` would also populate `context.metadata` with `design:*` entries was filed as [`microsoft/TypeScript#57533`](https://github.com/microsoft/TypeScript/issues/57533). The TypeScript team has not committed to this; the canonical response from Ryan Cavanaugh's team is that emitting type information into the Stage 3 path conflates the TypeScript compiler with what should be a pure-JS proposal. Practical consequence: **the legacy metadata emission cannot be ported to Stage 3 without TypeScript-team buy-in that hasn't happened.**

### 6.4 Where Stage 3 is supported today

- **TypeScript 5.0+**: Stage 3 decorators (no flag); ships with `experimentalDecorators: false` by default in TS 5.x. `Symbol.metadata` proposal shipped TS 5.2.
- **esbuild 0.21.0+**: Stage 3 decorators + metadata ([`#3760`](https://github.com/evanw/esbuild/issues/3760)).
- **swc**: `decoratorVersion: "2022-03"` and `"2023-11"` options; some `context.metadata` bugs ([`#7957`](https://github.com/swc-project/swc/issues/7957)).
- **Babel**: `@babel/plugin-proposal-decorators` with `version: "2023-05"`.
- **oxc**: **not yet shipped** ([`#9170`](https://github.com/oxc-project/oxc/issues/9170)). Boshen reopened in March 2026 specifically to unblock Vite v8 adopters. Blocked on [`tc39/test262#4103`](https://github.com/tc39/test262/issues/4103) (the conformance test suite). Current workaround per the oxc thread: use Babel or SWC together.

### 6.5 NestJS / TypeORM / Angular Stage 3 migration status

- **NestJS**: not announced; framework has not migrated; v12 alphas (May 2026) do not mention it.
- **TypeORM**: not announced.
- **Angular**: AOT is already metadata-free; JIT still depends on legacy; no public Stage-3-of-JIT roadmap.
- **class-validator / class-transformer**: not announced.
- **InversifyJS**: v7 explored Stage 3 patterns in experimental APIs but the dominant surface is still legacy.

**Time horizon**: years, not quarters. The migration is a major-version-of-NestJS-shaped problem because it changes the DI contract.

### 6.6 Implication for Nub

Stage 3 decorators (the proposal-decorators side) **do** need transpiler support for `target: ES2022` and below. Oxc has not shipped this yet ([`#9170`](https://github.com/oxc-project/oxc/issues/9170)). For v0.1, Nub's default runtime target is modern Node (Node 24.13.1+ per `ts-transpilation.md`), which already supports Stage 3 decorator syntax at the JS-engine level — V8 ≥ 12.x. So Nub's load hook can in principle pass Stage 3 decorators through unchanged for the `target: esnext` case, but it cannot *transform* them to older targets until oxc ships [`#9170`](https://github.com/oxc-project/oxc/issues/9170). This is a separate ship gate from `emitDecoratorMetadata` and does not block the v0.1 commitment to legacy + metadata.

The Stage 3 metadata proposal is supported by V8 natively (since `Symbol.metadata` is just a symbol the runtime hands out); no transpiler action needed there for modern Node. The decorator authors themselves are responsible for populating `context.metadata`.

## 7. Bun's behavior in detail

The Bun comparison matters because Bun is the only competing runtime in Nub's strict comparable set (Node + TS runtime), and "does Bun support this?" sets the floor for what NestJS users will tolerate.

### 7.1 As of May 2026

- **Bun's runtime transpiler emits legacy `__legacyDecorateClassTS` / `__legacyMetadataTS` helpers** when both `experimentalDecorators: true` and `emitDecoratorMetadata: true` are set in the tsconfig the file is resolved under. Source: [`#20664`](https://github.com/oven-sh/bun/issues/20664) (TypeORM working under `bun run`).
- **Default behavior without those flags**: Bun emits TC39 Stage 3 decorator transforms. So an unconfigured Bun installation breaks NestJS unless tsconfig explicitly opts into legacy.
- **`bun build` and `Bun.Transpiler` had a regression in v1.3.10** ([`#27575`](https://github.com/oven-sh/bun/issues/27575)) where `experimentalDecorators: true` was silently ignored and TC39 decorators were always emitted. Partial fix in [`#27582`](https://github.com/oven-sh/bun/pull/27582) (Feb 2026); full fix landed across [`#27266`](https://github.com/oven-sh/bun/pull/27266) and later 1.3.x patches.
- **`extends`-chain tsconfig merging was buggy** for `experimentalDecorators` until PR [`#30478`](https://github.com/oven-sh/bun/pull/30478) (~Nov 2026). The standing workaround in the meantime: inline both flags in the immediate `tsconfig.json` instead of relying on inheritance.
- **Type-reference fallback bug** ([`#7591`](https://github.com/oven-sh/bun/issues/7591), open since 2023): some type references emit `Object` where tsc emits the resolved class. The original 2023 report's repro still produced wrong output as of July 2025. Same family of issues as oxc's type-inference fallback (§3).

### 7.2 Bun's net stance

Bun **wants** to support `emitDecoratorMetadata`. Their 1.3.10 blog post said *"Legacy decorators (`experimentalDecorators: true` in tsconfig.json) continue to work as before."* The regressions and edge-case bugs cited above are bugs, not policy. NestJS-on-Bun is officially supported but operationally fragile through May 2026.

### 7.3 Implication for Nub

If Nub didn't support `emitDecoratorMetadata`, Nub would be the only mainstream alternative-runtime / TS-direct-execution tool in May 2026 (alongside tsx) without it. Bun has it, Node-with-ts-node has it, Node-with-@swc-node has it, NestJS CLI's swc builder has it. Nub-on-oxc gets it essentially for free because oxc has already done the engineering. The bar to beat is roughly *"work where Bun works, plus the bugs Bun has, minus the bugs we fix"* — and the bar to be acceptable is *"NestJS quickstart from the official docs runs."*

## 8. Recommendation for Nub

### 8.1 Option matrix

| Option | What it means | Adoption impact | Engineering cost | oxc gating |
|--------|---------------|------------------|------------------|------------|
| **(A) Support in v0.1** | Wire oxc's `decorator.legacy` + `decorator.emitDecoratorMetadata` options into the load-hook transpile path. Document the `Object`-fallback divergence. Tell users to `import "reflect-metadata"` themselves. | NestJS / TypeORM / class-validator / InversifyJS / Typegoose all work out-of-box. Closes the biggest single adoption-barrier in server-side TS. Matches Bun's behavior. Better than tsx. | Low — flag wiring + tsconfig-respect. Already committed in `non-erasable-syntax.md`. | **Not blocked.** oxc has it. |
| **(B) Defer to v0.x** | Ship v0.1 without metadata. Tell NestJS users they're not the v0.1 audience. Document that legacy decorators emit but `design:*` keys are absent. Plan to ship metadata in v0.2 or v0.3. | Cuts the addressable v0.1 user base by the entire NestJS world. NestJS docs will list Nub in the "doesn't work" column. Adoption story for v0.1 becomes "tsx with a faster cold start," which is a narrower wedge. | Trivially lower than (A) — flag stays off. | n/a |
| **(C) Skip indefinitely** | Make a "we don't bring legacy forward" stance. Tell users to migrate to Stage 3 decorators or use compat mode (`--node` + pre-compiled output from `tsc`). | Permanently excludes the legacy-decorator ecosystem. Cleaner story, smaller surface, but the Stage-3 migration is years away in the frameworks that matter. Effectively "Nub for greenfield apps only." Brand-wise: aggressive, defensible, but cedes the upgrade-an-existing-NestJS-app pitch to Bun. | n/a | n/a |

### 8.2 Recommendation: **Option (A)** — ship `emitDecoratorMetadata` in v0.1

Rationale, in order of weight:

1. **Already committed.** `non-erasable-syntax.md` lists `emitDecoratorMetadata` under "What we ship", decided 2026-05-18. This doc validates that commitment against current upstream status; it does not propose new scope.
2. **Upstream is ready.** oxc-transformer has shipped the emission path (PR [`#8614`](https://github.com/oxc-project/oxc/pull/8614), Feb 2025) and refined it through April-May 2025. The metadata transform is exercised in production by Vite/Rolldown-adopting TS projects. The remaining gaps are edge cases (computed keys, `accessor` + legacy), not the emission pathway.
3. **Compatibility is paramount.** Nub's stated contract is that code targeting Node must run on Nub byte-for-byte. Every existing NestJS app is "code targeting Node." Skipping metadata violates this commitment in the most-visible-possible way.
4. **The brand-boundary cost is zero.** No `NUB_*` env var, no `globalThis.nub`, no `@nub/*` package, no source patch. We respect the user's tsconfig flags and we don't auto-inject `reflect-metadata`. The user owns the polyfill the same way they do on `tsc`, on Bun, and on ts-node.
5. **Bun parity.** If Nub lacks what Bun has, the conversation with a NestJS adopter is "use Bun." If Nub matches what Bun has — including with the same `Object`-fallback edge cases that Bun also exhibits — the conversation is on the merits.
6. **Closes the tsx gap.** tsx's #1 longstanding open issue is `emitDecoratorMetadata` support ([`#347`](https://github.com/privatenumber/tsx/issues/347)). Nub was already going to be "tsx-shaped but faster"; this turns it into "tsx-shaped but faster *and* covers tsx's most-requested unsupported feature."

### 8.3 Caveats to document (not blockers)

1. **The `Object`-fallback divergence** ([oxc docs](https://oxc.rs/docs/guide/usage/transformer/typescript)): types that require type inference fall back to `Object`. This matches `tsc`'s behavior for *externally* resolved type references but diverges on intra-file type-alias / mapped-type / conditional-type cases that `tsc` does resolve. Bun has the same family of bugs ([`#7591`](https://github.com/oven-sh/bun/issues/7591)). Workaround: explicit `@Reflect.metadata("design:paramtypes", [...])` for the rare bitten case. Document in `non-erasable-syntax.md` open-questions or a new troubleshooting note.
2. **`reflect-metadata` polyfill is user-installed.** Auto-injecting violates additivity (mutates `globalThis.Reflect`). Document in user-facing docs: "If you see `Reflect.getMetadata is not a function`, add `import 'reflect-metadata'` to your entry file." Same instruction every other TS runtime gives.
3. **`const enum` cross-file inlining** is a separate non-erasable-syntax open question (already tracked in `non-erasable-syntax.md`); not in scope here.
4. **Stage 3 decorator transforms not yet in oxc.** Modern Node (V8 ≥ 12.x) accepts Stage 3 decorator syntax natively, so for `target: esnext` we can pass through. For older targets requiring downlevel emit, we currently can't transform — but this is the existing oxc gap, not a regression from this decision. Track separately and revisit when [`oxc#9170`](https://github.com/oxc-project/oxc/issues/9170) ships.
5. **`accessor` + legacy decorators** ([`oxc#20133`](https://github.com/oxc-project/oxc/issues/20133)): used by Lit's migration path. Partial fix in [`#20348`](https://github.com/oxc-project/oxc/pull/20348). Watch for full landing.

### 8.4 What this means for Nub's docs

- `ts-transpilation.md`: no change required (decorator handling is delegated to `non-erasable-syntax.md`).
- `non-erasable-syntax.md`: "What we ship" section already lists `emitDecoratorMetadata`. Cite this research doc from the open-questions section on `emitDecoratorMetadata` edge cases and link to oxc's docs page. Update the `reflect-metadata polyfill` open-question entry to note that the polyfill stance is now firm policy (additivity-derived), not an open question.
- `whitepaper.md`: add (in the parent agent's follow-up edit) a sentence to the TS-compatibility section confirming legacy decorators + metadata work, since this is a frequent "does your TS runtime do X" question.

## 9. Open questions

- **NestJS Stage-3 migration timeline.** Unknown publicly. If a Stage-3-aligned NestJS major ships within v0.x's lifetime, Nub's Stage 3 decorator transform gap ([`oxc#9170`](https://github.com/oxc-project/oxc/issues/9170)) becomes urgent. Currently no evidence this is imminent.
- **Concrete real-world incidence of the oxc `Object`-fallback divergence on NestJS / TypeORM codebases.** We have the theoretical edge case documented; we don't have a survey of how often it bites a real NestJS app. The right test is to run the NestJS sample apps + the TypeORM sample apps under Nub and count failures. Defer to integration-test phase.
- **`reflect-metadata` package interaction with `module.registerHooks` order.** `reflect-metadata` mutates `globalThis.Reflect` on import. If the user's first `import "reflect-metadata"` happens after a decorator-using class is already evaluated (because Nub's hook resolved the decorated file first), the `__metadata` calls no-op silently. This is the same trap that exists on `tsc` and Bun — usually solved by the user putting `import "reflect-metadata"` at the very top of their entry file — but we should test that our `--import` preload order doesn't make this worse. File under integration-test follow-ups.
- **`Bun.Transpiler` shape regressions.** Bun's bug history on this surface is ongoing. If Nub discovers that NestJS apps that work on Nub **don't** work on Bun (or vice versa), the marketing surface around "Nub = Bun-compatible TS execution" needs trimming. Not a blocker for the recommendation.
- **TC39 decorator-metadata adoption telemetry.** Whether the Stage 3 metadata proposal will see meaningful framework adoption in 2026-2027 affects whether oxc's [`#9170`](https://github.com/oxc-project/oxc/issues/9170) becomes load-bearing for Nub. Currently low signal.

## Sources

### Primary specs / docs

- [TypeScript: TSConfig Option `emitDecoratorMetadata`](https://www.typescriptlang.org/tsconfig/emitDecoratorMetadata.html)
- [tc39/proposal-decorators (Stage 3)](https://github.com/tc39/proposal-decorators)
- [tc39/proposal-decorator-metadata (Stage 3 since Nov 2023)](https://github.com/tc39/proposal-decorator-metadata)
- [`reflect-metadata` npm package](https://www.npmjs.com/package/reflect-metadata)

### oxc

- [oxc docs: TypeScript transformer — Decorators](https://oxc.rs/docs/guide/usage/transformer/typescript)
- [`oxc-project/oxc#8614` feat(transformer): support for transforming legacy decorator (merged 2025-02-09)](https://github.com/oxc-project/oxc/pull/8614)
- [`oxc-project/oxc#10633` fix: fallback to `Object` when a type reference refers to a type symbol (merged 2025-04-27)](https://github.com/oxc-project/oxc/pull/10633)
- [`oxc-project/oxc#10632` fix: keep imports when referenced as metadata](https://github.com/oxc-project/oxc/pull/10632)
- [`oxc-project/oxc#9170` transformer: ecma (Stage 3) decorators — held pending test262 conformance, reopened March 2026 to unblock Vite v8](https://github.com/oxc-project/oxc/issues/9170)
- [`oxc-project/oxc#20133` transformer: support lowering `accessor` with legacy decorators (2026-03)](https://github.com/oxc-project/oxc/issues/20133)
- [`oxc-project/oxc#20348` feat: lower `accessor` with legacy decorators (2026-03)](https://github.com/oxc-project/oxc/pull/20348)
- [`oxc-project/oxc#20418` transformer: legacy decorator on computed property key — still open](https://github.com/oxc-project/oxc/issues/20418)
- [oxc_transformer source: `decorator/legacy/metadata.rs`](https://github.com/oxc-project/oxc/blob/main/crates/oxc_transformer/src/decorator/legacy/metadata.rs)

### swc

- [swc.rs: Compilation — `legacyDecorator`, `decoratorMetadata`, `decoratorVersion`](https://swc.rs/docs/configuration/compilation)
- [`swc-project/swc#6824` Emit decorator metadata: union-with-null divergence vs tsc — open](https://github.com/swc-project/swc/issues/6824)
- [`swc-project/swc#11784` `swc` transforms decorators when `target: esnext` (inconsistent with `tsc`)](https://github.com/swc-project/swc/issues/11784)
- [`swc-project/swc#7957` `undefined` is passed for `context.metadata` field in a Stage 3 decorator](https://github.com/swc-project/swc/issues/7957)
- [`swc-project/swc#11698` fix(decorators): resolve 2022-03 issues #9565/#9078/#9079](https://github.com/swc-project/swc/issues/11698)

### esbuild

- [`evanw/esbuild#257` Support emitting TypeScript decorator metadata — closed/wontfix](https://github.com/evanw/esbuild/issues/257)
- [`evanw/esbuild` commit `5e7cf25` fix #3760: implement decorator metadata proposal (Symbol.metadata; v0.21.0)](https://github.com/evanw/esbuild/commit/5e7cf259752f500d75c5640b1d72fbf498be9dcd)
- [thebenforce.com: How to Use TypeScript Decorators with esbuild (workaround pattern)](https://thebenforce.com/post/typescript-decorators-esbuild)
- [`esbuild-plugin-emit-decorator-metadata` on npm](https://npmx.dev/package/esbuild-plugin-emit-decorator-metadata)

### tsgo

- [`microsoft/typescript-go` README — status table](https://github.com/microsoft/typescript-go)
- [`microsoft/typescript-go#2926` Implement ES decorator transform](https://github.com/microsoft/typescript-go/pull/2926)
- (Integration-shape constraints covered in [`tsgo-vs-oxc-for-transpile.md`](tsgo-vs-oxc-for-transpile.md))

### Bun

- [`oven-sh/bun#27575` `experimentalDecorators: true` in tsconfig has no effect — Bun.Transpiler / bun build always emit TC39 decorators (2026-02)](https://github.com/oven-sh/bun/issues/27575)
- [`oven-sh/bun#27582` fix(transpiler): pass experimentalDecorators/emitDecoratorMetadata to Bun.Transpiler parse options (2026-02)](https://github.com/oven-sh/bun/pull/27582)
- [`oven-sh/bun#30478` resolver: merge experimentalDecorators across tsconfig extends chain (2026-11)](https://github.com/oven-sh/bun/pull/30478)
- [`oven-sh/bun#6326` Bun does not emit decorator metadata if tsconfig inherits the configuration from another file](https://github.com/oven-sh/bun/issues/6326)
- [`oven-sh/bun#20664` Decorated properties are being removed (regressed in v1.3, patched in `#27266`)](https://github.com/oven-sh/bun/issues/20664)
- [`oven-sh/bun#7591` emitDecoratorMetadata can cause "Cannot access uninitialized variable." at runtime (open since 2023)](https://github.com/oven-sh/bun/issues/7591)
- [`oven-sh/bun#27266` fix(transpiler): keep decorated class fields without initializers in class body (2026-02)](https://github.com/oven-sh/bun/pull/27266)

### tsx / ts-node / @swc-node/register

- [`privatenumber/tsx#347` Support `emitDecoratorMetadata` — closed/won't-fix; pattern is "switch to ts-node"](https://github.com/privatenumber/tsx/issues/347)
- [`@swc-node/register` on npm — "Respect the boolean value in tsconfig" for `experimentalDecorators` + `emitDecoratorMetadata`](https://www.npmjs.com/package/@swc-node/register)
- [Stack Overflow: How to watch and reload ts-node when TypeScript files change — modern dev setups](https://stackoverflow.com/questions/37979489/how-to-watch-and-reload-ts-node-when-typescript-files-change)

### Node amaro / strip-types

- [`nodejs/amaro#200` Experiment with typescript-go — Marco Ippolito 2025-05-26: "we should keep using SWC for the foreseeable future"](https://github.com/nodejs/amaro/issues/200)

### Frameworks

- [docs.nestjs.com/recipes/swc — NestJS canonical SWC dev recipe with `legacyDecorator` + `decoratorMetadata`](https://docs.nestjs.com/recipes/swc)
- [docs.nestjs.com/cli/overview — "We recommend using the SWC builder for faster builds (10x more performant than the default TypeScript compiler)"](https://docs.nestjs.com/cli/overview)
- [DEV: Stage 3 vs Legacy TypeScript Decorators in a NestJS App](https://dev.to/gabrielanhaia/stage-3-vs-legacy-typescript-decorators-in-a-nestjs-app-p2f)
- [TypeScript World: Nest Can't Resolve Dependencies '?' at Index 0 After Disabling emitDecoratorMetadata on TS 5.5](https://typescriptworld.com/nest-cant-resolve-dependencies-at-index-0-after-disabling-emitdecoratormetadata-on-ts-5-5)
- [docs.nestjs.com/techniques/validation — class-validator integration via ValidationPipe](https://docs.nestjs.com/techniques/validation)
- [`swc-project/swc#1920` Usage with TypeORM — `ColumnTypeUndefinedError` without metadata](https://github.com/swc-project/swc/discussions/1920)
- [`angular/angular packages/compiler/design/architecture.md` — Ivy AOT renders metadata at build time](https://github.com/angular/angular/blob/main/packages/compiler/design/architecture.md)
- [`microsoft/TypeScript#57533` Expose design-time type information in TC39 decorator metadata when `emitDecoratorMetadata: true` — open request, TS team has not committed](https://github.com/microsoft/TypeScript/issues/57533)

### npm download stats (used for ecosystem sizing in §4)

- [npmx.dev: @nestjs organization — @nestjs/core ~8.4M/wk, @nestjs/common ~8.7M/wk, @nestjs/typeorm ~2.4M/wk (April 2026)](https://npmx.dev/org/nestjs)
- [`@nestjs/core` on npm — weekly downloads ~10.7M](https://registry.npmjs.org/@nestjs/core)

### Internal cross-references

- `../runtime/ts-transpilation.md` — TS transpile load-hook plan; oxc-first.
- `../runtime/non-erasable-syntax.md` — committed scope: legacy decorators + `emitDecoratorMetadata`.
- `../runtime/jsx-transpilation.md` — sibling per-feature plan.
- `../runtime/source-maps.md` — sibling per-feature plan.
- [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md) — N-API path (resolved); oxc 178k transpiles/sec on fixture exercising decorators.
- [`tsgo-vs-oxc-for-transpile.md`](tsgo-vs-oxc-for-transpile.md) — tsgo not viable; oxc confirmed.
- [`tsx-architecture.md`](tsx-architecture.md) — tsx's architecture and the esbuild dependency.
- [`node-swc-vs-oxc-choice.md`](node-swc-vs-oxc-choice.md) — Node's choice of SWC for amaro; reasoning carries.
- `../architecture.md#augmenter-not-fork` — mechanism test (oxc satisfies via Node's standard extension surface).
- `../philosophy.md#additivity` — additivity policy; basis for not auto-injecting `reflect-metadata`.
- [`AGENTS.md`](../../AGENTS.md) — brand-boundary rules; no `NUB_*` env var / `globalThis.nub` / `nub:*` namespace / `@nub/*` package introduced by this decision.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
