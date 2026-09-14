#!/usr/bin/env bash
# Build the daily-driver fixture: a minimal Vite + React + TypeScript app using
# pnpm as the package manager. Scaffolded by hand (not by `npm create vite`) so
# the result is deterministic, version-pinned, and fast to install.
#
# The fixture is designed to exercise the full nub surface in a realistic
# project-shaped context:
#   - `nub install`      — PM engine with a real pnpm lockfile
#   - `nub run build`    — executes a Vite build (TS + JSX transpiled by Vite,
#                           not nub — this tests the script-runner path, not the
#                           transpile path)
#   - `nub run type-check` — `tsc --noEmit` on real TS source
#   - `nub <file.ts>`    — nub's own TS transpile on a standalone script that
#                           imports from the fixture's node_modules
#   - `nub --node <script>` — same script with augmentation off (proves the
#                              `--node` flag disables transpilation)
#   - `nub run test`     — `vitest run` (a real test runner using node_modules)
#
# Usage: tests/daily-driver/make-fixture.sh [dest-dir]
# Default dest: /tmp/nub-daily-driver
set -euo pipefail

DEST="${1:-/tmp/nub-daily-driver}"
rm -rf "$DEST"
mkdir -p "$DEST"
cd "$DEST"

cat > package.json <<'JSON'
{
  "name": "nub-daily-driver",
  "private": true,
  "version": "0.0.0",
  "type": "module",
  "packageManager": "pnpm@12.4.1",
  "scripts": {
    "build":      "vite build",
    "type-check": "tsc --noEmit",
    "test":       "vitest run"
  },
  "dependencies": {
    "react":     "18.3.1",
    "react-dom": "18.3.1"
  },
  "devDependencies": {
    "@types/react":     "18.3.20",
    "@types/react-dom": "18.3.6",
    "@vitejs/plugin-react": "4.3.4",
    "typescript": "5.7.3",
    "vite":       "6.3.5",
    "vitest":     "3.2.4"
  }
}
JSON

# pnpm 12 fails an install whose dependency build scripts are undecided, and
# esbuild (under Vite) has one. The project records the decision, as a real app
# approving it would.
cat > pnpm-workspace.yaml <<'YAML'
allowBuilds:
  esbuild: true
YAML

cat > tsconfig.json <<'JSON'
{
  "compilerOptions": {
    "target": "ES2020",
    "lib": ["ES2020", "DOM"],
    "module": "ESNext",
    "moduleResolution": "bundler",
    "jsx": "react-jsx",
    "strict": true,
    "noEmit": true,
    "skipLibCheck": true
  },
  "include": ["src"]
}
JSON

cat > vite.config.ts <<'TS'
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
export default defineConfig({ plugins: [react()] });
TS

mkdir -p src

cat > src/main.tsx <<'TSX'
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
TSX

cat > src/App.tsx <<'TSX'
import React from "react";
export const add = (a: number, b: number): number => a + b;
export default function App() {
  return <div>daily-driver: {add(1, 2)}</div>;
}
TSX

cat > src/App.test.ts <<'TS'
import { test, expect } from "vitest";
import { add } from "./App";
test("add returns the sum", () => {
  expect(add(2, 3)).toBe(5);
});
TS

mkdir -p public
# Vite expects index.html at the project root (not in public/).
cat > index.html <<'HTML'
<!doctype html>
<html><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>
HTML

# Standalone script used by `nub <file.ts>` test — imports from node_modules,
# and uses a const enum (non-erasable TypeScript syntax that Node's built-in
# strip-only mode cannot handle, so `nub --node` must fail while `nub` passes).
cat > run-kleur.ts <<'TS'
// Tests nub's TS transpile path with a real node_modules import.
// kleur ships as CJS; this import must work (CJS-from-ESM augmentation).
// const enum is non-erasable: Node's strip-only mode rejects it, which is
// the load-bearing negative control for the `nub --node` scenario.
import kleur from "kleur";
const enum Color { Green = "GREEN" }
const msg: string = kleur.green(`DAILY-TS-OK ${Color.Green}`);
console.log(msg);
TS

# Add kleur as an extra dep for the standalone TS script.
node -e "
const fs = require('fs');
const pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'));
pkg.dependencies['kleur'] = '4.1.5';
fs.writeFileSync('package.json', JSON.stringify(pkg, null, 2) + '\n');
"

echo "daily-driver fixture scaffolded at $DEST"
echo "Run \`pnpm install\` (or \`nub install\`) to populate node_modules."
