# Detecting whether an install script wants the network: HTTP proxy vs. OS-enforced block

**Question:** can pointing a lifecycle script's `HTTP_PROXY`/`HTTPS_PROXY` at a loopback listener tell us, per package, whether that package attempts network access — and which hosts it wants? The capability ladder answers only pass/fail, at the cost of a 55-state walk; a proxy log would name the hosts.

**Verdict: usable as a one-sided detector, never as a network-need oracle.** A request arriving at the proxy proves the package wants the network and names the host, with no false positives observed. Proxy *silence* proves nothing: on a 15-package panel it missed one of the nine packages that genuinely need the network, and the mechanism behind that miss is structural rather than version-gated. A proxy alone under-reports network need, which is the unsafe direction for a grant.

## Results

Three arms per package, each differing from the control in exactly one variable, run on macOS 26.5.2 arm64 against real Seatbelt enforcement with nub's build jail:

| arm | grant | proxy env |
| --- | --- | --- |
| CTRL | `{"write":"disk","network":true}` | none — ground-truth artifacts |
| P | `{"write":"disk","network":true}` | pointed at a logging loopback proxy that forwards |
| B | `{"write":"disk"}` | none — Seatbelt denies all egress |

Classification compares **artifacts**, not exit codes: several packages fall back to a local build when denied the network and still exit 0.

| package | proxy reqs from script | hosts seen | client | block arm | ladder `grant.network` | class |
| --- | --- | --- | --- | --- | --- | --- |
| `@apollo/rover@0.23.0` | 3 | rover.apollo.dev, github.com, release-assets.githubusercontent.com | node | rc 1, 7 paths lost | true | agree / network |
| `dprint@0.19.2` | 2 | github.com, release-assets.githubusercontent.com | **curl** | rc 1, 5 paths lost | true | agree / network |
| `esbuild@0.11.23` | 1 | registry.npmjs.org | node | rc 1, 11 paths lost | true | agree / network |
| `gifsicle@4.0.1` | 1 | raw.githubusercontent.com | node | rc 0, binary differs | true | agree / network |
| `iedriver@4.0.0` | 4 | github.com, release-assets.githubusercontent.com | node | rc 1, 4 paths lost | true | agree / network |
| `keytar@7.9.0` | 2 | github.com, release-assets.githubusercontent.com | node | rc 0, 2 paths differ | true | agree / network |
| `mongodb-memory-server@9.5.0` | 2 | fastdl.mongodb.org | node | rc 0, 1 path lost | true | agree / network |
| `purescript@0.15.9` | 2 | github.com, release-assets.githubusercontent.com | node | rc 1, 8 paths lost | true | agree / network |
| `@arkweid/lefthook@0.7.7` | 0 | — | — | identical to control | false | agree / no network |
| `@evilmartians/lefthook@2.1.10` | 0 | — | — | identical to control | false | agree / no network |
| `cz-customizable@2.6.0` | 0 | — | — | identical to control | false | agree / no network |
| `esbuild@0.21.5` | 0 | — | — | identical to control | false | agree / no network |
| `node-jq@4.4.0` | **0** | — | — | **rc 1, 6 paths lost** | true | ⛔ **disagree — false negative** |
| `device-detector-js@1.0.9` | 1 | github.com | **git-remote-https** | identical to control | false | proxy-positive, block-neutral |
| `phantomjs-prebuilt@2.1.16` | 0 | — | — | rc 1, control also rc 1 | true | excluded — broken at full grant |

Of the 14 answerable packages: **8 agree on network, 4 agree on no network, 1 disagrees, 1 is proxy-positive but block-neutral.** Every proxy hit corresponded to a real network attempt; the detector produced no false positives. It produced one false negative out of nine network-needing packages.

The panel was drawn from the darwin-arm64 corpus records, chosen to span binary downloaders, non-downloading postinstalls, and non-Node lifecycle entries (a `sh` script, a `curl`, a `git` fetch, a `prebuild-install`). Two intended members were dropped: `svf-lib@1.0.999` downloads a multi-gigabyte LLVM release, and `use-mask-input@3.3.2` runs `npx patch-package`, whose interactive registry fetch measures npx rather than the package.

## The one disagreement, and why no Node version fixes it

