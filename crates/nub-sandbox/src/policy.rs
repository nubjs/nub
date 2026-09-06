//! The resolved sandbox policy IR (`SandboxPolicy`).
//!
//! This is the compile target (Boundary A): fully RESOLVED plain data with NO
//! residual surface syntax — no presets, no `...:#/pointer` reuse, no glob-of-globs,
//! no sentinels. The compiler discharges all of that; a backend consumes
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
    /// This policy uses Nub's dependency-lifecycle build-jail profile.
    /// Set only by [`crate::compile_build_jail`]. Both profiles use the same
    /// unprivileged backends; this marker selects build-specific grants and behavior.
    ///
    /// Skipped in serde deliberately: it is a provenance marker for backend selection, not
    /// part of the policy IR, and adding it to the serialized form would churn every dump
    /// and snapshot without describing any confinement.
    #[serde(skip)]
    pub build_jail: bool,
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
/// wins (one ordered list, no "which list does an entry land in" ambiguity). The
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
    /// Whether an author named this path or something speculated it. Only the mount-plan
    /// layer consults it, so it defaults to [`FsOrigin::Authored`] and stays out of the
    /// serialized IR unless a speculated grant produced the rule.
    #[serde(default, skip_serializing_if = "FsOrigin::is_authored")]
    pub origin: FsOrigin,
}

/// Whether a path was named or guessed.
///
/// Some grants are speculated rather than authored: `$tooldirs` enumerates a dozen
/// ecosystems' cache dirs, and the build jail grants a provisioned toolchain's header
/// and module dirs sight-unseen. Most of those exist on no given machine. That makes
/// "the source is missing" mean opposite things by origin — an authored path that is
/// not there is an authoring mistake worth refusing, while an absent speculated one is
/// simply a machine without that toolchain and grants nothing either way. Backends that
/// must materialize a grant (the Bubblewrap bind plan) need the distinction; matchers,
/// which only ever narrow, do not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsOrigin {
    /// A path the policy author (or a preset acting for them) named directly.
    #[default]
    Authored,
    /// A path guessed at — a built-in set member (`$tooldirs`) or a toolchain subtree
    /// derived from the interpreter's location. Absent means "not on this machine".
    Speculative,
    /// Speculative, PLUS: the subtree is one NUB ITSELF owns and it holds only public
    /// bytes — the PM store, the tools dir, provisioned Node headers. Absent-tolerant
    /// exactly like [`Speculative`](FsOrigin::Speculative); the extra claim is about
    /// WHOSE data it is, which lets a backend satisfy the grant with a persistent,
    /// machine-wide read instead of a per-run one.
    ///
    /// ⛔ ONLY FOR nub's OWN PUBLIC CACHES. Never a project path, never a user home,
    /// never anything carrying user data or credentials: a backend is licensed to make
    /// this readable to sandboxed processes OTHER than the one being launched.
    ///
    /// WHY IT EXISTS. On Windows a LowBox token reaches a file only where that file's
    /// DACL names its AppContainer SID, and the SID is minted PER RUN — so a grant means
    /// writing an ACE and removing it again every launch. Windows inheritance is STATIC,
    /// so setting an inheritable ACE rewrites every existing child's DACL right then:
    /// measured in-product, the PM store grant plus its revoke is 10,553 ms of a
    /// 13,845 ms fixed per-launch cost across 25,526 entries, i.e. 76% of it, and it
    /// scales linearly (2,000 entries ⇒ 886 ms). Marking the subtree lets the backend
    /// publish it ONCE to `ALL APPLICATION PACKAGES` instead, after which
    /// `already_granted_to_appcontainers` skips it on every later launch — the same
    /// reason `%ProgramFiles%\nodejs` costs nothing today.
    ///
    /// The exposure that buys it, stated: other sandboxed apps on the machine can read
    /// nub's store. It does NOT widen what the jailed script reaches (it already reads
    /// the store to read its own dependencies), and the store holds public npm package
    /// content a script could fetch anyway. Private-REGISTRY package content is the one
    /// non-public thing there; if that is ever deemed sensitive, the alternative is the
    /// per-closure narrowing behind `NUB_SANDBOX_NARROW_STORE_READS`.
    ///
    /// POSIX ignores the distinction entirely: Seatbelt compiles an SBPL ruleset and
    /// Landlock installs one, both evaluated at access time, so a path costs nothing to
    /// add and there is no per-run ACE to avoid.
    NubOwnedPublic,
}

