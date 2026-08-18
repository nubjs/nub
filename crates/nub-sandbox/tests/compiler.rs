//! Compiler tests: the wrapper trichotomy, preset expansion, per-axis fold, the
//! env type grammar, `$(…)` trust gating, and the error surface.

mod common;

use nub_sandbox::compiler::{CompileError, compile};
use nub_sandbox::policy::{Effect, EnvFormat, FsAccess, Inspection, NetTarget, TmpMode};
use serde_json::{Value, json};

// ── wrapper trichotomy ────────────────────────────────────────────────────────

#[test]
fn false_fully_unjails() {
    let ctx = common::ctx(false, &[("SECRET", "x")]);
    let p = compile(&json!(false), &ctx).unwrap();
    assert!(
        matches!(p.fs.rules.default_effect, Effect::Allow),
        "fs allow-all"
    );
    assert!(!p.net.enforce, "net not enforcing");
    assert!(!p.env.enforce, "env is not confining");
    assert_eq!(
        p.env.constructed.get("SECRET").map(String::as_str),
        Some("x"),
        "the relaxed target environment is resolved at compile time"
    );
}

/// A cmd.exe ancestor puts hidden `=C:`-style per-drive entries in the ambient snapshot
/// (Rust's `env::vars()` surfaces them verbatim). The backend rejects a `=`-bearing key
/// as un-encodable, so a spawn under cmd.exe would fail closed on the two postures that
/// take ambient names wholesale: `sandbox: false` clones the map, and a `vars: ["*"]`
/// glob matches every name. Ingestion drops them, so neither can.
#[test]
fn ambient_ingestion_drops_cmd_exe_shell_positional_keys() {
    let ctx = common::ctx(true, &[("=C:", "C:\\work"), ("KEEP", "1")]);
    for surface in [json!(false), json!({ "vars": ["*"] })] {
        let p = compile(&surface, &ctx).unwrap();
        assert!(
            !p.env.constructed.keys().any(|k| k.contains('=')),
            "{surface} constructed a `=`-named key: {:?}",
            p.env.constructed
        );
        assert!(
            !p.env.withheld.iter().any(|k| k.contains('=')),
            "a shell-positional entry is not a policy decision, so it is not `withheld`"
        );
        assert!(p.env.constructed.contains_key("KEEP"), "{surface}");
    }
}

#[test]
fn true_is_secure_default_per_axis() {
    let ctx = common::ctx(
        true,
        &[("PATH", "/usr/bin"), ("AWS_SECRET_ACCESS_KEY", "sk")],
    );
    let p = compile(&json!(true), &ctx).unwrap();
    assert!(p.net.enforce && p.net.rules.is_empty(), "net deny-all");
    assert!(p.env.enforce, "env constructed");
    assert!(
        p.env.constructed.contains_key("PATH"),
        "baseline keeps PATH"
    );
    assert!(
        !p.env.constructed.contains_key("AWS_SECRET_ACCESS_KEY"),
        "baseline drops secrets"
    );
}

#[test]
fn absent_granular_axis_floors_complete_statement() {
    // THE security inversion (D4/D5): a present granular block is a COMPLETE
    // statement — an axis it does NOT list FLOORS, not relaxes. `{ fs: [...] }`
    // confines fs AND floors net (deny-all, enforcing) + env (strip-all). Fails
    // closed, no invisible grants.
    let ctx = common::ctx(true, &[("ANYTHING", "1")]);
    let p = compile(&json!({ "fs": ["./data"] }), &ctx).unwrap();
    assert!(
        matches!(p.fs.rules.default_effect, Effect::Deny),
        "fs confined"
    );
    assert!(
        p.net.enforce && p.net.rules.is_empty(),
        "net floors: deny-all, enforcing"
    );
    assert!(
        p.env.enforce && p.env.constructed.is_empty(),
        "env floors: strip-all"
    );
    assert!(
        p.env.withheld.contains(&"ANYTHING".to_string()),
        "the stripped ambient var is recorded withheld"
    );
}

#[test]
fn tmp_mode_folds_from_the_tmp_key() {
    let ctx = common::ctx(true, &[]);
    // `$tmp` is the private-dir sentinel: a truthy permission → Private (fresh per-run dir),
    // `false` → Deny. Either sets the TmpMode and emits NO ordinary fs rule (the backend owns
    // the per-run dir + shared-tmp denial). The rest of the fs axis folds normally alongside.
    let p = compile(&json!({ "fs": { "./": "r", "$tmp": "rw" } }), &ctx).unwrap();
    assert_eq!(p.fs.tmp, TmpMode::Private);
    assert_eq!(
        compile(&json!({ "fs": { "$tmp": true } }), &ctx)
            .unwrap()
            .fs
            .tmp,
        TmpMode::Private
    );
    // `"r"` on a fresh empty dir is degenerate, so it too maps to Private (rw).
    assert_eq!(
        compile(&json!({ "fs": { "$tmp": "r" } }), &ctx)
            .unwrap()
            .fs
            .tmp,
        TmpMode::Private
    );
    assert_eq!(
        compile(&json!({ "fs": { "$tmp": false } }), &ctx)
            .unwrap()
            .fs
            .tmp,
        TmpMode::Deny
    );
    // Absent `$tmp` stays Shared (no tmp confinement); Shared is unreachable via the sentinel.
    assert_eq!(
        compile(&json!({ "fs": ["./"] }), &ctx).unwrap().fs.tmp,
        TmpMode::Shared
    );
    // A bogus value on `$tmp` (including the dropped `"private"`/`"deny"` keywords) is rejected.
    assert!(matches!(
        compile(&json!({ "fs": { "$tmp": "private" } }), &ctx),
        Err(CompileError::Shape { .. })
    ));

    // A `$tmp/subpath` key maps INTO the private dir (→ Private mode), and — the fix — emits
    // NO ordinary fs rule pointing at the shared host tmp (`/tmp`).
    let sub = compile(&json!({ "fs": { "./": "r", "$tmp/scratch": "rw" } }), &ctx).unwrap();
    assert_eq!(sub.fs.tmp, TmpMode::Private);
    assert!(
        sub.fs
            .rules
            .entries
            .iter()
            .all(|e| !e.matcher.as_str().contains("/tmp")),
        "`$tmp/scratch` must not leak an fs rule into the shared host tmp"
    );
    // Array form: a `$tmp` / `$tmp/…` entry sets Private; a `!`-negated one sets Deny.
    assert_eq!(
        compile(&json!({ "fs": ["./", "$tmp/scratch"] }), &ctx)
            .unwrap()
            .fs
            .tmp,
        TmpMode::Private
    );
    assert_eq!(
        compile(&json!({ "fs": ["./", "!$tmp"] }), &ctx)
            .unwrap()
            .fs
            .tmp,
        TmpMode::Deny
    );
    // A backslash subpath is a subpath too (path-separator), → Private.
    assert_eq!(
        compile(&json!({ "fs": { "$tmp\\scratch": "rw" } }), &ctx)
            .unwrap()
            .fs
            .tmp,
        TmpMode::Private
    );
    // A `$tmp` name with a NON-separator suffix (`$tmp*`, `$tmp.bak`) would otherwise leak
    // into the shared host tmp via `expand_symbolic`, so it is a hard shape error — object AND
    // array form.
    for bad in [
        json!({ "fs": { "$tmp*": "rw" } }),
        json!({ "fs": { "$tmp.bak": "r" } }),
    ] {
        assert!(
            matches!(compile(&bad, &ctx), Err(CompileError::Shape { .. })),
            "malformed `$tmp` suffix must be a shape error, not a shared-tmp leak"
        );
    }
    assert!(matches!(
        compile(&json!({ "fs": ["$tmp*"] }), &ctx),
        Err(CompileError::Shape { .. })
    ));
    // `$tmpx` is a DIFFERENT `$name` (`tmpx`), not the `$tmp` sentinel — an unrecognized
    // sentinel, which is a hard error under the v2 grammar (not a silent literal path).
    assert!(matches!(
        compile(&json!({ "fs": { "$tmpx": "r" } }), &ctx),
        Err(CompileError::Shape { .. })
    ));
}

#[test]
fn fs_dollar_sentinel_cache_ok_unknown_errors() {
    let ctx = common::ctx(true, &[]);
    // `$cache` is a recognized sentinel — expanded to the platform cache dir, never left
    // as a literal `$cache` path.
    let p = compile(&json!({ "fs": { "$cache/tool": "r" } }), &ctx).unwrap();
    assert!(
        p.fs.rules
            .entries
            .iter()
            .all(|e| !e.matcher.as_str().contains('$')),
        "$cache must be expanded, not carried as a literal"
    );
    assert!(
        p.fs.rules
            .entries
            .iter()
            .any(|e| e.matcher.as_str().contains("tool")),
        "the $cache subpath survives expansion"
    );
    // An unrecognized `$name` is a hard error (v2 grammar) — not a silent literal path.
    // `$home` in particular is dropped: home is `~` now.
    for bad in [
        json!({ "fs": { "$data": "r" } }),
        json!({ "fs": ["$home/x"] }),
    ] {
        assert!(
            matches!(compile(&bad, &ctx), Err(CompileError::Shape { .. })),
            "unrecognized $name must be a shape error, got {:?}",
            compile(&bad, &ctx)
        );
    }
    // `$( … )` command substitution is recognized BEFORE `$name` (the paren disambiguation),
    // so it still resolves at load time (StubRunner: `store path` → an absolute path).
    assert!(compile(&json!({ "fs": { "$(store path)": "r" } }), &ctx).is_ok());
}

#[test]
fn fs_deprecated_angle_sentinels_are_a_hard_error_with_migration_hint() {
    // P0-F1: the pre-v2 `<tmp>`/`<cache>`/`<home>` fs sentinels were renamed to
    // `$tmp`/`$cache`/`~`. They must ERROR (not silently degrade to an inert literal
    // rule that leaves `tmp_mode = Shared`, re-exposing the host tmp under a broad read).
    let ctx = common::ctx(true, &[]);
    let cases = [
        (json!({ "fs": { "<tmp>": "rw" } }), "$tmp"),
        (json!({ "fs": { "<cache>": "r" } }), "$cache"),
        (json!({ "fs": { "<home>": "r" } }), "~"),
        (json!({ "fs": ["<tmp>/scratch"] }), "$tmp"),
        (json!({ "fs": ["!<home>/.ssh"] }), "~"),
    ];
    for (bad, hint) in cases {
        match compile(&bad, &ctx) {
            Err(CompileError::Shape { message, .. }) => assert!(
                message.contains(hint),
                "expected a migration hint to `{hint}`, got: {message}"
            ),
            other => panic!("expected a shape error for {bad:?}, got {other:?}"),
        }
    }
    // A `<…>` that is not one of the three renamed forms still errors (the whole
    // angle-bracket syntax is gone), just without a specific `→` hint.
    assert!(matches!(
        compile(&json!({ "fs": ["<something>"] }), &ctx),
        Err(CompileError::Shape { .. })
    ));
}

#[test]
fn empty_object_is_deny_all() {
    // `sandbox: {}` = deny-all, the opposite of `sandbox: true` (D5): every axis
    // floors because none is listed.
    let ctx = common::ctx(true, &[("PATH", "/bin"), ("SECRET", "s")]);
    let p = compile(&json!({}), &ctx).unwrap();
    assert!(
        matches!(p.fs.rules.default_effect, Effect::Deny) && p.fs.rules.entries.is_empty(),
        "fs deny-all"
    );
    assert!(p.net.enforce && p.net.rules.is_empty(), "net deny-all");
    assert!(
        p.env.enforce && p.env.constructed.is_empty(),
        "env strip-all"
    );
}

// ── naked-`...` rejection (v2: no implicit inheritance) ────────────────────────
// The env-base / scope-inheritance tests that used `["..."]` were removed with the
// mechanism itself in P4; naked `...`/`!...` rejection is covered here + by
// `naked_sentinel_is_rejected_on_every_axis` below (the migration regression).

#[test]
fn sentinel_negation_is_a_shape_error_on_every_axis() {
    // `"!..."` — a negated inheritance sentinel — is meaningless and rejected in
    // all three axis array parsers (never treated as a deny of a literal `...`).
    let ctx = common::ctx(true, &[]);
    for surface in [
        // array form, every axis
        json!({ "fs": ["!..."] }),
        json!({ "net": ["!..."] }),
        json!({ "vars": ["!..."] }),
        // object-key form, every axis (env supports `"..."` inherit but rejects `"!..."`;
        // fs/net reject both a negated sentinel AND a bare `"..."` object key)
        json!({ "vars": { "!...": true } }),
        json!({ "fs": { "!...": "rw" } }),
        json!({ "net": { "!...": true } }),
        json!({ "fs": { "...": "rw" } }),
        json!({ "net": { "...": true } }),
    ] {
        let err = compile(&surface, &ctx).unwrap_err();
        assert!(
            matches!(err, CompileError::Shape { .. }),
            "`!...`/`...` object key must be a shape error for {surface}"
        );
    }
}

#[test]
fn net_bare_string_host_value_is_a_shape_error() {
    // A net OBJECT value is `true | false` only (brokering moved to the secrets axis).
    // A bare string (`{"example.com": "r"}`) or the old `{ "env": [...] }` broker
    // object must fail loud, never be silently treated as anything. A bool stays valid.
    let ctx = common::ctx(true, &[]);
    for surface in [
        json!({ "net": { "example.com": "r" } }),
        json!({ "net": { "example.com": "rw" } }),
        json!({ "net": { "*.example.com": "allow" } }),
        json!({ "net": { "example.com": { "env": ["TOKEN"] } } }),
    ] {
        match compile(&surface, &ctx).unwrap_err() {
            CompileError::Shape { message, .. } => assert!(
                message.contains("host value"),
                "names the offending construct: {message}"
            ),
            other => panic!("expected Shape for {surface}, got {other:?}"),
        }
    }
    assert!(compile(&json!({ "net": { "example.com": true } }), &ctx).is_ok());
}

#[test]
fn net_malformed_cidr_and_slash_host_are_shape_errors() {
    // An entry with `/` is parsed as a CIDR; an out-of-range prefix, a non-numeric
    // prefix, or a slash-bearing hostname that can't be a valid CIDR must be a
    // shape error naming it as a failed CIDR — in both array and object forms.
    let ctx = common::ctx(true, &[]);
    for surface in [
        json!({ "net": ["10.0.0.0/99"] }),    // IPv4 prefix > 32
        json!({ "net": ["10.0.0.0/abc"] }),   // non-numeric prefix
        json!({ "net": ["::1/129"] }),        // IPv6 prefix > 128
        json!({ "net": ["example.com/24"] }), // slash-bearing hostname, not an IP
        json!({ "net": { "10.0.0.0/99": true } }),
        json!({ "net": { "example.com/24": true } }),
    ] {
        match compile(&surface, &ctx).unwrap_err() {
            CompileError::Shape { message, .. } => assert!(
                message.contains("CIDR"),
                "names the offending construct as a CIDR: {message}"
            ),
            other => panic!("expected Shape for {surface}, got {other:?}"),
        }
    }
    // A well-formed CIDR still compiles.
    assert!(compile(&json!({ "net": ["10.0.0.0/8"] }), &ctx).is_ok());
}

#[test]
fn glob_env_type_validates_every_matching_var() {
    // A glob-keyed env type (`{ "VITE_*": "port" }`) type-validates EVERY ambient
    // var it matches, not just the first — an invalid later var errors, and all
    // matches pass through when every one is valid.
    let mixed = common::ctx(true, &[("VITE_A", "80"), ("VITE_B", "notaport")]);
    match compile(&json!({ "vars": { "VITE_*": "port" } }), &mixed).unwrap_err() {
        CompileError::Validation { path, .. } => {
            assert_eq!(
                path, "VITE_B",
                "the invalid var is named, not the first match"
            )
        }
        other => panic!("expected Validation naming VITE_B, got {other:?}"),
    }
    // The FIRST match invalid also errors (proves the fold doesn't skip index 0).
    let first_bad = common::ctx(true, &[("VITE_A", "notaport"), ("VITE_B", "443")]);
    assert!(matches!(
        compile(&json!({ "vars": { "VITE_*": "port" } }), &first_bad).unwrap_err(),
        CompileError::Validation { path, .. } if path == "VITE_A"
    ));
    // All valid → every matching var survives, each validated.
    let all_ok = common::ctx(true, &[("VITE_A", "80"), ("VITE_B", "443")]);
    let p = compile(&json!({ "vars": { "VITE_*": "port" } }), &all_ok).unwrap();
    assert_eq!(
        p.env.constructed.get("VITE_A").map(String::as_str),
        Some("80")
    );
    assert_eq!(
        p.env.constructed.get("VITE_B").map(String::as_str),
        Some("443")
    );
}

#[test]
fn a_secrets_validation_error_redacts_the_value_but_names_the_key() {
    // L1: a `secrets` value that fails format validation must NOT echo the secret — the
    // key is still named (via the error path) so the failure stays actionable.
    let ctx = common::ctx(true, &[("STRIPE_KEY", "super-secret")]);
    match compile(&json!({ "secrets": { "STRIPE_KEY": "port" } }), &ctx).unwrap_err() {
        CompileError::Validation { path, message } => {
            assert_eq!(path, "STRIPE_KEY", "the key is named");
            assert!(
                !message.contains("super-secret"),
                "the secret value must not leak: {message}"
            );
            assert!(
                message.contains("<redacted>"),
                "the value is redacted: {message}"
            );
        }
        other => panic!("expected Validation redacting the value, got {other:?}"),
    }
    // Companion: a NON-secret `vars` value DOES show — seeing the bad PORT is the useful,
    // common case.
    let ctx = common::ctx(true, &[("PORT", "notaport")]);
    match compile(&json!({ "vars": { "PORT": "port" } }), &ctx).unwrap_err() {
        CompileError::Validation { message, .. } => {
            assert!(
                message.contains("notaport"),
                "a non-secret value stays visible: {message}"
            );
        }
        other => panic!("expected Validation showing the value, got {other:?}"),
    }
}

