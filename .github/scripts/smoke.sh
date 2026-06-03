#!/usr/bin/env bash
# Post-publish smoke for the installed @nubjs/nub. Run by every test-install leg
# (ubuntu / macOS / windows / linux-musl) against the freshly published package.
# Assumes `nub` + `nubx` are already on PATH (the workflow installs them).
#
# Exits non-zero with a count of failures. Each check prints PASS/FAIL with the
# actual-vs-expected on failure so a CI failure is self-debugging without a rerun.
set -u
PASS=0; FAIL=0
ck() { # ck "name" "expected" "actual"
  if [ "$2" = "$3" ]; then printf '  PASS  %s\n' "$1"; PASS=$((PASS+1));
  else printf '  FAIL  %s\n        expected: %s\n        actual:   %s\n' "$1" "$2" "$3"; FAIL=$((FAIL+1)); fi
}

# Windows (Git-bash) vs POSIX — a few checks rely on a chmod'd shebang bin that
# only resolves on POSIX; guard those.
case "$(uname -s 2>/dev/null)" in MINGW*|MSYS*|CYGWIN*) WIN=1 ;; *) WIN=0 ;; esac

echo "=== environment ==="
node --version
if command -v ldd >/dev/null 2>&1; then echo "libc: $(ldd --version 2>&1 | head -1)"; else echo "libc: n/a (no ldd)"; fi
nub --version

WORK="${RUNNER_TEMP:-/tmp}/nub-smoke"
rm -rf "$WORK"; mkdir -p "$WORK"; cd "$WORK"
# Script runs a file (not an inline `node -e "…"` whose nested quotes are a cmd.exe
# quoting hazard, orthogonal to whether `nub run` works) — tests the common path.
printf '{"name":"smoke","version":"1.0.0","scripts":{"hello":"node hello.js"}}\n' > package.json

echo "=== TypeScript surface ==="
printf 'enum E { A="a", B="b" }\nconsole.log(E.A+E.B)\n' > enum.ts
ck "enum" "ab" "$(nub enum.ts 2>&1)"
printf 'namespace N { export const v = 7 }\nconsole.log(N.v)\n' > ns.ts
ck "namespace" "7" "$(nub ns.ts 2>&1)"
printf 'class P { constructor(public x: number, private y: number){} sum(){return this.x+this.y} }\nconsole.log(new P(2,3).sum())\n' > pp.ts
ck "parameter properties" "5" "$(nub pp.ts 2>&1)"
printf 'const x = 5 satisfies number\nconsole.log(x)\n' > sat.ts
ck "satisfies operator" "5" "$(nub sat.ts 2>&1)"

echo "=== decorators (legacy experimentalDecorators + emitDecoratorMetadata) ==="
printf '{"compilerOptions":{"experimentalDecorators":true,"emitDecoratorMetadata":true}}\n' > tsconfig.json
printf 'function up(_t:any,_k:any,d:any){const o=d.value;d.value=function(){return String(o.apply(this,arguments)).toUpperCase()};return d}\nclass G{ @up g(){return "ok"} }\nconsole.log(new G().g())\n' > dec.ts
ck "legacy decorator" "OK" "$(nub dec.ts 2>&1)"

echo "=== polyfills ==="
printf 'console.log(typeof Temporal?.Now?.instant)\n' > tmp.ts
ck "Temporal polyfill" "function" "$(nub tmp.ts 2>&1)"
printf 'console.log(typeof URLPattern)\n' > urlp.ts
ck "URLPattern" "function" "$(nub urlp.ts 2>&1)"

echo "=== data-format imports (nub-native addon, per-platform) ==="
printf 'name: nub\nport: 5432\nnested:\n  a: 1\n' > d.yaml
printf 'import c from "./d.yaml"\nconsole.log(c.name+":"+c.port+":"+c.nested.a)\n' > yaml.ts
ck "YAML import" "nub:5432:1" "$(nub yaml.ts 2>&1)"
printf 'title = "x"\n[server]\nport = 8080\n' > d.toml
printf 'import c from "./d.toml"\nconsole.log(c.title+":"+c.server.port)\n' > toml.ts
ck "TOML import" "x:8080" "$(nub toml.ts 2>&1)"
printf '{ name: "j5", count: 3, /* c */ }\n' > d.json5
printf 'import c from "./d.json5"\nconsole.log(c.name+":"+c.count)\n' > json5.ts
ck "JSON5 import" "j5:3" "$(nub json5.ts 2>&1)"
printf '{ // c\n "k": "v"\n}\n' > d.jsonc
printf 'import c from "./d.jsonc"\nconsole.log(c.k)\n' > jsonc.ts
ck "JSONC import" "v" "$(nub jsonc.ts 2>&1)"

echo "=== .env auto-load + precedence + same-file expansion ==="
printf 'BASE=world\nGREETING=hi-${BASE}\nONLY_ENV=1\n' > .env
printf 'GREETING=local-win\n' > .env.local
printf 'console.log(process.env.GREETING+"|"+process.env.ONLY_ENV)\n' > env.ts
ck ".env load + .env.local precedence + expansion" "local-win|1" "$(nub env.ts 2>&1)"

echo "=== module resolution (extensionless .ts import) ==="
printf 'export const v = "from-lib"\n' > lib.ts
printf 'import { v } from "./lib"\nconsole.log(v)\n' > main.ts
ck "extensionless .ts import" "from-lib" "$(nub main.ts 2>&1)"

echo "=== nub run <script> ==="
# `nub run` echoes the command ("$ …") to STDERR before running it; the script's own
# output goes to STDOUT. Drop stderr so we assert on the script output, not the echo.
printf 'console.log(42)\n' > hello.js
ck "nub run hello" "42" "$(nub run hello 2>/dev/null | tail -1)"

echo "=== node drop-in flag passthrough ==="
ck "nub -e" "4" "$(nub -e 'console.log(2+2)' 2>&1)"
ck "nub -p" "9" "$(nub -p '3*3' 2>&1)"

if [ "$WIN" -eq 0 ]; then
  echo "=== nubx (POSIX local bin) ==="
  mkdir -p node_modules/.bin
  printf '#!/usr/bin/env node\nconsole.log("bin:"+process.argv.slice(2).join(","))\n' > node_modules/.bin/greet
  chmod +x node_modules/.bin/greet
  ck "nubx local bin + args" "bin:a,b" "$(nubx greet a b 2>&1 | tail -1)"
  ck "nubx --node (compat on exec verb)" "bin:z" "$(nubx --node greet z 2>&1 | tail -1)"
else
  echo "=== nubx local-bin checks skipped on Windows (shebang-bin resolution differs) ==="
fi

echo ""
echo "=== RESULTS: $PASS passed, $FAIL failed ($(uname -s 2>/dev/null), node $(node --version)) ==="
[ "$FAIL" -eq 0 ] || exit 1
