# Sandbox network config surfaces — what a USER can actually write (Codex vs SRT vs Claude Code)

**Question:** When Codex, SRT (`@anthropic-ai/sandbox-runtime`), and Claude Code let a user restrict a sandboxed process's network, what does the user actually WRITE, in which file, and does that config let them express rules at the level of HTTP verbs / paths / headers — or only host/domain allowlists?

**The distinction this doc keeps sharp everywhere:** ENGINE CAPABILITY (what the proxy code can filter internally — a `filterRequest` hook, a rules struct) vs EXPOSED USER-CONFIG SURFACE (what a user can specify in the documented config file). An engine can do per-request filtering while the user-facing config only exposes a host allowlist.

## Bottom line (answers the maintainer's question)

- **Verb/path/header-level rules as a real, exposed, user-facing config surface exist in EXACTLY ONE of the three: Codex** — via `[permissions.<profile>.network.mitm.hooks.*]` in `config.toml`, where a user writes `methods`, `path_prefixes`, `query`, `headers`, `body` matchers. **BUT the caveat is load-bearing:** those hook matchers drive *header actions* (`strip_request_headers` / `inject_request_headers`), **not allow/deny**. Codex's actual *access* decision is host-allowlist + a global coarse `mode = "full" | "limited"` verb toggle (limited = GET/HEAD/OPTIONS only, applied globally, not per-path). So even in Codex you cannot write "deny POST to `/admin/*`" as an access rule — you can write "on POST to `/repos/openai/*`, strip the Authorization header."
- **SRT: NO.** The user-facing JSON settings file (`~/.srt-settings.json`) is host/domain allowlist only (`allowedDomains` / `deniedDomains`), plus unix-socket paths, local-binding, and TLS-terminate domain scoping. The domain schema *explicitly rejects* protocols, paths, and ports. Verb/path/header filtering DOES exist as an engine capability — `network.filterRequest`, a `(Request) => {action}` callback — but it is a **function**, reachable ONLY by a programmatic embedder (TS `SandboxManager.initialize(config)`), and is **unrepresentable in the JSON settings file** (JSON can't carry a function; the zod schema requires `typeof v === 'function'`).
- **Claude Code: NO — and this is the crux the maintainer flagged.** CC vendors SRT but wires it up **host-allowlist-only.** Its user-facing sandbox network schema (settings.json `sandbox.network`) is `allowedDomains` / `deniedDomains` / `allowUnixSockets` / `allowAllUnixSockets` / `allowLocalBinding` / `allowMachLookup` / `httpProxyPort` — no method, no path, no header, no body. CC's bundled cli.js contains **zero** references to `filterRequest`, `tlsTerminate`, `path_prefixes`, or MITM hooks. Network permissions a CC user writes are host/domain only: `sandbox.network.allowedDomains`/`deniedDomains`, plus `WebFetch(domain:...)` / deny permission rules that are validated to be `domain:hostname` (URLs and paths are rejected). **The maintainer is right: verb/path-level network rules are not a thing you can write in Claude Code.**

## Cross-tool comparison

Cell legend: **user** = expressible in the documented user-facing config file · **engine-only** = the proxy can do it but only via programmatic/embedder code, not the config file · **no** = not present at all.

| Granularity          | Codex (`config.toml`)                    | SRT (`~/.srt-settings.json`)        | Claude Code (`settings.json` `sandbox.network`) |
|----------------------|------------------------------------------|-------------------------------------|-------------------------------------------------|
| Host / domain glob   | **user** (`network.domains`, allow/deny) | **user** (`allowedDomains`/`denied`)| **user** (`allowedDomains`/`deniedDomains`)     |
| CIDR / IP literal    | **user** (IP literals in `domains`)      | **user** (host entry, no CIDR mask) | **user** (host entry, no CIDR mask)             |
| Port                 | no (domains are host-only)               | **no** (schema rejects `:`)         | no (domains are host-only)                      |
| HTTP verb / method   | **user, GLOBAL only** (`mode=limited`) + hook *matcher* | engine-only (`filterRequest`) | **no**                                    |
| URL path             | **user, hook MATCHER only** (`path_prefixes`) — drives header actions, not allow/deny | engine-only (`filterRequest`) | **no** |
| Request headers      | **user, match + action** (`match.headers`, `strip/inject_request_headers`) | engine-only (`filterRequest`) | **no** |
| Request body         | **user, hook matcher** (`match.body`)    | engine-only (`filterRequest` body)  | **no**                                          |
| Allow/deny AT verb/path granularity | **no** (hooks only strip/inject headers; access = host + global mode) | engine-only (`filterRequest` returns allow/deny) | **no** |

The single decisive row: **"allow/deny at verb/path granularity" is not a user-facing surface in any of the three.** It exists only as SRT's engine-only `filterRequest`. Codex exposes the finest *matching* vocabulary to users, but bends it to header manipulation, not access control.

---

## Codex

### Config file + schema

Codex's network proxy (`codex-network-proxy`) reads from Codex's merged `config.toml` (via `codex-core`), under the selected permissions profile — `[permissions.<profile>.network]`. Source: `codex/codex-rs/network-proxy/README.md` (Quickstart → Configure) and `codex/codex-rs/network-proxy/src/config.rs`.

Top-level network schema — `NetworkProxyConfig` / `NetworkProxySettings` (`config.rs:19`, `config.rs:128`):
- `enabled`, `proxy_url`, `socks_url`, `enable_socks5`, `allow_upstream_proxy`, `allow_local_binding`, `dangerously_allow_non_loopback_proxy` — transport/plumbing.
- `mode: NetworkMode` — `full` (default) or `limited` (`config.rs:288`).
- `domains: NetworkDomainPermissions` — the host allowlist (`config.rs:42`, `config.rs:48`), each entry `allow` | `deny`.
- `unix_sockets: NetworkUnixSocketPermissions` (`config.rs:121`) — macOS socket-path allowlist.
- `mitm_hooks: Vec<MitmHookConfig>` (`config.rs:156`) — the per-request hooks.

MITM hook schema — `MitmHookConfig` (`mitm_hook.rs:27`), all fields deserialized straight from `config.toml`:
```
MitmHookConfig       { host, match: MitmHookMatchConfig, actions: MitmHookActionsConfig }   // mitm_hook.rs:27
MitmHookMatchConfig  { methods, path_prefixes, query, headers, body }                        // mitm_hook.rs:37
MitmHookActionsConfig{ strip_request_headers, inject_request_headers }                        // mitm_hook.rs:47
```

### Granularity a user can express

Real `config.toml` snippet (verbatim from `network-proxy/README.md`) — the finest a user can write:
```toml
# host allowlist (host-level, glob)
[permissions.workspace.network.domains]
"*.openai.com" = "allow"
"127.0.0.1"    = "allow"
"evil.example" = "deny"

# global coarse verb toggle: "limited" = GET/HEAD/OPTIONS only
mode = "limited"

# per-request MITM hook: matches host+verb+path, ACTION = strip a header
[permissions.workspace.network.mitm.hooks.github_write]
host          = "api.github.com"
methods       = ["POST", "PUT"]
path_prefixes = ["/repos/openai/"]
action        = ["strip_auth"]

[permissions.workspace.network.mitm.actions.strip_auth]
strip_request_headers = ["authorization"]
```

- **Host/domain glob:** yes, user-expressible. `Exact` / `*.host` / `**.host`; global `*` rejected (`policy.rs` DomainPattern; README).
- **CIDR/IP:** IP literals (`127.0.0.1`, `::1`) are allowable domain entries; no CIDR-mask syntax.
- **Port:** not in the domain allowlist (host-only).
- **Verb (global):** `mode = "limited"` blocks all non-safe methods process-wide — `allows_method` permits only GET/HEAD/OPTIONS (`policy.rs:288`, `config.rs:302`). This is a preset, not per-path.
- **Verb / path / header / body (per-request):** user-expressible as **matchers** in a hook (`match.methods`, `match.path_prefixes` with `pattern:`/`literal:`/glob, `match.query`, `match.headers`, `match.body`). BUT the only **actions** are `strip_request_headers` / `inject_request_headers` (`mitm_hook.rs:47`, `mitm_hook.rs:94`). There is **no `deny`/`block` action** keyed on verb/path — a hook mutates headers, it does not gate access.

### Verdict

**Partial-yes, with a caveat that inverts the naive reading.** Codex is the only one of the three whose user config file contains verb/path/header/body vocabulary — but that vocabulary drives header stripping/injection, not allow/deny. User-facing *access* control is host-allowlist + a global `full`/`limited` verb mode. You cannot write "deny POST to `/x`" in Codex config; you can write "on POST to `/x`, strip Authorization."

---

## SRT (`@anthropic-ai/sandbox-runtime`, standalone, v0.0.61)

### Config file + schema

Default user config: `~/.srt-settings.json` (override with `srt --settings <path>`). Loaded by `config-loader.ts` via `JSON.parse` → `SandboxRuntimeConfigSchema.safeParse` (`sandbox-runtime/src/utils/config-loader.ts:19`, `:44`, `:47`). Network schema lives in `sandbox-runtime/src/sandbox/sandbox-config.ts`.

User-facing `network` fields (README §"Network Configuration"; `sandbox-config.ts`):
- `allowedDomains: string[]` (`sandbox-config.ts:231`) — allow-only host list; empty = no network.
- `deniedDomains: string[]` (`:234`) — checked first; `*` accepted here (deny-all).
- `denyByDefault` (`:243`), `allowUnixSockets` (`:249`), `allowAllUnixSockets`, `allowLocalBinding`.
- `tlsTerminate` (`:312`) — `{ caCertPath?, caKeyPath?, excludeDomains }`; turns on in-process HTTPS MITM. Its `excludeDomains` (`:334`) is still **host-pattern** scoping — it decides *which hosts get terminated*, not verb/path rules.
- `filterRequest` (`:299`) — **the per-request hook, and the whole crux for SRT.**

The domain pattern schema **explicitly rejects paths, ports, and protocols** (`sandbox-config.ts:16-19`): any entry containing `://`, `/`, or `:` fails validation. So a user cannot smuggle a path or port into `allowedDomains` even if they tried — the surface is bare hostname/wildcard by construction.

### The engine-only per-request capability

`filterRequest` is declared as `z.custom<FilterRequestCallback>(v => typeof v === 'function', …)` (`sandbox-config.ts:299-301`). Because the settings file is `JSON.parse`d, it can never yield a function, so a `filterRequest` in `~/.srt-settings.json` fails schema validation — it is **structurally unreachable from the config file.** It is supplied only via the programmatic path: `SandboxManager.initialize(config)` with a TS `SandboxRuntimeConfig` (README §Programmatic; `src/index.ts`).

The callback's power (this is the real per-request filtering, `sandbox-runtime/src/sandbox/request-filter.ts`):
```ts
export type RequestDecision       = { action: 'allow' | 'deny', reason?: string }   // request-filter.ts:19
export type FilterRequestCallback = (request: Request) => Promise<RequestDecision>  // request-filter.ts:39
```
`request` is a web-standard `Request` → method, URL (path), headers, and a lazy body (`request-filter.ts:32`, `:88-95`). So the engine CAN allow/deny on verb + path + header + body — but only in embedder code, and only for plain HTTP + (when `tlsTerminate` is set) terminated HTTPS.

`NetworkRestrictionConfig` (`sandbox-schemas.ts:83`) — `{ allowedHosts, deniedHosts }` — is the *internal* structure the host allowlist compiles to; it is host-only and is not the user surface.

### Verdict

**No** at the user-facing config surface. `~/.srt-settings.json` (and the `srt` CLI) expose host/domain allowlists, unix sockets, local binding, and TLS-terminate domain scoping — nothing at verb/path/header/body. Verb/path/header/body allow/deny is a genuine **engine-only** capability (`filterRequest`), reachable exclusively by a programmatic embedder.

---

## Claude Code (`@anthropic-ai/claude-code`)

CC ships the sandbox as a vendored copy of SRT's engine (symbols `SandboxManager`, `SandboxRuntimeConfigSchema`, `convertToSandboxRuntimeConfig` are present in the bundle). Evidence read from the **readable bundled `cli.js` of v2.1.112** (v2.1.150 ships a compiled native binary with no readable JS; the schema below is stable across both).

### Config file + schema

User surface: `sandbox` block in `.claude/settings.json` (user/project/local/managed layers). The network sub-schema is `LH4`, referenced as `network` inside `SandboxRuntimeConfigSchema` (`_p1 = object({ network: LH4.describe("Network restrictions configuration"), … })`).

`LH4` fields (verbatim describe strings from the bundle):
- `allowedDomains: string[]` — `'List of allowed domains (e.g., ["github.com", "*.npmjs.org"])'`
- `deniedDomains: string[]` — `"List of denied domains"`
- `allowUnixSockets: string[]?` — macOS socket paths (ignored on Linux)
- `allowAllUnixSockets: boolean?`
- `allowLocalBinding: boolean?`
- `allowMachLookup: string[]?` — macOS XPC/Mach service names (trailing-wildcard)
- `httpProxyPort: number?` — external proxy port

Separately, CC's **permission rules** feed the host allowlist: `WebFetch(domain:...)` allow rules and deny rules map into `allowedDomains`/`deniedDomains`. `WebFetch` permission content is validated to be **`domain:hostname` only** — the validator rejects URLs and paths (`"WebFetch permissions must use 'domain:' prefix"`, examples `WebFetch(domain:example.com)`).

### Granularity a user can express

A CC user writes network policy two ways, both host/domain-only:
```jsonc
// .claude/settings.json
{
  "sandbox": {
    "network": {
      "allowedDomains": ["github.com", "*.npmjs.org"],
      "deniedDomains": ["telemetry.example.com"],
      "allowUnixSockets": ["/var/run/docker.sock"],
      "allowLocalBinding": false
    }
  },
  "permissions": { "allow": ["WebFetch(domain:api.github.com)"] }   // domain-only; paths/URLs rejected
}
```
- Host/domain glob: yes. CIDR/IP: host entries, no mask. Port: no. **Verb: no. Path: no. Header: no. Body: no.**

### Does CC expose SRT's `filterRequest`? No.

The v2.1.112 bundle contains **0** occurrences of `filterRequest`, **0** of `tlsTerminate`, **0** of `path_prefixes`, **0** of MITM-hook symbols (grep counts over `cli.js`). CC's vendored SRT is wired host-allowlist-only; the per-request filtering capability is neither exposed in settings nor present in the shipped engine surface.

### Verdict

**No.** Claude Code's user-facing network config is host/domain allowlist (+ unix sockets, local binding, mach lookup, external proxy port). There is no user-writable verb/path/header rule anywhere, and CC does not surface SRT's engine-level `filterRequest`. This directly confirms the maintainer's recollection.

---

## Implications for nub's sandbox design

- If nub wants **user-writable verb/path/header network rules**, it would be doing something none of these three expose to users. Codex is the closest prior art, but its per-request vocabulary is scoped to header mutation, and its access control is host + global read-only mode. That is a signal about where the industry has actually drawn the usable-config line: **host allowlist is the universal user surface; per-request filtering is an embedder/engine concern.**
- The SRT/CC split is the cleanest illustration of ENGINE vs SURFACE: the SAME engine ships `filterRequest`, and the SAME engine as vendored by CC exposes only host allowlists. A capability existing in the engine says nothing about whether users can reach it.
- A defensible nub posture: host/domain allowlist as the user surface (matches all three), with any per-request logic kept as an internal/programmatic mechanism rather than a documented config knob — unless nub deliberately chooses to be the first to expose verb/path allow-deny to users, which none of the incumbents do.

## Changelog
- 2026-07-09 — Initial write-up. Codex `config.toml` mitm hooks (verb/path/header matchers → header actions) from `network-proxy/src/{config,mitm_hook,policy}.rs`; SRT host-only JSON settings + engine-only `filterRequest` from `sandbox-config.ts`/`request-filter.ts`/`config-loader.ts` (v0.0.61); Claude Code host-allowlist-only surface + absence of `filterRequest`/`tlsTerminate` from the `@anthropic-ai/claude-code@2.1.112` bundled `cli.js` (`LH4` network schema).