#[test]
fn overlapping_axes_classify_the_key_sensitive_fail_safe() {
    // `vars: ["*"]` (sensitive:false) + `secrets: ["FOO"]` (sensitive:true): FOO must be
    // sensitive regardless of axis order, AND keep its real value (the child needs it).
    let ctx = common::ctx(true, &[("FOO", "topsecret"), ("BAR", "ok")]);
    let p = compile(&json!({ "vars": ["*"], "secrets": ["FOO"] }), &ctx).unwrap();
    assert_eq!(
        p.env.constructed.get("FOO").map(String::as_str),
        Some("topsecret"),
        "the child still receives the real value"
    );
    assert!(
        p.env.sensitive_keys.contains(&"FOO".to_string()),
        "FOO is sensitive (fail-safe): {:?}",
        p.env.sensitive_keys
    );
    assert!(
        !p.env.sensitive_keys.contains(&"BAR".to_string()),
        "a plain var is not sensitive: {:?}",
        p.env.sensitive_keys
    );
    // Glob variant: a `vars` entry names the exact key, a `secrets` GLOB also matches it →
    // still sensitive (any matching sensitive pattern wins).
    let ctx = common::ctx(true, &[("MY_TOKEN", "abc123")]);
    let p = compile(
        &json!({ "vars": ["MY_TOKEN"], "secrets": ["*_TOKEN"] }),
        &ctx,
    )
    .unwrap();
    assert!(
        p.env.sensitive_keys.contains(&"MY_TOKEN".to_string()),
        "a glob secret marks the key sensitive: {:?}",
        p.env.sensitive_keys
    );
}

#[test]
fn empty_fs_entry_is_rejected_fail_loud() {
    // `fs: [""]` used to grant the whole filesystem (fail-OPEN). Now a shape error
    // (D3), for both an empty and a whitespace-only entry, array and object forms.
    let ctx = common::ctx(true, &[]);
    for surface in [
        json!({ "fs": [""] }),
        json!({ "fs": ["   "] }),
        json!({ "fs": { "": "rw" } }),
    ] {
        assert!(
            matches!(compile(&surface, &ctx), Err(CompileError::Shape { .. })),
            "empty fs entry must fail loud for {surface}"
        );
    }
}

#[test]
fn keys_inside_an_axis_object_do_not_implicitly_inherit() {
    // A present axis object is self-contained: `env: { FOO }` is EXACTLY {FOO},
    // never FOO-plus-inherited. `"..."` is the only add-parent mechanism. (Locked
    // so the future scope-chain frontend can't regress key-level inheritance.)
    let ctx = common::ctx(true, &[("FOO", "1"), ("PATH", "/bin"), ("BAR", "2")]);
    let p = compile(&json!({ "vars": { "FOO": true } }), &ctx).unwrap();
    assert_eq!(p.env.constructed.len(), 1, "only the named key");
    assert!(p.env.constructed.contains_key("FOO"));
    assert!(
        !p.env.constructed.contains_key("PATH"),
        "no implicit baseline inherit"
    );
    assert!(
        !p.env.constructed.contains_key("BAR"),
        "no implicit ambient inherit"
    );
    // fs object likewise: only the named path is granted, deny base elsewhere.
    let fp = compile(&json!({ "fs": { "./x": "rw" } }), &ctx).unwrap();
    assert!(
        matches!(fp.fs.rules.default_effect, Effect::Deny),
        "deny base"
    );
}

// ── presets ───────────────────────────────────────────────────────────────────

/// The build jail's COARSE egress grant, asserted as ONE property across its TWO spellings.
///
/// ⛔ THE SPELLING IS PER-OS AND THAT IS WHY THIS IS A FUNCTION. `build_jail_net` renders the same
/// boolean two ways: Linux keeps `enforce` with a catch-all Allow naming no host, because
/// `build_seccomp` hangs the whole socket-family ceiling and the io_uring block on that flag, so
/// clearing it would re-permit AF_UNIX/AF_VSOCK/AF_PACKET as well as egress; macOS and Windows
/// spell it `enforce = false`, which is what reaches the AppContainer `internetClient` capability
/// and what keeps Seatbelt from starting a proxy. An assertion hardcoding either spelling passes on
/// one platform and fails on the other — which is exactly what happened when three sites here were
/// re-aimed at the baseline grant on 2026-08-17 in the macOS spelling alone, leaving
/// `cargo test -p nub-sandbox` red on Linux and unnoticed because no CI leg runs this target there.
///
/// NEITHER spelling may carry a per-host rule: a concrete hostname would be a gate two of the three
/// backends cannot honour, so the only shapes permitted are no rule at all or a single catch-all.
fn assert_coarse_egress_allow(p: &nub_sandbox::policy::SandboxPolicy, who: &str) {
    for rule in &p.net.rules {
        match &rule.target {
            nub_sandbox::policy::NetTarget::Host(h) => assert_eq!(
                h, "*",
                "{who}: the build jail must emit no per-host rule, found `{h}`"
            ),
            other => panic!("{who}: unexpected non-host build-jail net target: {other:?}"),
        }
    }
    if cfg!(target_os = "linux") {
        assert!(
            p.net.enforce && p.net.rules.len() == 1,
            "{who}: Linux spells coarse-allow as a catch-all Allow under a KEPT `enforce`, so the \
             socket-family ceiling and the io_uring block stay in place and only AF_INET/AF_INET6 \
             are lifted — got enforce={} rules={:?}",
            p.net.enforce,
            p.net.rules
        );
    } else {
        assert!(
            !p.net.enforce && p.net.rules.is_empty(),
            "{who}: macOS and Windows spell coarse-allow as `enforce = false` with no rule — got \
             enforce={} rules={:?}",
            p.net.enforce,
            p.net.rules
        );
    }
    assert!(
        nub_sandbox::matcher::HostMatcher::new(&p.net).admits("evil.test"),
        "{who}: the grant is COARSE, so a host no list ever carried is admitted too. Per-host \
         enforcement was dropped deliberately; do not restore it here"
    );
}

#[test]
fn build_jail_preset_expands() {
    // The STATIC `--sandbox build-jail` preset: tight, default-deny read of the
    // project + `$tooldirs` (the OS backends supply the system/toolchain closure under a
    // minimal root), a private tmp, NO egress, strip-all env. The per-package WRITE grant +
    // provisioned-interpreter read + scrubbed lifecycle env are the interposition's job (see
    // `build_jail_interposition_*`), NOT this static preset.
    //
    // Egress is COARSE-ALLOW here, not deny-all: this arm carries no package identity, so the
    // resolution falls to `baseline_caps()`, whose `network` is true — set by `4001cec5c5 sandbox:
    // give an uncatalogued package a baseline grant instead of nothing` on 2026-08-16, which left
    // this and four sibling assertions pinning the policy it replaced. `--sandbox build-jail` names
    // no package, which resolves the same way as the `None` aube hands over for a fetched checkout.
    // The catalogued arm is `build_jail_interposition_gates_egress_on_package_identity`.
    let ctx = common::ctx(true, &[("PATH", "/bin"), ("NPM_TOKEN", "t")]);
    let p = compile(&json!("build-jail"), &ctx).unwrap();
    assert_coarse_egress_allow(&p, "the static build-jail preset");
    // ⛔ EVERY HOST IS ADMITTED, BECAUSE COARSE-ALLOW IS NOT A HOST LIST. This looped asserting the
    // opposite. What the assertion is still worth proving is that the outcome is UNIFORM — no host is
    // treated specially, so nothing here has quietly become a per-host gate again. `evil.test` and a
    // former `$downloads` host must be indistinguishable, which is exactly what a coarse grant means.
    let hosts = nub_sandbox::matcher::HostMatcher::new(&p.net);
    for host in ["nodejs.org", "evil.test", "api.github.com", "ghcr.io"] {
        assert!(
            hosts.admits(host),
            "a coarse grant admits every host uniformly, with no host treated specially: `{host}`"
        );
    }
    assert!(
        p.env.enforce && p.env.constructed.is_empty(),
        "static build-jail strips env"
    );
    // Windows takes the SHARED tmp: `Private` was never enforced there — `tmp_lost_axis`
    // reported it lost on every confined spawn while `make_private_tmp` allocated a dir the
    // confined path never used, because the OS redirects an AppContainer's TEMP into its own
    // profile regardless.
    assert_eq!(
        matches!(p.fs.tmp, nub_sandbox::policy::TmpMode::Private),
        !cfg!(windows),
        "build-jail gives a private per-run tmp off Windows, and the shared tmp on Windows"
    );
    let m = nub_sandbox::matcher::PathMatcher::new(&p.fs.rules);
    let proj = common::homes().project;
    // The DEPENDENCY TREE is READ-only (NOT write) — the per-package write is per-spawn.
    let d = m.decide(&proj.join("node_modules/.bin/node-gyp-build"));
    assert!(
        matches!(d.effect, Effect::Allow)
            && matches!(d.access, nub_sandbox::policy::FsAccess::Read),
        "static build-jail reads the dependency tree but does not write it"
    );
    // The consumer's top-level manifest is readable as ONE FILE — two packages at corpus
    // scale crash with an uncaught ENOENT without it — while the directory holding it is
    // not. That pairing is the tightening the read-set measurement bought: a dependency's
    // install script cannot read the consumer's source, its config, its `.git/hooks/`, or
    // its CI workflows.
    assert!(
        matches!(m.decide(&proj.join("package.json")).effect, Effect::Allow),
        "build-jail reads the consumer's top-level package.json"
    );
    for outside in [
        "src/app.ts",
        "tsconfig.json",
        ".git/hooks/pre-commit",
        ".github/workflows/ci.yml",
    ] {
        assert!(
            matches!(m.decide(&proj.join(outside)).effect, Effect::Deny),
            "build-jail must not read <proj>/{outside} — only the dependency tree \
             and the top-level manifest"
        );
    }
    // The CONSUMER's secrets stay denied — by being outside every grant, not by a deny rule.
    // The project root is ungranted (only `package.json` and `node_modules` are), so a
    // project-level `.env`/`.envrc` is unreachable by construction.
    for secret in [".env", ".envrc", ".npmrc", ".env/keys.txt"] {
        assert!(
            matches!(m.decide(&proj.join(secret)).effect, Effect::Deny),
            "build-jail must deny <proj>/{secret}"
        );
    }
    // A VENDORED `.env` inside the granted dependency tree is readable, and that is the
    // model. The build jail compiles to a pure allowlist: it grants `node_modules` wholesale
    // because lifecycle scripts resolve their hoisted tooling out of it, and a file inside a
    // granted subtree is readable. Denying it would mean a deny nested inside a grant — which
    // Landlock (no deny primitive at any ABI) and Windows AppContainer (a deny-ACE naming the
    // container's own SID is inert against its own child) cannot express, and which rejected
    // every read-granting build-jail policy on Windows outright. It is also the dependency's
    // OWN shipped file, not a consumer credential.
    for vendored in ["node_modules/pkg/.env", "node_modules/.env/keys.txt"] {
        assert!(
            matches!(m.decide(&proj.join(vendored)).effect, Effect::Allow),
            "a vendored {vendored} is inside the granted dependency tree"
        );
    }
    assert!(
        matches!(
            m.decide(&common::homes().home.join(".ssh/id_rsa")).effect,
            Effect::Deny
        ),
        "build-jail must deny the home secret set"
    );
    // D6: the two password-hash files are denied. macOS's Seatbelt base still grants
    // `/etc` as a subpath, so this carve-out is what keeps them unreadable there.
    for shadow in ["/etc/shadow", "/etc/gshadow"] {
        assert!(
            matches!(m.decide(std::path::Path::new(shadow)).effect, Effect::Deny),
            "build-jail must deny {shadow}"
        );
    }
    // The toolchain read is nub's OWN PM cache — where it bootstraps node-gyp — and NOT
    // the broad `$tooldirs` set the jail used to take. The other ecosystems' caches carry
    // no part of a Node build closure and are out of the read set.
    //
    // Both real entry points are pinned, because a confined script now resolves node-gyp
    // ONLY from here: the interposition skips the ambient-PATH probe outright, so a host's
    // global node-gyp is never reachable and these two paths are the whole toolchain.
    for gyp in [
        "nub/pm/tools/node-gyp/v12/node_modules/.bin/node-gyp",
        "nub/pm/tools/node-gyp/lazy-bin/node-gyp",
    ] {
        assert!(
            matches!(
                m.decide(&common::homes().cache.join(gyp)).effect,
                Effect::Allow
            ),
            "build-jail must grant nub's bootstrapped node-gyp: {gyp}"
        );
    }
    for tooldir in [
        ".cargo/registry/pkg",
        ".m2/repository/x",
        ".bun/install/cache/y",
    ] {
        assert!(
            matches!(
                m.decide(&common::homes().home.join(tooldir)).effect,
                Effect::Deny
            ),
            "build-jail must not grant the broad $tooldirs set — ~/{tooldir}"
        );
    }
    for denied in [
        std::path::PathBuf::from("/usr/lib/libc.so"),
        common::homes().home.join("notes.txt"),
    ] {
        assert!(
            matches!(m.decide(&denied).effect, Effect::Deny),
            "static build-jail must NOT grant whole-fs read to {}",
            denied.display()
        );
    }
}

/// Dropping `/opt` from the Linux minimal-root floor must not strand an interpreter that
/// LIVES under `/opt`. This is the single most likely way the narrowing breaks CI: a
/// GitHub Actions runner keeps Node under `/opt/hostedtoolcache/node/<ver>/<arch>`, so
/// before the floor stopped mounting `/opt` wholesale that interpreter was reachable by
/// accident. It has to stay reachable ON PURPOSE — through the per-spawn interpreter and
/// extra-read grants, which are what this pins.
///
/// Asserted against the compiled POLICY rather than a launch: the grant must hold on a
/// host that has no `/opt/hostedtoolcache` to bind, which is every host this suite runs on.
/// The tree is drive-anchored on Windows for the same reason `common::homes()` is: a
/// drive-less absolute path is not absolute there, so the compiler cannot anchor it and
/// every candidate would fall through to the default deny — a green that proves nothing.
#[test]
fn build_jail_reaches_an_interpreter_living_under_opt() {
    use std::collections::BTreeMap;
    let out_of_tree = |rel: &str| -> std::path::PathBuf {
        let drive = if cfg!(windows) { "C:" } else { "" };
        std::path::PathBuf::from(format!("{drive}{rel}"))
    };
    let homes = common::homes();
    let node_root = out_of_tree("/opt/hostedtoolcache/node/26.0.0/arm64");
    let interpreter = node_root.join("bin/node");
    let p = nub_sandbox::compile_build_jail(
        homes.clone(),
        &homes.project.join("node_modules/native"),
        None,
        None,
        vec![interpreter.clone()],
        vec![
            node_root.join("include/node"),
            node_root.join("lib/node_modules"),
        ],
        BTreeMap::new(),
    )
    .unwrap();
    let m = nub_sandbox::matcher::PathMatcher::new(&p.fs.rules);
    for reachable in [
        interpreter.clone(),
        node_root.join("bin/npm"),
        node_root.join("include/node/node_api.h"),
        node_root.join("lib/node_modules/npm/bin/npm-cli.js"),
    ] {
        assert!(
            matches!(m.decide(&reachable).effect, Effect::Allow),
            "a /opt-resident Node toolchain must stay granted: {}",
            reachable.display()
        );
    }
    // The grant is the toolchain, not `/opt`. Everything else under it stays withheld —
    // that is the ~11 GB of unrelated third-party software a CI runner keeps there.
    for withheld in [
        out_of_tree("/opt/vendorware/creds.txt"),
        out_of_tree("/opt/hostedtoolcache/Python/3.12.0/x64/bin/python"),
        node_root.join("lib/private.txt"),
    ] {
        assert!(
            matches!(m.decide(&withheld).effect, Effect::Deny),
            "dropping /opt must leave {} withheld",
            withheld.display()
        );
    }
}

