//! U1 resolution-model tests: the clobber warning, object-level `"..."`, and
//! cross-scope `"..."` inheritance driven against SYNTHETIC scope chains (the
//! project frontend that feeds real multi-scope configs does not exist yet — this
//! locks the resolution LOGIC).

mod common;

use nub_sandbox::compiler::CompileError;
use nub_sandbox::compiler::compile;
use nub_sandbox::compiler::compile_with_warnings;
use nub_sandbox::compiler::scope::{ChainScope, resolve_chain};
use nub_sandbox::matcher::{HostMatcher, PathMatcher};
use nub_sandbox::policy::{Effect, FsAccess};
use serde_json::{Value, json};

fn warns(surface: Value) -> bool {
    let ctx = common::ctx(true, &[]);
    !compile_with_warnings(&surface, &ctx).unwrap().1.is_empty()
}

// ── clobber warning (D2b/D6) ───────────────────────────────────────────────────

#[test]
fn total_shadow_warns() {
    // A later entry whose match-set covers an earlier one → the earlier is dead.
    assert!(
        warns(json!({ "fs": ["./data", "!./data"] })),
        "X then !X (equal set)"
    );
    assert!(warns(json!({ "fs": ["./data", "./data"] })), "duplicate");
    assert!(
        warns(json!({ "fs": ["!./data/secret", "./data"] })),
        "narrow deny then broad allow covering it (order-matters footgun)"
    );
    assert!(
        warns(json!({ "fs": ["./a", "**"] })),
        "later ** covers everything"
    );
    assert!(
        warns(json!({ "net": ["in.sentry.io", "*.sentry.io"], "proxy": "auto" })),
        "host then covering wildcard"
    );
    assert!(
        warns(json!({ "net": ["a.com", "*"], "proxy": "auto" })),
        "later * covers all hosts"
    );
    assert!(
        warns(json!({ "env": ["VITE_URL", "VITE_*"] })),
        "key then covering prefix glob"
    );
    assert!(warns(json!({ "env": ["FOO", "FOO"] })), "duplicate env key");
    assert!(
        warns(json!({ "env": ["NODE_ENV", "*"] })),
        "later * covers all keys"
    );
}

#[test]
fn partial_override_and_sentinels_stay_silent() {
    // The intended granular idiom (allow a block, deny one key inside it) is NOT a
    // shadow — the earlier entry still decides for the rest of its set.
    assert!(
        !warns(json!({ "fs": ["./data", "!./data/secret"] })),
        "broad allow, narrow deny"
    );
    assert!(
        !warns(json!({ "env": ["*", "!*_TOKEN"] })),
        "allow-all then strip a class"
    );
    assert!(
        !warns(json!({ "net": ["*", "!*.evil.com"], "proxy": "auto" })),
        "best-effort denylist"
    );
    assert!(!warns(json!({ "fs": ["./a", "./b"] })), "disjoint paths");
    assert!(
        !warns(json!({ "env": ["FOO", "BAR", "!*_TOKEN"] })),
        "disjoint keys + class deny"
    );
    // `"..."` / `"!..."` are excluded from clobber analysis (they expand to many
    // rules); the canonical `["...", specific]` idiom never warns.
    assert!(
        !warns(json!({ "fs": ["...", "./data"] })),
        "inherit then specific"
    );
    assert!(
        !warns(json!({ "env": ["*", "..."] })),
        "allow-all then inherit"
    );
    // A three-entry partial (allow block, deny sub, re-allow sub-sub) stays silent.
    assert!(
        !warns(json!({ "fs": ["./data", "!./data/secret", "./data/secret/pub"] })),
        "nested partial overrides"
    );
}

// ── object-level `"..."` (inherit base for unlisted axes) ──────────────────────

