//! The resolved sandbox policy IR (`SandboxPolicy`).
//!
//! This is the compile target (Boundary A): fully RESOLVED plain data with NO
//! residual surface syntax — no presets, no `"..."` spread, no glob-of-globs, no
//! inheritance tokens. The compiler discharges all of that; a backend consumes
//! ONLY the IR and is a pure `IR → OS-primitive` translator.
//!
//! Every type is `serde`-round-trippable. That is a hard requirement: the
//! conformance fixtures assert against a serialized IR, and `--sandbox` can dump
//! it for debugging. Field/entry order is deterministic (`Vec` preserves order,
//! `constructed` is a `BTreeMap`) so snapshots are stable across the matrix.
//!
//! Evaluation model, uniform across the fs/net axes: an ordered entry list plus a
//! `default_effect` base. `decide()` walks the entries and the LAST match wins;
//! nothing matching falls back to `default_effect`. There is no magic floor and
//! no deny-priority (per .fray/sandbox.md "Pure last-match-wins") — the built-in
//! secret denies the compiler injects are ordinary entries subject to the same
//! rule, so a later user allow can override one by ordering.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One resolved policy for one spawned process. Every axis composes
/// independently. Produced by [`crate::compile`], consumed by [`crate::apply`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub fs: FsPolicy,
    pub net: NetPolicy,
    pub env: EnvPolicy,
    pub pid: PidPolicy,
}

/// Allow or Deny — the verdict of a single rule and the base of a ruleset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
}

// ── filesystem ───────────────────────────────────────────────────────────────

/// Filesystem confinement: ONE ordered last-match-wins ruleset (each Allow
/// carrying its access) plus the tmp posture.
///
/// Provenance: design.md §2.1 sketches parallel `read`/`write` rulesets, but a
/// single ruleset with per-Allow access is strictly more faithful to last-match-
/// wins and removes the "which list does `"..."` splice into" ambiguity. The
/// read-generous/write-tight posture falls out naturally: secure defaults are
/// `[Allow ** access=read, Deny <secrets>]` (everything readable but the secret
/// set, nothing writable), and a `"./data": "rw"` grant appends
/// `Allow ./data access=readwrite` — one list, no floor. Backends derive the
/// read-set (Allow with any access) and write-set (Allow with ReadWrite) from it;
/// a Deny removes both read and write at that path. "No write-without-read" is
/// structural — [`FsAccess`] has no write-only variant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FsPolicy {
    pub rules: FsRuleSet,
    pub tmp: TmpMode,
}

/// Throwaway-tmp handling for the sandboxed child.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TmpMode {
    /// The host tmp is visible (default until a backend tightens it).
    #[default]
    Shared,
    /// A private per-run tmp is mounted; the host tmp is hidden.
    Private,
    /// No tmp access at all.
    Deny,
}

/// An ordered fs ruleset evaluated last-match-wins over a `default_effect` base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsRuleSet {
    pub entries: Vec<FsRule>,
    pub default_effect: Effect,
}

impl Default for FsRuleSet {
    fn default() -> Self {
        // Fail-closed base: an empty ruleset denies everything.
        Self {
            entries: Vec::new(),
            default_effect: Effect::Deny,
        }
    }
}

/// One fs rule: a canonicalized glob, its effect, and (for an Allow) the access
/// it grants. A Deny carries no access. Write-without-read is deliberately
/// unrepresentable — the surface has no `"w"` ladder value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsRule {
    pub matcher: CanonGlob,
    pub effect: Effect,
    pub access: FsAccess,
}

/// The access an fs Allow grants. On the `write` ruleset a `ReadWrite` allow is
/// the write grant; `Read` on `write` is inert (no write). Modeled per-axis so
/// one surface entry (`"./data": "rw"`) can seed both rulesets consistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsAccess {
    Read,
    ReadWrite,
}

impl FsAccess {
    /// The single access every Deny rule carries. A deny removes both read AND
    /// write regardless of this field (every backend/matcher reads `.access` only
    /// under an `Effect::Allow` arm; deny arms are `(Effect::Deny, _)`), so the
    /// mode is inert on a deny — normalized to one value so the IR has a uniform
    /// deny representation and two denies differing only in an inert access don't
    /// yield divergent IR/snapshots (D20).
    pub const DENY: FsAccess = FsAccess::Read;
}

// ── network ──────────────────────────────────────────────────────────────────

