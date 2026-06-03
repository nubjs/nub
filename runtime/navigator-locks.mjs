// Web Locks API polyfill for Node 22.x (native on Node 24.5+).
// Single-process only — locks don't coordinate across workers.

if (typeof globalThis.navigator === "object" && typeof globalThis.navigator.locks === "undefined") {
  // Track held locks: name → { mode, count } where count > 1 means shared holders
  const held = new Map();
  const queue = new Map();

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

  function canAcquire(name, mode) {
    const current = held.get(name);
    if (!current) return true;
    if (mode === "shared" && current.mode === "shared") return true;
    return false;
  }

  function acquire(name, mode) {
    const current = held.get(name);
    if (current && mode === "shared" && current.mode === "shared") {
      current.count++;
    } else {
      held.set(name, { mode, count: 1 });
    }
  }

  function release(name) {
    const current = held.get(name);
    if (!current) return;
    current.count--;
    if (current.count <= 0) {
      held.delete(name);
      drainQueue(name);
    }
  }

  function drainQueue(name) {
    const q = queue.get(name);
    if (!q || q.length === 0) return;

    // Try to grant as many queued requests as possible.
    // If the first queued is shared, grant all consecutive shared requests.
    // If the first queued is exclusive, grant only that one.
    const first = q[0];
    if (canAcquire(name, first.mode)) {
      if (first.mode === "exclusive") {
        q.shift();
        first.resolve();
      } else {
        // Grant all consecutive shared requests.
        while (q.length > 0 && q[0].mode === "shared") {
          const req = q.shift();
          req.resolve();
        }
      }
    }
  }

  class LockManager {
    async request(name, optionsOrCallback, callback) {
      let options = {};
      if (typeof optionsOrCallback === "function") {
        callback = optionsOrCallback;
      } else {
        options = optionsOrCallback || {};
      }

      const mode = options.mode || "exclusive";
      const ifAvailable = options.ifAvailable || false;
      const signal = options.signal;

      if (signal?.aborted) {
        throw signal.reason || new DOMException("Lock request aborted", "AbortError");
      }

      if (!canAcquire(name, mode)) {
        if (ifAvailable) {
          return callback(null);
        }
        await new Promise((resolve, reject) => {
          if (!queue.has(name)) queue.set(name, []);
          queue.get(name).push({ resolve, reject, mode });
          if (signal) {
            signal.addEventListener("abort", () => {
              const q = queue.get(name) || [];
              const idx = q.findIndex((e) => e.resolve === resolve);
              if (idx !== -1) q.splice(idx, 1);
              reject(signal.reason || new DOMException("Lock request aborted", "AbortError"));
            }, { once: true });
          }
        });
      }

      acquire(name, mode);
      const lock = new Lock(name, mode);

      try {
        return await callback(lock);
      } finally {
        release(name);
      }
    }

    async query() {
      const heldLocks = [];
      for (const [name, info] of held) {
        for (let i = 0; i < info.count; i++) {
          heldLocks.push({ name, mode: info.mode, clientId: "" });
        }
      }
      const pending = [];
      for (const [name, q] of queue) {
        for (const req of q) {
          pending.push({ name, mode: req.mode, clientId: "" });
        }
      }
      return { held: heldLocks, pending };
    }
  }

  Object.defineProperty(globalThis.navigator, "locks", {
    value: new LockManager(),
    enumerable: true,
    configurable: true,
  });
}
