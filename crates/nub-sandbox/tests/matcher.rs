//! Matcher tests: symbolic-root expansion, canonicalize-including-nonexistent,
//! host-glob apex/subdomain semantics, and CIDR dispatch.

mod common;

use nub_sandbox::matcher::host::{host_glob_matches, host_pattern_is_valid};
use nub_sandbox::matcher::path::{canonicalize_including_nonexistent, expand_symbolic};
use nub_sandbox::matcher::{HostMatcher, PathMatcher};
use nub_sandbox::policy::{Effect, FsAccess, NetPolicy, NetRule, NetTarget};
use std::path::PathBuf;

// ── symbolic expansion ────────────────────────────────────────────────────────

#[test]
fn expands_home_and_symbolic_roots() {
    let h = common::homes();
    assert_eq!(
        expand_symbolic("~/.ssh", &h),
        format!("{}/.ssh", h.home.display())
    );
    assert_eq!(expand_symbolic("~", &h), h.home.to_string_lossy());
    assert_eq!(
        expand_symbolic("<tmp>/x", &h),
        format!("{}/x", h.tmp.display())
    );
    assert_eq!(
        expand_symbolic("<cache>/y", &h),
        format!("{}/y", h.cache.display())
    );
}

#[test]
fn expands_bare_relative_under_project() {
    let h = common::homes();
    assert_eq!(
        expand_symbolic("./data", &h),
        format!("{}/data", h.project.display())
    );
    assert_eq!(
        expand_symbolic("data/**", &h),
        format!("{}/data/**", h.project.display())
    );
}

#[test]
fn absolute_paths_pass_through_slash_normalized() {
    let h = common::homes();
    // Backslashes normalize to forward slashes even in an absolute literal.
    assert_eq!(expand_symbolic("/etc/hosts", &h), "/etc/hosts");
    assert_eq!(expand_symbolic("/a\\b", &h), "/a/b");
}

// ── canonicalize including non-existent ───────────────────────────────────────

#[test]
fn canonicalizes_nonexistent_tail_without_erroring() {
    // The disavowed-backend trap: canonicalize must NOT Err on a path whose tail
    // does not exist — it resolves the existing prefix and appends the rest.
    let tmp = std::env::temp_dir();
    let target = tmp.join("nub-sbx-does-not-exist-xyz").join("child");
    let canon = canonicalize_including_nonexistent(&target);
    // The existing prefix (temp_dir) is resolved (e.g. /tmp→/private/tmp on mac);
    // the non-existent tail is preserved.
    assert!(canon.ends_with("nub-sbx-does-not-exist-xyz/child"));
    assert!(canon.is_absolute());
}

#[test]
fn canonicalize_collapses_parent_dir_in_nonexistent_tail() {
    let tmp = std::env::temp_dir();
    let target = tmp.join("nub-sbx-a").join("..").join("nub-sbx-b");
    let canon = canonicalize_including_nonexistent(&target);
    assert!(canon.ends_with("nub-sbx-b"));
    assert!(!canon.to_string_lossy().contains(".."));
}

// ── fs last-match-wins ────────────────────────────────────────────────────────

#[test]
fn path_matcher_last_match_wins_over_default() {
    use nub_sandbox::compiler::compile;
    use serde_json::json;
    // `["...", "!~/.ssh", "~/.ssh/config"]`: generous read, deny the ssh subtree,
    // then re-allow one file — the LAST match wins, so config is readable.
    let ctx = common::ctx(true, &[]);
    let policy = compile(&json!({ "fs": ["...", "!~/.ssh", "~/.ssh/config"] }), &ctx).unwrap();
    let m = PathMatcher::new(&policy.fs.rules);

    let home = common::homes().home;
    let readable = |p: PathBuf| matches!(m.decide(&p).effect, Effect::Allow);
    assert!(
        readable(home.join("notes.txt")),
        "generous read allows a normal file"
    );
    assert!(!readable(home.join(".ssh/id_rsa")), "ssh subtree denied");
    assert!(
        readable(home.join(".ssh/config")),
        "later specific allow wins"
    );
}