/// EGRESS IS GATED ON PACKAGE IDENTITY — the whole resolution rule, in one place.
///
/// Asserted through `compile_build_jail` rather than against the catalog accessor, because the
/// accessor was already correct while nothing consumed it: the defect this pins is that the
/// compiled POLICY ignored the package. Each row therefore names a real catalog fact, so a
/// catalog edit that moved a package between classes would fail here rather than silently
/// change what a user gets.
#[test]
fn build_jail_interposition_gates_egress_on_package_identity() {
    use std::collections::BTreeMap;
    let homes = common::homes();
    let proj = homes.project.clone();
    let ambient: BTreeMap<String, String> = [("PATH".to_string(), "/bin".to_string())]
        .into_iter()
        .collect();

    let compile_for = |name: Option<&str>| {
        let dir = proj.join("node_modules/.aube/x@1.0.0/node_modules/x");
        nub_sandbox::compile_build_jail(
            homes.clone(),
            &dir,
            name,
            Some("1.0.0"),
            Vec::new(),
            Vec::new(),
            ambient.clone(),
        )
        .expect("compile build-jail")
    };

    // GRANTED — named in `networkHosts[].fetchedBy`, which is what an entry means. Both are
    // real catalog facts (`cypress` fetches its binary, `@prisma/engines` its query engines),
    // so a catalog edit that moved either out would fail here rather than silently change what
    // a user gets.
    //
    // THE GRANT IS COARSE ON EVERY PLATFORM, and the host list is gone. Per-host was withdrawn
    // because only macOS could enforce it — Linux needs a netns it cannot require, Windows'
    // loopback exemption is admin-only — so gating the platform most developers use meant an
    // incomplete list erroring for them alone. The product fact is therefore asserted in BOTH
    // directions: a catalogued package reaches a `$downloads` host AND a host that was never on
    // any list. The second is the behaviour change, and asserting only the first would keep
    // passing if per-host enforcement came back.
    //
    // The SPELLING still diverges, and that is load-bearing rather than incidental (see
    // `preset::build_jail_net`). macOS and Windows compile to coarse-allow: it is the only
    // spelling that reaches the AppContainer `internetClient` capability, which the backend grants
    // on exactly `!net.enforce`. Linux keeps `enforce` with a catch-all naming no host, because
    // `build_seccomp` hangs the whole socket-family ceiling and the io_uring block on that flag —
    // relaxing it to grant egress would also re-permit AF_UNIX, AF_VSOCK and AF_PACKET.
    for admitted in ["cypress", "@prisma/engines"] {
        let p = compile_for(Some(admitted));
        let hosts = nub_sandbox::matcher::HostMatcher::new(&p.net);
        assert!(
            hosts.admits("nodejs.org"),
            "{admitted} is catalogued, so it reaches the network"
        );
        assert!(
            hosts.admits("evil.test"),
            "{admitted}: the grant is COARSE — a host no list ever carried is admitted too. \
             Per-host enforcement was dropped deliberately; do not restore it here"
        );
        // NO PER-HOST RULE, in either spelling. A concrete hostname in a build-jail policy would
        // be a gate two of three backends cannot honour, so the only shapes permitted here are no
        // rule at all (coarse-allow) or a single catch-all.
        for rule in &p.net.rules {
            match &rule.target {
                nub_sandbox::policy::NetTarget::Host(h) => assert_eq!(
                    h, "*",
                    "{admitted}: the build jail must emit no per-host rule, found `{h}`"
                ),
                other => {
                    panic!("{admitted}: unexpected non-host build-jail net target: {other:?}")
                }
            }
        }
        if cfg!(target_os = "linux") {
            assert!(
                p.net.enforce,
                "{admitted}: Linux keeps enforcing so the socket-family ceiling and the \
                 io_uring block stay in place; only AF_INET/AF_INET6 are lifted"
            );
        } else {
            assert!(
                !p.net.enforce,
                "{admitted}: coarse-allow is what hands Windows' AppContainer its \
                 internetClient capability, and what keeps macOS from starting a proxy"
            );
        }
    }

    // DENIED — the default, and the case that matters. `left-pad` is an ordinary dependency
    // with no catalog entry: exactly the Shai-Hulud shape, a package that could acquire a
    // lifecycle script nobody reviewed. `None` is what aube hands over when the spawn root is
    // a checkout it fetched. The third clause — a package both observed in `fetchedBy` and
    // refused in `notGranted.packages` — is pinned at `catalog::parse` instead, where the
    // subtraction happens: this catalog lists no refused package, so asserting it here would
    // pass against a generator that had stopped subtracting at all.
    //
    // ⛔ SPELLED PER PLATFORM, exactly like the granted arm above. This block once read "UNIFORM
    // ACROSS PLATFORMS … there is no spelling to branch on", which was true only while the arm was
    // deny-all; the baseline made it coarse-ALLOW, and coarse-allow is the shape that has two
    // spellings. Asserting the macOS one here is what left this red on Linux.
    for uncatalogued in [Some("left-pad"), None] {
        let p = compile_for(uncatalogued);
        // It asserted deny-all until 2026-08-17, which is what the baseline change replaced: the net
        // axis no longer withholds from an unknown package at all, and the filesystem axis is what does.
        assert_coarse_egress_allow(&p, &format!("{uncatalogued:?}"));
        assert!(
            nub_sandbox::matcher::HostMatcher::new(&p.net).admits("nodejs.org"),
            "{uncatalogued:?} takes the baseline's coarse grant, so every host is reachable — \
             including a former $downloads host, which must not be special-cased back into existence"
        );
    }
}

/// A VERSION-SCOPED EGRESS ENTRY BINDS THROUGH THE PRODUCTION COMPILE, not merely through the
/// catalog accessor — the same distinction the identity test above exists for, applied to the
/// other half of the key.
///
/// `esbuild` is the shipped case and the boundary is its own code: `optionalDependencies`
/// landed in 0.13.0, so from there up its `install.js` resolves the prebuilt platform package
/// and opens no socket, while below it the `npm install` shell-out is the only path. Both arms
/// are asserted because the DENIED one is what proves the scope is real — an entry matching
/// every version satisfies the granted arm on its own, which is the state this replaced.
#[test]
fn build_jail_interposition_honours_a_version_scoped_egress_entry() {
    use std::collections::BTreeMap;
    let homes = common::homes();
    let dir = homes
        .project
        .join("node_modules/.aube/esbuild@x/node_modules/esbuild");
    let ambient: BTreeMap<String, String> = [("PATH".to_string(), "/bin".to_string())]
        .into_iter()
        .collect();
    let admits = |version: &str| {
        let p = nub_sandbox::compile_build_jail(
            homes.clone(),
            &dir,
            Some("esbuild"),
            Some(version),
            Vec::new(),
            Vec::new(),
            ambient.clone(),
        )
        .expect("compile build-jail");
        nub_sandbox::matcher::HostMatcher::new(&p.net).admits("registry.npmjs.org")
    };

    // ⛔ THE BOUNDARY MOVED FROM 0.13.0 TO 0.28.1 WHEN THE MEASURED CATALOG BECAME AUTHORITATIVE,
    // AND THE TWO SOURCES GENUINELY DISAGREE — this is not a stale expectation.
    //
    // The v1 table asserted, by reasoning about `optionalDependencies`, that only versions below
    // 0.13.0 need the registry. The baked v2 catalog carries a MEASURED band, `<0.28.1: network`,
    // whose own notes list what it was observed on: 0.11.23, 0.14.54, 0.15.18, 0.16.17, 0.17.19,
    // 0.18.20, 0.19.12, 0.20.2, 0.21.5, 0.23.1, 0.24.2, 0.25.12, 0.26.0, 0.27.7. So versions the v1
    // entry says need no registry were each measured USING one.
    //
    // Left unresolved on purpose: a cold-cache corpus run can manufacture a fetch that a normal
    // install satisfies from `optionalDependencies`, so the measurement may be an artifact of the
    // harness. What is NOT in doubt is which way to fail — withholding egress across 0.13-0.27 of a
    // package this popular is an under-grant, the one outcome the design rejects, while granting it
    // is the safe over-grant. Tracked for re-measurement.
    //
    // What this test is FOR is unchanged: proving a version-scoped entry is selected BY VERSION
    // rather than applied wholesale. It now asserts the authoritative catalog's own band boundary.
    // ⛔ AND THE BOUNDARY MOVED AGAIN, to `<0.28.2`, when the catalog was re-baked with per-OS overlays
    // (`45b6cb07`). The list below pinned 0.28.1 as OUTSIDE the band; the baked entry's own notes say it
    // "covers everything below 0.28.2" and list 0.28.1 among the versions measured. So 0.28.1 belongs on
    // the admitting side. Two independent stalenesses met in this one test — a moved band and the
    // baseline change — which is why the assertion is now written against the catalog's own bound.
    for needs_it in [
        "0.11.23", "0.12.29", "0.13.0", "0.25.12", "0.27.7", "0.28.1",
    ] {
        assert!(
            admits(needs_it),
            "esbuild {needs_it} is below the measured `<0.28.2` band, which grants egress"
        );
    }
    for does_not in ["0.28.2", "0.29.0"] {
        assert!(
            !admits(does_not),
            "esbuild {does_not} is at or above the band bound, where the entry's `default` \
             withholds egress — the assertion that proves the band is chosen by version"
        );
    }
}

#[test]
fn build_jail_interposition_confines_write_grants_interpreter_and_scrubs_env() {
    use std::collections::BTreeMap;
    // The production path: `compile_build_jail` for one dep lifecycle spawn. Its package
    // dir is WRITABLE, the provisioned interpreter (outside `/usr`) is readable, and the
    // constructed lifecycle env is kept minus credential-shaped keys.
    let homes = common::homes();
    let proj = homes.project.clone();
    let package_dir = proj.join("node_modules/.aube/left-pad@1.0.0/node_modules/left-pad");
    // A provisioned Node under nub's data dir — NOT under `/usr`, so the tight-read base
    // cannot reach it; the interpreter grant is the load-bearing addition.
    let interpreter = homes.home.join(".local/share/nub/node/22.15.0/bin/node");
    // The Node root (`bin/node`'s grandparent) and its `include/node` header dir — the
    // embedder derives these and passes them as the per-spawn extra reads so node-gyp
    // compiles offline. It lives under nub's version store, outside `$tooldirs`.
    let node_root = homes.home.join(".local/share/nub/node/22.15.0");
    let include_node = node_root.join("include/node");
    let ambient: BTreeMap<String, String> = [
        ("PATH", "/bin"),
        ("NODE", interpreter.to_str().unwrap()),
        ("npm_node_execpath", interpreter.to_str().unwrap()),
        ("npm_config_nodedir", node_root.to_str().unwrap()),
        ("npm_package_name", "left-pad"),
        ("NPM_TOKEN", "super-secret"),
        ("AWS_SECRET_ACCESS_KEY", "leak"),
        ("npm_config_//registry.npmjs.org/:_authToken", "leak"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();

    let p = nub_sandbox::compile_build_jail(
        homes.clone(),
        &package_dir,
        None,
        None,
        vec![interpreter.clone()],
        vec![include_node.clone()],
        ambient,
    )
    .unwrap();
    let m = nub_sandbox::matcher::PathMatcher::new(&p.fs.rules);

    // WRITE confined to the own package dir; the rest of the project is read-only.
    let pkg = m.decide(&package_dir.join("build/Release/addon.node"));
    assert!(
        matches!(pkg.effect, Effect::Allow)
            && matches!(pkg.access, nub_sandbox::policy::FsAccess::ReadWrite),
        "the dep's own package dir is writable"
    );
    // A SIBLING dependency stays readable — a lifecycle script's own deps and their
    // `.bin` shims are hoisted to the consumer's `node_modules`, so this is the grant
    // `node-gyp-build` and `prebuild-install` actually resolve through.
    let sibling = m.decide(&proj.join("node_modules/.bin/prebuild-install"));
    assert!(
        matches!(sibling.effect, Effect::Allow)
            && matches!(sibling.access, nub_sandbox::policy::FsAccess::Read),
        "the sibling dependency tree is read-only"
    );
    // The consumer's own source is NOT readable — the whole point of the narrowing.
    assert!(
        matches!(m.decide(&proj.join("src/app.ts")).effect, Effect::Deny),
        "the consuming project's source is outside the jail's read set"
    );
    // The provisioned interpreter (and its bin dir) is readable.
    assert!(
        matches!(m.decide(&interpreter).effect, Effect::Allow),
        "the provisioned interpreter is granted read"
    );
    assert!(
        matches!(
            m.decide(&interpreter.parent().unwrap().join("npm")).effect,
            Effect::Allow
        ),
        "the interpreter's bin dir is granted read"
    );
    // The provisioned Node's `include/node` header tree is readable so node-gyp compiles
    // offline (the store path is outside `$tooldirs` and the interpreter grant, so this
    // extra-read grant is what makes it reachable).
    assert!(
        matches!(
            m.decide(&include_node.join("node_api.h")).effect,
            Effect::Allow
        ),
        "the Node header dir is granted read"
    );
    // A `.env` inside the package dir is readable: that dir is granted read-WRITE outright,
    // so the script could overwrite the file anyway and a read-deny protected nothing. It is
    // the dependency's own shipped file. The home secret set below is what actually matters,
    // and it holds by being ungranted.
    assert!(
        matches!(m.decide(&package_dir.join(".env")).effect, Effect::Allow),
        "the package dir is granted rw, so its own .env is readable"
    );
    assert!(
        matches!(
            m.decide(&homes.home.join(".ssh/id_rsa")).effect,
            Effect::Deny
        ),
        "the home secret set stays denied"
    );
    // Egress: COARSE-ALLOW. `left-pad` carries no catalog entry, so it takes the baseline, and the
    // baseline grants network; the meaningful half is the absence of a per-host list, which is what
    // the design withdrew. The axis is exercised across all three resolution classes in
    // `build_jail_interposition_gates_egress_on_package_identity`.
    assert_coarse_egress_allow(&p, "left-pad");
    assert!(
        nub_sandbox::matcher::HostMatcher::new(&p.net).admits("nodejs.org"),
        "an uncatalogued package takes the baseline's coarse grant, so every host is reachable; \
         what the jail withholds from it is on the filesystem axis, not this one"
    );
    // Env: the constructed lifecycle env is KEPT minus credential-shaped keys.
    assert!(p.env.enforce);
    assert_eq!(
        p.env
            .constructed
            .get("npm_package_name")
            .map(String::as_str),
        Some("left-pad"),
        "build hints / package env are kept"
    );
    assert_eq!(
        p.env.constructed.get("PATH").map(String::as_str),
        Some("/bin"),
        "PATH is kept (a build needs it)"
    );
    assert_eq!(
        p.env
            .constructed
            .get("npm_config_nodedir")
            .map(String::as_str),
        node_root.to_str(),
        "npm_config_nodedir (points node-gyp at the local headers) is kept"
    );
    for cred in [
        "NPM_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "npm_config_//registry.npmjs.org/:_authToken",
    ] {
        assert!(
            !p.env.constructed.contains_key(cred),
            "credential-shaped key {cred} must be withheld"
        );
        assert!(
            p.env.withheld.contains(&cred.to_string()),
            "withheld list records {cred}"
        );
    }
}

#[test]
fn unknown_preset_is_a_hard_error_naming_the_set() {
    let ctx = common::ctx(true, &[]);
    let err = compile(&json!("no-such-preset"), &ctx).unwrap_err();
    assert!(matches!(err, CompileError::UnknownPreset { .. }));
    assert!(err.to_string().contains("build-jail"));
}

#[test]
fn path_like_string_is_an_unresolved_file_ref() {
    let ctx = common::ctx(true, &[]);
    // A leading `./`/`../`/`/`/`~` or an extension = file-ref.
    for reference in [
        "./policy.json",
        "../p.json",
        "/abs/p.json",
        "~/p.json",
        "p.json",
    ] {
        let err = compile(&json!(reference), &ctx).unwrap_err();
        assert!(
            matches!(err, CompileError::FileRefUnresolved { .. }),
            "{reference} should be a file-ref"
        );
    }
    // A bare identifier (no leading-dot, no extension) = preset — matching
    // nub-cli's project_config classifier exactly (Phase R unified the two).
    assert!(matches!(
        compile(&json!("build-jail-x"), &ctx).unwrap_err(),
        CompileError::UnknownPreset { .. }
    ));
}

// ── unknown keys fail loud ────────────────────────────────────────────────────

#[test]
fn unknown_axis_key_fails() {
    let ctx = common::ctx(true, &[]);
    let err = compile(&json!({ "fs": true, "bogus": 1 }), &ctx).unwrap_err();
    assert!(matches!(err, CompileError::Shape { .. }));
}

// ── policy-file self-exclusion (Phase 5) ───────────────────────────────────────

#[test]
fn policy_file_is_denied_under_a_broad_grant_while_a_sibling_stays_usable() {
    // The policy source file is auto-excluded (read AND write) from every fs grant, so a
    // sandboxed process can neither read nor tamper with the policy that confines it —
    // even under a whole-project (`.`) or whole-fs (`/`) grant. A sibling under the SAME
    // grant stays read+write (the negative control), proving the deny is exact-path, not
    // a broad shadow.
    use nub_sandbox::matcher::PathMatcher;
    use nub_sandbox::policy::FsAccess;
    let proj = common::homes().project;
    let policy = proj.join("policy.jsonc");
    let ctx = common::ctx(true, &[]).with_policy_files(vec![policy.clone()]);
    for surface in [json!({ "fs": ["."] }), json!({ "fs": ["/"] })] {
        let p = compile(&surface, &ctx).unwrap();
        let m = PathMatcher::new(&p.fs.rules);
        assert_eq!(
            m.decide(&policy).effect,
            Effect::Deny,
            "policy file denied (read+write) under {surface}"
        );
        let sib = m.decide(&proj.join("sibling.txt"));
        assert_eq!(
            sib.effect,
            Effect::Allow,
            "sibling readable under {surface}"
        );
        assert_eq!(
            sib.access,
            FsAccess::ReadWrite,
            "sibling writable under {surface}"
        );
    }
}

#[test]
fn policy_file_deny_survives_an_explicit_allow_later_in_the_list() {
    // Floor precedence, not last-match-loses: an explicit allow of the exact policy path
    // AFTER the broad grant still loses to the self-exclusion, which is appended after
    // every user entry.
    use nub_sandbox::matcher::PathMatcher;
    let proj = common::homes().project;
    let policy = proj.join("policy.jsonc");
    let ctx = common::ctx(true, &[]).with_policy_files(vec![policy.clone()]);
    let p = compile(&json!({ "fs": [".", "./policy.jsonc"] }), &ctx).unwrap();
    let m = PathMatcher::new(&p.fs.rules);
    assert_eq!(m.decide(&policy).effect, Effect::Deny);
}

#[test]
fn policy_file_deny_escapes_glob_metachars_in_the_path() {
    // A policy path traversing a glob-metachar segment (Next.js App Router `[id]`, or a
    // brace `{a,b}`) must STILL be denied under a broad grant. The candidate is a literal
    // subject string, so without escaping the deny pattern the `[id]`/`{a,b}` would be read
    // as a glob (a char class / alternation) that does NOT match the literal path — a silent
    // fail-open leaving the policy file read+write.
    use nub_sandbox::matcher::PathMatcher;
    let proj = common::homes().project;
    for seg in ["[id]", "{a,b}"] {
        let policy = proj.join(seg).join("policy.jsonc");
        let ctx = common::ctx(true, &[]).with_policy_files(vec![policy.clone()]);
        let p = compile(&json!({ "fs": ["."] }), &ctx).unwrap();
        let m = PathMatcher::new(&p.fs.rules);
        assert_eq!(
            m.decide(&policy).effect,
            Effect::Deny,
            "policy file under a `{seg}` segment must stay denied"
        );
    }
}

#[test]
fn inline_policy_with_no_source_file_is_not_self_excluded() {
    // An inline policy has no distinct source file (an empty `policy_files`), so nothing is
    // auto-excluded — a broad grant reads/writes every non-secret path. Pins the
    // inline-case decision (self-exclusion is opt-in via a non-empty source-path set).
    use nub_sandbox::matcher::PathMatcher;
    let proj = common::homes().project;
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "fs": ["."] }), &ctx).unwrap();
    let m = PathMatcher::new(&p.fs.rules);
    assert_eq!(
        m.decide(&proj.join("policy.jsonc")).effect,
        Effect::Allow,
        "no self-exclusion without a policy source file"
    );
}

