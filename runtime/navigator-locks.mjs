// Web Locks API polyfill for Node < 24.5 (native on Node 24.5+, #58666).
// Single-process only — locks don't coordinate across worker threads (a deliberate
// scope: the common in-process serialization use is covered; cross-thread coordination
// would need a SharedArrayBuffer waitlist and is out of scope until there's demand).
//
// Spec: https://w3c.github.io/web-locks/. Modeled to match Node's own
// internal/locks.js (the native impl on 24.5+) so behavior is identical across the
// version boundary — including `steal`, AbortSignal integration, and the option/name
// validation that the WPT web-locks suite exercises. Requires a `navigator` object to
// host `navigator.locks`; navigator-shim.mjs backfills that on Node < 21 and MUST run
// first.

if (typeof globalThis.navigator === "object" && typeof globalThis.navigator.locks === "undefined") {
  const AbortSig = globalThis.AbortSignal;

  const abortError = (msg) => new DOMException(msg || "The operation was aborted", "AbortError");
  const stolenError = () => abortError("The lock was stolen by another request");
  const notSupported = (msg) => new DOMException(msg, "NotSupportedError");

  class Lock {
    #name;
    #mode;
    constructor(name, mode) {
      this.#name = name;
      this.#mode = mode;
    }
    get name() { return this.#name; }
    get mode() { return this.#mode; }
  }

  // held: name → { mode, holders:Set<Holder> }. A shared grant has N holders sharing
  // one record; an exclusive grant has exactly one. queue: name → Array<Waiter> (FIFO).
  const held = new Map();
  const queue = new Map();

  function canAcquire(name, mode) {
    const rec = held.get(name);
    if (!rec) return true;
    return mode === "shared" && rec.mode === "shared";
  }

  // A FRESH request may be granted immediately only when no waiters are already
  // queued for the name — otherwise it must queue behind them. This is the
  // reader/writer FAIRNESS rule: a new shared request must NOT barge ahead of a
  // pending exclusive request and join the current shared holders, which would
  // starve the writer (WPT mode-mixed "An exclusive lock between shared locks").
  // drainQueue, which only ever grants from the FRONT of the queue, keeps using
  // canAcquire directly.
  function canGrantNow(name, mode) {
    const q = queue.get(name);
    if (q && q.length > 0) return false;
    return canAcquire(name, mode);
  }

  function addHolder(name, mode, holder) {
    const rec = held.get(name);
    if (rec) rec.holders.add(holder);
    else held.set(name, { mode, holders: new Set([holder]) });
  }

  function removeHolder(name, holder) {
    const rec = held.get(name);
    if (!rec) return;
    rec.holders.delete(holder);
    if (rec.holders.size === 0) held.delete(name);
  }

  function enqueue(name, waiter) {
    let q = queue.get(name);
    if (!q) queue.set(name, (q = []));
    q.push(waiter);
  }

  // Grant as many head-of-queue waiters as the current held state allows: a head
  // exclusive takes the lock alone; a run of head shared waiters is all granted.
  function drainQueue(name) {
    const q = queue.get(name);
    if (!q || q.length === 0) return;
    while (q.length > 0) {
      const first = q[0];
      if (!canAcquire(name, first.mode)) break;
      q.shift();
      first.grant(); // synchronously addHolders, so canAcquire reflects it next iter
      if (first.mode === "exclusive") break;
    }
    if (q.length === 0) queue.delete(name);
  }

  // Run `cb(lock)` as a holder of (name, mode); return the promise that settles when
  // the holder releases (cb's result/throw) — or rejects early if a steal BREAKS it.
  // The callback is invoked one microtask later (matching Node), so a synchronously
  // signaled abort is observed before the callback runs.
  function runHolder(name, mode, cb) {
    const lock = new Lock(name, mode);
    let broken = false;
    let rejectReleased;
    const holder = {
      mode,
      break(reason) {
        if (broken) return;
        broken = true;
        // Remove WITHOUT draining — the stealer takes the lock next; queued waiters
        // stay pending until the steal's own holder releases.
        removeHolder(name, holder);
        rejectReleased(reason);
      },
    };
    addHolder(name, mode, holder);
    return new Promise((resolve, reject) => {
      rejectReleased = reject;
      Promise.resolve()
        .then(() => cb(lock))
        .then(
          (value) => { if (!broken) { removeHolder(name, holder); resolve(value); drainQueue(name); } },
          (err) => { if (!broken) { removeHolder(name, holder); reject(err); drainQueue(name); } },
        );
    });
  }

  // The lock engine: returns the `released` promise. `cb` receives the granted Lock
  // (or null on an ifAvailable miss). Mirrors the grant/steal/ifAvailable/queue shape
  // of Node's internalBinding('locks').request.
  function engineRequest(name, mode, steal, ifAvailable, cb) {
    if (steal) {
      const rec = held.get(name);
      if (rec) for (const h of [...rec.holders]) h.break(stolenError());
      return runHolder(name, "exclusive", cb);
    }
    if (canGrantNow(name, mode)) {
      return runHolder(name, mode, cb);
    }
    if (ifAvailable) {
      return Promise.resolve().then(() => cb(null));
    }
    return new Promise((resolve, reject) => {
      const waiter = {
        mode,
        settled: false,
        grant() {
          if (waiter.settled) return;
          waiter.settled = true;
          runHolder(name, mode, cb).then(resolve, reject);
        },
      };
      enqueue(name, waiter);
    });
  }

  class LockManager {
    async request(name, optionsOrCallback, maybeCallback) {
      let options = optionsOrCallback;
      let callback = maybeCallback;
      if (callback === undefined) {
        callback = options;
        options = undefined;
      }

      // WebIDL DOMString coercion (throws TypeError on a Symbol, like the binding).
      name = `${name}`;
      if (typeof callback !== "function") {
        throw new TypeError("Failed to execute 'request' on 'LockManager': parameter 2 is not a function.");
      }
      if (options === undefined || typeof options === "function") options = {};

      const mode = options.mode === undefined ? "exclusive" : options.mode;
      if (mode !== "exclusive" && mode !== "shared") {
        throw new TypeError(`Failed to execute 'request' on 'LockManager': '${mode}' is not a valid value for enumeration LockMode.`);
      }
      const ifAvailable = !!options.ifAvailable;
      const steal = !!options.steal;
      const signal = options.signal;
      if (signal !== undefined && signal !== null && !(AbortSig && signal instanceof AbortSig)) {
        throw new TypeError("Failed to execute 'request' on 'LockManager': member signal is not of type AbortSignal.");
      }

      // Already-aborted signal rejects with its reason BEFORE the option-combo checks
      // (matching Node's signal.throwIfAborted() ordering).
      if (signal && signal.aborted) {
        throw signal.reason || abortError();
      }
      if (name[0] === "-") {
        throw notSupported("Lock name may not start with hyphen '-'");
      }
      if (ifAvailable && steal) {
        throw notSupported("ifAvailable and steal options cannot be used together");
      }
      if (mode !== "exclusive" && steal) {
        throw notSupported("mode must be 'exclusive' when using the steal option");
      }
      if (signal && (steal || ifAvailable)) {
        throw notSupported("signal cannot be used with the steal or ifAvailable options");
      }

      // Signal path: the callback is deferred and skipped if the signal aborts before
      // it runs; the abort rejects the OUTER promise iff the callback hasn't entered
      // (lockGranted false). Verbatim shape of Node's internal/locks.js so a queued
      // abort releases the slot (callback skipped) and the next waiter is granted.
      if (signal) {
        return new Promise((resolve, reject) => {
          let lockGranted = false;
          const onAbort = () => {
            if (!lockGranted) reject(signal.reason || abortError());
          };
          signal.addEventListener("abort", onAbort, { once: true });
          const wrapped = (lock) =>
            Promise.resolve().then(() => {
              if (signal.aborted) return undefined;
              lockGranted = true;
              return callback(lock);
            });
          const released = engineRequest(name, mode, false, false, wrapped);
          released.then(resolve, reject).finally(() => signal.removeEventListener("abort", onAbort));
        });
      }

      return engineRequest(name, mode, steal, ifAvailable, (lock) => callback(lock));
    }

    async query() {
      // clientId is per-realm and opaque: `node-<pid>-0` (single-process; this polyfill
      // does not coordinate across worker threads, so the threadId slot is always 0 —
      // and we avoid importing node:worker_threads to keep this module builtin-free for a
      // cheap bootstrap). The cross-context distinct-id WPT cases are browser-specific.
      const clientId = `node-${process.pid}-0`;
      const heldOut = [];
      for (const [name, rec] of held) {
        for (let i = 0; i < rec.holders.size; i++) heldOut.push({ name, mode: rec.mode, clientId });
      }
      const pending = [];
      for (const [name, q] of queue) {
        for (const w of q) if (!w.settled) pending.push({ name, mode: w.mode, clientId });
      }
      return { held: heldOut, pending };
    }
  }

  Object.defineProperty(globalThis.navigator, "locks", {
    value: new LockManager(),
    enumerable: true,
    configurable: true,
  });
}