#[test]
fn object_spread_at_outermost_equals_sandbox_true() {
    // `{ "...": true }` at the outermost scope resolves to the built-in base for
    // every unlisted axis → exactly `sandbox: true`.
    let ctx = common::ctx(true, &[("PATH", "/bin"), ("SECRET_TOKEN", "s")]);
    let spread = compile(&json!({ "...": true }), &ctx).unwrap();
    let truth = compile(&json!(true), &ctx).unwrap();
    assert_eq!(spread, truth, "{{ \"...\": true }} ≡ sandbox: true");
}

#[test]
fn object_spread_keeps_unlisted_axes_from_flooring() {
    // With object-level `"..."`, an unlisted axis inherits the base instead of
    // flooring: `{ "...": true, "fs": ["./x"] }` keeps the secure net/env while
    // confining fs — the §4.3 "keep the project's net/env, adjust fs" idiom.
    let ctx = common::ctx(true, &[("PATH", "/bin"), ("MY_TOKEN", "leak")]);
    let p = compile(&json!({ "...": true, "fs": ["./x"] }), &ctx).unwrap();
    assert!(
        p.net.enforce && p.net.rules.is_empty(),
        "net inherited base (deny-all)"
    );
    assert!(
        p.env.constructed.contains_key("PATH"),
        "env inherited curated baseline"
    );
    assert!(
        !p.env.constructed.contains_key("MY_TOKEN"),
        "baseline still drops the secret"
    );
    // Contrast: WITHOUT the object spread, net + env would floor to strip-all.
    let floored = compile(&json!({ "fs": ["./x"] }), &ctx).unwrap();
    assert!(floored.env.constructed.is_empty(), "no spread → env floors");
}

// ── cross-scope `"..."` inheritance (synthetic chains) ─────────────────────────

fn chain(parent: &Value, child: &Value) -> nub_sandbox::policy::SandboxPolicy {
    let ctx = common::ctx(true, &[("FOO", "1"), ("BAR", "2"), ("PATH", "/bin")]);
    resolve_chain(
        &[
            ChainScope {
                label: "project",
                surface: Some(parent),
                caps: common::caps(true),
            },
            ChainScope {
                label: "script",
                surface: Some(child),
                caps: common::caps(true),
            },
        ],
        &ctx,
    )
    .unwrap()
    .0
}

#[test]
fn fs_spread_inherits_the_resolved_parent() {
    let parent = json!({ "fs": { "./a": "rw" } });
    // Child inherits parent fs via `"..."`, then adds ./b.
    let p = chain(&parent, &json!({ "fs": ["...", "./b"] }));
    let m = PathMatcher::new(&p.fs.rules);
    let proj = common::homes().project;
    assert!(rw(&m, &proj.join("a/x")), "inherited parent grant ./a");
    assert!(rw(&m, &proj.join("b/x")), "own grant ./b");
    // Child WITHOUT `"..."` floors: only ./b, parent's ./a NOT inherited.
    let f = chain(&parent, &json!({ "fs": ["./b"] }));
    let mf = PathMatcher::new(&f.fs.rules);
    assert!(rw(&mf, &proj.join("b/x")), "own grant ./b");
    assert!(
        matches!(mf.decide(&proj.join("a/x")).effect, Effect::Deny),
        "parent ./a is NOT inherited without an explicit `...`"
    );
}

#[test]
fn net_spread_inherits_the_resolved_parent() {
    let parent = json!({ "net": ["a.com"], "proxy": "auto" });
    let p = chain(
        &parent,
        &json!({ "net": ["...", "b.com"], "proxy": "auto" }),
    );
    let m = HostMatcher::new(&p.net);
    assert!(m.admits("a.com") && m.admits("b.com"), "inherit + extend");
    // No `"..."` → floor: only b.com.
    let f = chain(&parent, &json!({ "net": ["b.com"], "proxy": "auto" }));
    let mf = HostMatcher::new(&f.net);
    assert!(
        mf.admits("b.com") && !mf.admits("a.com"),
        "parent host NOT inherited"
    );

    let broker_parent = json!({
        "net": { "api.example.com": { "env": ["API_TOKEN"] } }
    });
    let broker_child = chain(&broker_parent, &json!({ "net": ["..."] }));
    assert_eq!(
        broker_child.net.brokers,
        chain(&broker_parent, &json!({ "...": true })).net.brokers
    );
    assert!(matches!(
        broker_child.net.inspection,
        nub_sandbox::policy::Inspection::TlsInspect
    ));

    let denied_broker = chain(
        &broker_parent,
        &json!({ "net": ["...", "!api.example.com"] }),
    );
    assert!(
        denied_broker.net.brokers.is_empty(),
        "a later inherited-host deny removes the broker capability"
    );
    assert!(
        !denied_broker.env.constructed.contains_key("API_TOKEN"),
        "the complete child statement floors env independently"
    );
}

