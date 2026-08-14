// Models a future TypeScript standard library after every proposal member Nub
// supplies has landed. These intentionally repeat @nubjs/types' declarations:
// interface merging must keep the pair collision-free with skipLibCheck disabled.
interface IteratorObject<T, TReturn, TNext> {
  chunks(chunkSize: number): IteratorObject<T[], undefined, unknown>;
  windows(windowSize: number): IteratorObject<T[], undefined, unknown>;
  includes(searchElement: T): boolean;
  join(separator?: string): string;
}

interface Math {
  sumPrecise(items: Iterable<number>): number;
}

interface RegExpConstructor {
  escape(string: string): string;
}

interface ErrorConstructor {
  isError(error: unknown): error is Error;
}

interface IteratorConstructor {
  concat<T extends readonly Iterable<unknown>[]>(
    ...items: T
  ): IteratorObject<T[number] extends Iterable<infer U> ? U : never, undefined, unknown>;
}

interface Map<K, V> {
  getOrInsert(key: K, defaultValue: V): V;
  getOrInsertComputed(key: K, callback: (key: K) => V): V;
}

interface WeakMap<K extends WeakKey, V> {
  getOrInsert(key: K, defaultValue: V): V;
  getOrInsertComputed(key: K, callback: (key: K) => V): V;
}

interface SymbolConstructor {
  readonly metadata: unique symbol;
}

interface Atomics {
  pause(iterationNumber?: number): void;
}

interface PromiseConstructor {
  allKeyed<T extends object>(promises: T): Promise<{ -readonly [K in keyof T]: Awaited<T[K]> }>;
  allSettledKeyed<T extends object>(
    promises: T,
  ): Promise<{ -readonly [K in keyof T]: PromiseSettledResult<Awaited<T[K]>> }>;
  try<T, U extends unknown[]>(
    callbackFn: (...args: U) => T | PromiseLike<T>,
    ...args: U
  ): Promise<Awaited<T>>;
}

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
