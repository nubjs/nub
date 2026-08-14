// @nubjs/types — ambient declarations for code authored against the Nub runtime.
//
// Nub augments Node with surfaces TypeScript doesn't know about. This package
// makes that nub-authored code typecheck so the parity bar holds: "if `tsc
// --noEmit` accepts your code, nub runs it."
//
// Declares Nub-only surfaces plus proposal APIs that are missing from a consumer's
// selected TypeScript lib. Proposal members augment the standard interfaces rather
// than redeclaring their global values: an older lib gains the methods, while a
// newer lib's declarations merge without TS2403/TS2717 conflicts. Runtime surfaces
// already covered by @types/node or the selected standard libraries remain absent.
// Add this package to a tsconfig with `types: ["node", "@nubjs/types"]`.
//
// MUST remain a global *script* file: NO top-level `import`/`export`. The wildcard
// `declare module "*.yaml"` declarations are only visible project-wide from a
// script file. (Adding `export {}` turns this into a module and silently breaks
// the data-import wildcards.) Globals are declared bare (`declare function …`,
// `declare var …`, `declare namespace …`) for the same reason.

// ── Data-format module imports (Nub load hook) ──
// Default export ONLY — data modules expose no named exports (a named import
// like `import { host } from "./c.yaml"` is a load-time error on nub, the same
// as Node's JSON modules). The object formats default to `Record<string,
// unknown>` so the default can be destructured with sound `unknown` keys —
// `import cfg from "./c.yaml"; const { host, port } = cfg;` gives `host`/`port:
// unknown`. This is the sound, typeable equivalent of named imports.
//
// CAVEAT: a top-level array or scalar (e.g. a YAML document whose root is a list
// or a bare string) is mistyped as a record by `Record<string, unknown>`; cast
// the default in that case (`import data from "./list.yaml"; const items = data
// as unknown as string[];`). `.txt` is always a `string`; `.json` is
// intentionally NOT declared — it's Node-native (resolveJsonModule).
declare module "*.yaml" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.yml" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.toml" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.jsonc" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.json5" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.txt" {
  const data: string;
  export default data;
}

// ── reportError (WinterTC min-common-API; runtime/polyfills.cjs) ──
// In no Node version, in no @types/node. Nub installs it on every supported version.
declare function reportError(error: unknown): void;

// ── lib.dom step-aside helpers (idiom from bun-types: packages/bun-types/bun.d.ts) ──
// These two ambient *type* aliases let us declare DOM-overlapping globals (today
// just `Worker`) WITHOUT colliding (TS2403/TS2430) when the consumer ALSO has them
// globally — e.g. `lib: ["dom"]`, or any other lib that declares `Worker`. They
// are pure type-level helpers: a global *script* may declare ambient `type`s
// freely (only a top-level `import`/`export` would turn this into a module), so
// this does NOT break the wildcard `declare module "*.yaml"` decls above.
//
// `__NubLibDomIsLoaded` — lib.dom defines the global `onabort`; its presence is the
// signal that DOM is loaded, so the DOM owns these globals and we must step aside.
// `__NubUseLibDomIfAvailable<K, T>` — when DOM is loaded, adopt whatever type
// `globalThis` already has for key K; otherwise fall back to our own shape T. This
// is exactly Bun's `Bun.__internal.{LibDomIsLoaded,UseLibDomIfAvailable}`, recast
// as bare ambient globals (with a `__Nub` prefix) so the file stays a script.
type __NubLibDomIsLoaded = typeof globalThis extends { onabort: any } ? true : false;
type __NubUseLibDomIfAvailable<GlobalThisKeyName extends PropertyKey, Otherwise> =
  __NubLibDomIsLoaded extends true
    ? typeof globalThis extends { [K in GlobalThisKeyName]: infer T }
      ? T
      : Otherwise
    : Otherwise;