#[test]
fn descendant_glob_excludes_the_directory_node() {
    use nub_sandbox::compiler::compile;
    use serde_json::json;

    let ctx = common::ctx(true, &[]);
    let policy = compile(&json!({ "fs": ["~", "!~/.ssh/**"] }), &ctx).unwrap();
    let matcher = PathMatcher::new(&policy.fs.rules);
    let ssh = common::homes().home.join(".ssh");

    assert!(matches!(matcher.decide(&ssh).effect, Effect::Allow));
    assert!(matches!(
        matcher.decide(&ssh.join("id_rsa")).effect,
        Effect::Deny
    ));
}

#[test]
fn fs_rw_grant_is_writable_read_only_grant_is_not() {
    use nub_sandbox::compiler::compile;
    use serde_json::json;
    let ctx = common::ctx(true, &[]);
    let policy = compile(&json!({ "fs": { "./rw": "rw", "./ro": "r" } }), &ctx).unwrap();
    let m = PathMatcher::new(&policy.fs.rules);
    let proj = common::homes().project;

    let d_rw = m.decide(&proj.join("rw/file"));
    assert!(matches!(d_rw.effect, Effect::Allow) && matches!(d_rw.access, FsAccess::ReadWrite));
    let d_ro = m.decide(&proj.join("ro/file"));
    assert!(matches!(d_ro.effect, Effect::Allow) && matches!(d_ro.access, FsAccess::Read));
}

#[test]
fn deny_is_not_dodged_by_parent_dir_traversal() {
    use nub_sandbox::compiler::compile;
    use serde_json::json;
    // A `..` bounce back into a denied subtree must still hit the deny — the
    // candidate is canonicalized (incl. non-existent tail) before matching.
    let ctx = common::ctx(true, &[]);
    let policy = compile(&json!({ "fs": ["...", "!~/.ssh"] }), &ctx).unwrap();
    let m = PathMatcher::new(&policy.fs.rules);
    let dodge = common::homes().home.join(".ssh/../.ssh/id_rsa");
    assert!(
        matches!(m.decide(&dodge).effect, Effect::Deny),
        "`..` traversal must not dodge the ssh deny"
    );
}

// ── host glob + CIDR ──────────────────────────────────────────────────────────

#[test]
fn host_wildcard_matches_apex_and_any_depth() {
    assert!(host_glob_matches("*.example.com", "example.com"), "apex");
    assert!(
        host_glob_matches("*.example.com", "api.example.com"),
        "one label"
    );
    assert!(
        host_glob_matches("*.example.com", "a.b.example.com"),
        "any depth"
    );
    assert!(!host_glob_matches("*.example.com", "example.org"));
    assert!(!host_glob_matches("*.example.com", "notexample.com"));
    assert!(host_glob_matches("*", "anything.at.all"));
}

#[test]
fn net_wildcard_matches_any_depth_but_not_sibling_domains() {
    // Security-critical wildcard contract (ratified): `*.example.com` admits the apex
    // and EVERY subdomain at any depth, and admits NOTHING that merely shares the
    // string "example.com" without a label boundary. The negative set is the real
    // vulnerability surface — a sibling/suffix-confusion host slipping past an allow
    // rule is an egress breakout. Enumerated exhaustively so a regression fails loudly.
    let pat = "*.example.com";

    // Property 1 — matches at every depth (apex counts as depth 0).
    for host in [
        "example.com",              // apex
        "a.example.com",            // one label
        "a.b.example.com",          // two labels
        "evil.deep.example.com",    // three labels
        "w.x.y.z.deep.example.com", // arbitrarily deep
        "API.Example.COM",          // case-insensitive apex
        "Api.Example.Com",          // case-insensitive subdomain
        "api.example.com.",         // FQDN trailing dot (D12)
    ] {
        assert!(host_glob_matches(pat, host), "must match: {host}");
    }

    // Property 2 — must NOT over-match. Every shape shares the substring "example.com"
    // (or a near-miss of it) but is a DIFFERENT registrable domain an attacker controls.
    for host in [
        "notexample.com", // prefix glued, no label boundary
        "evilexample.com",
        "fooexample.com",
        "myexample.com",
        "xexample.com",             // single-char prefix glue
        "example.comx",             // TLD suffix-extended
        "example.com.attacker.com", // apex as a leading label of an attacker zone
        "example.com.evil.com",
        "example.com-attacker.com", // hyphen-glued sibling
        "example.co",               // truncated TLD
        "example.org",              // different TLD
        "example.net",
        "wexample.com",
        "aexample.com",
    ] {
        assert!(!host_glob_matches(pat, host), "must NOT match: {host}");
    }
}