#[test]
fn policy_file_deny_is_injected_before_the_env_floor() {
    // The secret-file floor is recognized POSITIONALLY as the LAST fs entries
    // (`compiler::defaults::env_deny_floor_start`): the LEAF band (the secret FILE globs)
    // then the `.env*` SUBTREE band. The policy-file deny must land BEFORE that floor so
    // the invariant survives self-exclusion.
    //
    // Restated here rather than derived because the source of truth
    // (`compiler::defaults::ENV_DENY_{LEAF,SUBTREE}_GLOBS`) is crate-private and this is
    // an integration test. Adding a glob to the floor means updating this literal — the
    // in-crate consumers derive from those arrays and do not.
    const FLOOR: [&str; 8] = [
        "**/.env*",
        ".env*",
        "**/.npmrc",
        ".npmrc",
        "**/node_modules/npm/npmrc",
        "node_modules/npm/npmrc",
        "**/.env*/**",
        ".env*/**",
    ];
    let proj = common::homes().project;
    let ctx = common::ctx(true, &[]).with_policy_files(vec![proj.join("policy.jsonc")]);
    let p = compile(&json!({ "fs": ["."] }), &ctx).unwrap();
    let entries = &p.fs.rules.entries;
    // Anchor the floor at the policy-file deny, NOT at `len - FLOOR.len()`: a window
    // measured back from the END slides past a PREPENDED floor glob and still equals this
    // literal, so a stale copy would only ever be caught by an APPEND. Pinning "everything
    // after the policy-file deny" fixes the window's start, so length and order are held
    // in both directions.
    let policy_deny = entries
        .iter()
        .position(|r| r.effect == Effect::Deny && r.matcher.as_str().ends_with("policy.jsonc"))
        .expect("policy-file deny sits before the env floor");
    let floor = &entries[policy_deny + 1..];
    let globs: Vec<&str> = floor.iter().map(|r| r.matcher.as_str()).collect();
    assert_eq!(
        globs, FLOOR,
        "the floor is exactly what follows the policy-file deny"
    );
    assert!(
        floor.iter().all(|r| r.effect == Effect::Deny),
        "every floor entry is a deny"
    );
}

// ── env grammar ───────────────────────────────────────────────────────────────

#[test]
fn env_array_allowlist_and_deny_last_match_wins() {
    let ctx = common::ctx(
        true,
        &[
            ("NODE_ENV", "prod"),
            ("VITE_URL", "x"),
            ("API_TOKEN", "secret"),
            ("OTHER", "y"),
        ],
    );
    // allow NODE_ENV + VITE_*, then deny *_TOKEN.
    let p = compile(&json!({ "vars": ["NODE_ENV", "VITE_*", "!*_TOKEN"] }), &ctx).unwrap();
    let c = &p.env.constructed;
    assert_eq!(c.get("NODE_ENV").map(String::as_str), Some("prod"));
    assert_eq!(c.get("VITE_URL").map(String::as_str), Some("x"));
    assert!(!c.contains_key("API_TOKEN"), "denied");
    assert!(
        !c.contains_key("OTHER"),
        "not allowlisted → excluded (default-deny)"
    );
    assert!(p.env.withheld.contains(&"OTHER".to_string()));
}

#[test]
fn env_array_is_an_allowlist_not_required() {
    // Array exact keys are pass-through-if-present, NEVER required — the canonical
    // `["FOO", "BAR", "!*_TOKEN"]` must compile even when FOO/BAR are unset.
    let absent = common::ctx(true, &[("BAR", "b")]);
    let p = compile(&json!({ "vars": ["FOO", "BAR", "!*_TOKEN"] }), &absent).unwrap();
    assert!(!p.env.constructed.contains_key("FOO"), "absent FOO omitted");
    assert_eq!(p.env.constructed.get("BAR").map(String::as_str), Some("b"));

    // Object plain-keys, by contrast, stay REQUIRED (fail on missing).
    let err = compile(&json!({ "vars": { "FOO": true } }), &absent).unwrap_err();
    assert!(matches!(err, CompileError::MissingRequired { .. }));
}

#[test]
fn env_user_key_case_mirrors_os() {
    // D16: a user env key mirrors the OS. On POSIX env names are case-sensitive, so
    // an explicit `!vite_url` does NOT deny `VITE_URL` (it survives). On Windows env
    // names are one var regardless of case, so `!vite_url` DOES catch `VITE_URL`
    // (it is withheld). Same source, opposite verdict — the enforcement follows the
    // OS resource. (The Windows branch is exercised on the Windows VM / CI.)
    let ctx = common::ctx(true, &[("VITE_URL", "keep")]);
    let p = compile(&json!({ "vars": ["VITE_*", "!vite_url"] }), &ctx).unwrap();
    let got = p.env.constructed.get("VITE_URL").map(String::as_str);
    if cfg!(windows) {
        assert_eq!(
            got, None,
            "Windows: case-insensitive `!vite_url` denies VITE_URL"
        );
    } else {
        assert_eq!(
            got,
            Some("keep"),
            "POSIX: case-sensitive `!vite_url` spares VITE_URL"
        );
    }
}

#[test]
fn env_user_exact_key_case_mirrors_os() {
    // D16 for the EXACT-key form (not only globs): a `path` allow catches ambient
    // `PATH` only on Windows; on POSIX they are distinct vars.
    let ctx = common::ctx(true, &[("PATH", "/bin")]);
    let p = compile(&json!({ "vars": ["path"] }), &ctx).unwrap();
    assert_eq!(
        p.env.constructed.contains_key("PATH"),
        cfg!(windows),
        "exact user key mirrors OS case"
    );
}

#[test]
fn env_required_key_satisfied_case_mirrored() {
    // D16 for the REQUIRED-key check: a required `PATH` is satisfied by an ambient
    // `Path` on Windows (constructed is keyed by the source casing, so the check
    // must compare case-mirrored, not exact) — but errors on POSIX where the
    // casings are distinct vars.
    let ctx = common::ctx(true, &[("Path", "/bin")]);
    let r = compile(&json!({ "vars": { "PATH": true } }), &ctx);
    if cfg!(windows) {
        assert!(
            r.unwrap().env.constructed.contains_key("Path"),
            "Windows: ambient Path satisfies required PATH"
        );
    } else {
        assert!(
            matches!(r.unwrap_err(), CompileError::MissingRequired { .. }),
            "POSIX: Path != PATH, required PATH is missing"
        );
    }
}

#[test]
fn fs_deny_access_is_normalized_to_one_value() {
    // D20: a deny's access is inert (a deny removes read+write), so every deny rule
    // carries `FsAccess::DENY` regardless of surface form. Without normalization the
    // array `!x` deny would emit ReadWrite (the array's allow access) and the object
    // `x: false` deny Read — divergent IR for identical enforcement.
    use nub_sandbox::policy::FsAccess;
    let ctx = common::ctx(true, &[]);
    let obj = compile(&json!({ "fs": { "/a": "rw", "/b": false } }), &ctx).unwrap();
    let arr = compile(&json!({ "fs": ["/a", "!/b"] }), &ctx).unwrap();
    for set in [&obj.fs.rules, &arr.fs.rules] {
        for rule in &set.entries {
            if rule.effect == Effect::Deny {
                assert_eq!(
                    rule.access,
                    FsAccess::DENY,
                    "deny access must be normalized"
                );
            }
        }
    }
    // The array `!/b` deny specifically must be Read, not the array-default ReadWrite.
    let arr_deny = arr
        .fs
        .rules
        .entries
        .iter()
        .find(|r| r.effect == Effect::Deny)
        .expect("array deny present");
    assert_eq!(arr_deny.access, FsAccess::DENY);
    // An allow's access is untouched by the normalization.
    assert!(
        obj.fs
            .rules
            .entries
            .iter()
            .any(|r| r.effect == Effect::Allow && r.access == FsAccess::ReadWrite),
        "an allow's ReadWrite is preserved"
    );
}