impl FsOrigin {
    pub fn is_authored(&self) -> bool {
        matches!(self, FsOrigin::Authored)
    }

    /// Whether an ABSENT source is ordinary rather than an authoring mistake. Both
    /// speculated origins tolerate absence; only [`Authored`](FsOrigin::Authored) does not.
    pub fn tolerates_absent(&self) -> bool {
        matches!(self, FsOrigin::Speculative | FsOrigin::NubOwnedPublic)
    }
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
/// wins list the egress proxy evaluates by SNI/IP; the base is deny-all. A
/// fine-grained allow must request that proxy explicitly, except a credential
/// broker which necessarily starts a terminating proxy itself.
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
    /// The resolved proxy posture, DERIVED by the compiler (never authored — there is
    /// no `proxy` config key). A pure function of the net axis: coarse `net: true` /
    /// `net: false` and an all-deny net stay [`ProxyMode::Disabled`] (no proxy); a
    /// fine-grained allow derives [`ProxyMode::Auto`] (the egress proxy runs).
    /// Termination of the brokered hosts is carried by [`Inspection`], not this field.
    #[serde(default)]
    pub mode: ProxyMode,
    /// The tier the compiler DERIVED (default [`Inspection::Connection`]). A pure
    /// function of `brokers` + `mode`; materialized in the IR so a `--sandbox` dump
    /// states the posture explicitly (proposal §4). Never a user input.
    #[serde(default)]
    pub inspection: Inspection,
    /// Per-host credential brokers. Non-empty ⇒ the tier is
    /// [`Inspection::TlsInspect`]. The IR carries only environment-variable names;
    /// each apply resolves their parent values into redacted proxy state and gives
    /// the child fresh opaque markers instead.
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
            mode: ProxyMode::Disabled,
            inspection: Inspection::Connection,
            brokers: Vec::new(),
        }
    }
}

/// The proxy posture, DERIVED by the compiler from the net axis (never authored —
/// there is no `proxy` config key). [`Self::Disabled`] and [`Self::Auto`] are the only
/// two the compiler emits; [`Self::Passthrough`] / [`Self::Terminate`] are dormant,
/// retained (unreachable) so the IR and the backend `terminate_all` read stay stable —
/// a deferred Phase-2 cleanup, not live grammar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// No proxy. Derived for coarse `net: true` / `net: false` and an all-deny net.
    #[default]
    Disabled,
    /// The egress proxy runs. Derived for a fine-grained net allow; termination of the
    /// brokered hosts is governed by [`Inspection::TlsInspect`], not this variant.
    Auto,
    /// Dormant (never derived). Historically the explicit "block MITM" posture.
    Passthrough,
    /// Dormant (never derived). Historically forced termination of all allowed TLS.
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

/// A per-host credential broker. At apply time each named parent variable is replaced
/// in the child's environment by a fresh opaque marker. On HTTPS egress to this exact
/// host, the terminating proxy replaces marker occurrences in HTTP/1.1 request header
/// values with the real secret. No secret is serialized in this IR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CredentialBroker {
    /// One exact literal hostname. Wildcards, IP literals, and CIDRs are rejected.
    pub host: String,
    /// Exact parent environment-variable names whose values may cross this host.
    pub env: Vec<String>,
}

/// Whether a name is Windows shell-positional state rather than an environment
/// variable. `cmd.exe` writes hidden per-drive current-directory entries into the
/// environment block — `=C:`, `=D:`, and `=ExitCode` — whose NAME begins with `=`,
/// and Rust's `env::vars()` surfaces them verbatim, so ANY nub spawn under a
/// cmd.exe ancestor snapshots them. That matters here because nub runs npm
/// lifecycle scripts through cmd.exe on Windows.
///
/// They are shell state, not configuration: cmd regenerates its own set at startup
/// and a child spawned from any other parent never had them, so dropping them costs
/// a confined child nothing. Dropping is also the only option — a name containing
/// `=` cannot round-trip a `KEY=VALUE` block, which is why the backend rejects one
/// outright. That reject is the injection guard and stays; these are filtered where
/// the ambient env ENTERS the policy so a legitimate cmd.exe ancestor cannot fail
/// the launch. Only a LEADING `=` is legitimate — an interior `=` cannot come from
/// a real environment block (the OS splits at the first `=` past position 0), so it
/// still reaches the reject.
pub fn is_shell_positional_env_key(key: &str) -> bool {
    key.starts_with('=')
}

