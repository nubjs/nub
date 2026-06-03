# Benchmarks

Reproducible benchmark suite for Nub. All benchmarks use [hyperfine](https://github.com/sharkdp/hyperfine).

## Prerequisites

```sh
# Install hyperfine
brew install hyperfine   # macOS
# or: cargo install hyperfine

# Build the release binary
cargo build --release

# Install comparison tools (if not already present)
npm install -g pnpm@latest tsx
```

## Running the benchmarks

### Quick run (script runner + TS execution)

```sh
# Set up the benchmark project
mkdir -p /tmp/nub-bench && cd /tmp/nub-bench
echo '{"name":"bench","scripts":{"noop":"echo hi","hello-ts":"node hello.ts","hello-js":"node hello.js"}}' > package.json
echo 'console.log("hello")' > hello.ts
echo 'console.log("hello")' > hello.js

NUB=./target/release/nub  # adjust path

# Script runner overhead (pure orchestration)
hyperfine --warmup 3 --runs 20 \
  "$NUB run noop" \
  "pnpm run noop" \
  "npm run noop"

# TS execution
hyperfine --warmup 3 --runs 20 \
  "$NUB hello.ts" \
  "tsx hello.ts" \
  "node hello.js"

# Script runner + TS
hyperfine --warmup 3 --runs 20 \
  "$NUB run hello-ts" \
  "pnpm run hello-ts"

# Cold vs warm transpile cache
rm -rf ~/.cache/nub/transpile/*
hyperfine --warmup 0 --runs 10 "$NUB hello.ts"  # cold
hyperfine --warmup 3 --runs 20 "$NUB hello.ts"  # warm
```

### Profiling preload overhead

```sh
# Bare Node baseline
hyperfine --warmup 5 --runs 30 "node -e 'console.log(1)'"

# Full preload init time (measured from inside Node)
node -e "
const t0 = performance.now();
await import('./runtime/preload.mjs');
console.log('preload init: ' + (t1-t0).toFixed(1) + 'ms');
"

# Just registerHooks (no imports)
cat > /tmp/bare-hooks.mjs << 'EOF'
import module from "node:module";
module.registerHooks({ resolve(s,c,n){return n(s,c)}, load(u,c,n){return n(u,c)} });
EOF
hyperfine --warmup 5 --runs 30 "node --import file:///tmp/bare-hooks.mjs -e 'console.log(1)'"
```

### Multi-file project

The `benchmarks/multi-file/` directory contains a 100-module project for testing import-graph scaling:

```sh
hyperfine --warmup 3 --runs 10 "$NUB benchmarks/multi-file/entry.ts"
```

## Current results

See [results.md](results.md) for the latest numbers.

## Hardware

Results are hardware-dependent. Always record:
- CPU model + core count
- RAM
- Node version (`node --version`)
- Nub version (`nub --version`)
- Rust version (`rustc --version`)
- OS + architecture