#[test]
fn env_object_types_validate() {
    let ctx = common::ctx(true, &[("PORT", "8080"), ("COUNT", "12")]);
    let p = compile(
        &json!({ "vars": { "PORT": "port", "COUNT": "integer" } }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        p.env.constructed.get("PORT").map(String::as_str),
        Some("8080")
    );
    assert!(
        p.env
            .schema
            .iter()
            .any(|r| r.key == "PORT" && r.format == Some(EnvFormat::Port))
    );

    let bad = common::ctx(true, &[("PORT", "notaport")]);
    let err = compile(&json!({ "vars": { "PORT": "port" } }), &bad).unwrap_err();
    assert!(matches!(err, CompileError::Validation { .. }));
}

#[test]
fn vars_object_form_mirrors_the_array_form_and_forbids_broker() {
    // `vars` shares `parse_env_surface` with `secrets`: the object form is identical
    // except `brokerTo` is secrets-only. Object values carry the per-var access — `true`
    // passes the ambient value (like an array entry), `false` strips it (like `!key`).
    let ctx = common::ctx(
        true,
        &[("NODE_ENV", "prod"), ("PATH", "/bin"), ("EXTRA", "x")],
    );
    let array = compile(&json!({ "vars": ["NODE_ENV", "PATH"] }), &ctx).unwrap();
    let object = compile(
        &json!({ "vars": { "NODE_ENV": true, "PATH": true, "EXTRA": false } }),
        &ctx,
    )
    .unwrap();
    // Same resolved env either way: the two named vars pass, EXTRA is withheld.
    assert_eq!(object.env.constructed, array.env.constructed);
    assert_eq!(
        object.env.constructed.get("NODE_ENV").map(String::as_str),
        Some("prod")
    );
    assert_eq!(
        object.env.constructed.get("PATH").map(String::as_str),
        Some("/bin")
    );
    assert!(
        !object.env.constructed.contains_key("EXTRA"),
        "`false` strips the var"
    );
    // `vars` entries are never sensitive (that is the axis distinction, not a per-var knob).
    assert!(object.env.sensitive_keys.is_empty(), "vars are non-secret");

    // `brokerTo` is a secrets-only capability — rejected on a `vars` entry even in a
    // trusted scope (so the rejection is the axis check, not the credential-broker gate).
    match compile(
        &json!({ "vars": { "API_KEY": { "brokerTo": ["api.example.com"] } } }),
        &ctx,
    )
    .unwrap_err()
    {
        CompileError::Shape { message, .. } => assert!(
            message.contains("secrets"),
            "brokerTo on vars names the secrets axis: {message}"
        ),
        other => panic!("expected a Shape error rejecting brokerTo on vars, got {other:?}"),
    }
}

#[test]
fn env_number_rejects_non_finite() {
    // `number` means a finite numeric string — `inf`/`nan` are not values.
    let ok = common::ctx(true, &[("RATIO", "1.5")]);
    assert!(compile(&json!({ "vars": { "RATIO": "number" } }), &ok).is_ok());
    for bad in ["inf", "nan", "infinity"] {
        let ctx = common::ctx(true, &[("RATIO", bad)]);
        assert!(
            matches!(
                compile(&json!({ "vars": { "RATIO": "number" } }), &ctx),
                Err(CompileError::Validation { .. })
            ),
            "`{bad}` must be rejected as a number"
        );
    }
}

#[test]
fn env_regex_and_enum_union() {
    let ctx = common::ctx(true, &[("MODE", "dev"), ("SHA", "abc123")]);
    let p = compile(
        &json!({ "vars": { "MODE": "enum:dev|prod", "SHA": "/^[a-f0-9]+$/" } }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        p.env.constructed.get("MODE").map(String::as_str),
        Some("dev")
    );
    assert_eq!(
        p.env.constructed.get("SHA").map(String::as_str),
        Some("abc123")
    );

    let bad = common::ctx(true, &[("MODE", "staging")]);
    assert!(compile(&json!({ "vars": { "MODE": "enum:dev|prod" } }), &bad).is_err());
    // An empty `enum:` member is a shape error, not a silently-accepted type.
    let empty = common::ctx(true, &[("MODE", "dev")]);
    assert!(compile(&json!({ "vars": { "MODE": "enum:dev||prod" } }), &empty).is_err());
}

#[test]
fn env_regex_is_checked_even_when_optional_and_unmatched() {
    let ctx = common::ctx(true, &[]);
    let err = compile(&json!({ "vars": { "SHA?": "/[a-/" } }), &ctx).unwrap_err();
    match err {
        CompileError::Shape { path, message } => {
            assert_eq!(path, "vars.SHA?");
            assert!(message.contains("invalid regex"), "{message}");
        }
        other => panic!("expected invalid regex shape error, got {other:?}"),
    }
}

#[test]
fn env_unknown_type_names_the_supported_set() {
    let ctx = common::ctx(true, &[("X", "1")]);
    let err = compile(&json!({ "vars": { "X": "email" } }), &ctx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("integer") && msg.contains("port"),
        "names the closed set: {msg}"
    );
}

#[test]
fn env_required_missing_key_errors_optional_is_ok() {
    let ctx = common::ctx(true, &[]);
    // required (no `?`) and absent → error.
    let err = compile(&json!({ "vars": { "DATABASE_URL": true } }), &ctx).unwrap_err();
    assert!(matches!(err, CompileError::MissingRequired { .. }));
    // optional (`?`) and absent → fine.
    assert!(compile(&json!({ "vars": { "DATABASE_URL?": true } }), &ctx).is_ok());
}

#[test]
fn sensitivity_is_set_by_the_axis_not_a_key() {
    // Phase 1: sensitivity is no longer a per-entry `sensitive` extras key — the AXIS
    // decides. Every `vars` entry is public (`sensitive:false`); every `secrets` entry
    // is redacted (`sensitive:true`), across the array and object (incl. format) forms.
    let ctx = common::ctx(
        true,
        &[("PUB", "1"), ("FMT", "2"), ("TOK", "3"), ("DB", "4")],
    );
    let p = compile(
        &json!({
            "vars": { "PUB": true, "FMT": { "format": "string" } },
            "secrets": { "TOK": true, "DB": { "format": "string" } },
        }),
        &ctx,
    )
    .unwrap();
    let rule = |k: &str| p.env.schema.iter().find(|r| r.key == k).unwrap();
    assert!(!rule("PUB").sensitive, "a vars entry is public");
    assert!(
        !rule("FMT").sensitive,
        "a vars entry with a format is still public"
    );
    assert!(rule("TOK").sensitive, "a secrets entry is redacted");
    assert!(
        rule("DB").sensitive,
        "a secrets entry with a format is still redacted"
    );
    // The array form marks sensitivity the same way.
    let arr = compile(&json!({ "vars": ["PUB"], "secrets": ["TOK"] }), &ctx).unwrap();
    let ar = |k: &str| arr.env.schema.iter().find(|r| r.key == k).unwrap();
    assert!(!ar("PUB").sensitive, "array vars entry is public");
    assert!(ar("TOK").sensitive, "array secrets entry is redacted");
}

#[test]
fn env_extras_validate_optional_and_format() {
    // `optional` must be a boolean; `format` must be a string. (`sensitive` is no
    // longer an extras key — see env_extras_reject_removed_sensitivity_keys.)
    let ctx = common::ctx(true, &[("X", "1")]);
    let opt_err = compile(&json!({ "vars": { "X": { "optional": 1 } } }), &ctx).unwrap_err();
    match opt_err {
        CompileError::Shape { path, message } => {
            assert_eq!(path, "vars.X.optional");
            assert_eq!(message, "optional must be a boolean");
        }
        other => panic!("expected optional shape error, got {other:?}"),
    }
    let fmt_err = compile(&json!({ "vars": { "X": { "format": 1 } } }), &ctx).unwrap_err();
    match fmt_err {
        CompileError::Shape { path, message } => {
            assert_eq!(path, "vars.X.format");
            assert_eq!(message, "format must be a string");
        }
        other => panic!("expected format shape error, got {other:?}"),
    }
}

#[test]
fn env_extras_reject_removed_sensitivity_keys() {
    // `sensitive` (moved to the axis) and the older `secret`/`public` pair are no
    // longer extras keys — each is an unknown-option shape error.
    let ctx = common::ctx(true, &[("X", "1")]);
    for key in ["sensitive", "secret", "public"] {
        let err = compile(&json!({ "vars": { "X": { key: true } } }), &ctx).unwrap_err();
        match err {
            CompileError::Shape { message, .. } => {
                assert!(
                    message.contains(key) && message.contains("unknown env option"),
                    "{message}"
                );
            }
            other => panic!("expected an unknown-option shape error naming `{key}`, got {other:?}"),
        }
    }
}

#[test]
fn env_extras_reject_literal_value_injection() {
    // A sandbox controls WHICH env vars pass, not what they are set to — literal
    // `value:` injection was removed, so `{ "value": … }` is now an unknown option
    // on both axes, like any other unrecognized key.
    let ctx = common::ctx(true, &[("NAME", "1")]);
    for axis in ["vars", "secrets"] {
        let err = compile(&json!({ axis: { "NAME": { "value": "x" } } }), &ctx).unwrap_err();
        match err {
            CompileError::Shape { path, message } => {
                assert_eq!(path, format!("{axis}.NAME.value"));
                assert!(
                    message.contains("value") && message.contains("unknown env option"),
                    "{message}"
                );
            }
            other => panic!("expected an unknown-option shape error naming `value`, got {other:?}"),
        }
    }
}

#[test]
fn vars_and_secrets_construct_the_child_env_together() {
    // Both axes are the same env mechanism: each named key reaches the child with its
    // real value; only the schema's `sensitive` mark (redaction) differs by axis.
    let ctx = common::ctx(true, &[("FOO", "bar"), ("DB_URL", "postgres://s")]);
    let p = compile(&json!({ "vars": ["FOO"], "secrets": ["DB_URL"] }), &ctx).unwrap();
    assert_eq!(
        p.env.constructed.get("FOO").map(String::as_str),
        Some("bar")
    );
    assert_eq!(
        p.env.constructed.get("DB_URL").map(String::as_str),
        Some("postgres://s"),
        "a secret reaches the child with its real value"
    );
    let rule = |k: &str| p.env.schema.iter().find(|r| r.key == k).unwrap();
    assert!(!rule("FOO").sensitive, "a var is public");
    assert!(rule("DB_URL").sensitive, "a secret is redacted");
}

#[test]
fn vars_secrets_name_collision_takes_the_secrets_rule() {
    // vars entries precede secrets in the single last-match-wins list, so a name in
    // BOTH axes records the later secrets rule (`sensitive:true`) — fail-safe toward
    // redaction. A sibling matched only by the `vars: ["*"]` catch-all stays public.
    let ctx = common::ctx(true, &[("FOO", "1"), ("PLAIN", "2")]);
    let p = compile(&json!({ "vars": ["*"], "secrets": ["FOO"] }), &ctx).unwrap();
    assert_eq!(p.env.constructed.get("FOO").map(String::as_str), Some("1"));
    assert_eq!(
        p.env.constructed.get("PLAIN").map(String::as_str),
        Some("2")
    );
    let foo = p.env.schema.iter().find(|r| r.key == "FOO").unwrap();
    assert!(foo.sensitive, "the colliding name takes the secrets rule");
    let star = p.env.schema.iter().find(|r| r.key == "*").unwrap();
    assert!(!star.sensitive, "the vars catch-all stays public");
}

#[test]
fn vars_star_passes_all_and_emits_a_schema_rule() {
    // `vars: "*"` (and back-compat `vars: true`) pass every ambient variable. Unlike
    // the old `env: true` short-circuit, both now emit a real `"*"` Allow schema rule.
    let ctx = common::ctx(true, &[("A", "1"), ("MY_TOKEN", "t")]);
    for surface in [json!({ "vars": "*" }), json!({ "vars": true })] {
        let p = compile(&surface, &ctx).unwrap();
        assert!(p.env.constructed.contains_key("A"), "{surface} passes A");
        assert!(
            p.env.constructed.contains_key("MY_TOKEN"),
            "{surface} passes everything, secrets included"
        );
        assert!(
            p.env.schema.iter().any(|r| r.key == "*" && !r.sensitive),
            "{surface} emits a public `\"*\"` schema rule"
        );
    }
}

#[test]
fn secrets_rejects_catch_all_and_string_shapes() {
    // `secrets` must NAME each secret: a catch-all `"*"`/`true` or any string is a
    // shape error. `vars` accepts only `"*"` as a string (a non-`"*"` string errors).
    let ctx = common::ctx(true, &[("X", "1")]);
    for bad in [
        json!({ "secrets": "*" }),
        json!({ "secrets": true }),
        json!({ "secrets": "DB_URL" }),
        json!({ "vars": "everything" }),
    ] {
        assert!(
            matches!(compile(&bad, &ctx), Err(CompileError::Shape { .. })),
            "{bad} must be a shape error, got {:?}",
            compile(&bad, &ctx)
        );
    }
    // `secrets: false` / `[]` are accepted (explicit no-secrets).
    assert!(compile(&json!({ "secrets": false }), &ctx).is_ok());
    assert!(compile(&json!({ "vars": ["X"], "secrets": [] }), &ctx).is_ok());
}

#[test]
fn secrets_brokerto_transposes_onto_the_net_broker_and_engages_tls_inspect() {
    // `secrets.<name>.brokerTo` is the Phase-2 broker surface: it transposes onto
    // net.brokers (one broker per host, env = the secret), withholds the real value,
    // and derives the TlsInspect tier — provided the host is also named in `net`.
    let ctx = common::ctx(true, &[("GITHUB_TOKEN", "real-token")]);
    let p = compile(
        &json!({
            "net": ["api.github.com"],
            "secrets": { "GITHUB_TOKEN": { "brokerTo": ["api.github.com"] } }
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(p.net.brokers.len(), 1);
    assert_eq!(p.net.brokers[0].host, "api.github.com");
    assert_eq!(p.net.brokers[0].env, vec!["GITHUB_TOKEN"]);
    assert!(
        !p.env.constructed.contains_key("GITHUB_TOKEN"),
        "the brokered secret's real value is withheld from the child"
    );
    assert!(matches!(p.net.inspection, Inspection::TlsInspect));
    let serialized = serde_json::to_string(&p).unwrap();
    assert!(serialized.contains("GITHUB_TOKEN") && !serialized.contains("real-token"));
}

#[test]
fn env_integer_format_leniency_is_the_rust_i64_parse() {
    // D19: the `integer` format validates via Rust's i64 parse — it ACCEPTS a leading
    // sign and leading zeros (`+5`, `007`) but REJECTS a radix prefix (`0x10`), a
    // non-integer shape (`5.0`, `1_000`), and an out-of-i64-range value. Pin the exact
    // leniency so a future validator swap can't silently widen or narrow it.
    for good in ["5", "+5", "007", "-42", "0"] {
        let ctx = common::ctx(true, &[("N", good)]);
        assert!(
            compile(&json!({ "vars": { "N": "integer" } }), &ctx).is_ok(),
            "`{good}` must validate as an integer"
        );
    }
    for bad in ["0x10", "99999999999999999999999999", "5.0", "1_000"] {
        let ctx = common::ctx(true, &[("N", bad)]);
        assert!(
            matches!(
                compile(&json!({ "vars": { "N": "integer" } }), &ctx),
                Err(CompileError::Validation { .. })
            ),
            "`{bad}` must be rejected as an integer"
        );
    }
}

#[test]
fn env_empty_array_is_strip_all_not_passthrough() {
    // D13: `env: []` is an allowlist with ZERO allow entries → deny base, no allow =
    // strip-all. The mental-model trap is reading an empty list as "no restrictions";
    // it is the opposite. Enforcing, every ambient var withheld, none constructed.
    let ctx = common::ctx(true, &[("FOO", "1"), ("BAR", "2"), ("BAZ", "3")]);
    let p = compile(&json!({ "vars": [] }), &ctx).unwrap();
    assert!(p.env.enforce, "an explicit env axis always enforces");
    for k in ["FOO", "BAR", "BAZ"] {
        assert!(!p.env.constructed.contains_key(k), "{k} must be stripped");
        assert!(
            p.env.withheld.contains(&k.to_string()),
            "{k} must be recorded withheld"
        );
    }
}

#[cfg(windows)]
#[test]
fn constrained_windows_env_forms_keep_startup_essentials() {
    // Every constraining surface constructs the child environment. The Windows
    // loader/AppContainer startup variables are mechanism requirements, so they
    // survive an allowlist or deny-all just as they do `env: false`.
    let ctx = common::ctx(
        true,
        &[
            ("SystemRoot", "C:/Windows"),
            ("SystemDrive", "C:"),
            ("TEMP", "C:/Temp"),
            ("TMP", "C:/Tmp"),
            ("LOCALAPPDATA", "C:/Users/me/AppData/Local"),
            ("ONLY", "allowed"),
            ("SECRET_TOKEN", "withheld"),
        ],
    );
    for surface in [
        json!({ "vars": false }),
        json!({ "vars": [] }),
        json!({ "vars": ["ONLY"] }),
        json!({ "vars": { "ONLY": true } }),
    ] {
        let policy = compile(&surface, &ctx).unwrap();
        for key in ["SystemRoot", "SystemDrive", "TEMP", "TMP", "LOCALAPPDATA"] {
            assert!(
                policy
                    .env
                    .constructed
                    .keys()
                    .any(|actual| actual.eq_ignore_ascii_case(key)),
                "{surface} must retain {key}"
            );
        }
        assert!(
            !policy.env.constructed.contains_key("SECRET_TOKEN"),
            "{surface} must not re-admit unrelated ambient env"
        );
    }
}

#[test]
fn env_lone_deny_strips_everything_not_all_but_x() {
    // D14: a lone `["!X"]` is the other trap — it reads like "allow all EXCEPT X", but
    // an array is an allowlist and there is no allow base, so it strips EVERYTHING (X and
    // every other var alike). To keep sibling vars you must allow them explicitly first.
    let ctx = common::ctx(true, &[("X", "1"), ("Y", "2"), ("Z", "3")]);
    let p = compile(&json!({ "vars": ["!X"] }), &ctx).unwrap();
    assert!(p.env.enforce);
    for k in ["X", "Y", "Z"] {
        assert!(
            !p.env.constructed.contains_key(k),
            "{k} stripped — no allow base means `!X` does not spare Y/Z"
        );
        assert!(p.env.withheld.contains(&k.to_string()), "{k} withheld");
    }
}

// ── $(…) substitution + trust gate ────────────────────────────────────────────

#[test]
fn substitution_resolves_in_trusted_home() {
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "vars": { "GREETING": "$(echo hi)" } }), &ctx).unwrap();
    assert_eq!(
        p.env.constructed.get("GREETING").map(String::as_str),
        Some("hi")
    );
}

#[test]
fn substitution_embedded_in_a_larger_value() {
    let ctx = common::ctx(true, &[]);
    let p = compile(
        &json!({ "vars": { "URL": "https://$(echo hi)/path" } }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        p.env.constructed.get("URL").map(String::as_str),
        Some("https://hi/path")
    );
}

#[test]
fn substitution_forbidden_in_untrusted_home() {
    let ctx = common::ctx(false, &[]);
    let err = compile(&json!({ "vars": { "X": "$(echo hi)" } }), &ctx).unwrap_err();
    assert!(matches!(err, CompileError::UntrustedSubstitution { .. }));
}

#[test]
fn substitution_failure_surfaces() {
    let ctx = common::ctx(true, &[]);
    let err = compile(&json!({ "vars": { "X": "$(fail)" } }), &ctx).unwrap_err();
    assert!(matches!(err, CompileError::Substitution { .. }));
}

#[test]
fn unterminated_substitution_is_named_not_unknown_type() {
    // D18: a `$(` with no balanced close is a substitution-shaped error at the string
    // value position, whether bare or embedded in a larger value — never a silent
    // literal or a confusing "unknown env type". The runner must NOT fire (nothing to run).
    struct PanicRunner;
    impl nub_sandbox::CommandRunner for PanicRunner {
        fn run(&self, _: &str) -> Result<String, String> {
            panic!("an unterminated `$(` must not reach the runner");
        }
    }
    let ctx = nub_sandbox::compiler::CompileCtx {
        homes: common::homes(),
        cwd: common::homes().project,
        policy_files: Vec::new(),
        caps: nub_sandbox::compiler::ScopeCapabilities::approved(),
        ambient_env: std::collections::BTreeMap::new(),
        document: serde_json::Value::Null,
        interpreter: Vec::new(),
        runner: Box::new(PanicRunner),
    };
    for surface in [
        json!({ "vars": { "X": "$(op read" } }),
        json!({ "vars": { "X": "postgres://$(op read@h" } }),
        // The command text carries a single quote — must NOT fall through to a
        // union-parse / "unknown env type" error (the coarse-guard gap).
        json!({ "vars": { "X": "$(op read 'op://vault/db/pw'" } }),
        // A leading `/` must NOT be mistaken for a regex and skip the check.
        json!({ "vars": { "X": "/$(cmd" } }),
    ] {
        match compile(&surface, &ctx).unwrap_err() {
            CompileError::Substitution { message, .. } => {
                assert!(
                    message.contains("$(") && message.contains("closing"),
                    "{message}"
                );
            }
            other => panic!("expected a substitution-shaped error for {surface}, got {other:?}"),
        }
    }
}

#[test]
fn mixed_balanced_then_unterminated_substitution_errors() {
    // D18: a value with a balanced span THEN an unterminated `$(` must not ship the
    // unterminated tail as a silent literal. The balanced span DOES run (so a real
    // runner, not a panic-runner), then the residual opener is rejected.
    let ctx = common::ctx(true, &[]);
    for surface in [
        json!({ "vars": { "X": "$(echo hi) $(oops" } }),
        json!({ "vars": { "X": "$(echo hi)$(oops" } }),
    ] {
        match compile(&surface, &ctx).unwrap_err() {
            CompileError::Substitution { message, .. } => {
                assert!(message.contains("closing"), "{message}");
            }
            other => panic!("expected a substitution-shaped error for {surface}, got {other:?}"),
        }
    }
}

#[test]
fn glob_object_key_reports_optional_in_schema() {
    // D9: a glob object key is inherently optional (matches however many keys, zero
    // included) — it reports optional in the schema even without a trailing `?`, and
    // never triggers the required-var check when it matches nothing.
    let ctx = common::ctx(true, &[("VITE_URL", "x")]);
    let p = compile(&json!({ "vars": { "VITE_*": true } }), &ctx).unwrap();
    let rule = p.env.schema.iter().find(|r| r.key == "VITE_*").unwrap();
    assert!(rule.optional, "a glob key is optional in the schema");
    // A glob matching nothing does not error (contrast a required exact key).
    assert!(compile(&json!({ "vars": { "NOPE_*": true } }), &ctx).is_ok());
}

#[test]
fn glob_key_substitution_is_rejected_before_running() {
    // A `$(…)` on a glob key has no single key to bind to → rejected at parse,
    // BEFORE the command runs (the runner panics if reached).
    struct PanicRunner;
    impl nub_sandbox::CommandRunner for PanicRunner {
        fn run(&self, _: &str) -> Result<String, String> {
            panic!("a glob-key `$(…)` must be rejected before it executes");
        }
    }
    let ctx = nub_sandbox::compiler::CompileCtx {
        homes: common::homes(),
        cwd: common::homes().project,
        policy_files: Vec::new(),
        caps: nub_sandbox::compiler::ScopeCapabilities::approved(),
        ambient_env: std::collections::BTreeMap::new(),
        document: serde_json::Value::Null,
        interpreter: Vec::new(),
        runner: Box::new(PanicRunner),
    };
    let surface = json!({ "vars": { "FOO_*": "$(echo hi)" } });
    assert!(matches!(
        compile(&surface, &ctx).unwrap_err(),
        CompileError::Shape { .. }
    ));
}

// ── net fold ──────────────────────────────────────────────────────────────────

#[test]
fn net_array_hosts_and_cidr_classify() {
    let ctx = common::ctx(true, &[]);
    let p = compile(
        &json!({ "net": ["*.sentry.io", "10.0.0.0/8", "!evil.com"] }),
        &ctx,
    )
    .unwrap();
    assert!(p.net.enforce);
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(m.admits("in.sentry.io"));
    assert!(m.admits("10.2.3.4"));
    assert!(!m.admits("evil.com"));
    assert!(!m.admits("unlisted.com"));
}

#[test]
fn net_bad_cidr_is_a_shape_error_at_its_path() {
    let ctx = common::ctx(true, &[]);
    let err = compile(&json!({ "net": ["10.0.0.0/999"] }), &ctx).unwrap_err();
    match err {
        CompileError::Shape { path, .. } => assert_eq!(path, "net.0", "error points at the entry"),
        other => panic!("expected Shape, got {other:?}"),
    }
}

#[test]
fn net_per_host_object_option_is_rejected_for_now() {
    let ctx = common::ctx(true, &[]);
    let err = compile(&json!({ "net": { "*.x.com": { "port": 443 } } }), &ctx).unwrap_err();
    assert!(matches!(err, CompileError::Shape { .. }));
}

#[test]
fn coarse_net_never_enables_a_proxy() {
    use nub_sandbox::policy::ProxyMode;

    let ctx = common::ctx(true, &[]);
    let unrestricted = compile(&json!({ "net": true }), &ctx).unwrap();
    assert!(!unrestricted.net.enforce);
    assert_eq!(unrestricted.net.mode, ProxyMode::Disabled);

    let denied = compile(&json!({ "net": false }), &ctx).unwrap();
    assert!(denied.net.enforce && denied.net.rules.is_empty());
    assert_eq!(denied.net.mode, ProxyMode::Disabled);

    let err = compile(&json!({ "net": false, "proxy": "auto" }), &ctx).unwrap_err();
    assert!(matches!(err, CompileError::Shape { path, .. } if path == "proxy"));
}

#[test]
fn net_mid_host_glob_is_a_shape_error_at_its_path() {
    // D11: a `*` outside the leading `*.` position is ambiguous — it would match
    // nothing at runtime, so it fails loud at compile time.
    let ctx = common::ctx(true, &[]);
    for (cfg, want_path) in [
        (json!({ "net": ["api.*.com"] }), "net.0"),
        (json!({ "net": ["ok.example", "foo*bar.com"] }), "net.1"),
        (json!({ "net": { "api.*.com": true } }), "net.api.*.com"),
        // Degenerate empty-apex wildcard: must fail loud, NOT strip down to a
        // bare `*` allow-all (fail-open in a security primitive).
        (json!({ "net": ["*."] }), "net.0"),
        (json!({ "net": ["*.."] }), "net.0"),
    ] {
        match compile(&cfg, &ctx).unwrap_err() {
            CompileError::Shape { path, message } => {
                assert_eq!(path, want_path, "error points at the offending entry");
                assert!(
                    message.contains("host pattern"),
                    "names the problem: {message}"
                );
            }
            other => panic!("expected Shape for {cfg}, got {other:?}"),
        }
    }
}

#[test]
fn net_host_brace_alternation_is_a_shape_error() {
    // Braces are NOT part of the host grammar (only `*` / `*.suffix`) — a `{a,b}` host
    // would be a literal that matches nothing, so a `!{evil,bad}.com` deny would be
    // inert. Fail loud, same class as the mid-host glob. (fs globs DO support braces.)
    let ctx = common::ctx(true, &[]);
    for (cfg, want_path) in [
        (json!({ "net": ["{a,b}.com"] }), "net.0"),
        (json!({ "net": ["ok.example", "!{evil,bad}.com"] }), "net.1"),
        (
            json!({ "net": { "api.{a,b}.com": true } }),
            "net.api.{a,b}.com",
        ),
    ] {
        match compile(&cfg, &ctx).unwrap_err() {
            CompileError::Shape { path, message } => {
                assert_eq!(path, want_path, "error points at the offending entry");
                assert!(message.contains("brace"), "names the problem: {message}");
            }
            other => panic!("expected Shape for {cfg}, got {other:?}"),
        }
    }
}

#[test]
fn env_key_brace_alternation_is_a_shape_error() {
    // Env-var-NAME patterns are a narrower grammar than fs globs — a `{`/`}` is
    // rejected the same class as a mid-host glob (list the keys, or use `*`).
    let ctx = common::ctx(true, &[("FOO_A", "1"), ("FOO_B", "2")]);
    for (cfg, want_path) in [
        (json!({ "vars": ["FOO_{A,B}"] }), "vars.0"),
        (json!({ "vars": ["OK", "!SECRET_{X,Y}"] }), "vars.1"),
        (json!({ "vars": { "FOO_{A,B}": true } }), "vars.FOO_{A,B}"),
    ] {
        match compile(&cfg, &ctx).unwrap_err() {
            CompileError::Shape { path, message } => {
                assert_eq!(path, want_path, "error points at the offending entry");
                assert!(message.contains("brace"), "names the problem: {message}");
            }
            other => panic!("expected Shape for {cfg}, got {other:?}"),
        }
    }
}

#[test]
fn net_private_symbolic_target_folds_and_gates_the_opt_in() {
    use nub_sandbox::matcher::host::net_allows_private;
    let ctx = common::ctx(true, &[]);

    // `<private>` (and its `<local>` alias) fold to NetTarget::Private and set the opt-in.
    for tok in ["<private>", "<local>"] {
        let p = compile(&json!({ "net": [tok] }), &ctx).unwrap();
        assert_eq!(p.net.rules.len(), 1);
        assert!(matches!(p.net.rules[0].target, NetTarget::Private), "{tok}");
        assert!(
            net_allows_private(&p.net),
            "{tok} must set the private opt-in"
        );
    }

    // A bare `*` allow-all does NOT set the opt-in — the private ranges stay blocked.
    let p = compile(&json!({ "net": ["*"] }), &ctx).unwrap();
    assert!(
        !net_allows_private(&p.net),
        "`*` must NOT re-open the private ranges"
    );

    // `!<private>` after an opt-in reclose it (last-match-wins on the token).
    let p = compile(&json!({ "net": ["<private>", "!<private>"] }), &ctx).unwrap();
    assert!(
        !net_allows_private(&p.net),
        "a later `!<private>` must reclose the opt-in"
    );

    // An unknown angle-bracket token is a loud shape error, not a silent no-match host.
    match compile(&json!({ "net": ["<privat>"] }), &ctx).unwrap_err() {
        CompileError::Shape { path, message } => {
            assert_eq!(path, "net.0");
            assert!(message.contains("recognized net target"), "{message}");
        }
        other => panic!("expected Shape, got {other:?}"),
    }
}

#[test]
fn net_leading_wildcard_and_bare_star_still_accepted() {
    // D11 must not over-reject: the two valid wildcard forms compile.
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "net": ["*.example.com", "*"] }), &ctx).unwrap();
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(m.admits("a.b.example.com"));
    assert!(m.admits("anything.at.all"));
}