/// Network confinement. `enforce = false` means "no net restriction" (the
/// wrapper/axis `true` case). When enforcing, `rules` is an ordered last-match-
/// wins list the egress proxy (S6) evaluates by SNI/IP; the base is deny-all.
///
/// Provenance: design.md §2.1 sketches `allow_hosts`/`allow_cidrs` allow-lists.
/// The IR keeps a single ordered `rules` list instead so `!`-deny + last-match-
/// wins compose on the net axis exactly as they do on fs — an allow-list can't
/// express `["*", "!*.evil.com"]` faithfully. `admits()` gives the proxy the
/// resolved allow set when it needs a flat view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetPolicy {
    pub enforce: bool,
    pub rules: Vec<NetRule>,
    pub default_effect: Effect,
    /// The `proxy` wrapper knob (default [`ProxyMode::Auto`]). Governs whether the
    /// egress proxy may TERMINATE TLS to enforce a capability that can't be checked
    /// from outside the stream. Authored at the wrapper level (sibling of net); the
    /// compiler folds it onto the net axis it governs.
    #[serde(default)]
    pub mode: ProxyMode,
    /// The tier the compiler DERIVED (default [`Inspection::Connection`]). A pure
    /// function of `brokers` + `mode`; materialized in the IR so a `--sandbox` dump
    /// states the posture explicitly (proposal §4). Never a user input.
    #[serde(default)]
    pub inspection: Inspection,
    /// Per-host credential brokers (proposal §5 — cut-1 marquee). Non-empty ⇒ the tier
    /// is [`Inspection::TlsInspect`] and the proxy terminates each brokered host to
    /// inject the credential. The resolved secret lives here IN-MEMORY ONLY (see
    /// [`HeaderInject::value`]) — never the child env, never a serialized dump.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub brokers: Vec<CredentialBroker>,
}

impl Default for NetPolicy {
    fn default() -> Self {
        // Off by default: no rules, not enforcing. The compiler flips `enforce`
        // on for any explicit net policy.
        Self {
            enforce: false,
            rules: Vec::new(),
            default_effect: Effect::Deny,
            mode: ProxyMode::Auto,
            inspection: Inspection::Connection,
            brokers: Vec::new(),
        }
    }
}

/// The `proxy` wrapper knob — whether the egress proxy may terminate TLS (the MITM
/// tier). The default reading of the maintainer's ask: derive the tier from the rules,
/// never a user-set boolean.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Derive the tier from the rules: terminate only hosts whose own rules require it
    /// (a credential-inject rule); tunnel everything else blind. The default.
    #[default]
    Auto,
    /// Forbid termination. A rule that REQUIRES it is a COMPILE ERROR (never a silent
    /// drop) — the explicit "block MITM" posture; net stays connection-level only.
    Passthrough,
    /// Force termination of all allowed TLS even under host-only rules (the
    /// domain-fronting-closure hardening posture).
    Terminate,
}

/// The enforcement tier the compiler derived for the net axis. Recorded in the IR for
/// dump/fixture visibility; recomputed by the compiler, never authored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Inspection {
    /// Today's SNI-peek proxy: no TLS code on the path, no CA in existence.
    #[default]
    Connection,
    /// The MITM tier: a per-run ephemeral CA terminates the hosts that need it,
    /// everything else stays a blind splice.
    #[serde(rename = "tls-inspect")]
    TlsInspect,
}

/// One net rule: a host pattern or a CIDR, plus its effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetRule {
    pub target: NetTarget,
    pub effect: Effect,
}

/// A per-host credential broker (proposal §5, cut-1 marquee): on egress to `host` the
/// terminating proxy STRIPS then re-injects each header, so the sandboxed child
/// authenticates to an allowlisted upstream WITHOUT ever holding the secret. A broker
/// forces [`Inspection::TlsInspect`] for its host. HTTPS-only by construction — the
/// proxy refuses to inject over an unterminated/plaintext channel (never expose a
/// secret on an unverified wire).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CredentialBroker {
    /// The host pattern the broker governs — same glob grammar as a net [`NetTarget::Host`].
    pub host: String,
    pub injects: Vec<HeaderInject>,
}

/// One header the broker sets on egress. Strip-then-set: the child's own value for
/// `header` (if any) is removed FIRST, so a child-supplied — possibly leaked-real —
/// credential can never survive alongside the injected one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeaderInject {
    /// The HTTP header name to set (e.g. `Authorization`).
    pub header: String,
    /// The resolved header value. `#[serde(skip)]` keeps the secret out of EVERY
    /// serialized IR (a `--sandbox` dump, a conformance fixture) and the redacting
    /// [`Secret`] `Debug` keeps it out of logs/panics. The IR is compiler-built and
    /// consumed directly by `apply()` — never deserialized-then-applied — so skipping
    /// it loses nothing at runtime.
    #[serde(skip)]
    pub value: Secret,
}