/// Environment names the backend owns after the policy env has been constructed.
///
/// A broker marker written under one of these keys would be overwritten before
/// spawn by proxy, CA, or private-tmp plumbing while the real secret remained
/// live in proxy state. Rejecting the collision keeps the marker→secret pair
/// structurally usable rather than creating a broker grant the child cannot
/// present. Windows environment keys are case-insensitive; POSIX keys are not.
pub(crate) fn credential_env_name_is_reserved(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "ALL_PROXY",
        "NODE_USE_ENV_PROXY",
        "NODE_EXTRA_CA_CERTS",
        "SSL_CERT_FILE",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
        "GIT_SSL_CAINFO",
        "PIP_CERT",
        "NPM_CONFIG_CAFILE",
        "npm_config_cafile",
        "CARGO_HTTP_CAINFO",
        "AWS_CA_BUNDLE",
        "DENO_CERT",
        "TMPDIR",
        "TMP",
        "TEMP",
    ];

    if cfg!(windows) {
        RESERVED
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(name))
    } else {
        RESERVED.contains(&name)
    }
}

/// Legacy IPv4 textual forms accepted by libc resolvers but rejected by Rust's
/// canonical `IpAddr` parser (for example `127.1`, `2130706433`, or
/// `0x7f000001`). A broker host must be a DNS name, not a spelling that resolves
/// numerically without DNS.
pub(crate) fn broker_host_is_legacy_ipv4_literal(host: &str) -> bool {
    let components = host.split('.').collect::<Vec<_>>();
    !components.is_empty()
        && components.len() <= 4
        && components.iter().all(|component| {
            !component.is_empty()
                && (component.bytes().all(|byte| byte.is_ascii_digit())
                    || component
                        .strip_prefix("0x")
                        .or_else(|| component.strip_prefix("0X"))
                        .is_some_and(|hex| {
                            !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                        }))
        })
}

/// A net rule targets a host pattern (glob or literal), a CIDR block, or the
/// symbolic private-range class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetTarget {
    /// A hostname pattern. `*.example.com` matches any-depth subdomains but NOT the
    /// apex `example.com` (subdomains-only — list the apex separately to allow it;
    /// sandbox.mdx `net` grammar).
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
    /// Whether a host/compiler has resolved the target environment snapshot.
    /// Backends reject `false` instead of guessing between ambient inheritance and
    /// a deliberately empty environment.
    #[serde(default)]
    pub resolved: bool,
    /// Whether the environment axis is confining. `constructed` is the resolved
    /// target environment in both modes: an unconfined policy snapshots the
    /// compiler's ambient input instead of consulting process-global state later.
    pub enforce: bool,
    pub constructed: BTreeMap<String, String>,
    pub schema: Vec<EnvRule>,
    /// The names the policy deliberately WITHHELD from the child (present in the
    /// ambient env, denied by policy). Surfaced verbatim in a failure hint — nub
    /// knows exactly what it removed. Deterministic (sorted) for stable output.
    pub withheld: Vec<String>,
    /// The CONCRETE constructed keys classified sensitive — materialized here so an
    /// output redactor scrubs their values without re-globbing `schema` (which is
    /// keyed by PATTERN, not concrete key). Fail-safe: a key is sensitive iff ANY
    /// matching rule is sensitive (order-robust; `vars:["*"]`+`secrets:["FOO"]` still
    /// marks FOO). Names only — never values — so it leaks nothing `schema`/
    /// `constructed` do not already carry. Deterministic (sorted).
    #[serde(default)]
    pub sensitive_keys: Vec<String>,
}

impl EnvPolicy {
    /// Resolve a non-confining policy to an explicit target-environment snapshot.
    /// Direct IR callers use this instead of relying on apply-time ambient reads —
    /// typically over a raw `env::vars()`, so this is one of the two points where an
    /// ambient snapshot enters the policy and is filtered accordingly (see
    /// [`is_shell_positional_env_key`]; the other is `CompileCtx::new`).
    pub fn resolved(mut constructed: BTreeMap<String, String>) -> Self {
        constructed.retain(|key, _| !is_shell_positional_env_key(key));
        Self {
            resolved: true,
            constructed,
            ..Self::default()
        }
    }
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

/// A fully-resolved fs glob: symbolic roots (`~`/`$tmp`/`$cache`/`./`)
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