#[test]
fn net_trailing_dot_is_stripped_so_it_cannot_dodge_a_deny() {
    // D12: `evil.com.` in config normalizes to `evil.com`, and a connect to the
    // dotted form matches a dotless rule.
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "net": ["ok.example.", "!evil.com."] }), &ctx).unwrap();
    match &p.net.rules[0].target {
        nub_sandbox::policy::NetTarget::Host(h) => {
            assert_eq!(h, "ok.example", "dot stripped in IR")
        }
        other => panic!("expected Host, got {other:?}"),
    }
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(m.admits("ok.example."));
    assert!(m.admits("ok.example"));
    assert!(!m.admits("evil.com."), "trailing-dot deny still bites");
    assert!(!m.admits("evil.com"));
}

// ── credential brokering (the capability-derived MITM tier) ───────────────────

#[test]
fn broker_compiles_to_a_broker_and_engages_tls_inspect() {
    let ctx = common::ctx(true, &[("STRIPE_TOKEN", "real-parent-secret")]);
    let p = compile(
        &json!({
            "net": ["api.stripe.com"],
            "secrets": { "STRIPE_TOKEN": { "brokerTo": ["api.stripe.com"] } }
        }),
        &ctx,
    )
    .unwrap();
    // The brokered host is allowed by the net axis (brokering does NOT open it itself).
    assert!(
        p.net.rules.iter().any(
            |r| matches!(&r.target, NetTarget::Host(h) if h == "api.stripe.com")
                && matches!(r.effect, Effect::Allow)
        ),
        "brokered host must be allowed in net"
    );
    assert_eq!(p.net.brokers.len(), 1);
    assert_eq!(p.net.brokers[0].host, "api.stripe.com");
    assert_eq!(p.net.brokers[0].env, vec!["STRIPE_TOKEN"]);
    assert!(
        !p.env.constructed.contains_key("STRIPE_TOKEN"),
        "the serializable IR must withhold the brokered real value"
    );
    let serialized = serde_json::to_string(&p).unwrap();
    assert!(serialized.contains("STRIPE_TOKEN"));
    assert!(!serialized.contains("real-parent-secret"));
    // The proxy auto-starts (Auto) and derives the TlsInspect tier: credential
    // injection cannot work without a terminating proxy.
    assert!(matches!(p.net.mode, nub_sandbox::policy::ProxyMode::Auto));
    assert!(matches!(p.net.inspection, Inspection::TlsInspect));
}

#[test]
fn two_secrets_to_one_host_coalesce_into_a_single_broker() {
    // The transpose keys brokers by host: two secrets brokered to the same host merge
    // into ONE CredentialBroker with both env names (the shape every consumer relies
    // on — a per-secret push would duplicate the host and drop all but the first).
    let ctx = common::ctx(true, &[("GH_TOKEN", "a"), ("GH_APP_KEY", "b")]);
    let p = compile(
        &json!({
            "net": ["api.github.com"],
            "secrets": {
                "GH_TOKEN": { "brokerTo": ["api.github.com"] },
                "GH_APP_KEY": { "brokerTo": ["api.github.com"] }
            }
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(p.net.brokers.len(), 1, "one broker per host");
    assert_eq!(p.net.brokers[0].host, "api.github.com");
    assert_eq!(p.net.brokers[0].env, vec!["GH_TOKEN", "GH_APP_KEY"]);
}

#[test]
fn fine_grained_allow_auto_starts_the_proxy_at_connection_tier() {
    // A fine-grained net allow with no brokered secret AUTO-STARTS the egress proxy
    // (Auto) at the Connection tier — no authored `proxy`, no TLS termination. This
    // was a compile error before Phase 2 (it demanded an authored proxy).
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "net": ["api.example.com"] }), &ctx).unwrap();
    assert!(p.net.brokers.is_empty());
    assert!(matches!(p.net.mode, nub_sandbox::policy::ProxyMode::Auto));
    assert!(
        matches!(p.net.inspection, Inspection::Connection),
        "a host-only allow must not engage TLS termination"
    );
}

#[test]
fn authored_proxy_is_a_targeted_migration_error() {
    // `proxy` is no longer an authored knob (the tier is derived). A stray `proxy`
    // gets a targeted error pointing that out, not a generic unknown-key one.
    let ctx = common::ctx(true, &[]);
    for surface in [
        json!({ "net": ["api.example.com"], "proxy": "auto" }),
        json!({ "net": ["api.example.com"], "proxy": "terminate" }),
        json!({ "net": true, "proxy": "auto" }),
    ] {
        match compile(&surface, &ctx).unwrap_err() {
            CompileError::Shape { path, message } => {
                assert_eq!(path, "proxy");
                assert!(
                    message.contains("derives") && message.contains("remove"),
                    "names the migration: {message}"
                );
            }
            other => panic!("expected a targeted proxy Shape error for {surface}, got {other:?}"),
        }
    }
}

#[test]
fn brokerto_host_not_in_net_is_a_compile_error() {
    let ctx = common::ctx(true, &[("API_TOKEN", "t")]);
    // The brokered host is not admitted by the net allowlist → hard error naming it.
    let err = compile(
        &json!({
            "net": ["other.example"],
            "secrets": { "API_TOKEN": { "brokerTo": ["api.example.com"] } }
        }),
        &ctx,
    )
    .unwrap_err();
    match err {
        CompileError::Shape { path, message } => {
            assert_eq!(path, "secrets.API_TOKEN.brokerTo");
            assert!(
                message.contains("api.example.com") && message.contains("not allowed"),
                "names the un-admitted host: {message}"
            );
        }
        other => panic!("expected a brokerTo-not-in-net Shape error, got {other:?}"),
    }
}

#[test]
fn brokerto_with_coarse_net_is_a_compile_error() {
    // A broker needs a fine-grained proxy to serve it: coarse `net: true` (admits the
    // host but starts no proxy) and an all-deny `net` both error rather than silently
    // shipping an inert broker.
    let ctx = common::ctx(true, &[("API_TOKEN", "t")]);
    for net in [json!(true), json!(false), json!(["!api.example.com"])] {
        let err = compile(
            &json!({
                "net": net,
                "secrets": { "API_TOKEN": { "brokerTo": ["api.example.com"] } }
            }),
            &ctx,
        )
        .unwrap_err();
        assert!(
            matches!(err, CompileError::Shape { path, .. } if path == "secrets.API_TOKEN.brokerTo"),
            "coarse/all-deny net + brokerTo must error at the brokerTo path (net={net})"
        );
    }
}

#[test]
fn brokerto_requires_an_exact_literal_hostname() {
    let ctx = common::ctx(true, &[("API_TOKEN", "t")]);
    for host in [
        "*",
        "*.example.com",
        "10.0.0.1",
        "10.0.0.0/8",
        "<private>",
        "127.1",
        "2130706433",
        "0x7f000001",
        "0177.0.0.1",
    ] {
        let err = compile(
            &json!({
                "net": ["api.example.com"],
                "secrets": { "API_TOKEN": { "brokerTo": [host] } }
            }),
            &ctx,
        )
        .unwrap_err();
        assert!(
            matches!(err, CompileError::Shape { .. }),
            "non-literal broker host unexpectedly compiled: {host}"
        );
    }
}

#[test]
fn brokerto_is_a_trusted_only_capability() {
    let ctx = common::ctx(false, &[("API_TOKEN", "t")]);
    let err = compile(
        &json!({
            "net": ["api.example.com"],
            "secrets": { "API_TOKEN": { "brokerTo": ["api.example.com"] } }
        }),
        &ctx,
    )
    .unwrap_err();
    match err {
        CompileError::Shape { message, .. } => assert!(
            message.contains("credential brokering"),
            "names the trusted-only capability: {message}"
        ),
        other => panic!("expected the trusted-only Shape error, got {other:?}"),
    }
}

#[test]
fn brokerto_on_a_vars_entry_is_a_shape_error() {
    // Brokering protects a sensitive value; it is a secrets-only knob.
    let ctx = common::ctx(true, &[("API_TOKEN", "t")]);
    let err = compile(
        &json!({
            "net": ["api.example.com"],
            "vars": { "API_TOKEN": { "brokerTo": ["api.example.com"] } }
        }),
        &ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, CompileError::Shape { path, .. } if path == "vars.API_TOKEN.brokerTo"),
        "brokerTo on a vars entry must error at its path"
    );
}

#[test]
fn brokerto_rejects_bad_secret_keys_hosts_and_value_combo() {
    let ctx = common::ctx(true, &[("API_TOKEN", "t")]);
    // Bad secret KEY: glob, optional (`?`), and a reserved plumbing name.
    for secrets in [
        json!({ "*_TOKEN": { "brokerTo": ["api.example.com"] } }),
        json!({ "API_TOKEN?": { "brokerTo": ["api.example.com"] } }),
        json!({ "HTTPS_PROXY": { "brokerTo": ["api.example.com"] } }),
        json!({ "NODE_EXTRA_CA_CERTS": { "brokerTo": ["api.example.com"] } }),
        json!({ "TMPDIR": { "brokerTo": ["api.example.com"] } }),
    ] {
        assert!(
            compile(
                &json!({ "net": ["api.example.com"], "secrets": secrets }),
                &ctx
            )
            .is_err(),
            "bad brokered secret key must be rejected: {secrets}"
        );
    }
    // Bad brokerTo host list: empty, and duplicate hosts.
    for hosts in [json!([]), json!(["api.example.com", "api.example.com"])] {
        assert!(
            compile(
                &json!({
                    "net": ["api.example.com"],
                    "secrets": { "API_TOKEN": { "brokerTo": hosts } }
                }),
                &ctx
            )
            .is_err(),
            "bad brokerTo host list must be rejected: {hosts}"
        );
    }
}

// ── fs $(…) command substitution ───────────────────────────────────────────────

#[test]
fn fs_substitution_resolves_and_grants_whole_and_embedded() {
    // A `$(…)` fs path resolves at load time and flows into the matcher exactly as
    // a literal would: whole-value (object key), embedded in a larger path, and the
    // array form (ReadWrite). The stub `store path` → `<homes().home>/.store`.
    use nub_sandbox::matcher::PathMatcher;
    use nub_sandbox::policy::FsAccess;
    let ctx = common::ctx(true, &[]);

    let obj = compile(&json!({ "fs": { "$(store path)": "rw" } }), &ctx).unwrap();
    let d = PathMatcher::new(&obj.fs.rules).decide(&common::homes().home.join(".store/x"));
    assert!(matches!(d.effect, Effect::Allow) && matches!(d.access, FsAccess::ReadWrite));

    let emb = compile(&json!({ "fs": { "$(store path)/v3": "r" } }), &ctx).unwrap();
    let d = PathMatcher::new(&emb.fs.rules).decide(&common::homes().home.join(".store/v3/pkg"));
    assert!(matches!(d.effect, Effect::Allow) && matches!(d.access, FsAccess::Read));

    let arr = compile(&json!({ "fs": ["$(store path)"] }), &ctx).unwrap();
    let d = PathMatcher::new(&arr.fs.rules).decide(&common::homes().home.join(".store/y"));
    assert!(matches!(d.effect, Effect::Allow) && matches!(d.access, FsAccess::ReadWrite));

    // `$(…)` resolution is UNCONDITIONAL — nub has no trust axis to gate on, so the
    // `ctx` trust flag does not change whether a command runs.
    let untrusted = compile(
        &json!({ "fs": ["$(store path)"] }),
        &common::ctx(false, &[]),
    )
    .unwrap();
    let d = PathMatcher::new(&untrusted.fs.rules).decide(&common::homes().home.join(".store/z"));
    assert!(matches!(d.effect, Effect::Allow));
}

#[test]
fn fs_substitution_failure_empty_and_multiline_fail_closed() {
    // Fail-CLOSED corners: a non-zero exit, empty output (would fail-OPEN to a
    // whole-fs grant), and multi-line output are each a hard compile error naming
    // the substitution — never a silently-dropped or wrong grant.
    let ctx = common::ctx(true, &[]);
    for (surface, needle) in [
        (json!({ "fs": ["$(fail)"] }), "failure"),
        (json!({ "fs": ["$(empty path)"] }), "empty"),
        (json!({ "fs": ["$(two paths)"] }), "multi-line"),
    ] {
        match compile(&surface, &ctx).unwrap_err() {
            CompileError::Substitution { message, .. } => {
                assert!(message.contains(needle), "{message} (want {needle})");
            }
            other => panic!("expected a substitution error for {surface}, got {other:?}"),
        }
    }
}