#[test]
fn env_spread_inherits_the_resolved_parent() {
    let parent = json!({ "env": { "FOO": true } }); // parent env = {FOO}
    // Object-form `"..."` key inherits the parent env, then adds BAR.
    let p = chain(&parent, &json!({ "env": { "...": true, "BAR": true } }));
    assert_eq!(
        p.env.constructed.get("FOO").map(String::as_str),
        Some("1"),
        "inherited FOO"
    );
    assert_eq!(
        p.env.constructed.get("BAR").map(String::as_str),
        Some("2"),
        "own BAR"
    );
    assert!(
        !p.env.constructed.contains_key("PATH"),
        "ambient PATH not pulled in"
    );
    // No `"..."` → floor: BAR only, parent FOO NOT inherited.
    let f = chain(&parent, &json!({ "env": { "BAR": true } }));
    assert!(
        f.env.constructed.contains_key("BAR") && !f.env.constructed.contains_key("FOO"),
        "FOO not inherited"
    );
    // A child deny after inherit removes an inherited key (last-match).
    let d = chain(&parent, &json!({ "env": { "...": true, "FOO": false } }));
    assert!(
        !d.env.constructed.contains_key("FOO"),
        "child deny removes inherited FOO"
    );
}

#[test]
fn keyless_scope_cascades_the_parent_whole_policy() {
    // A scope with NO `sandbox` key inherits its parent's WHOLE policy (cascade —
    // it can't escape confinement by saying nothing).
    let ctx = common::ctx(true, &[("FOO", "1")]);
    let parent =
        json!({ "fs": { "./a": "rw" }, "net": ["a.com"], "proxy": "auto", "env": { "FOO": true } });
    let (p, _) = resolve_chain(
        &[
            ChainScope {
                label: "project",
                surface: Some(&parent),
                caps: common::caps(true),
            },
            ChainScope {
                label: "script",
                surface: None,
                caps: common::caps(true),
            }, // keyless
        ],
        &ctx,
    )
    .unwrap();
    let parent_policy = compile(&parent, &ctx).unwrap();
    assert_eq!(p, parent_policy, "keyless child == parent (cascade)");
}

#[test]
fn object_spread_inner_inherits_parent_for_unlisted_axes() {
    // `{ "...": true, "fs": ["...", "./b"] }` at an inner scope: inherit parent
    // net+env, and inherit-then-extend parent fs.
    let parent =
        json!({ "fs": { "./a": "rw" }, "net": ["a.com"], "proxy": "auto", "env": { "FOO": true } });
    let p = chain(&parent, &json!({ "...": true, "fs": ["...", "./b"] }));
    let m = PathMatcher::new(&p.fs.rules);
    let proj = common::homes().project;
    assert!(
        rw(&m, &proj.join("a/x")) && rw(&m, &proj.join("b/x")),
        "fs inherit + extend"
    );
    assert!(HostMatcher::new(&p.net).admits("a.com"), "net inherited");
    assert!(p.env.constructed.contains_key("FOO"), "env inherited");
}

fn rw(m: &PathMatcher, path: &std::path::Path) -> bool {
    let d = m.decide(path);
    matches!(d.effect, Effect::Allow) && matches!(d.access, FsAccess::ReadWrite)
}

// ── per-scope capabilities in ONE compile (P7) ─────────────────────────────────