#[test]
fn net_wildcard_holds_on_enforced_proxy_decider() {
    // The per-host allow/deny verdict on both enforced proxy paths flows through the
    // egress proxy's StaticDecider,
    // which delegates to the SAME HostMatcher. Assert the wildcard contract survives
    // that delegation end-to-end — a match/no-match set proven at the enforcement seam,
    // not only the matcher unit.
    use nub_sandbox::proxy::{Decision, GrantDecider, Host, StaticDecider};

    let decider = StaticDecider::new(NetPolicy {
        enforce: true,
        default_effect: Effect::Deny,
        rules: vec![NetRule {
            target: NetTarget::Host("*.example.com".into()),
            effect: Effect::Allow,
        }],
        ..Default::default()
    });
    let allowed = |h: &str| matches!(decider.decide(&Host::Name(h.into())), Decision::Allow);

    assert!(allowed("example.com"), "apex admitted at the proxy seam");
    assert!(
        allowed("a.b.example.com"),
        "any-depth admitted at the proxy seam"
    );
    assert!(
        !allowed("example.com.attacker.com"),
        "suffix-confusion denied at the proxy seam"
    );
    assert!(
        !allowed("evilexample.com"),
        "sibling denied at the proxy seam"
    );
}

#[test]
fn host_literal_is_exact_case_insensitive() {
    assert!(host_glob_matches("Example.COM", "example.com"));
    assert!(!host_glob_matches("example.com", "api.example.com"));
}

#[test]
fn host_trailing_dot_normalized_on_both_sides() {
    // D12: a FQDN trailing dot is the same host per DNS — it cannot dodge a rule,
    // and a rule written with one still matches the dotless connect target.
    assert!(
        host_glob_matches("evil.com", "evil.com."),
        "connect-side dot"
    );
    assert!(
        host_glob_matches("evil.com.", "evil.com"),
        "pattern-side dot"
    );
    assert!(host_glob_matches("evil.com.", "evil.com."), "both");
    assert!(
        host_glob_matches("*.example.com", "api.example.com."),
        "wildcard vs dotted host"
    );
    // Exactly one dot stripped: a doubled trailing dot stays malformed.
    assert!(!host_glob_matches("evil.com", "evil.com.."));
}

#[test]
fn host_pattern_grammar_accepts_only_bare_and_leading_wildcard() {
    // D11: valid forms.
    assert!(host_pattern_is_valid("*"));
    assert!(host_pattern_is_valid("*.example.com"));
    assert!(host_pattern_is_valid("example.com"));
    assert!(host_pattern_is_valid("api.internal.example.com"));
    // Mid-host / malformed wildcards are rejected.
    assert!(!host_pattern_is_valid("api.*.com"));
    assert!(!host_pattern_is_valid("foo*bar.com"));
    assert!(!host_pattern_is_valid("*foo.com"));
    assert!(!host_pattern_is_valid("*.foo*bar.com"));
    assert!(!host_pattern_is_valid("**.com"));
    assert!(!host_pattern_is_valid("example.*"));
    // Degenerate empty-apex wildcards: rejected so they can't strip down to a
    // bare `*` allow-all (fail loud, never fail open).
    assert!(!host_pattern_is_valid("*."));
    assert!(!host_pattern_is_valid("*.."));
    // Brace alternation is not part of the host grammar — rejected (fs globs support
    // braces; net hosts do not).
    assert!(!host_pattern_is_valid("{a,b}.com"));
    assert!(!host_pattern_is_valid("api.{a,b}.com"));
}

#[test]
fn net_matcher_admits_by_last_match_and_cidr() {
    let policy = NetPolicy {
        enforce: true,
        default_effect: Effect::Deny,
        rules: vec![
            NetRule {
                target: NetTarget::Host("*.sentry.io".into()),
                effect: Effect::Allow,
            },
            NetRule {
                target: NetTarget::Cidr("10.0.0.0/8".parse().unwrap()),
                effect: Effect::Allow,
            },
        ],
        ..Default::default()
    };
    let m = HostMatcher::new(&policy);
    assert!(m.admits("ingest.sentry.io"));
    assert!(m.admits("10.1.2.3"), "IP in CIDR");
    assert!(!m.admits("evil.com"), "deny-all base");
    assert!(!m.admits("192.168.1.1"), "IP outside CIDR");
}