// ── Browser-shape Worker global (runtime/worker-polyfill.mjs) ──
// Nub ships the WHATWG/browser subset of `Worker` over node:worker_threads.Worker.
// @types/node has NO global `Worker` (only node:worker_threads' class), so this is
// the genuine gap. `MessageEvent`, `ErrorEvent`, and `MessagePort` are ALREADY
// global in @types/node>=25 (web-globals/fetch.d.ts + messaging.d.ts) — verified
// empirically — so they are referenced from there and intentionally NOT redeclared
// here (redeclaring them collides: TS2403).
//
// Step-aside: when `lib: ["dom"]` is in play, the DOM's own `Worker` wins — the
// interface body resolves to `{}` (via `__NubLibWorkerOrNubWorker`) and our `var`
// adopts the DOM type (via `__NubUseLibDomIfAvailable`), so the two coexist with
// NO TS2403/TS2430 collision. When DOM is absent (the normal Node case), our full
// browser-shape declaration applies unchanged.
interface WorkerOptions {
  type?: "module" | "classic";
  name?: string;
  credentials?: "omit" | "same-origin" | "include";
  // `eval: true` runs the constructor's first argument as the worker's source
  // (Node's worker_threads inline form) instead of resolving it as a URL.
  eval?: true;
}
interface __NubWorker extends EventTarget {
  readonly name: string;
  postMessage(message: any, transfer?: readonly (ArrayBuffer | MessagePort)[]): void;
  // Returns the underlying worker_threads `Promise<exitCode>` (additive
  // void→value widening; spec code that ignores the return is unaffected).
  terminate(): Promise<number>;
  onmessage: ((this: Worker, ev: MessageEvent) => any) | null;
  onmessageerror: ((this: Worker, ev: MessageEvent) => any) | null;
  onerror: ((this: Worker, ev: ErrorEvent) => any) | null;
  // node:worker_threads EventEmitter surface, delegated to the underlying real
  // Worker. The node channel carries Node's shapes — `message` the RAW posted
  // value, `error` a bare `Error`, `exit` the numeric exit code, `online` no arg —
  // distinct from the web channel above (`MessageEvent`/`ErrorEvent`). The adders
  // return the handle for chaining.
  on(event: "message", listener: (value: any) => void): this;
  on(event: "messageerror", listener: (error: Error) => void): this;
  on(event: "error", listener: (err: Error) => void): this;
  on(event: "exit", listener: (exitCode: number) => void): this;
  on(event: "online", listener: () => void): this;
  on(event: string | symbol, listener: (...args: any[]) => void): this;
  once(event: "message", listener: (value: any) => void): this;
  once(event: "messageerror", listener: (error: Error) => void): this;
  once(event: "error", listener: (err: Error) => void): this;
  once(event: "exit", listener: (exitCode: number) => void): this;
  once(event: "online", listener: () => void): this;
  once(event: string | symbol, listener: (...args: any[]) => void): this;
  addListener(event: "message", listener: (value: any) => void): this;
  addListener(event: "messageerror", listener: (error: Error) => void): this;
  addListener(event: "error", listener: (err: Error) => void): this;
  addListener(event: "exit", listener: (exitCode: number) => void): this;
  addListener(event: "online", listener: () => void): this;
  addListener(event: string | symbol, listener: (...args: any[]) => void): this;
  off(event: string | symbol, listener: (...args: any[]) => void): this;
  removeListener(event: string | symbol, listener: (...args: any[]) => void): this;
  emit(event: string | symbol, ...args: any[]): boolean;
}
type __NubLibWorkerOrNubWorker = __NubLibDomIsLoaded extends true ? {} : __NubWorker;
interface Worker extends __NubLibWorkerOrNubWorker {}
declare var Worker: __NubUseLibDomIfAvailable<
  "Worker",
  {
    prototype: Worker;
    new (scriptURL: string | URL, options?: WorkerOptions): Worker;
  }
>;

