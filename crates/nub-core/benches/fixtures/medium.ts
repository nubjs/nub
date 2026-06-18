// Shared bench fixture: a realistic medium TS module (~3 KB). Used by
// cache_hash.rs as the source text the transpile cache key hashes over.
import type { Readable } from "node:stream";
import { EventEmitter } from "node:events";

export interface Config {
  name: string;
  retries: number;
  tags: readonly string[];
  nested: { enabled: boolean; threshold?: number };
}

export type Handler<T> = (input: T) => Promise<Result<T>>;

export type Result<T> =
  | { ok: true; value: T }
  | { ok: false; error: Error };

export enum Level {
  Debug = 0,
  Info = 1,
  Warn = 2,
  Error = 3,
}

const DEFAULTS: Config = {
  name: "default",
  retries: 3,
  tags: ["a", "b"],
  nested: { enabled: true },
};

export class Pipeline<T extends Record<string, unknown>> extends EventEmitter {
  private readonly handlers: Handler<T>[] = [];
  constructor(
    private readonly config: Config = DEFAULTS,
    public level: Level = Level.Info,
  ) {
    super();
  }

  use(handler: Handler<T>): this {
    this.handlers.push(handler);
    return this;
  }

  async run(input: T): Promise<Result<T>> {
    let current = input;
    for (const handler of this.handlers) {
      const result = await handler(current);
      if (!result.ok) {
        this.emit("error", result.error);
        return result;
      }
      current = result.value;
    }
    return { ok: true, value: current };
  }

  get size(): number {
    return this.handlers.length;
  }
}

export function compose<A, B, C>(
  f: (a: A) => B,
  g: (b: B) => C,
): (a: A) => C {
  return (a) => g(f(a));
}

export async function collect<T>(source: Readable): Promise<T[]> {
  const out: T[] = [];
  for await (const chunk of source) {
    out.push(chunk as T);
  }
  return out;
}

const registry = new Map<string, Pipeline<Record<string, unknown>>>();

export function register(name: string, p: Pipeline<Record<string, unknown>>): void {
  if (registry.has(name)) {
    throw new Error(`duplicate: ${name}`);
  }
  registry.set(name, p);
}

export const levels: Record<Level, string> = {
  [Level.Debug]: "debug",
  [Level.Info]: "info",
  [Level.Warn]: "warn",
  [Level.Error]: "error",
};