/// A resolved secret held in nub's PARENT process only. Serialization drops it (the
/// containing field is `#[serde(skip)]`) and `Debug` redacts it, so a policy dump, a
/// trace line, or a panic can never spill the credential. The inner field is PRIVATE:
/// [`Secret::expose`] is the sole read path (grep it to audit every reader), so no
/// `.0` access can slip past the redaction.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a resolved secret value.
    pub fn new(value: String) -> Self {
        Secret(value)
    }
    /// Borrow the raw value. The ONE call site that needs it is the proxy's egress
    /// header injection; grep for `.expose()` to audit every reader.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str("Secret(\"\")")
        } else {
            f.write_str("Secret(\"<redacted>\")")
        }
    }
}

/// A net rule targets a host pattern (glob or literal), a CIDR block, or the
/// symbolic private-range class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetTarget {
    /// A hostname pattern. `*.example.com` matches the apex AND any-depth
    /// subdomains (a deliberate divergence from TLS's one-label wildcard, chosen
    /// for fewer footguns — see .fray/sandbox.md matcher spec).
    Host(String),
    /// A CIDR block for IP-literal egress.
    Cidr(ipnet::IpNet),
    /// The symbolic `<private>` (`<local>`) target: the RFC1918 IPv4 ranges
    /// (`10/8`, `172.16/12`, `192.168/16`) plus IPv6 ULA (`fc00::/7`), which are
    /// blocked by default at the egress proxy. Only this EXPLICIT target re-permits
    /// them — a bare `*` does not (mirrors Codex's non-wildcard local-allowlist).
    /// The always-blocked cloud-metadata / link-local surface is NOT re-opened by it.
    Private,
}

// ── environment ──────────────────────────────────────────────────────────────

/// Environment confinement. `constructed` is the ACTUAL child env nub builds —
/// env access is undetectable (a plain memory read of the populated environ), so
/// enforcement is construction, not interception: a withheld var is simply absent.
/// `schema` carries per-key validation + the `sensitive` mark for downstream
/// consumers (log redaction); the `$(…)` resolver's output is already baked into
/// `constructed` by the compiler.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnvPolicy {
    /// When `false` the child INHERITS the ambient env untouched (no confinement —
    /// the unconfined / absent-axis case). When `true` the child env is EXACTLY
    /// `constructed` — the scrub is construction, not subtraction.
    pub enforce: bool,
    pub constructed: BTreeMap<String, String>,
    pub schema: Vec<EnvRule>,
    /// The names the policy deliberately WITHHELD from the child (present in the
    /// ambient env, denied by policy). Surfaced verbatim in a failure hint — nub
    /// knows exactly what it removed. Deterministic (sorted) for stable output.
    pub withheld: Vec<String>,
}

/// A single env-key rule carried for validation + redaction. Enforcement of the
/// value itself is via `constructed`; this is the metadata twin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvRule {
    /// The key or glob key (`VITE_*`) the rule governs.
    pub key: String,
    /// Whether the value is sensitive (default-on; `sensitive: false` opts out of
    /// redaction). The single mark replacing the old `secret`/`public` pair (D17).
    pub sensitive: bool,
    /// Optional value type the compiler validated the value against.
    pub format: Option<EnvFormat>,
    /// `true` if the key is optional (object-form trailing `?` / `optional`).
    pub optional: bool,
}

/// The closed env value-type grammar (`integer | number | port`). String formats
/// (email/url/…) deliberately do NOT ship; `/regex/` covers them until real
/// demand (.fray/sandbox-config-spec.md — FORMAT trimmed 2026-07-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvFormat {
    Integer,
    Number,
    Port,
}

// ── pid ──────────────────────────────────────────────────────────────────────

/// PID/isolation posture. `isolate` requests env-read isolation on Linux (§2.4);
/// Bubblewrap supplies a private PID namespace and fresh procfs whenever a Linux
/// policy needs enforcement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidPolicy {
    pub isolate: bool,
}

// ── canonical glob ───────────────────────────────────────────────────────────

/// A fully-resolved fs glob: symbolic roots (`~`/`<tmp>`/`<home>`/`<cache>`/`./`)
/// already expanded and slashes normalized to `/`. Case-insensitivity is applied
/// at MATCH time (via globset's flag on Windows/macOS), NOT baked here, so the
/// serialized IR is byte-identical across OSes and snapshots stay stable.
/// Serializes as its string; the matcher compiles it to a `globset` matcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonGlob(pub String);

impl CanonGlob {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
