use nub_sandbox::conformance::{Fixture, run_fixture};
use nub_sandbox::policy::{Effect, FsAccess, Inspection, ProxyMode};
use nub_sandbox::{
    CommandRunner, CompileCtx, Homes, ScopeCapabilities, compile, compile_build_jail,
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
    let mut ctx = CompileCtx::new(
        Homes {
            home: PathBuf::from("/home/sandbox"),
            cache: PathBuf::from("/home/sandbox/.cache"),
            tmp: PathBuf::from("/tmp/nub-private"),
            project: PathBuf::from("/project"),
        },
        PathBuf::from("/project"),
        caps,
        BTreeMap::from([
            // Required by README JSON example 8 (`vars.HOME: true`).
            ("HOME".to_string(), "/home/sandbox".to_string()),
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

#[test]
fn public_readme_json_examples_compile() {
    let mut count = 0;
    for block in include_str!("../README.md").split("```json\n").skip(1) {
        let example = block.split("```").next().expect("every JSON fence closes");
        let surface = serde_json::from_str(example)
            .unwrap_or_else(|error| panic!("README JSON must parse:\n{example}\n{error}"));
        compile(&surface, &ctx(ScopeCapabilities::approved()))
            .unwrap_or_else(|error| panic!("README policy must compile:\n{example}\n{error}"));
        count += 1;
    }
    assert!(
        count >= 8,
        "the README grammar examples must remain covered"
    );
}

#[test]
fn grammar_preserves_positive_fs_merging_net_order_and_env_provenance() {
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
            {"path": "/project/output/logs/build.log", "read": true, "write": true},
            {"path": "/home/sandbox/.config/tool/config", "read": true, "write": false},
            {"path": "/home/sandbox/.cache/tool/cache", "read": true, "write": true}
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
    assert!(run_fixture(&fixture, &ctx(ScopeCapabilities::approved())).is_empty());

    let policy = compile(&fixture.sandbox, &ctx(ScopeCapabilities::approved())).unwrap();
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
    assert!(
        fs.fs
            .rules
            .entries
            .iter()
            .any(|rule| rule.matcher.as_str().contains("/resolved/fs"))
    );

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
    assert!(
        policy
            .fs
            .rules
            .entries
            .iter()
            .all(|rule| rule.effect == Effect::Allow)
    );
    assert!(policy.fs.rules.entries.iter().any(|rule| {
        rule.matcher
            .as_str()
            .contains("/project/node_modules/example")
            && rule.access == FsAccess::ReadWrite
    }));
}