#[test]
fn net_not_enforcing_admits_everything() {
    let policy = NetPolicy {
        enforce: false,
        ..Default::default()
    };
    let m = HostMatcher::new(&policy);
    assert!(m.admits("anything.com"));
}

// ── secret defaults deny .env at any depth ────────────────────────────────────

#[test]
fn generous_read_still_denies_dotenv_and_ssh() {
    use nub_sandbox::compiler::compile;
    use serde_json::json;
    let ctx = common::ctx(true, &[]);
    let policy = compile(&json!(true), &ctx).unwrap(); // secure defaults
    let m = PathMatcher::new(&policy.fs.rules);
    let home = common::homes().home;
    let proj = common::homes().project;

    assert!(matches!(
        m.decide(&proj.join("src/index.ts")).effect,
        Effect::Allow
    ));
    assert!(
        matches!(m.decide(&proj.join(".env")).effect, Effect::Deny),
        ".env denied"
    );
    assert!(
        matches!(
            m.decide(&proj.join("packages/app/.env.local")).effect,
            Effect::Deny
        ),
        "nested .env denied"
    );
    assert!(
        matches!(m.decide(&home.join(".ssh/id_rsa")).effect, Effect::Deny),
        "ssh denied"
    );
}

// ── the `.env*` default-deny (Feature 2) ──────────────────────────────────────
// Reads of any `.env*`-basename file are denied by DEFAULT on every read-granting
// fs policy — including the OBJECT form (which never spliced `"..."`) — and the deny
// beats a broad dir-allow regardless of order, yet yields to an EXPLICIT exact-file
// allow (informed consent). Verified at the compiler+matcher (engine-pure) layer,
// the shared cross-backend contract.

fn read_denied(surface: serde_json::Value, path: &str) -> bool {
    use nub_sandbox::compiler::compile;
    let ctx = common::ctx(true, &[]);
    let policy = compile(&surface, &ctx).unwrap();
    let m = PathMatcher::new(&policy.fs.rules);
    matches!(
        m.decide(&common::homes().project.join(path)).effect,
        Effect::Deny
    )
}

#[test]
fn object_form_dir_allow_still_denies_dotenv() {
    use serde_json::json;
    // The core gap: an object-form `{ "./": "r" }` grants the project but must NOT
    // expose `<proj>/.env` — the object form never spliced the secret set, so before
    // Feature 2 this leaked. `src/index.ts` stays readable.
    assert!(read_denied(json!({ "fs": { "./": "r" } }), ".env"));
    assert!(read_denied(
        json!({ "fs": { "./": "r" } }),
        "sub/.env.local"
    ));
    assert!(!read_denied(json!({ "fs": { "./": "r" } }), "src/index.ts"));
    // Array-form allowlist, same guarantee.
    assert!(read_denied(json!({ "fs": ["./"] }), ".env"));
}

#[test]
fn dotenv_deny_beats_a_trailing_broad_allow() {
    use serde_json::json;
    // The `["...", "./"]` footgun: a trailing dir-allow re-matches `<proj>/.env` last,
    // so under pure last-match it would re-expose it. The `.env*` deny is injected AFTER
    // every band-1 rule, so it wins regardless of authored order.
    assert!(read_denied(json!({ "fs": ["...", "./"] }), ".env"));
    assert!(read_denied(json!({ "fs": ["./", "..."] }), ".env"));
}