The `jq` binary (807,984 bytes) is present in the control and proxy arms and absent in the block arm, so `node-jq@4.4.0` genuinely needs the network. The proxy logged 78 requests during that run, every one of them from `nub` itself.

The download runs through `node-downloader-helper@2.1.11`, whose `__initProtocol` builds its own agent and passes it explicitly:

```js
// node-downloader-helper@2.1.11, dist/index.js — the request never consults proxy env
this.__defaultHttpsAgent = new https.Agent({ keepAlive: false });
// ...
b.agent = this.__defaultHttpsAgent;   // options.agent, handed to https.request
```

Node's built-in proxy support attaches to the **default global agent** — the upstream changelog for the feature says so directly: "the default global agent would parse the `http_proxy`/`HTTP_PROXY`, `https_proxy`/`HTTPS_PROXY`, `no_proxy`/`NO_PROXY` settings from the environment variables". Supplying `options.agent` opts out of that path entirely, so no proxy variable, and no `NODE_USE_ENV_PROXY`, has any effect.

Reproduced in isolation, with every proxy variable set and the switch on:

```
custom-agent https.get status 200      # the request succeeded, direct to registry.npmjs.org
--- PROXY connect lines ---     0      # the proxy never saw it
--- CONNECT-HOOK connect lines ---
{"kind":"connect","host":"registry.npmjs.org","loopback":false}
```

A custom agent is ordinary practice in download libraries — it is how a package sets `keepAlive`, a timeout, or a certificate. This class of miss is not a version floor that will age out.

## Proxy honouring is Node-version-gated, and the gate is recent

Measured across nine Node majors, each with a positive control (`fetch`, agreed to be proxy-aware from 24.0) and a negative control (the same probe with `NODE_USE_ENV_PROXY` unset):

| Node | `https.get`, switch on | `https.get`, switch unset | `fetch`, switch on |
| --- | --- | --- | --- |
| 18.20.4 | direct | direct | direct |
| 20.19.0 | direct | direct | direct |
| 22.15.0 | direct | direct | direct |
| 22.16.0 | direct | — | — |
| 22.23.1 | **proxy** | direct | **proxy** |
| 23.11.0 | direct | direct | direct |
| 24.1.0 | direct | direct | **proxy** |
| 24.2.0 | direct | — | — |
| 24.9.0 | **proxy** | — | — |
| 24.17.0 | **proxy** | direct | **proxy** |
| 25.9.0 | **proxy** | direct | **proxy** |
| 26.5.0 | **proxy** | direct | **proxy** |

Three facts follow, and each bounds what a proxy detector can see:

