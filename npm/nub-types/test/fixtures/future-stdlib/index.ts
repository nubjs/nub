const keyed = await Promise.allKeyed({ count: Promise.resolve(1) });
const count: number = keyed.count;
const chunks: IteratorObject<number[], undefined, unknown> = Iterator.from([1, 2]).chunks(2);
const precise: number = Math.sumPrecise([1, 2]);
const metadata: symbol = Symbol.metadata;
const bytes: Uint8Array<ArrayBuffer> = Uint8Array.fromHex("00");
Atomics.pause();
void [count, chunks, precise, metadata, bytes];
export {};
