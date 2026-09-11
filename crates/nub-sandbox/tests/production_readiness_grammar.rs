use nub_sandbox::conformance::{run_fixture, Fixture};
use nub_sandbox::policy::{Effect, FsAccess, Inspection, ProxyMode};
use nub_sandbox::{
    compile, compile_build_jail, CommandRunner, CompileCtx, Homes, ScopeCapabilities,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

struct FixedRunner;

impl CommandRunner for FixedRunner {
    fn run(&self, command: &str) -> Result<String, String> {
        match command {
            "cache-location" => Ok("/resolved/cache".to_string()),
            "fs-location" => Ok("/resolved/fs".to_string()),
            other => Err(format!("unexpected documented command `{other}`")),
        }
    }
}

fn ctx(caps: ScopeCapabilities) -> CompileCtx {
    let root = test_root();
    let mut ctx = CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("home/cache"),
            tmp: root.join("tmp"),
            project: root.join("project"),
        },
        root.join("project"),
        caps,
        BTreeMap::from([
            // Required by README JSON example 8 (`vars.HOME: true`).
            ("HOME".to_string(), root.join("home").display().to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("PORT".to_string(), "3000".to_string()),
            ("MODE".to_string(), "production".to_string()),
            ("API_TOKEN".to_string(), "token-value".to_string()),
            ("UV_CACHE_DIR".to_string(), "/ambient/cache".to_string()),
            // Required by README JSON example 9 (`vars.YARN_CACHE_FOLDER: true`).
            (
                "YARN_CACHE_FOLDER".to_string(),
                "/ambient/yarn-cache".to_string(),
            ),
        ]),
    );
    ctx.runner = Box::new(FixedRunner);
    ctx
}

fn test_root() -> PathBuf {
    std::env::temp_dir().join("nub-sandbox-production-readiness-grammar")
}

fn readme_json_examples() -> Vec<String> {
    let mut examples = Vec::new();
    let mut lines = Vec::new();
    let mut in_json_fence = false;
    for line in include_str!("../README.md").lines() {
        if line == "```json" {
            assert!(!in_json_fence, "README JSON fences must not nest");
            in_json_fence = true;
            lines.clear();
        } else if line == "```" && in_json_fence {
            examples.push(lines.join("\n"));
            in_json_fence = false;
        } else if in_json_fence {
            lines.push(line);
        }
    }
    assert!(!in_json_fence, "every README JSON fence must close");
    examples
}

#[test]
fn public_readme_json_examples_compile() {
    let examples = readme_json_examples();
    assert_eq!(examples.len(), 9, "README JSON grammar inventory changed");
    for (index, example) in examples.iter().enumerate() {
        let surface = serde_json::from_str(example).unwrap_or_else(|error| {
            panic!(
                "README JSON example {} must parse:\n{example}\n{error}",
                index + 1
            )
        });
        compile(&surface, &ctx(ScopeCapabilities::approved())).unwrap_or_else(|error| {
            panic!(
                "README JSON example {} must compile:\n{example}\n{error}",
                index + 1
            )
        });
    }
}

#[test]
fn grammar_preserves_positive_fs_merging_net_order_and_env_provenance() {
    let root = test_root();
    let surface = json!({
        "fs": {
            "./output": "rw",
            "./output/logs": "r",
            "$home/.config/tool": "r",
            "$cache/tool": "rw",
            "$tmp": "rw",
            "$tooldirs": "r"
        },
        "net": ["*", "!admin.example.com", "admin.example.com", "<private>"],
        "vars": {
            "PORT": "port",
            "MODE": "enum:development|production",
            "UV_CACHE_DIR": "$(cache-location)"
        },
        "secrets": {
            "API_TOKEN": {"format": "/token-.+/", "brokerTo": ["registry.example.com"]}
        }
    });
    let fixture: Fixture = serde_json::from_value(json!({
        "name": "documented public policy",
        "sandbox": surface,
        "fs": [
            {"path": root.join("project/output/logs/build.log").display().to_string(), "read": true, "write": true},
            {"path": root.join("home/.config/tool/config").display().to_string(), "read": true, "write": false},
            {"path": root.join("home/cache/tool/cache").display().to_string(), "read": true, "write": true}
        ],
        "net": [
            {"host": "admin.example.com", "admit": true},
            {"host": "10.2.3.4", "admit": true},
            {"host": "registry.example.com", "admit": true}
        ],
        "env": [
            {"key": "PORT", "present": true, "value": "3000"},
            {"key": "MODE", "present": true, "value": "production"},
            {"key": "UV_CACHE_DIR", "present": true, "value": "/resolved/cache"},
            {"key": "API_TOKEN", "present": false}
        ]
    }))
    .expect("fixture is valid JSON");
    let context = ctx(ScopeCapabilities::approved());
    let mismatches = run_fixture(&fixture, &context);
    assert!(
        mismatches.is_empty(),
        "documented public policy mismatches:\n{mismatches:#?}"
    );

    let policy = compile(&fixture.sandbox, &context).unwrap();
    assert_eq!(policy.net.mode, ProxyMode::Auto);
    assert_eq!(policy.net.inspection, Inspection::TlsInspect);
    assert_eq!(policy.net.brokers.len(), 1);
    assert_eq!(policy.net.brokers[0].host, "registry.example.com");
    assert_eq!(policy.net.brokers[0].env, vec!["API_TOKEN"]);
    assert!(policy.env.withheld.contains(&"API_TOKEN".to_string()));
}

