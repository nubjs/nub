# U5 MITM credential-brokering tier — independent security re-audit

**Verdict: GO.** All seven target security properties HOLD. No CRITICAL / HIGH / MEDIUM findings. One LOW (bounded, confined, recommend-only) resource-exhaustion vector and a set of INFO-level defense-in-depth / documentation notes. The tier is safe to ship as-is; the LOW + INFO items are a hardening backlog, not blockers.

- **Scope:** the U5 MITM credential-brokering tier of nub-sandbox — TLS interception + credential brokering + ephemeral CA.
- **Pinned code (Gate 1):** PR #414, head `b624730c6ffae97e9b7cee021a2d19a9a0a0877b`, branch `sandbox-primitives`. Read-only worktree at `~/.cache/nub/worktrees/u5-audit`. Files: `crates/nub-sandbox/src/proxy/{ca,mitm,sni,handshake,mod}.rs`, `src/policy.rs`, `src/compiler/{fold,resolve,mod}.rs`, `src/matcher/host.rs`, `src/backend/mod.rs`, `crates/nub-cli/src/cli.rs` (sole compile caller).
- **Reference (Gate):** sound TLS-MITM / secrets-handling security practice (not a competitor tool). Design intent grounded in `wiki/research/sandbox-mitm-tier-proposal.md` §5–7.
- **Method:** independent code trace by the audit lead + three fresh-context adversarial Opus reviewers (one per property cluster, each tasked to BREAK the properties), then a refutation pass verifying every surfaced finding against the code. Empirical: all 149 `nub-sandbox` tests pass (`cargo test -p nub-sandbox --lib` 87✓, `--test compiler` 52✓, `--test proxy` 10✓); reviewers B & C compiled + ran standalone parser/matcher probes.

---

## Verified-sound properties (all seven HOLD)

### P1 — Ephemeral CA lifecycle — HOLDS (high confidence)

The CA private key is in-memory only and never escapes; the OS trust store is never touched.

- `MitmCa.ca_key: KeyPair` (`ca.rs:34`) is used at exactly two in-process signing sites — `params.self_signed(&ca_key)` (`ca.rs:59`) and `.signed_by(&leaf_key, &self.ca_cert, &self.ca_key)` (`ca.rs:104`). Never serialized, written, logged, or placed in any env.
- `write_bundle` (`ca.rs:115-133`) emits only `ca_cert.pem()` (public CERT) + public root DERs — no key block. Test `ca_bundle_holds_public_certs_and_never_the_key` (`ca.rs:144`) asserts `!bundle.contains("PRIVATE KEY")`.
- **No OS trust-store manipulation exists anywhere in the crate.** A crate-wide grep for `add-trusted-cert|keychain|certutil|SecTrust|/etc/ssl|update-ca|import.*cert` finds only the module doc's *negative* assertion (`ca.rs:12`) and child-facing fs *deny*-globs protecting `Library/Keychains` (`defaults.rs:39-40`). No `Command`/process spawns any cert tool. Trust reaches the child solely via `NODE_EXTRA_CA_CERTS`-class env pointing at the public bundle (`backend/mod.rs:210-227`).
- No leak surface: `MitmCa`, `MitmEngine`, and `http1::Request` carry **no `#[derive(Debug)]`** (grep-confirmed), so key/secret cannot leak via Debug formatting; `mint_err`/`tls_err` format library error enums, not material.
- Ephemeral: `_bundle: NamedTempFile` (`ca.rs:42`, 0600 mkstemp) is removed on drop — test `bundle_file_is_removed_when_the_ca_drops` (`ca.rs:164`). The `MitmEngine` (holding the CA) is stashed on `Prepared` and dropped after the child exits.
- The ephemeral leaf key (`leaf_key.serialize_der()`, `ca.rs:108`) lives only in the proxy's rustls `ServerConfig`; the child sees only the leaf CERT during the handshake, never the leaf key.
- CA/MITM engine is built **only** under `Inspection::TlsInspect` (`backend/mod.rs:188-201`); a host-only policy instantiates no CA and no TLS code ("MITM never instantiated" — test `host_only_policy_stays_connection_tier_no_mitm`).

### P2 — Credential confinement — HOLDS for env + request-view (one inherent residual: F2)

The injected secret is never in the child env and never in the child's authored request; the child holds nothing.

