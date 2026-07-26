//! Compiler tests: the wrapper trichotomy, preset expansion, per-axis fold, the
//! env type grammar, `$(…)` trust gating, and the error surface.

mod common;

use nub_sandbox::compiler::{CompileError, compile};
use nub_sandbox::policy::{Effect, EnvFormat, Inspection, NetTarget, TmpMode};
use serde_json::json;

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

// ── complete-statement floor + `"..."` scope inheritance (U1) ──────────────────

#[test]
fn env_spread_alone_is_the_curated_baseline_equals_sandbox_true() {
    // THE env-base fix (D2): `env: ["..."]` inherits the curated baseline — the
    // SAME env `sandbox: true` produces, secret-free — NOT strip-all (the old
    // denies-only bug) and NOT axis `env: true` passthrough (which keeps secrets).
    let env = &[
        ("PATH", "/usr/bin"),
        ("HOME", "/home/u"),
        ("PWD", "/proj"),
        ("npm_config_target", "22"),
        ("npm_config_email", "me@x.com"), // credential-shaped npm_config → dropped
        ("AWS_SECRET_ACCESS_KEY", "sk"),
        ("MY_TOKEN", "t"),
        ("RANDOM_VAR", "v"), // non-baseline, non-secret → not in the curated allowlist
    ];
    let ctx = common::ctx(true, env);
    let spread = compile(&json!({ "vars": ["..."] }), &ctx).unwrap();
    let truth = compile(&json!(true), &ctx).unwrap();
    // The whole env axis is identical to `sandbox: true`'s — the single source of
    // truth (`baseline_allows`) guarantees no drift.
    assert_eq!(
        spread.env, truth.env,
        "env: [\"...\"] must equal sandbox: true's curated env exactly"
    );
    let c = &spread.env.constructed;
    assert!(
        c.contains_key("PATH") && c.contains_key("HOME"),
        "baseline kept"
    );
    assert!(
        c.contains_key("PWD"),
        "PWD is a baseline key, kept (not stripped)"
    );
    assert!(c.contains_key("npm_config_target"), "build hint kept");
    assert!(
        !c.contains_key("npm_config_email"),
        "npm credential dropped"
    );
    assert!(
        !c.contains_key("AWS_SECRET_ACCESS_KEY") && !c.contains_key("MY_TOKEN"),
        "secrets dropped"
    );
    assert!(
        !c.contains_key("RANDOM_VAR"),
        "non-baseline var not granted"
    );
}