#[test]
fn dependency_scope_rejects_dynamic_env_and_brokering_but_not_fs_resolution() {
    let dependency = ctx(ScopeCapabilities::dependency());
    let fs = compile(&json!({"fs": {"$(fs-location)": "r"}}), &dependency)
        .expect("filesystem substitution is inert data in every source scope");
    assert!(fs
        .fs
        .rules
        .entries
        .iter()
        .any(|rule| rule.matcher.as_str().contains("/resolved/fs")));

    let substitution = compile(
        &json!({"vars": {"UV_CACHE_DIR": "$(cache-location)"}}),
        &ctx(ScopeCapabilities::dependency()),
    )
    .expect_err("dependency-authored config must not execute an env substitution");
    assert!(substitution.to_string().contains("not permitted"));

    let broker = compile(
        &json!({
            "net": ["registry.example.com"],
            "secrets": {"API_TOKEN": {"brokerTo": ["registry.example.com"]}}
        }),
        &ctx(ScopeCapabilities::dependency()),
    )
    .expect_err("dependency-authored config must not broker a parent secret");
    assert!(broker.to_string().contains("trusted-only"));
}

#[test]
fn malformed_or_removed_grammar_fails_before_it_can_broaden_access() {
    for surface in [
        json!({"unknown": true}),
        json!({"proxy": "terminate"}),
        json!({"fs": {"./work": false}}),
        json!({"fs": ["!./work"]}),
        json!({"fs": {"$tmp/work": "rw"}}),
        json!({"fs": {"$tooldirs/cache": "rw"}}),
        json!({"net": ["api.*.example.com"]}),
        json!({"net": ["$tooldirs"]}),
        json!({"vars": {"PORT": {"unexpected": true}}}),
        json!({"secrets": true}),
    ] {
        assert!(
            compile(&surface, &ctx(ScopeCapabilities::approved())).is_err(),
            "must reject {surface}"
        );
    }
}

#[test]
fn explicit_reuse_is_ordered_and_unlisted_axes_floor() {
    let document = json!({
        "shared": {
            "fs": {"./out": "rw"},
            "net": ["*", "!blocked.example.com"],
            "vars": ["PATH"]
        }
    });
    let policy = compile(
        &json!({
            "fs": {"...:#/shared/fs": true},
            "net": ["...:#/shared/net", "blocked.example.com"],
            "vars": ["...:#/shared/vars"]
        }),
        &ctx(ScopeCapabilities::approved()).with_document(document),
    )
    .expect("explicit list reuse compiles against the source document");
    assert_eq!(policy.fs.rules.default_effect, Effect::Deny);
    assert_eq!(
        policy.env.constructed.get("PATH"),
        Some(&"/usr/bin".to_string())
    );
    assert!(nub_sandbox::matcher::HostMatcher::new(&policy.net).admits("blocked.example.com"));

    let floored = compile(
        &json!({"fs": ["./out"]}),
        &ctx(ScopeCapabilities::approved()),
    )
    .expect("a partial object is a complete policy");
    assert!(floored.net.enforce);
    assert_eq!(floored.net.mode, ProxyMode::Disabled);
    assert!(floored.env.enforce);
    assert!(!floored.env.constructed.contains_key("PATH"));
}

#[test]
fn generated_build_jail_policy_is_positive_only_and_marks_its_provenance() {
    let homes = Homes {
        home: PathBuf::from("/home/sandbox"),
        cache: PathBuf::from("/cache"),
        tmp: PathBuf::from("/tmp"),
        project: PathBuf::from("/project"),
    };
    let policy = compile_build_jail(
        homes,
        std::path::Path::new("/project/node_modules/example"),
        Some("example"),
        Some("1.0.0"),
        vec![PathBuf::from("/toolchain/bin/node")],
        vec![PathBuf::from("/toolchain/include/node")],
        BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
    )
    .expect("generated build-jail policy compiles");
    assert!(policy.build_jail);
    assert_eq!(policy.fs.rules.default_effect, Effect::Deny);
    assert!(policy
        .fs
        .rules
        .entries
        .iter()
        .all(|rule| rule.effect == Effect::Allow));
    assert!(policy.fs.rules.entries.iter().any(|rule| {
        rule.matcher
            .as_str()
            .contains("/project/node_modules/example")
            && rule.access == FsAccess::ReadWrite
    }));
}