#[test]
fn fs_substitution_deny_prefix_and_unterminated() {
    // `!$(…)` resolves the DENY target (the `!` is stripped before the command
    // runs, so its output is a path, never a smuggled operator); an unterminated
    // `$(` is named, not shipped as a literal path.
    use nub_sandbox::matcher::PathMatcher;
    let ctx = common::ctx(true, &[]);

    let deny = compile(&json!({ "fs": [".", "!$(store path)"] }), &ctx).unwrap();
    let d = PathMatcher::new(&deny.fs.rules).decide(&common::homes().home.join(".store/secret"));
    assert!(
        matches!(d.effect, Effect::Deny),
        "!$(…) denies the resolved path"
    );

    match compile(&json!({ "fs": ["$(store path"] }), &ctx).unwrap_err() {
        CompileError::Substitution { message, .. } => assert!(message.contains("closing")),
        other => panic!("expected an unterminated-substitution error, got {other:?}"),
    }
}

// ── built-in sets: $trusted (net) + $tooldirs (fs) ──────────────────────────────

#[test]
fn trusted_set_expands_in_place_admitting_listed_and_denying_unlisted() {
    // `$trusted` expands the curated host set at its position; a later authored host
    // composes with it under last-match-wins. Nothing outside either is admitted.
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "net": ["$trusted", "api.mycompany.com"] }), &ctx).unwrap();
    assert!(p.net.enforce);
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(m.admits("nodejs.org"), "a listed $trusted host is admitted");
    assert!(
        m.admits("registry.npmjs.org"),
        "another listed host is admitted"
    );
    assert!(
        m.admits("cache.nixos.org"),
        "a *.suffix member matches its subdomain"
    );
    assert!(
        m.admits("api.mycompany.com"),
        "the authored host composes with the set"
    );
    assert!(!m.admits("evil.test"), "an unlisted host is denied");
    assert!(
        !m.admits("evil.s3.amazonaws.com"),
        "the removed object-store wildcard does not re-admit an exfil sink"
    );
}

#[test]
fn downloads_set_expands_to_the_install_time_hosts_only() {
    // `$downloads` is its own set, not an alias of `$trusted`: it admits the install-time
    // artifact hosts and NOT the far broader agent surface `$trusted` carries — most
    // pointedly `registry.npmjs.org`, whose publish route answers on the same hostname.
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "net": ["$downloads"] }), &ctx).unwrap();
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(
        m.admits("nodejs.org"),
        "a listed $downloads host is admitted"
    );
    assert!(
        m.admits("cdn.cypress.io"),
        "another listed host is admitted"
    );
    assert!(
        !m.admits("registry.npmjs.org"),
        "$downloads must not inherit $trusted's write-capable retentions"
    );
    assert!(!m.admits("evil.test"), "an unlisted host is denied");
    assert!(
        !m.admits("secret.cdn.cypress.io"),
        "wildcard-free: a subdomain of a listed host is NOT admitted, so a DNS label \
         cannot carry exfiltrated bytes"
    );
}

#[test]
fn negated_trusted_set_denies_each_host() {
    // `!$trusted` expands-then-negates: each member becomes a Deny. Ordered after a
    // broad `*` allow, it re-strips the trusted set (`["*", "!$trusted"]`).
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "net": ["*", "!$trusted"] }), &ctx).unwrap();
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(!m.admits("nodejs.org"), "!$trusted denies a listed host");
    assert!(
        m.admits("anything.else"),
        "the broad allow still admits a non-member"
    );
}

#[test]
fn trusted_set_admits_a_brokered_host_in_the_set() {
    // §5 broker-ordering regression: `$trusted` expands into `net.rules` BEFORE
    // `transpose_brokers`/`validate_brokers`, so a broker whose host is a $trusted
    // member is admitted (HostMatcher sees the expanded allow) and the tier derives.
    let ctx = common::ctx(true, &[("NPM_TOKEN", "real-token")]);
    let p = compile(
        &json!({
            "net": ["$trusted"],
            "secrets": { "NPM_TOKEN": { "brokerTo": ["registry.npmjs.org"] } }
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(p.net.brokers.len(), 1);
    assert_eq!(p.net.brokers[0].host, "registry.npmjs.org");
    assert!(matches!(p.net.inspection, Inspection::TlsInspect));
    assert!(
        !p.env.constructed.contains_key("NPM_TOKEN"),
        "the brokered secret's real value is withheld"
    );
}

#[test]
fn brokered_host_not_in_trusted_set_is_a_compile_error() {
    // A brokerTo host that is NOT a $trusted member is not admitted → the "brokered
    // but not allowed" error, proving `$trusted` is the ONLY thing admitting the host.
    let ctx = common::ctx(true, &[("API_TOKEN", "t")]);
    let err = compile(
        &json!({
            "net": ["$trusted"],
            "secrets": { "API_TOKEN": { "brokerTo": ["not-in-trusted.example"] } }
        }),
        &ctx,
    )
    .unwrap_err();
    match err {
        CompileError::Shape { path, message } => {
            assert_eq!(path, "secrets.API_TOKEN.brokerTo");
            assert!(
                message.contains("not-in-trusted.example") && message.contains("not allowed"),
                "names the un-admitted host: {message}"
            );
        }
        other => panic!("expected a brokerTo-not-in-net Shape error, got {other:?}"),
    }
}

#[test]
#[cfg(not(windows))]
fn tooldirs_set_grants_the_cache_dirs_while_the_dotenv_floor_still_bites() {
    // `$tooldirs` grants read on the package-manager / toolchain cache dirs (nub store,
    // npm cacache), composes with an ordinary `./dist` rw grant, and the `.env*` floor
    // is still injected (a read-granting policy). Non-Windows: the Windows read-confine
    // limitation rejects a read-deny-inside-grant pre-launch (conformance covers that).
    use nub_sandbox::matcher::PathMatcher;
    use nub_sandbox::policy::FsAccess;
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "fs": { "$tooldirs": "r", "./dist": "rw" } }), &ctx).unwrap();
    let m = PathMatcher::new(&p.fs.rules);
    let home = common::homes().home;
    let proj = common::homes().project;

    let store = m.decide(&home.join(".local/share/nub/store/pkg/index.js"));
    assert!(
        matches!(store.effect, Effect::Allow) && matches!(store.access, FsAccess::Read),
        "the nub CAS store is readable under $tooldirs"
    );
    let cacache = m.decide(&home.join(".npm/_cacache/content-v2/sha512/ab/cd"));
    assert!(
        matches!(cacache.effect, Effect::Allow) && matches!(cacache.access, FsAccess::Read),
        "npm's cacache is readable under $tooldirs"
    );
    let dist = m.decide(&proj.join("dist/bundle.js"));
    assert!(
        matches!(dist.effect, Effect::Allow) && matches!(dist.access, FsAccess::ReadWrite),
        "the authored ./dist grant composes with the set"
    );
    assert!(
        matches!(m.decide(&proj.join(".env")).effect, Effect::Deny),
        ".env stays denied — the floor beats a read-granting $tooldirs policy"
    );
    assert!(
        matches!(m.decide(&proj.join("src/index.js")).effect, Effect::Deny),
        "an ungranted project path stays default-denied"
    );
}

#[test]
#[cfg(not(windows))]
fn negated_tooldirs_set_denies_each_dir() {
    // `!$tooldirs` (array form — the ORDERED surface) expands-then-negates: each member
    // becomes a Deny. Ordered after a broad `~` grant it re-confines the tool caches
    // while leaving the rest of home usable. (Object-key order is sorted, so re-confining
    // after a broad grant is an array-form concern; a `{ "$tooldirs": false }` object
    // value is the object-form deny.)
    use nub_sandbox::matcher::PathMatcher;
    use nub_sandbox::policy::FsAccess;
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "fs": ["~", "!$tooldirs"] }), &ctx).unwrap();
    let m = PathMatcher::new(&p.fs.rules);
    let home = common::homes().home;
    assert!(
        matches!(
            m.decide(&home.join(".cargo/registry/x")).effect,
            Effect::Deny
        ),
        "!$tooldirs denies a tool cache dir carved out of the broad ~ grant"
    );
    let other = m.decide(&home.join("notes.txt"));
    assert!(
        matches!(other.effect, Effect::Allow) && matches!(other.access, FsAccess::ReadWrite),
        "a non-tooldir home path stays fully usable"
    );

    // Object-form deny via a `false` value also confines the set.
    let obj = compile(
        &json!({ "fs": { "$tooldirs": false, "./dist": "rw" } }),
        &ctx,
    )
    .unwrap();
    let mo = PathMatcher::new(&obj.fs.rules);
    assert!(
        matches!(
            mo.decide(&home.join(".cargo/registry/x")).effect,
            Effect::Deny
        ),
        "{{ \"$tooldirs\": false }} denies the tool caches"
    );
}

#[test]
fn tooldirs_array_form_grants_readwrite() {
    // Array-form `$tooldirs` grants ReadWrite (like a bare path array entry).
    use nub_sandbox::matcher::PathMatcher;
    use nub_sandbox::policy::FsAccess;
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "fs": ["$tooldirs"] }), &ctx).unwrap();
    let d = PathMatcher::new(&p.fs.rules).decide(&common::homes().home.join(".cargo/registry/x"));
    assert!(matches!(d.effect, Effect::Allow) && matches!(d.access, FsAccess::ReadWrite));
}

#[test]
fn wrong_axis_and_unknown_sets_are_shape_errors() {
    // A set on the wrong axis, and an unknown `$name` on either axis, are hard shape
    // errors — never a silent nothing-matching rule.
    let ctx = common::ctx(true, &[]);
    let cases = [
        json!({ "net": ["$tooldirs"] }),           // fs set on net
        json!({ "net": { "$trusted": true } }),    // net set as an object key
        json!({ "net": { "$downloads": true } }),  // the other net set, same rejection
        json!({ "fs": ["$trusted"] }),             // net set on fs
        json!({ "fs": ["$downloads"] }),           // net set on fs
        json!({ "fs": ["$frobnicate"] }),          // unknown set on fs
        json!({ "net": ["$frobnicate"] }),         // unknown set on net
        json!({ "fs": { "$tooldirs/sub": "r" } }), // a set takes no subpath
        json!({ "fs": ["$tooldirs/sub"] }),        // a set takes no subpath (array)
    ];
    for surface in cases {
        assert!(
            matches!(compile(&surface, &ctx), Err(CompileError::Shape { .. })),
            "expected a Shape error for {surface}"
        );
    }
}

#[test]
fn wrong_axis_error_messages_point_at_the_right_axis() {
    // The diagnostics name the correct axis so the fix is obvious.
    let ctx = common::ctx(true, &[]);
    match compile(&json!({ "fs": ["$trusted"] }), &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(
                message.contains("network") && message.contains("net"),
                "{message}"
            );
        }
        other => panic!("expected Shape, got {other:?}"),
    }
    match compile(&json!({ "net": ["$tooldirs"] }), &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(
                message.contains("filesystem") && message.contains("fs"),
                "{message}"
            );
        }
        other => panic!("expected Shape, got {other:?}"),
    }
}

// ── `...:#/pointer` list reuse (Phase 4) ───────────────────────────────────────
// A `...:#/pointer` fs/net array entry splices the referenced same-document list's RAW
// entries at its position (they re-fold through the ordinary per-entry path, so every
// in-place expander + the `.env*` floor compose for free). `#` is the DOCUMENT root, so
// the surface passed to `compile` is `document["sandbox"]` and the ctx carries the whole
// document.

