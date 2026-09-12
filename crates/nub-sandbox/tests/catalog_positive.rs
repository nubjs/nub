use nub_sandbox::policy::{Effect, FsAccess};
use nub_sandbox::{
    CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile, compile_build_jail,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

const ENV_PROBE: &str = "SANDBOX_CATALOG_ENV_PROBE";
const BASELINE: [(&str, &str); 3] = [
    ("PYTHONDONTWRITEBYTECODE", "1"),
    ("npm_config_logs_max", "0"),
    ("npm_config_update_notifier", "false"),
];

#[test]
fn catalog_environment_child() {
    let Ok(mode) = std::env::var(ENV_PROBE) else {
        return;
    };
    assert!(matches!(mode.as_str(), "jail" | "ordinary"));
    for (name, baseline) in BASELINE {
        let expected = if mode == "jail" {
            baseline
        } else {
            "caller-value"
        };
        assert_eq!(std::env::var(name).as_deref(), Ok(expected), "{name}");
    }
    println!("CATALOG_ENV_OK:{mode}");
}

#[test]
fn baked_environment_reaches_real_children_without_changing_ordinary_policies() {
    let (_root, _policy, homes) = policy_for("__catalog_env_fixture__", "1.0.0");
    let executable = std::env::current_exe().unwrap();
    let mut ambient: BTreeMap<String, String> = std::env::vars().collect();
    for (name, _) in BASELINE {
        ambient.insert(name.into(), "caller-value".into());
    }
    let jail = compile_build_jail(
        homes.clone(),
        &homes.project.join("node_modules/fixture"),
        Some("__catalog_env_fixture__"),
        Some("1.0.0"),
        vec![executable.clone()],
        Vec::new(),
        ambient.clone(),
    )
    .unwrap();
    let context = CompileCtx::new(
        homes.clone(),
        homes.project.clone(),
        ScopeCapabilities::approved(),
        ambient,
    );
    let ordinary = compile(
        &json!({"fs": ["./", "$tmp"], "net": false, "vars": true}),
        &context,
    )
    .unwrap();
    for (mode, mut policy) in [("jail", jail), ("ordinary", ordinary)] {
        policy.env.constructed.insert(ENV_PROBE.into(), mode.into());
        let sandbox = Sandbox::new(&policy).unwrap();
        let prepared = sandbox
            .prepare(
                CommandSpec::new(&executable)
                    .args(["--exact", "catalog_environment_child", "--nocapture"])
                    .cwd(&homes.project),
            )
            .unwrap();
        assert!(
            prepared.degradation.lost.is_empty(),
            "{mode}: {:?}",
            prepared.degradation
        );
        let output = prepared.output().unwrap();
        sandbox.close();
        assert!(output.status.success(), "{mode}: {output:?}");
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains(&format!("CATALOG_ENV_OK:{mode}"))
        );
    }
}

fn policy_for(
    package: &str,
    version: &str,
) -> (tempfile::TempDir, nub_sandbox::SandboxPolicy, Homes) {
    let root = tempfile::tempdir().expect("fixture root");
    let home = root.path().join("home");
    let project = root.path().join("project");
    let cache = root.path().join("cache");
    let package_dir = project.join("node_modules").join("fixture");
    for path in [&home, &project, &cache, &package_dir] {
        std::fs::create_dir_all(path).expect("fixture directory");
    }
    let homes = Homes {
        home,
        cache,
        tmp: root.path().join("tmp"),
        project,
    };
    let policy = compile_build_jail(
        homes.clone(),
        &package_dir,
        Some(package),
        Some(version),
        vec![PathBuf::from(if cfg!(windows) {
            "C:/Windows/System32/cmd.exe"
        } else {
            "/bin/sh"
        })],
        Vec::new(),
        BTreeMap::new(),
    )
    .expect("catalog build-jail policy compiles");
    (root, policy, homes)
}

#[test]
fn catalog_user_home_grant_includes_a_credential_canary() {
    let (_root, policy, homes) = policy_for("@pulumi/aws-native", "1.0.0");
    let canary = homes.home.join(".npmrc");
    let matcher = nub_sandbox::matcher::PathMatcher::new(&policy.fs.rules);

    assert_eq!(
        matcher.decide(&canary).effect,
        Effect::Allow,
        "the real catalog userHome grant must be literal, including credential-bearing files"
    );
    assert_eq!(
        matcher.decide(&canary).access,
        FsAccess::ReadWrite,
        "the userHome catalog grant must remain writable"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn catalog_disk_read_is_a_literal_read_only_root_grant() {
    let (_root, policy, _homes) = policy_for("@mui/x-telemetry", "1.0.0");

    assert!(
        policy.fs.rules.entries.iter().any(|rule| {
            rule.effect == Effect::Allow
                && rule.access == FsAccess::Read
                && rule.matcher.as_str() == "**"
        }),
        "the real read:disk catalog entry must emit a literal root read grant"
    );
    assert_eq!(
        policy.fs.rules.default_effect,
        Effect::Deny,
        "read:disk must not make the filesystem writable"
    );
}