/// An outer APPROVED (`nub.jsonc`) scope over a surface.
fn approved_scope(surface: &Value) -> ChainScope<'_> {
    ChainScope {
        label: "nub.jsonc",
        surface: Some(surface),
        caps: common::caps(true),
    }
}

/// The mixed chain the old compile-wide `trusted` bool could NOT express: an OUTER
/// approved scope (`nub.jsonc`/`scriptsMeta`) uses `$(…)` + credential brokering while an
/// INNER dependency-controlled scope (`dependenciesMeta`) in the SAME compile is
/// denied both — the fold gates read each scope's OWN caps, not a chain-wide flag.
#[test]
fn per_scope_capabilities_gate_each_scope_independently() {
    // ctx.caps is deliberately DEPENDENCY: the chain must ignore it and read each
    // ChainScope's caps. StubRunner resolves `$(echo hi)` → "hi".
    let ctx = common::ctx(false, &[]);
    let approved = json!({
        "env": { "TOKEN": "$(echo hi)" },
        "net": { "api.example.com": { "env": ["BROKER_TOKEN"] } },
    });
    let outer = approved_scope;

    // (1) Outer approved uses both capabilities; the inner dependency scope only
    //     inherits (no gated op) → the whole chain compiles, and the outer's resolved
    //     `$(…)` value + broker survive. Proves ctx.caps (dependency) is IGNORED.
    let inner_inert = json!({ "...": true });
    let (policy, _) = resolve_chain(
        &[
            outer(&approved),
            ChainScope {
                label: "dependenciesMeta",
                surface: Some(&inner_inert),
                caps: common::caps(false),
            },
        ],
        &ctx,
    )
    .unwrap();
    assert_eq!(
        policy.env.constructed.get("TOKEN").map(String::as_str),
        Some("hi"),
        "approved scope's `$(…)` resolved despite a dependency ctx + inner dependency scope"
    );
    assert!(
        policy
            .net
            .brokers
            .iter()
            .any(|b| b.host == "api.example.com"),
        "approved scope's broker survived into the mixed-chain policy"
    );

    // (2) The inner dependency scope's OWN `$(…)` is denied even though an approved
    //     scope precedes it — env_substitution is per-scope, not chain-wide.
    let inner_subst = json!({ "env": { "X": "$(echo hi)" } });
    let err = resolve_chain(
        &[
            outer(&approved),
            ChainScope {
                label: "dependenciesMeta",
                surface: Some(&inner_subst),
                caps: common::caps(false),
            },
        ],
        &ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, CompileError::UntrustedSubstitution { .. }),
        "inner dependency scope's `$(…)` is gated by its own caps: {err:?}"
    );

    // (3) The inner dependency scope's OWN broker is denied (credential_broker).
    let inner_broker = json!({ "net": { "api.example.com": { "env": ["BROKER_TOKEN"] } } });
    let err = resolve_chain(
        &[
            outer(&approved),
            ChainScope {
                label: "dependenciesMeta",
                surface: Some(&inner_broker),
                caps: common::caps(false),
            },
        ],
        &ctx,
    )
    .unwrap_err();
    match err {
        CompileError::Shape { message, .. } => assert!(
            message.contains("credential brokering"),
            "inner dependency scope's broker is denied by the broker gate: {message}"
        ),
        other => panic!("expected the credential-broker Shape error, got {other:?}"),
    }

    // (4) Control: the SAME inner `$(…)` surface under an APPROVED identity compiles —
    //     the gate keys on scope IDENTITY (caps), not chain position.
    let (ok, _) = resolve_chain(
        &[
            outer(&approved),
            ChainScope {
                label: "scriptsMeta",
                surface: Some(&inner_subst),
                caps: common::caps(true),
            },
        ],
        &ctx,
    )
    .unwrap();
    assert_eq!(
        ok.env.constructed.get("X").map(String::as_str),
        Some("hi"),
        "an approved inner scope resolves `$(…)` — the gate is identity, not position"
    );
}