#[test]
fn reuse_fs_pointer_splices_the_referenced_list_in_order() {
    // §6.1: `./src` (from the reused list) precedes `./dist` — splice position + order.
    let doc = json!({
        "shared": { "fs": ["./src"] },
        "sandbox": { "fs": ["...:#/shared/fs", "./dist"] }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    let allows: Vec<String> =
        p.fs.rules
            .entries
            .iter()
            .filter(|r| r.effect == Effect::Allow)
            .map(|r| r.matcher.as_str().to_string())
            .collect();
    let src = allows
        .iter()
        .position(|m| m.contains("/src"))
        .expect("reused ./src rule present");
    let dist = allows
        .iter()
        .position(|m| m.contains("/dist"))
        .expect("./dist rule present");
    assert!(
        src < dist,
        "the reused ./src rules precede ./dist: {allows:?}"
    );
}

#[test]
fn reuse_net_pointer_admits_reused_and_authored_hosts() {
    // §6.2: a reused net list + a directly-authored host both land in the allowlist.
    let doc = json!({
        "shared": { "net": ["registry.npmjs.org"] },
        "sandbox": { "net": ["...:#/shared/net", "api.example.com"] }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(m.admits("registry.npmjs.org"), "reused host admitted");
    assert!(m.admits("api.example.com"), "authored host admitted");
    assert!(!m.admits("evil.com"));
}

#[test]
fn reuse_composes_with_builtin_set_expanders_at_the_splice() {
    // §6.3 / the P3↔P4 seam: a reused net list's `$trusted` expands at the splice, and a
    // reused fs list's `$tooldirs` + `$tmp` yield the tooldir rules AND set the tmp MODE,
    // with the `.env*` floor still LAST.
    let doc = json!({
        "shared": { "net": ["$trusted"], "fs": ["$tooldirs", "$tmp", "./src"] },
        "sandbox": {
            "net": ["...:#/shared/net", "api.example.com"],
            "fs": ["...:#/shared/fs"]
        }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(
        m.admits("api.example.com"),
        "authored host after the reused set"
    );
    assert!(
        p.net.rules.iter().any(|r| r.effect == Effect::Allow),
        "reused $trusted expanded to allow rules at the splice"
    );
    assert!(
        matches!(p.fs.tmp, TmpMode::Private),
        "reused $tmp set the outer private-tmp mode"
    );
    let last = p.fs.rules.entries.last().expect("fs has entries");
    assert!(
        last.matcher.as_str().contains(".env"),
        "the `.env*` floor is still the last band: {:?}",
        last.matcher.as_str()
    );
}

#[test]
fn reuse_deny_after_splice_overrides_a_reused_allow() {
    // §6.4: last-match-wins across the splice — a `!host` after the reuse token denies a
    // host the reused list allowed.
    let doc = json!({
        "shared": { "net": ["registry.npmjs.org", "api.example.com"] },
        "sandbox": { "net": ["...:#/shared/net", "!registry.npmjs.org"] }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(
        !m.admits("registry.npmjs.org"),
        "deny after the splice wins"
    );
    assert!(
        m.admits("api.example.com"),
        "other reused host still admitted"
    );
}

#[test]
fn reuse_cycle_is_a_shape_error_naming_the_chain() {
    // §6.5: #/a/net reuses #/b/net which reuses #/a/net → a Shape error naming the cycle.
    let doc = json!({
        "a": { "net": ["...:#/b/net"] },
        "b": { "net": ["...:#/a/net"] },
        "sandbox": { "net": ["...:#/a/net"] }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    match compile(&doc["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("cycle"), "names the cycle: {message}")
        }
        other => panic!("expected a cycle Shape error, got {other:?}"),
    }
}

#[test]
fn reuse_dangling_pointer_is_a_shape_error_naming_the_pointer() {
    // §6.6: a pointer resolving to no node → a Shape error naming it.
    let doc = json!({ "sandbox": { "fs": ["...:#/nope"] } });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    match compile(&doc["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => assert!(
            message.contains("#/nope") && message.contains("does not resolve"),
            "{message}"
        ),
        other => panic!("expected a dangling-pointer Shape error, got {other:?}"),
    }
}

#[test]
fn reuse_non_array_target_is_a_shape_error() {
    // §6.7: a pointer to a non-array node → "must reference a list".
    let doc = json!({ "shared": { "fs": true }, "sandbox": { "fs": ["...:#/shared"] } });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    match compile(&doc["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("must reference a list"), "{message}")
        }
        other => panic!("expected a non-array Shape error, got {other:?}"),
    }
}

#[test]
fn reuse_bad_pointer_syntax_is_a_shape_error() {
    // §6.8: a pointer missing the leading `#`, and a bare `#` (the whole document).
    for (surface, needle) in [
        (
            json!({ "sandbox": { "fs": ["...:/shared/fs"] } }),
            "beginning with `#`",
        ),
        (
            json!({ "sandbox": { "fs": ["...:#"] } }),
            "must name a list",
        ),
    ] {
        let ctx = common::ctx_with_document(true, &[], surface.clone());
        match compile(&surface["sandbox"], &ctx).unwrap_err() {
            CompileError::Shape { message, .. } => assert!(message.contains(needle), "{message}"),
            other => panic!("expected a Shape error, got {other:?}"),
        }
    }
}

#[test]
fn naked_sentinel_is_rejected_on_every_axis() {
    // §6.9: naked `...`/`!...` (array + object key, every axis) and a top-level
    // `{ "...": ... }` are migration Shape errors; the array form carries the reuse hint.
    let ctx = common::ctx(true, &[]);
    for surface in [
        json!({ "fs": ["..."] }),
        json!({ "net": ["..."] }),
        json!({ "vars": ["..."] }),
        json!({ "fs": ["!..."] }),
        json!({ "fs": { "...": true } }),
        json!({ "net": { "...": true } }),
        json!({ "vars": { "...": true } }),
        json!({ "...": true }),
        // Whitespace-padded forms must NOT slip past the reject into a literal grant
        // (the reject trims, agreeing with the reuse parser).
        json!({ "fs": ["  ...  "] }),
        json!({ "fs": { "  ...  ": true } }),
    ] {
        assert!(
            matches!(compile(&surface, &ctx), Err(CompileError::Shape { .. })),
            "naked `...` must be a Shape error for {surface}"
        );
    }
    let hint = compile(&json!({ "fs": ["..."] }), &ctx)
        .unwrap_err()
        .to_string();
    assert!(
        hint.contains("...:#/pointer"),
        "migration hint present: {hint}"
    );
}

// ── `...:#/pointer` OBJECT-KEY spread (general spread — reverses P4's OQ2) ───────
// The `...:#/pointer` token is a GENERAL spread: as an OBJECT KEY it resolves the pointer
// to an OBJECT and splices its key→value entries at the slot, re-folding through the
// ordinary object-entry path (so built-in sets + the `.env*` floor compose, and a LOCAL
// key after the spread overrides a spliced one by last-match). Uniform with the array form.

#[test]
fn reuse_object_spread_splices_and_local_key_overrides() {
    // The spread splices `#/shared/fs`'s entries, then the LOCAL `./dist: "rw"` AFTER the
    // spread overrides the spliced `./dist: "r"` by last-match (JS `{...shared, "./dist": "rw"}`).
    let doc = json!({
        "shared": { "fs": { "./src": "r", "./dist": "r" } },
        "sandbox": { "fs": { "...:#/shared/fs": true, "./dist": "rw" } }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    let m = nub_sandbox::matcher::PathMatcher::new(&p.fs.rules);
    let src = m.decide(&common::homes().project.join("src/a.rs"));
    assert!(
        matches!(src.effect, Effect::Allow),
        "spliced ./src is granted"
    );
    let dist = m.decide(&common::homes().project.join("dist/out.js"));
    assert!(matches!(dist.effect, Effect::Allow), "./dist is granted");
    assert_eq!(
        dist.access,
        FsAccess::ReadWrite,
        "local ./dist:rw after the spread overrides the spliced ./dist:r (last-match)"
    );
}

#[test]
fn reuse_object_spread_composes_builtin_sets_and_env_floor() {
    // A spliced object's `$tmp`/`$tooldirs` fire at the splice (the tmp MODE is set, the
    // tooldir rules land) and the `.env*` floor is still the LAST band — the object twin of
    // `reuse_composes_with_builtin_set_expanders_at_the_splice`.
    let doc = json!({
        "shared": { "fs": { "$tmp": "rw", "$tooldirs": "r", "./src": "r" } },
        "sandbox": { "fs": { "...:#/shared/fs": true } }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    assert!(
        matches!(p.fs.tmp, TmpMode::Private),
        "spliced $tmp set the outer private-tmp mode"
    );
    assert!(
        p.fs.rules.entries.iter().any(|r| r.effect == Effect::Allow),
        "spliced $tooldirs + ./src expanded to allow rules"
    );
    let last = p.fs.rules.entries.last().expect("fs has entries");
    assert!(
        last.matcher.as_str().contains(".env"),
        "the `.env*` floor is still the last band: {:?}",
        last.matcher.as_str()
    );
}

#[test]
fn reuse_object_spread_net_admits_reused_and_authored_hosts() {
    // A `...:#/pointer` object key on net splices `"<host>": bool` entries; a deny after the
    // spread wins by last-match, and an authored host after it is still admitted.
    let doc = json!({
        "shared": { "net": { "registry.npmjs.org": true, "api.example.com": true } },
        "sandbox": { "net": { "...:#/shared/net": true, "registry.npmjs.org": false, "extra.example.com": true } }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(m.admits("api.example.com"), "spliced host admitted");
    assert!(
        m.admits("extra.example.com"),
        "authored host after the spread admitted"
    );
    assert!(
        !m.admits("registry.npmjs.org"),
        "a `false` after the spread overrides a spliced allow (last-match)"
    );
}

#[test]
fn reuse_object_spread_env_vars_and_secrets() {
    // The spread is uniform on the env family: a `...:#/pointer` object key in vars/secrets
    // splices the referenced object's entries. The vars-spliced key is public; the
    // secrets-spliced key reaches the child with its real value and is marked sensitive.
    let doc = json!({
        "shared": {
            "vars": { "FOO": true },
            "secrets": { "DB_URL": true }
        },
        "sandbox": {
            "vars": { "...:#/shared/vars": true, "BAR": true },
            "secrets": { "...:#/shared/secrets": true }
        }
    });
    let ctx = common::ctx_with_document(
        true,
        &[("FOO", "1"), ("BAR", "2"), ("DB_URL", "postgres://s")],
        doc.clone(),
    );
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    assert_eq!(
        p.env.constructed.get("FOO").map(String::as_str),
        Some("1"),
        "spliced var"
    );
    assert_eq!(
        p.env.constructed.get("BAR").map(String::as_str),
        Some("2"),
        "authored var"
    );
    assert_eq!(
        p.env.constructed.get("DB_URL").map(String::as_str),
        Some("postgres://s"),
        "spliced secret reaches the child with its real value"
    );
    let rule = |k: &str| p.env.schema.iter().find(|r| r.key == k).unwrap();
    assert!(!rule("FOO").sensitive, "a spliced vars entry stays public");
    assert!(
        rule("DB_URL").sensitive,
        "a spliced secrets entry is redacted"
    );
}

#[test]
fn reuse_object_spread_multiple_spreads_compose_in_order() {
    // Two spreads in one object splice in written order; a later spread's key overrides an
    // earlier spread's same key by last-match.
    let doc = json!({
        "a": { "fs": { "./x": "r", "./shared": "r" } },
        "b": { "fs": { "./y": "r", "./shared": "rw" } },
        "sandbox": { "fs": { "...:#/a/fs": true, "...:#/b/fs": true } }
    });
    let ctx = common::ctx_with_document(true, &[], doc.clone());
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    let m = nub_sandbox::matcher::PathMatcher::new(&p.fs.rules);
    assert!(
        matches!(
            m.decide(&common::homes().project.join("x/f")).effect,
            Effect::Allow
        ),
        "first spread's ./x granted"
    );
    assert!(
        matches!(
            m.decide(&common::homes().project.join("y/f")).effect,
            Effect::Allow
        ),
        "second spread's ./y granted"
    );
    assert_eq!(
        m.decide(&common::homes().project.join("shared/f")).access,
        FsAccess::ReadWrite,
        "the second spread's ./shared:rw overrides the first spread's ./shared:r (last-match)"
    );
}

#[test]
fn reuse_object_spread_type_mismatch_is_a_shape_error() {
    // The general resolver type-checks per context: an ARRAY pointer used as an object key,
    // and an OBJECT pointer used as an array entry, are both fail-loud Shape errors.
    let array_as_object_key = json!({
        "shared": { "fs": ["./src"] },
        "sandbox": { "fs": { "...:#/shared/fs": true } }
    });
    let ctx = common::ctx_with_document(true, &[], array_as_object_key.clone());
    match compile(&array_as_object_key["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("must reference an object"), "{message}")
        }
        other => panic!("expected an object type-mismatch Shape error, got {other:?}"),
    }

    let object_as_array_entry = json!({
        "shared": { "fs": { "./src": "r" } },
        "sandbox": { "fs": ["...:#/shared/fs"] }
    });
    let ctx = common::ctx_with_document(true, &[], object_as_array_entry.clone());
    match compile(&object_as_array_entry["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("must reference a list"), "{message}")
        }
        other => panic!("expected a list type-mismatch Shape error, got {other:?}"),
    }
}

#[test]
fn reuse_object_spread_cycle_dangling_and_depth_are_shape_errors() {
    // The object form threads the SAME cycle stack + depth belt as the array form.
    let cycle = json!({
        "a": { "fs": { "...:#/b/fs": true } },
        "b": { "fs": { "...:#/a/fs": true } },
        "sandbox": { "fs": { "...:#/a/fs": true } }
    });
    let ctx = common::ctx_with_document(true, &[], cycle.clone());
    match compile(&cycle["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => assert!(message.contains("cycle"), "{message}"),
        other => panic!("expected a cycle Shape error, got {other:?}"),
    }

    let dangling = json!({ "sandbox": { "fs": { "...:#/nope": true } } });
    let ctx = common::ctx_with_document(true, &[], dangling.clone());
    match compile(&dangling["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("does not resolve"), "{message}")
        }
        other => panic!("expected a dangling-pointer Shape error, got {other:?}"),
    }

    // A chain of object spreads longer than MAX_REUSE_DEPTH (64) trips the depth belt.
    let mut root = serde_json::Map::new();
    for i in 0..64u32 {
        let mut inner = serde_json::Map::new();
        inner.insert(format!("...:#/n{}/fs", i + 1), json!(true));
        root.insert(format!("n{i}"), json!({ "fs": Value::Object(inner) }));
    }
    root.insert("sandbox".into(), json!({ "fs": { "...:#/n0/fs": true } }));
    let deep = Value::Object(root);
    let ctx = common::ctx_with_document(true, &[], deep.clone());
    match compile(&deep["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("nested too deeply"), "{message}")
        }
        other => panic!("expected a depth Shape error, got {other:?}"),
    }
}

#[test]
fn reuse_object_spread_value_must_be_the_true_placeholder() {
    // The spread key's value is a placeholder — the spliced entries carry their own values —
    // so only `true` is accepted; a meaningful value (a mode, `false`) is rejected fail-loud.
    let doc = |v: Value| {
        json!({
            "shared": { "fs": { "./src": "r" } },
            "sandbox": { "fs": { "...:#/shared/fs": v } }
        })
    };
    for bad in [json!("rw"), json!(false), json!({ "x": 1 })] {
        let surface = doc(bad.clone());
        let ctx = common::ctx_with_document(true, &[], surface.clone());
        match compile(&surface["sandbox"], &ctx).unwrap_err() {
            CompileError::Shape { message, .. } => {
                assert!(
                    message.contains("placeholder value `true`"),
                    "for {bad}: {message}"
                )
            }
            other => panic!("expected a spread-value Shape error for {bad}, got {other:?}"),
        }
    }
    // `true` is accepted (the spliced entries carry their values).
    let ok = doc(json!(true));
    let ctx = common::ctx_with_document(true, &[], ok.clone());
    assert!(
        compile(&ok["sandbox"], &ctx).is_ok(),
        "the `true` placeholder compiles"
    );
}

#[test]
fn reuse_object_key_spread_compiles_and_negated_object_key_is_rejected() {
    // Regression: P4's OQ2 rejection is GONE — a `...:#/pointer` OBJECT key now compiles
    // (fs AND net). A NEGATED object-key reuse stays rejected, consistent with the array form.
    let fs = json!({
        "shared": { "fs": { "./src": "r" } },
        "sandbox": { "fs": { "...:#/shared/fs": true } }
    });
    let ctx = common::ctx_with_document(true, &[], fs.clone());
    assert!(
        compile(&fs["sandbox"], &ctx).is_ok(),
        "fs object-key spread compiles"
    );

    let net = json!({
        "shared": { "net": { "api.example.com": true } },
        "sandbox": { "net": { "...:#/shared/net": true } }
    });
    let ctx = common::ctx_with_document(true, &[], net.clone());
    assert!(
        compile(&net["sandbox"], &ctx).is_ok(),
        "net object-key spread compiles"
    );

    // Negated reuse (`!...:#/pointer`) stays rejected in BOTH container forms — the object
    // key AND the array entry — for the same OQ1 reason (a spliced node carries its own
    // allow/deny; negating the whole splice is ill-defined).
    for negated in [
        json!({ "shared": { "fs": { "./src": "r" } }, "sandbox": { "fs": { "!...:#/shared/fs": true } } }),
        json!({ "shared": { "fs": ["./src"] }, "sandbox": { "fs": ["!...:#/shared/fs"] } }),
    ] {
        let ctx = common::ctx_with_document(true, &[], negated.clone());
        match compile(&negated["sandbox"], &ctx).unwrap_err() {
            CompileError::Shape { message, .. } => {
                assert!(message.contains("negated reuse"), "{message}")
            }
            other => panic!("expected a negated-reuse Shape error for {negated}, got {other:?}"),
        }
    }
}

// ── `...:#/pointer` ARRAY reuse on the env family (vars/secrets) ────────────────
// Completes spread uniformity: the array form now works on the env axes exactly as on
// fs/net (P4 wired only fs/net arrays, leaving env-array a silent no-op). A spliced list
// re-folds in place, and a spliced entry's sensitivity follows the SPLICE SITE's axis.

#[test]
fn reuse_env_array_splices_vars_and_secrets_in_order() {
    let doc = json!({
        "shared": { "vars": ["FOO"], "secrets": ["DB_URL"] },
        "sandbox": {
            "vars": ["...:#/shared/vars", "BAR"],
            "secrets": ["...:#/shared/secrets", "API_TOKEN"]
        }
    });
    let ctx = common::ctx_with_document(
        true,
        &[
            ("FOO", "1"),
            ("BAR", "2"),
            ("DB_URL", "u"),
            ("API_TOKEN", "t"),
        ],
        doc.clone(),
    );
    let p = compile(&doc["sandbox"], &ctx).unwrap();
    assert_eq!(
        p.env.constructed.get("FOO").map(String::as_str),
        Some("1"),
        "spliced var"
    );
    assert_eq!(
        p.env.constructed.get("BAR").map(String::as_str),
        Some("2"),
        "authored var after the splice"
    );
    assert_eq!(
        p.env.constructed.get("DB_URL").map(String::as_str),
        Some("u"),
        "spliced secret"
    );
    assert_eq!(
        p.env.constructed.get("API_TOKEN").map(String::as_str),
        Some("t"),
        "authored secret after the splice"
    );
    let rule = |k: &str| p.env.schema.iter().find(|r| r.key == k).unwrap();
    assert!(
        !rule("FOO").sensitive,
        "a list spliced under vars stays public"
    );
    assert!(
        rule("DB_URL").sensitive,
        "a list spliced under secrets is redacted"
    );
}

#[test]
fn reuse_env_array_errors_match_fs_net_and_close_the_silent_hole() {
    // A spliced env-array pointer must resolve to a LIST (matching fs/net); an object node
    // is a fail-loud type mismatch, not a silent literal selector.
    let mismatch = json!({
        "shared": { "vars": { "FOO": true } },
        "sandbox": { "vars": ["...:#/shared/vars"] }
    });
    let ctx = common::ctx_with_document(true, &[], mismatch.clone());
    match compile(&mismatch["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("must reference a list"), "{message}")
        }
        other => panic!("expected a list type-mismatch, got {other:?}"),
    }

    // Regression: `["...:#/x"]` with a missing target was a SILENT no-op (an allowlist
    // selector for a var literally named `...:#/x`) before env-array reuse was wired — it is
    // now a fail-loud dangling error, proving the grant-hole is closed.
    let dangling = json!({ "sandbox": { "vars": ["...:#/nope"] } });
    let ctx = common::ctx_with_document(true, &[], dangling.clone());
    match compile(&dangling["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("does not resolve"), "{message}")
        }
        other => panic!("expected a dangling error, got {other:?}"),
    }

    // Cycle through the env-array path (the same shared cycle stack).
    let cycle = json!({
        "a": { "vars": ["...:#/b/vars"] },
        "b": { "vars": ["...:#/a/vars"] },
        "sandbox": { "vars": ["...:#/a/vars"] }
    });
    let ctx = common::ctx_with_document(true, &[], cycle.clone());
    match compile(&cycle["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => assert!(message.contains("cycle"), "{message}"),
        other => panic!("expected a cycle error, got {other:?}"),
    }

    // Negated env-array reuse stays rejected, uniform with fs/net.
    let negated = json!({
        "shared": { "vars": ["FOO"] },
        "sandbox": { "vars": ["!...:#/shared/vars"] }
    });
    let ctx = common::ctx_with_document(true, &[], negated.clone());
    match compile(&negated["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("negated reuse"), "{message}")
        }
        other => panic!("expected a negated-reuse error, got {other:?}"),
    }
}

#[test]
fn broker_validates_against_the_post_reuse_net_set() {
    // §6.10 / §5: a reused net list supplying the brokerTo host compiles (the broker
    // validates against the fully-expanded net set); a brokerTo host absent post-reuse errors.
    let ok = json!({
        "shared": { "net": ["api.github.com"] },
        "sandbox": {
            "net": ["...:#/shared/net"],
            "secrets": { "GITHUB_TOKEN": { "brokerTo": ["api.github.com"] } }
        }
    });
    let ctx = common::ctx_with_document(true, &[("GITHUB_TOKEN", "t")], ok.clone());
    let p = compile(&ok["sandbox"], &ctx).unwrap();
    assert_eq!(p.net.brokers.len(), 1);
    assert_eq!(p.net.brokers[0].host, "api.github.com");

    let bad = json!({
        "shared": { "net": ["registry.npmjs.org"] },
        "sandbox": {
            "net": ["...:#/shared/net"],
            "secrets": { "GITHUB_TOKEN": { "brokerTo": ["api.github.com"] } }
        }
    });
    let ctx = common::ctx_with_document(true, &[("GITHUB_TOKEN", "t")], bad.clone());
    match compile(&bad["sandbox"], &ctx).unwrap_err() {
        CompileError::Shape { message, .. } => {
            assert!(message.contains("brokered but not allowed"), "{message}")
        }
        other => panic!("expected a broker Shape error, got {other:?}"),
    }
}

#[test]
fn sandbox_true_fs_still_denies_home_secrets() {
    // §6.11 / decision-(a) guard: P4 is behavior-preserving — `sandbox: true` fs still
    // denies `~/.ssh` (via `secure_default_fs`'s direct home-secret denies, not a `...`
    // splice). If decision (a) later relocates those denies to a preset, this flips.
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!(true), &ctx).unwrap();
    let m = nub_sandbox::matcher::PathMatcher::new(&p.fs.rules);
    let ssh = common::homes().home.join(".ssh/id_rsa");
    assert!(
        matches!(m.decide(&ssh).effect, Effect::Deny),
        "`sandbox: true` fs denies ~/.ssh/id_rsa"
    );
}