#[test]
fn exact_file_allow_overrides_the_dotenv_deny_but_a_dir_allow_does_not() {
    use nub_sandbox::compiler::compile;
    use nub_sandbox::policy::FsAccess;
    use serde_json::json;
    // Naming the exact file grants it (informed consent); a sibling `.env` stays denied.
    let ctx = common::ctx(true, &[]);
    let policy = compile(
        &json!({ "fs": { "./": "r", "./.env.production": "r" } }),
        &ctx,
    )
    .unwrap();
    let m = PathMatcher::new(&policy.fs.rules);
    let proj = common::homes().project;
    let d = m.decide(&proj.join(".env.production"));
    assert!(
        matches!(d.effect, Effect::Allow) && matches!(d.access, FsAccess::Read),
        "the exact-file allow grants <proj>/.env.production"
    );
    assert!(
        matches!(m.decide(&proj.join(".env")).effect, Effect::Deny),
        "a sibling .env the user did NOT name stays denied"
    );
    // A GLOB allow of the same shape is NOT an exact-file allow and does NOT override.
    assert!(read_denied(
        json!({ "fs": { "./": "r", "./.env*": "r" } }),
        ".env"
    ));
    // An explicit exact-path DENY after an exact allow stays denied (user's last word).
    assert!(read_denied(
        json!({ "fs": ["./", "./.env", "!./.env"] }),
        ".env"
    ));
}

#[test]
fn fully_relaxed_fs_still_reads_dotenv_escape_hatch() {
    use nub_sandbox::compiler::compile;
    use serde_json::json;
    // `fs: true` / `sandbox: false` is the explicit total-relaxation escape hatch — the
    // default `.env*` deny does NOT apply to it (it is not a directory allowlist).
    for surface in [json!({ "fs": true }), json!(false)] {
        let ctx = common::ctx(true, &[]);
        let policy = compile(&surface, &ctx).unwrap();
        let m = PathMatcher::new(&policy.fs.rules);
        assert!(
            matches!(
                m.decide(&common::homes().project.join(".env")).effect,
                Effect::Allow
            ),
            "relaxed fs reads .env ({surface})"
        );
    }
}

#[test]
fn dotenv_deny_matches_the_env_basename_prefix_at_any_depth() {
    use serde_json::json;
    // Basename-prefix `.env`: the dotfile, dotted variants, direnv's `.envrc`, and a
    // `.env.d/` directory's contents — all denied, at the root and nested.
    for p in [
        ".env",
        ".env.local",
        ".env.production",
        ".envrc",
        "sub/.env",
        "packages/app/.env.test",
        ".env.d/secret",
    ] {
        assert!(read_denied(json!({ "fs": { "./": "r" } }), p), "{p} denied");
    }
    // A non-`.env`-prefixed dotfile is NOT swept.
    assert!(!read_denied(json!({ "fs": { "./": "r" } }), ".gitignore"));
}

#[test]
fn env_prefixed_directory_allow_does_not_reexpose_its_contents() {
    use serde_json::json;
    // THE SUB-DIRECTORY BYPASS: a `.env*`-NAMED directory is a secret container, and its
    // contents are covered by the `.env*/**` subtree deny. An exact allow of the DIRECTORY
    // (or its glob subtree) must NOT re-expose those contents — only a `.env*` LEAF file is
    // re-grantable (the subtree deny is ordered after the exact-file allows). This is the
    // regression guard for the band-3 subtree-twin over-grant.
    for (surface, leaked) in [
        (json!({ "fs": { "./.env.d": "r" } }), ".env.d/prod"),
        (json!({ "fs": { "./.env.d": "r" } }), ".env.d/nested/deep"),
        (
            json!({ "fs": { "./.environments": "r" } }),
            ".environments/prod.env",
        ),
        // A glob subtree allow of a `.env*` dir is a glob (not an exact FILE) → no override.
        (json!({ "fs": { "./.env.d/**": "r" } }), ".env.d/prod"),
        // array (rw) directory allow — same guarantee.
        (json!({ "fs": ["./.env.d"] }), ".env.d/prod"),
    ] {
        assert!(
            read_denied(surface.clone(), leaked),
            "{leaked} must stay denied under {surface}"
        );
    }
    // An exact-file allow of a `.env*` FILE inside a subdir still grants THAT one file.
    assert!(!read_denied(
        json!({ "fs": { "./": "r", "./sub/.env.local": "r" } }),
        "sub/.env.local"
    ));
    // …but a sibling `.env` in the same subdir stays denied.
    assert!(read_denied(
        json!({ "fs": { "./": "r", "./sub/.env.local": "r" } }),
        "sub/.env"
    ));
}