- **Sole non-test reader of `Secret::expose()` is the egress injection at `mitm.rs:340`** (grep-confirmed; the only other hits are the doc comment `policy.rs:254` and tests).
- **Secret never reaches the child env.** Provenance: `parse_inject` → `resolve_credential_value` → `Secret::new(resolved)` → `HeaderInject.value` → `policy.brokers[]` → `MitmEngine.brokers` (parent) → injection. The env `constructed` map is folded on a disjoint path; nothing writes a broker secret into it. `HeaderInject.value` is `#[serde(skip)]` (`policy.rs:236`) so a `--sandbox` dump drops it; `Secret`'s manual Debug redacts (`policy.rs:260-268`); inner field is private (`expose()` the sole read path).
- Strip-then-set (`mitm.rs:335-342`) removes every case-insensitive child copy of the header before injecting, in-proxy, *after* the child's request is read — the child never sees the injected value in the request it authored.
- CA-bundle tampering does not help: the bundle read-grant is read-only (`file-read*` macOS / `AccessFs::ReadFile` linux); rewriting it only changes what the *child* trusts and would break its own handshake to the proxy. The secret-injecting leg is entirely in the parent process; the child cannot interpose on it.

### P3 — Upstream cert verification — HOLDS (high confidence)

The proxy's outbound leg verifies the real upstream cert against real roots with hostname check; a MITM-of-the-MITM fails.

- Upstream `ClientConfig` (`mitm.rs:71-79`): `builder_with_provider(...).with_safe_default_protocol_versions().with_root_certificates(roots).with_no_client_auth()` — installs rustls's default `WebPkiServerVerifier` (full chain validation + SAN/hostname verification against the `ServerName`, on by default).
- **No verification bypass anywhere:** grep for `dangerous|with_custom_certificate_verifier|accept_invalid|verify_hostname|NoHostnameVerification` finds zero hits on the TLS path.
- **Roots are the REAL platform roots, NOT the ephemeral CA:** `roots` is built from `ca.native_roots()` (`mitm.rs:65`) = `rustls_native_certs::load_native_certs().certs` (`ca.rs:63`). The ephemeral CA is added *only* to the child bundle, never to the upstream root store — so a self-signed / mis-host upstream cert cannot be accepted. `add == 0` roots fails closed (`mitm.rs:66-70`, `ca.rs:64-68`).
- `ServerName::try_from(host)` (`mitm.rs:158`) uses the same `host` value that keyed the leaf mint, the broker match, and the upstream TCP connect — one variable, no SNI/leaf/ServerName desync (`mod.rs:215-229`). A DNS rebind or on-path interception of the proxy→upstream leg fails the handshake and drops.

### P4 — Broker scoping (universal host-glob, wildcard = user's own risk) — CHANGED by maintainer decision

**PROPERTY CHANGED (maintainer decision):** brokers previously required a literal host, with wildcards rejected at compile as a laundering guard. That restriction is REMOVED — a broker host now accepts the SAME universal host-glob syntax as any net rule (`*.example.com`, bare `*`), matched by the SAME `host_glob_matches`. No special-case, no warning: laundering-to-a-misconfigured-wildcard is the user's own risk (identical to any over-broad wildcard net allow), out of the threat model.

