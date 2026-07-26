//! Network host matching: host-glob vs CIDR dispatch and last-match-wins
//! evaluation of a [`NetPolicy`]'s ordered rules.
//!
//! Host wildcard semantics: `*.example.com` matches any-depth subdomain
//! (`a.b.example.com`) but NOT the apex `example.com` — list the apex separately
//! to allow it too. This is subdomains-only, unlike TLS's single-label wildcard
//! but tighter than an apex-inclusive match (sandbox.mdx `net` grammar).

use crate::policy::{Effect, NetPolicy, NetRule, NetTarget};
use std::net::{IpAddr, Ipv4Addr};

/// A compiled last-match-wins matcher over a [`NetPolicy`]'s rules.
pub struct HostMatcher<'a> {
    rules: &'a [NetRule],
    default_effect: Effect,
    enforce: bool,
}

impl<'a> HostMatcher<'a> {
    pub fn new(policy: &'a NetPolicy) -> Self {
        Self {
            rules: &policy.rules,
            default_effect: policy.default_effect,
            enforce: policy.enforce,
        }
    }

    /// Whether egress to `host` (a hostname or an IP literal) is admitted. When
    /// the policy does not enforce, everything is admitted. Otherwise the LAST
    /// matching rule wins; nothing matching falls back to `default_effect`.
    pub fn admits(&self, host: &str) -> bool {
        if !self.enforce {
            return true;
        }
        let ip = host.parse::<IpAddr>().ok();
        let mut winner = self.default_effect;
        for rule in self.rules {
            let hit = match &rule.target {
                NetTarget::Host(pat) => host_glob_matches(pat, host),
                NetTarget::Cidr(net) => ip.is_some_and(|ip| net.contains(&ip)),
                NetTarget::Private => ip.is_some_and(is_private_range),
            };
            if hit {
                winner = rule.effect;
            }
        }
        matches!(winner, Effect::Allow)
    }
}

/// Match a host pattern against a concrete host. Two forms:
///   `*.example.com` → any-depth subdomain, NOT the apex (case-insensitive);
///   a literal `example.com` → exact (case-insensitive).
/// A bare `*` matches any host. A single FQDN trailing dot on either side
/// (`example.com.`) is stripped first — the same host per DNS (D12), so a
/// connect to `example.com.` cannot dodge a rule written as `example.com`.
pub fn host_glob_matches(pattern: &str, host: &str) -> bool {
    let pat = strip_trailing_dot(pattern).to_ascii_lowercase();
    let host = strip_trailing_dot(host).to_ascii_lowercase();
    if pat == "*" {
        return true;
    }
    if let Some(suffix) = pat.strip_prefix("*.") {
        // Subdomains only (must end with `.<suffix>`); the apex is NOT matched —
        // list `example.com` separately to allow it (sandbox.mdx `net`).
        return host.ends_with(&format!(".{suffix}"));
    }
    pat == host
}

/// Strip a single FQDN trailing dot (D12). Exactly one — `example.com..` keeps
/// the inner dot so a genuinely malformed pattern still fails to match.
pub fn strip_trailing_dot(host: &str) -> &str {
    host.strip_suffix('.').unwrap_or(host)
}

/// Whether a host pattern is well-formed per the supported grammar: a bare `*`,
/// a leading-subdomain wildcard `*.suffix` (the ONLY wildcard position), or a
/// literal host with no wildcard. A `*` anywhere else (`api.*.com`, `foo*bar`)
/// is ambiguous and rejected at compile time (D11) rather than silently matching
/// nothing — the matcher only ever honors the two forms above, so a mid-host
/// glob is a typo the author must see.
pub fn host_pattern_is_valid(pattern: &str) -> bool {
    // Brace alternation is not part of the host grammar — the matcher would treat
    // `{a,b}.com` as a literal host and match nothing. The fold rejects it with a
    // brace-specific message before reaching here; this keeps the predicate honest
    // for any direct caller.
    if pattern.contains(['{', '}']) {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    if let Some(rest) = pattern.strip_prefix("*.") {
        // The wildcard label must be followed by a real suffix: non-empty, not
        // itself dot-led (`*.`/`*..` are a degenerate empty apex that would strip
        // down to a bare `*` allow-all — a fail-OPEN), and no further wildcard.
        return !rest.is_empty() && !rest.starts_with('.') && !rest.contains('*');
    }
    !pattern.contains('*')
}

/// The RFC1918 IPv4 private ranges (`10/8`, `172.16/12`, `192.168/16`) plus IPv6 ULA
/// (`fc00::/7`) — the `<private>` class the egress proxy blocks by default. Loopback
/// (`127/8`, `::1`) and link-local are deliberately NOT here: loopback is the proxy's
/// own carve, and link-local is the separate always-blocked SSRF surface. An IPv4-mapped
/// / IPv4-compatible IPv6 form is classified on its embedded v4, so a v4 private address
/// cannot be smuggled past as a v6 literal.
pub fn is_private_range(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4() {
            Some(v4) => is_private_v4(v4),
            // fc00::/7 (ULA) hand-rolled: the top 7 bits are `1111 110`.
            None => (v6.segments()[0] & 0xfe00) == 0xfc00,
        },
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    // `Ipv4Addr::is_private` is exactly 10/8 + 172.16/12 + 192.168/16.
    v4.is_private()
}

/// Whether the policy EXPLICITLY opted into private-range egress via a `<private>`
/// target (last-match-wins over `<private>` allow/deny entries). A bare `*` allow-all
/// does NOT set this — the private class stays blocked unless the user names it, mirroring
/// Codex's `is_explicit_local_allowlisted` wildcard rejection. Independent of `enforce`:
/// a non-enforcing (`net: true`) policy carries no `<private>` rule, so private stays
/// blocked there too.
pub fn net_allows_private(policy: &NetPolicy) -> bool {
    let mut opted_in = false;
    for rule in &policy.rules {
        if matches!(rule.target, NetTarget::Private) {
            opted_in = matches!(rule.effect, Effect::Allow);
        }
    }
    opted_in
}