- Built-in `node:http`/`node:https` proxy support arrived in **Node 24.5.0** (upstream PR #58980); the measurement brackets it at 24.2 direct, 24.9 proxied, and finds it backported into the 22 line between 22.16.0 and 22.23.1. `fetch` support arrived in 24.0.0 (PR #57165).
- Below that line — including nub's fast-tier classifier floor of 22.15 and the whole 18/20 support range — **no** built-in client honours proxy env. A detector run there sees only packages that route through a proxy-aware library or a non-Node child.
- Without `NODE_USE_ENV_PROXY=1` nothing routes at all, `fetch` on Node 26 included. The negative control is unambiguous.

## Two gaps in nub's own env allowlist

The build jail's default-deny allowlist (`build_jail_env_allowed`, `crates/nub-sandbox/src/compiler/defaults.rs`) admits `http_proxy`, `https_proxy`, `HTTP_PROXY`, `HTTPS_PROXY`, `no_proxy`, and `NO_PROXY` — but **not `NODE_USE_ENV_PROXY`, and not `ALL_PROXY`**.

Measured consequence: a jailed postinstall calling `fetch()` reached the network normally while the proxy logged nothing from it. Forcing `NODE_USE_ENV_PROXY=1` through the catalog's `env` array made all three synthetic controls route. **A proxy detector is inert against every built-in Node client until that key is admitted.** The sandbox path already knows this — `set_proxy_env` in `crates/nub-sandbox/src/backend/mod.rs` sets seven proxy keys, removes the bypass keys, and sets `NODE_USE_ENV_PROXY=1` — but the build jail constructs its lifecycle env through the allowlist instead, so it does not inherit that.

Worth noting alongside: nub already ships a complete loopback egress proxy (`crates/nub-sandbox/src/proxy/`) speaking HTTP `CONNECT` and SOCKS5 with per-host SNI gating, used for the sandbox net axis. On macOS the sandbox backend can also emit `(allow network* (remote ip "localhost:<port>"))`, which is the "reachable only through the proxy" shape. The build jail cannot express that today: its net axis is a per-package boolean, so its only two states are all egress or none.

## Where each detector is blind

The two candidate detectors miss disjoint sets, and both misses were reproduced rather than inferred:

| | HTTP proxy | connect-hook (`net_gate_shim.js`) |
| --- | --- | --- |
| Node client, default agent | seen, with hostname — only on Node ≥ 24.5 with the switch set | seen |
| Node client, **custom `options.agent`** | **missed** (`node-jq`) | seen |
| Spawned `curl` / `git` / `sh` | seen, with hostname (`dprint`, `device-detector-js`) | **missed** — a non-Node child is never reached |
| Native addon opening a raw socket | missed | missed |

The connect-hook's own header records the complementary measurement from the Windows corpus: 178 of the 179 packages that contact any host enter through Node or an npm `.cmd` shim. That is the reach of the hook, not of the proxy, and the two numbers describe different surfaces.

`device-detector-js@1.0.9` is the case that shows what a proxy buys beyond the ladder. Its `install.sh` runs `napa`, which fetches from GitHub over `git-remote-https`; the proxy logged that request by hostname. The block arm is byte-identical to the control and the ladder records `grant.network` as absent — the fetch is attempted but not needed. The proxy sees **intent**; the ladder sees only **need**. That is a real capability the 55-state walk cannot produce, and it is also why a proxy hit must not be read as "this package requires network."

## Recommendation

- **Use the proxy for what it is sound at:** naming hosts, and confirming a network attempt. As a source for a `networkHosts` catalog axis it is the only instrument that produces the data at all.
- **Never derive a network grant from proxy silence.** On this panel that would have under-granted `node-jq@4.4.0`, and below Node 24.5 it would under-grant nearly every package that uses a built-in client.
- **A trustworthy network-need signal needs the block arm.** Denying egress and diffing artifacts against a full-grant control is what actually answers "does this package need the network", and it is what the ladder already does.
- **Combining the proxy with the connect-hook narrows the blind spot but does not close it.** Together they cover every case measured here; a native addon opening a raw socket defeats both, and only OS-level enforcement sees that.
- **Admit `NODE_USE_ENV_PROXY` (and `ALL_PROXY`) to the build-jail env allowlist** before any proxy-based observation is attempted, or it will measure nothing and read as "no package uses the network."

## Method notes

Two harness defects each produced confident wrong findings before they were caught, and both are worth carrying forward:

- **A catalog-listed package's lifecycle runs during `nub install`, not `approve-builds`** — the log says `WARN defaultTrust: running build scripts for <pkg>`. Setting the proxy env only on the approve step made `gifsicle`, `dprint`, and `@apollo/rover` all read as proxy bypasses; all three agree once the env rides both steps. The tell was that the *same* dprint installed as a `file:` dependency, whose script does run under `approve-builds`, produced two proxy hits from `curl`. Separating nub's own registry traffic from the script's is done by client attribution — `lsof -nP -iTCP:<source-port>` on the proxy's accepted socket resolves each connection to a command and pid — not by which step the variable was set on.
- **The artifact digest must exclude nub's own bookkeeping.** Scanning the whole fixture root pulled in `home/.local/share/nub/store/v1/index/<hash>/…`, whose hash directory is per-run: two arms that did identical work differed by 254 paths. The package's own home caches stay in scope, since those are script output.

Instrument validation ran before any result was read, in both directions. Three synthetic `file:` packages — one calling `fetch`, one `https.get`, one spawning `curl` — succeed under the control and proxy arms and fail under the block arm with `ENOTFOUND`, `ENOTFOUND`, and `curl: (6) Could not resolve host`, which is what establishes that arm B's Seatbelt denial is real rather than assumed. A fourth probe confirmed the jailed interpreter is the host Node (v26.5.0) and that all seven proxy keys reach the lifecycle script and its `sh` grandchild. Every cell was checked for `catalog OVERRIDDEN` and the absence of `REJECTED`.

## Changelog

- 2026-08-05 — Initial write-up. N=15 on macOS/Seatbelt.