- Compile gate `validate_broker_host` (`fold.rs`) now rejects ONLY `/` (CIDR — no HTTP layer); wildcards/globs pass through, validated by `host_pattern_is_valid` (`host.rs:80-98`) exactly as a net allow/deny is.
- At match, `broker_for` → `host_glob_matches(&broker.host, sni)` (`mitm.rs:107-112`) — one consistent semantics with net allow/deny. A literal broker still matches exactly (case-insensitive, trailing-dot-normalized); a `*.suffix` broker matches the apex + any-depth subdomain and nothing else (no sibling/suffix-confusable — the matcher's proven semantics: `example.com.evil.com`, `notexample.com` do NOT match `*.example.com`).
- Injection is keyed to the **SNI** (`mod.rs:217-220`); the leaf mint, upstream connect, and upstream cert-verify all use that same SNI host — so the credential can reach only a host whose real cert validates as a name the broker pattern admits. With a literal broker this is one host; with a wildcard broker it is every host the pattern matches (including, if the user scopes it too broadly, an attacker-owned subdomain with a valid cert — the accepted user risk). Tests: `wildcard_broker_is_accepted_and_scopes_via_the_universal_matcher`, `wildcard_broker_terminates_and_injects_only_for_matching_hosts`, `cidr_broker_is_rejected_no_http_layer` ✓.

### P5 — Fail-closed on every error/edge path — HOLDS

- CA-mint failure → `start_proxy_if_needed` returns `None` → whole net coarse-**denies** (`backend/mod.rs:191-197`); never a blind-splice downgrade that would forward brokered requests un-injected.
- In `terminate()`, leaf-mint / TLS-handshake / upstream-connect / upstream-verify failures each return `Err` → `let _ = mitm::terminate(...); return Ok(())` drops (`mod.rs:228-230`) — never a splice fallback.
- **The secret-bearing request is written only after the upstream cert verifies:** ordering in `terminate()` is read_request → apply_injects → normalize → `connect_upstream` → write-after-verified-handshake (`mitm.rs:150-165`). rustls buffers plaintext and errors on cert failure before emitting app-data (0-RTT not enabled), so a failed verify yields zero credential egress.
- Under `proxy:"terminate"`, a no-SNI / IP-literal connection is an explicit drop (`mod.rs:235`); the `_ => splice` arm is provably unreachable under `terminate_all` (every host either terminates or drops).
- Plaintext (NotTls) to a brokered/terminate-all host → `terminate` replays the non-TLS prelude into rustls → handshake fails → drop, before any upstream connect or injection (never inject over an unverified wire).
- SNI Malformed / Incomplete → deny (`mod.rs:281-308`); oversize head (>64 KiB) / body (>1 MiB) / chunked body / client EOF mid-request → `Err` → drop (`mitm.rs:243,301,310-316`).
- `proxy:"passthrough"` + an inject rule is a hard **compile error** (`mod.rs:311-328`), never a silent rule drop. Tests: `passthrough_with_an_inject_rule_is_a_compile_error`, `stalled_tls_tunnel_fails_closed`, `socks5_allowed_forwards_denied_sni_drops` ✓.

### P6 — Inject gating (trusted-only) — HOLDS; and no untrusted path is wired today

- `if !ctx.trusted { reject }` sits at `fold.rs:313-318` — **before** `parse_inject`, `validate_broker_host`, `push_net_rule`, and the sole `policy.brokers.push` (`fold.rs:329-334`). An untrusted (`dependenciesMeta`) grant with an `inject` object adds no allow rule and no broker (cannot even force TLS termination). `brokers.push` has exactly one call site (grep-confirmed); no other net path (bool / rule / CIDR / array) creates a broker.
- `$(…)` in an inject value is double-gated: `resolve_credential_value` re-checks `!ctx.trusted` (`fold.rs:422-424`), and the CRLF/NUL guard is applied to the **resolved** (post-substitution) value (`fold.rs:402-407`), so a `$(…)` whose stdout contains CRLF cannot smuggle a header. Tests: `inject_is_a_trusted_only_capability`, `substitution_forbidden_in_untrusted_home` ✓.
- **No untrusted-reachable broker exists in the shipped product.** The only production compile caller is `cli.rs:2651` (`nub run --sandbox <policy-file>`), which hardcodes `trusted: true` — the policy is an explicit user-named file, the user's own trust domain. Every other `trusted:false` construction is in `tests/**`. The `dependenciesMeta` untrusted-tier sandbox-compile path is **not wired** in this cut; the `trusted=false` gates are correct, unit-tested defense-in-depth ahead of the feature.

### P7 — `$(…)` / header-name / host-SNI / connection-reuse — HOLDS

- `validate_header_name` restricts names to `[A-Za-z0-9_-]` (`fold.rs:441-453`) — a `:`/CR/LF/space in a name is rejected, so the serialized `"{name}: {value}\r\n"` cannot be split via the name.
- Value CRLF/NUL is compile-gated (`fold.rs:402-407`); a colon in a value is harmless.
- `$(…)` shells out the user's OWN trusted policy at compile time (`resolve.rs`) — by design, double trusted-gated, unreachable from untrusted input.
- No connection reuse across hosts: cut-1 forces `Connection: close`, one request per terminated connection, a fresh upstream TCP+TLS per connection (`mitm.rs:346-360`), with `ServerName = host` (the matched SNI) — a child multi-SNI/dual-name trick cannot split "what nub checks" from "what the server sees."
- Runtime `has_bare_crlf` (`mitm.rs:407-422`) + strip-then-set block a smuggled second `Authorization`. Reviewer B compiled + ran a byte-level probe confirming rejection of: bare-LF/CR/mixed framing, value-embedded `\r\n` splits, obs-fold continuations, mixed-case / trailing-space / leading-tab header pre-seeds, and duplicate / signed / hex / overflow / whitespace Content-Length. CONNECT/SOCKS host tokens are control-char-guarded (`handshake.rs:189-197`).

---

## Real findings (triaged, post-refutation)

None above LOW. All are recommend-only; investigation-scope audit lands no code.

| # | Finding | Severity | Confidence | Exploitable? |
|---|---------|----------|-----------|--------------|
| F1 | Parent resource exhaustion / slowloris by a confined child | LOW | high | No escape/exfil; bounded |
| F2 | Reflection-endpoint credential leak (inherent to header-injection brokering) | INFO | high | Only via a broker to a header-reflecting upstream |
| F3 | `chunked` request-body refusal bypassable via a duplicate `Transfer-Encoding` header | INFO | high | No (re-framing neutralizes) |
| F4 | SNI host not control-char-validated (asymmetry vs the CONNECT authority) | INFO | high | No (resolution fails closed) |
| F5 | Broker scoping is host-only / port-agnostic | INFO | high | No (same cert-verified host) |
| F6 | `validate_broker_host` does no hostname-syntax validation | INFO | med | No (never matches, or matches a real host) |
| F7 | SIGKILL leaves a public-only CA bundle in the temp dir | INFO | high | No (public certs only) |

### F1 — Parent resource exhaustion / slowloris (LOW, recommend-only)

The terminated leg sets **no read timeout** (`client.set_read_timeout(None)`, `mitm.rs:145` — the 10s `CLIENT_HELLO_TIMEOUT` covers only the pre-handshake SNI scan, `mod.rs:199`), the accept loop has **no connection/thread cap** (`mod.rs:183-187`, best-effort spawn), up to ~1 MiB of request body is buffered whole in the parent per connection (`mitm.rs:42,313-320`), and a fresh leaf key is generated per connection (`ca.rs:99`).

- **Repro:** a confined child opens N loopback connections, completes each TLS handshake against the ephemeral CA, then dribbles the HTTP head/body one byte at a time indefinitely → N parent threads + up to N×~1 MiB held with no timeout, plus N key-gens → parent memory / thread / FD pressure.
- **Why LOW:** the child is already sandboxed with no escape or exfiltration; the parent reaps the whole proxy when the child exits; crashing the supervisor kills the attacker's own code (self-defeating). Deliberately acknowledged in code comments (`mitm.rs:41-42,143-144`; `mod.rs:181-182`).
- **Recommendation (availability posture — maintainer call):** add a concurrent-connection cap and a bounded read timeout on the terminated leg; streaming the forward (already noted as the follow-up that lifts the 1 MiB cap) also removes the per-connection buffer.

### F2 — Reflection-endpoint credential leak (INFO — inherent limitation + doc wording)

The proxy injects the real secret and relays the upstream **response** verbatim back to the child (`io::copy`, `mitm.rs:169`). If a user brokers a credential to an upstream that reflects request headers in its response body (an echo/debug endpoint, or a confused-deputy on a child-set `Host`), the child recovers the raw secret.

- **This is inherent to header-injection brokering, not a nub defect** — no proxy can stop a trusted upstream from echoing a credential without endpoint semantics; the same posture holds for corporate auth proxies, `op run`, and git-credential brokers. It is bounded by the trusted-only + per-host-opt-in design (the host may be a literal or a user-chosen wildcard scoped to hosts the user trusts).
- **Recommendation (doc-only):** soften the absolute "The child NEVER holds the secret" wording (`mitm.rs:16`, `policy.rs:151`) to name the response-path residual, and add a user-facing broker-configuration caveat (do not broker credentials to header-reflecting hosts). No behavior change.

### F3 — `chunked` refusal is first-`Transfer-Encoding`-header only (INFO — defense-in-depth)

`read_request` inspects only the first `transfer-encoding` header via `header_get` (`mitm.rs:298`, `header_get` returns the first match, `mitm.rs:393-398`). A request with `Transfer-Encoding: identity` followed by `Transfer-Encoding: chunked` bypasses the explicit chunked refusal. **Not smuggling-exploitable** — `normalize_for_forward` strips ALL TE + re-frames with an accurate Content-Length + one-request-per-connection, so no ambiguous framing reaches the upstream — but the "refuse chunked" intent is silently defeated. Recommendation: scan all TE headers (highest-value of the INFO items, since it's the one place the intended refusal can be bypassed).

### F4 — SNI host not control-char-validated (INFO — asymmetry)

`parse_server_name` returns any non-empty UTF-8 as the SNI (`sni.rs:208-214`), whereas the CONNECT/SOCKS authority is control-char-guarded (`host_from_str`, `handshake.rs:189-197`). A control-char SNI flows to the decider + `connect_upstream`. **No exploit found** — the decider matches the same string, and `getaddrinfo` rejects NUL/newline so it fails closed at resolution — but the asymmetry is worth closing (apply the same control-char guard to the SNI host).

### F5 — Broker scoping is host-only / port-agnostic (INFO)

`NetTarget`/`HostMatcher` match host only (no port); `terminate` connects to the broker host at the child-chosen CONNECT port (`mitm.rs:157`, `mod.rs:229`). A child can direct the credential to `broker-host:<any-port>`. **Not exploitable** — bounded to the cert-verified broker host (its own trust domain); reaching an alternate reflecting port on the same real host is the broker host attacking itself. Worth a one-line LIMITATIONS note if per-port brokering is ever wanted.

### F6 — No hostname-syntax validation on the broker host (INFO)

`host_pattern_is_valid` accepts any brace-free host pattern — a literal or a `*`/`*.suffix` wildcard — so an IP literal (`192.168.1.1`), a space/unicode-bearing token, etc. are accepted as broker hosts. **Not exploitable** — a bogus broker host either never matches a real SNI or matches only cert-verified real hosts the pattern admits; no third-party leak (an IP-literal broker under an IP CONNECT authority with no SNI never fires — `terminate_host` is `None` → splice, no injection).

### F7 — SIGKILL leaves a public-only bundle in temp (INFO — cosmetic)

`NamedTempFile` Drop removes the bundle, but a hard kill (SIGKILL) bypasses Drop, leaving `nub-mitm-ca-*.pem` in `env::temp_dir()`. Contents are public certs only — no key or secret leak. Cosmetic.

---

## Considered-and-cleared (non-issues)

Vectors checked that are actually safe (coverage evidence):

- **CA key serialized / written / in an error or Debug** → no; only in-process `self_signed`/`signed_by`, no `Serialize`/`Debug` derive on `MitmCa`.
- **`write_bundle` emits a private key** → no; only `ca_cert.pem()` + public root DERs (test-asserted).
- **Trust-store shellout** (`security`/`certutil`/keychain) → none; only fs deny-globs on keychain paths.
- **Secret into child env `constructed`** → no; broker secret and env map are folded on disjoint paths; a second `.expose()` reader → none outside the injection + tests.
- **Child overwrites CA bundle to recover the secret** → no benefit (parent-side injection leg is untouchable) and grant is read-only.
- **Upstream `dangerous()`/custom verifier/hostname-skip/ephemeral-CA in the upstream roots** → none; roots are the real platform store; ephemeral CA is bundle-only. IP-literal ServerName defeating the hostname check → ServerName is always a DNS name; IP-literal targets carry no name and are not terminated (`mod.rs:219`).
- **Suffix / case / trailing-dot / IDN-confusable / null / space / longer-suffix laundering** → impossible for any given broker pattern; `host_glob_matches` admits only the exact host (literal) or the apex + any-depth subdomain (`*.suffix`), never a sibling/suffix-confusable — empirically refuted by the matcher probe. (An EXPLICIT user wildcard broker intentionally widens scope to every host the pattern matches — that is the accepted user risk per the maintainer decision, not a laundering bypass of the matcher.)
- **Authority ≠ SNI mismatch** (broker as authority + different SNI, or vice-versa) → injection keyed to SNI; leaf/connect/verify all use the SNI; the splice path carries no secret. No leak either direction.
- **Brokered host reaching the blind splice un-injected** → unreachable (`should_terminate ↔ broker_for` use the same `host` + immutable `brokers`, no TOCTOU); the splice path has no secret in scope regardless.
- **Bare CR/LF / value-embedded `\r\n` / obs-fold / mixed-case / trailing-space / leading-tab header pre-seed / empty-name header** → framing guard + strip-then-set + re-serialize (probe-verified); a smuggled second `Authorization` is stripped, exactly one injected value on the wire.
- **Duplicate / oversized / signed / hex / overflow / whitespace / leading-zero Content-Length; pipelined 2nd request; hidden-header-in-body** → first-match + re-frame + one-req/conn + `Connection: close`; bad forms fail closed (probe-verified).
- **Secret over an unverified channel** → write is post-verified-handshake; rustls buffers plaintext; no 0-RTT enabled.
- **DNS-rebinding of the broker host** → resolve-once + `is_blocked_egress_ip` SSRF guard + real-root cert-verify for the pinned ServerName → rebind fails the handshake and drops.
- **ReplayIo byte mis-feed** → none (full prelude drain then socket read; empty prelude → immediate passthrough).
- **Response-relay memory blowup** → streamed via an 8 KiB buffer, not whole-response buffered; upstream is the gate-allowed, cert-verified host.
- **HTTP/2 upstream ignoring the pinned `http/1.1` ALPN** → desync fails closed (drop), not a secret leak.
- **Reviewer B's INFO-4 (validate broker values are CR/LF-free at policy load)** → REFUTED as a finding: already implemented at `fold.rs:402-407` (compile-time CRLF/NUL guard on the resolved inject value).
- **CL leading `+`** → non-RFC-strict but cosmetic; neutralized by the re-frame.

---

## Already-known residuals (Gate 3 — decided/documented, NOT new findings)

- **macOS `KERN_PROCARGS2` ascendant-env read** — a launcher-handoff item in `crates/nub-sandbox/LIMITATIONS.md` (the launcher must scrub nub's own environ pre-spawn); not a U5 defect.
- **ECH outer-name only** — no-MITM SNI cannot see the encrypted inner name; documented `sni.rs:23-27` and in the proposal, an accepted bounded residual.
- **Broad RFC1918 SSRF open-by-design** and **NAT64/6to4 link-local embeddings** — deliberate maintainer posture calls in `LIMITATIONS.md` "Network".
- **Per-connection leaf mint (no cache)** — a perf follow-up noted in `ca.rs:91-94`; the proposal's "cached, keyed by SNI" is a perf note, and fresh-mint is if anything more conservative. Not a security divergence.

---

## Design-intent cross-check (Gate 3)

The code matches the decided posture in `sandbox-mitm-tier-proposal.md` §5–7 exactly: ephemeral per-run CA, key in-process only, cert-only bundle of CA + real roots, trust via child env, **never the OS trust store**; fail-safe over-confine on CA-mint failure; per-host minimality ("MITM unbuildable" by default); auto + one-line stderr notice for the honesty bar (`emit_mitm_notice`, `backend/mod.rs:231-248`); ALPN pinned to `http/1.1` on both legs. The `inject` capability (proposal §8.3 "reserved / large follow-on") was pulled into cut-1 as the marquee, with trusted-only hardening added post-proposal (PR #414 P2 fixes). The initial literal-host-only restriction was later removed by maintainer decision — broker hosts now use the universal host-glob matcher (see P4).

---

## Overall verdict: GO

Ship the U5 tier. All seven target security properties hold, with empirical test + probe backing. The only finding above INFO is F1, a bounded, confined, self-defeating availability issue that is a recommend-only hardening (connection cap + terminated-leg read timeout), not a ship blocker. Suggested follow-up order if hardening is pursued: F1 (LOW) → F3 (the one bypassable intent) → F4 (control-char SNI guard) → F2 (doc wording + user caveat) → F5/F6/F7 (minor). None require pre-ship action.

## Changelog

- 2026-07-10 — Initial write-up. Independent adversarial re-audit of PR #414 (`b624730c6f`) U5 MITM tier: all 7 properties HOLD; verdict GO; 1 LOW (F1 resource-exhaustion) + 6 INFO findings, all recommend-only; reviewer-B INFO-4 refuted as already-implemented.
- 2026-07-10 — **P4 property CHANGED (maintainer decision):** the literal-host-only broker restriction (the compile-time wildcard-rejection laundering guard) was REMOVED. Broker hosts now accept the same universal host-glob syntax as any net rule (`*.example.com`, bare `*`), matched by the same `host_glob_matches`, no special-case and no warning; laundering-to-a-misconfigured-wildcard is the user's own risk, out of the threat model. All other U5 properties (CA lifecycle, credential confinement, upstream cert-verify, fail-closed, trusted-only gating, CRLF/NUL guards) are UNCHANGED. Updated P4/F2/F6/considered-cleared/design-intent to match.