// ── import.meta.hot (Vite-compatible — v0.x, shape committed v0.1) ──
// Forward-compat commitment: ships now so framework authors can code against the
// shape. `import.meta.hot` is `undefined` unless `nub watch --hot` is active.
interface ImportMeta {
  readonly hot?: {
    readonly data: Record<string, any>;
    accept(): void;
    accept(cb: (mod: any) => void): void;
    accept(dep: string, cb: (mod: any) => void): void;
    accept(deps: readonly string[], cb: (mods: any[]) => void): void;
    dispose(cb: (data: Record<string, any>) => void): void;
    invalidate(): void;
    on(event: string, cb: (data: any) => void): void;
    send(event: string, data?: any): void;
  };
}

// ── Recently polyfilled proposal APIs (runtime/polyfills.cjs) ──
// Use declaration merging for existing ECMAScript globals. This is the same shape
// TypeScript's `lib.esnext.*` files and Bun's runtime types use: never redeclare
// `var Promise`, `var Symbol`, etc. Method members safely become overloads when a
// future standard lib adds them. The one property member (`Symbol.metadata`) is
// byte-for-byte TypeScript's already-settled `lib.esnext.decorators` declaration.

// Iterator chunking/includes/join. `IteratorObject` is in lib.es2015.iterable even
// when the ES2025 Iterator constructor is not selected, so built-in iterators gain
// these methods at the ES2024 target too. The versioned package entry point loads
// TypeScript's own Iterator constructor and base-helper declarations.
interface IteratorObject<T, TReturn, TNext> {
  chunks(chunkSize: number): IteratorObject<T[], undefined, unknown>;
  windows(windowSize: number): IteratorObject<T[], undefined, unknown>;
  includes(searchElement: T): boolean;
  join(separator?: string): string;
}

interface Math {
  sumPrecise(items: Iterable<number>): number;
}

interface SymbolConstructor {
  readonly metadata: unique symbol;
}

interface Atomics {
  pause(iterationNumber?: number): void;
}

// Promise.allKeyed / Promise.allSettledKeyed (TC39 await dictionary). The mapped
// types mirror the proposal README: the key set is preserved and each value is
// `Awaited`. Two runtime facts TypeScript cannot express — the result object has a
// null prototype, and only own enumerable keys appear at runtime.
interface PromiseConstructor {
  allKeyed<T extends object>(promises: T): Promise<{ -readonly [K in keyof T]: Awaited<T[K]> }>;
  allSettledKeyed<T extends object>(
    promises: T,
  ): Promise<{ -readonly [K in keyof T]: PromiseSettledResult<Awaited<T[K]>> }>;
}

// Uint8Array base64/hex. These signatures match TypeScript 6's
// lib.esnext.typedarrays exactly, so consumers on older TypeScript versions gain
// them and newer standard libraries merge the identical interface members.
interface Uint8Array<TArrayBuffer extends ArrayBufferLike> {
  toBase64(options?: {
    alphabet?: "base64" | "base64url" | undefined;
    omitPadding?: boolean | undefined;
  }): string;
  setFromBase64(
    string: string,
    options?: {
      alphabet?: "base64" | "base64url" | undefined;
      lastChunkHandling?: "loose" | "strict" | "stop-before-partial" | undefined;
    },
  ): { read: number; written: number };
  toHex(): string;
  setFromHex(string: string): { read: number; written: number };
}
interface Uint8ArrayConstructor {
  fromBase64(
    string: string,
    options?: {
      alphabet?: "base64" | "base64url" | undefined;
      lastChunkHandling?: "loose" | "strict" | "stop-before-partial" | undefined;
    },
  ): Uint8Array<ArrayBuffer>;
  fromHex(string: string): Uint8Array<ArrayBuffer>;
}

// ── Date.prototype.toTemporalInstant (runtime/preload-common.cjs installs it) ──
// Nub assigns the polyfill's `toTemporalInstant` onto Date.prototype on the floor
// (matching native Node, which ships it once Temporal is native).
interface Date {
  toTemporalInstant(): Temporal.Instant;
}