#[test]
fn env_spread_is_not_axis_true_passthrough() {
    // Guard the two DIFFERENT `true`s: axis `env: true` = passthrough (keeps the
    // secret); `env: ["..."]` = curated baseline (strips it).
    let ctx = common::ctx(true, &[("PATH", "/bin"), ("MY_TOKEN", "leak")]);
    let passthrough = compile(&json!({ "vars": true }), &ctx).unwrap();
    let baseline = compile(&json!({ "vars": ["..."] }), &ctx).unwrap();
    assert!(
        passthrough.env.constructed.contains_key("MY_TOKEN"),
        "axis env:true passes the secret through"
    );
    assert!(
        !baseline.env.constructed.contains_key("MY_TOKEN"),
        "env:[\"...\"] strips the secret (curated baseline)"
    );
}

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
fn sentinel_scalar_value_is_a_shape_error_but_file_ref_defers() {
    // The `"..."` value grammar is `true | "<file-ref>" | [list]`. A bare scalar
    // (`{"...": "port"}`, `{"...": 5}`, `{"...": false}`) is NOT a file-ref and must
    // be a clear shape error — never silently admitted into the file-extends branch
    // (which would later try to resolve a file literally named `port`). A path-like
    // string is a legit frontend-deferred file-ref and stays FileRefUnresolved.
    let ctx = common::ctx(true, &[("PORT", "80")]);

    // Malformed `"..."` values → shape error: bare scalars and arrays at the
    // wrapper level and in env, plus the fs/net array-only-sentinel object keys.
    for surface in [
        json!({ "...": "r" }),
        json!({ "...": "port" }),
        json!({ "...": 5 }),
        json!({ "...": false }),
        json!({ "...": ["./a.json"] }),
        json!({ "vars": { "...": "port" } }),
        json!({ "vars": { "...": 5 } }),
        // fs/net reject a `"..."` OBJECT key outright (array-only sentinel).
        json!({ "fs": { "...": "r" } }),
        json!({ "net": { "...": "r" } }),
    ] {
        assert!(
            matches!(compile(&surface, &ctx), Err(CompileError::Shape { .. })),
            "malformed `\"...\"` value must be a shape error for {surface}"
        );
    }

    // A genuine file-ref still defers to the frontend (unchanged) — no over-trigger.
    for surface in [
        json!({ "...": "./policy.json" }),
        json!({ "vars": { "...": "./p.json" } }),
    ] {
        assert!(
            matches!(
                compile(&surface, &ctx),
                Err(CompileError::FileRefUnresolved { .. })
            ),
            "a path-like `\"...\"` value is a deferred file-ref for {surface}"
        );
    }

    // `{"...": true}` still inherits (compiles clean) — the strictness must not
    // reject the one valid inline form.
    assert!(compile(&json!({ "...": true }), &ctx).is_ok());
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

#[test]
fn build_jail_preset_expands() {
    let ctx = common::ctx(true, &[("PATH", "/bin"), ("NPM_TOKEN", "t")]);
    let p = compile(&json!("build-jail"), &ctx).unwrap();
    assert!(
        p.net.enforce && p.net.rules.is_empty(),
        "build-jail denies egress"
    );
    assert!(
        p.env.enforce && p.env.constructed.is_empty(),
        "build-jail strips env"
    );
    // The project subtree is writable...
    let m = nub_sandbox::matcher::PathMatcher::new(&p.fs.rules);
    let proj = common::homes().project;
    let d = m.decide(&proj.join("build/out.o"));
    assert!(
        matches!(d.effect, Effect::Allow)
            && matches!(d.access, nub_sandbox::policy::FsAccess::ReadWrite)
    );
    // ...but the secret set stays DENIED even though `"./"` grants the whole subtree.
    // The `"./"` grant is the LAST matching entry for `<proj>/.env`, so without the
    // post-fold secret-floor re-assertion the jail would leak the project's own
    // `.env` to the untrusted lifecycle script it exists to confine. Guard the leaf,
    // a nested `.env`, the `.env` DIRECTORY form, and an outside-project secret.
    for secret in [".env", "packages/app/.env", ".env/keys.txt", ".envrc"] {
        assert!(
            matches!(m.decide(&proj.join(secret)).effect, Effect::Deny),
            "build-jail must deny <proj>/{secret}"
        );
    }
    assert!(
        matches!(
            m.decide(&common::homes().home.join(".ssh/id_rsa")).effect,
            Effect::Deny
        ),
        "build-jail must deny the home secret set"
    );
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
fn env_spread_defaults_deny_secrets_but_ordering_can_reallow() {
    let ctx = common::ctx(true, &[("GITHUB_TOKEN", "gh"), ("NORMAL", "n")]);
    // `["*", "..."]` — allow all, then secret defaults deny → GITHUB_TOKEN gone.
    let denied = compile(&json!({ "vars": ["*", "..."] }), &ctx).unwrap();
    assert!(!denied.env.constructed.contains_key("GITHUB_TOKEN"));
    assert!(denied.env.constructed.contains_key("NORMAL"));
    // `["...", "*"]` — defaults first, then allow-all wins by ordering.
    let allowed = compile(&json!({ "vars": ["...", "*"] }), &ctx).unwrap();
    assert!(
        allowed.env.constructed.contains_key("GITHUB_TOKEN"),
        "later allow wins"
    );
}

#[test]
fn env_secret_defaults_deny_uppercase_secrets_without_overmatching() {
    // The security regression that motivated Phase R: the `"..."` secret guards
    // were case-sensitive lowercase substrings, so real UPPERCASE secrets leaked.
    // Unambiguous tokens now match case-insensitively as SUBSTRINGS (catching
    // plurals, undelimited, and fused names); short/ambiguous tokens (pat/pwd/auth)
    // match only as whole SEGMENTS so look-alikes (`PATH`⊃`pat`, `AUTHOR`⊃`auth`)
    // survive.
    let secrets = [
        "MY_TOKEN",
        "MY_PASSWORD",
        "DATABASE_SECRET",
        "MY_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "my_token",                       // lowercase → case-insensitive
        "SESSION_TOKENS",                 // plural — substring rule catches it
        "DB_PASSWORDS",                   // plural
        "CREDENTIALS",                    // plural, bare
        "GOOGLE_APPLICATION_CREDENTIALS", // fused/plural
        "MYTOKEN",                        // undelimited
        "MYSQL_PWD",                      // short token as a whole segment
    ];
    let benign = ["PATH", "AUTHOR", "COMPATIBILITY", "HOME", "LANG"];
    let mut env: Vec<(&str, &str)> = secrets.iter().map(|k| (*k, "s")).collect();
    env.extend(benign.iter().map(|k| (*k, "ok")));

    let ctx = common::ctx(true, &env);
    let p = compile(&json!({ "vars": ["*", "..."] }), &ctx).unwrap();
    let c = &p.env.constructed;
    for leaked in secrets {
        assert!(
            !c.contains_key(leaked),
            "{leaked} must be denied by default"
        );
    }
    for kept in benign {
        assert!(
            c.contains_key(kept),
            "{kept} must survive — the guards must not over-match"
        );
    }
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
        &json!({ "vars": { "URL": { "value": "https://$(echo hi)/path" } } }),
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
    // D18: a `$(` with no balanced close is a substitution-shaped error at BOTH the
    // type position and the `value:` position — never a silent literal or a
    // confusing "unknown env type". The runner must NOT fire (nothing to run).
    struct PanicRunner;
    impl nub_sandbox::CommandRunner for PanicRunner {
        fn run(&self, _: &str) -> Result<String, String> {
            panic!("an unterminated `$(` must not reach the runner");
        }
    }
    let ctx = nub_sandbox::compiler::CompileCtx {
        homes: common::homes(),
        cwd: common::homes().project,
        caps: nub_sandbox::compiler::ScopeCapabilities::approved(),
        ambient_env: std::collections::BTreeMap::new(),
        runner: Box::new(PanicRunner),
    };
    for surface in [
        json!({ "vars": { "X": "$(op read" } }),
        json!({ "vars": { "X": { "value": "postgres://$(op read@h" } } }),
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
        json!({ "vars": { "X": { "value": "$(echo hi)$(oops" } } }),
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
        caps: nub_sandbox::compiler::ScopeCapabilities::approved(),
        ambient_env: std::collections::BTreeMap::new(),
        runner: Box::new(PanicRunner),
    };
    for surface in [
        json!({ "vars": { "FOO_*": "$(echo hi)" } }),
        json!({ "vars": { "FOO_*": { "value": "$(echo hi)" } } }),
    ] {
        assert!(matches!(
            compile(&surface, &ctx).unwrap_err(),
            CompileError::Shape { .. }
        ));
    }
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
    // brokerTo cannot combine with a literal `value`.
    assert!(
        compile(
            &json!({
                "net": ["api.example.com"],
                "secrets": { "API_TOKEN": { "brokerTo": ["api.example.com"], "value": "x" } }
            }),
            &ctx
        )
        .is_err(),
        "brokerTo + value must be rejected"
    );
}

// ── fs $(…) command substitution ───────────────────────────────────────────────

#[test]
fn fs_substitution_resolves_and_grants_whole_and_embedded() {
    // A `$(…)` fs path resolves at load time and flows into the matcher exactly as
    // a literal would: whole-value (object key), embedded in a larger path, and the
    // array form (ReadWrite). The stub `store path` → `/home/u/.store`.
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
    assert!(
        m.admits("api.github.com"),
        "a listed $trusted host is admitted"
    );
    assert!(
        m.admits("registry.npmjs.org"),
        "another listed host is admitted"
    );
    assert!(
        m.admits("in.sentry.io"),
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
fn negated_trusted_set_denies_each_host() {
    // `!$trusted` expands-then-negates: each member becomes a Deny. Ordered after a
    // broad `*` allow, it re-strips the trusted set (`["*", "!$trusted"]`).
    let ctx = common::ctx(true, &[]);
    let p = compile(&json!({ "net": ["*", "!$trusted"] }), &ctx).unwrap();
    let m = nub_sandbox::matcher::HostMatcher::new(&p.net);
    assert!(
        !m.admits("api.github.com"),
        "!$trusted denies a listed host"
    );
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
    let ctx = common::ctx(true, &[("GH_TOKEN", "real-token")]);
    let p = compile(
        &json!({
            "net": ["$trusted"],
            "secrets": { "GH_TOKEN": { "brokerTo": ["api.github.com"] } }
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(p.net.brokers.len(), 1);
    assert_eq!(p.net.brokers[0].host, "api.github.com");
    assert!(matches!(p.net.inspection, Inspection::TlsInspect));
    assert!(
        !p.env.constructed.contains_key("GH_TOKEN"),
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
        json!({ "fs": ["$trusted"] }),             // net set on fs
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
