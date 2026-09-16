use nub_sandbox::policy::FsAccess;
use nub_sandbox::{CompileCtx, Homes, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ToolPathRunner {
    path: String,
    calls: Arc<AtomicUsize>,
}

impl nub_sandbox::CommandRunner for ToolPathRunner {
    fn run(&self, command: &str) -> Result<String, String> {
        assert_eq!(command, "tool-cache-location");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.path.clone())
    }
}

fn ctx(env: &[(&str, &str)]) -> CompileCtx {
    CompileCtx::new(
        Homes {
            home: PathBuf::from("/home/sandbox"),
            tmp: PathBuf::from("/tmp/nub-private"),
            cache: PathBuf::from("/home/sandbox/.cache"),
            project: PathBuf::from("/project"),
        },
        PathBuf::from("/project"),
        ScopeCapabilities::approved(),
        env.iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[test]
fn home_alias_and_cache_expand_to_their_compiler_anchors() {
    let policy = compile(
        &json!({"fs": {"$home/.config/tool": "r", "$cache/tool": "rw"}}),
        &ctx(&[]),
    )
    .expect("the four-root grammar compiles");
    let rules: Vec<_> = policy
        .fs
        .rules
        .entries
        .iter()
        .map(|rule| rule.matcher.as_str())
        .collect();
    assert!(
        rules
            .iter()
            .any(|rule| rule.contains("/home/sandbox/.config/tool"))
    );
    assert!(
        rules
            .iter()
            .any(|rule| rule.contains("/home/sandbox/.cache/tool"))
    );
    assert!(policy.fs.rules.entries.iter().any(|rule| {
        rule.matcher.as_str().contains("/home/sandbox/.config/tool")
            && rule.access == FsAccess::Read
    }));
    assert!(policy.fs.rules.entries.iter().any(|rule| {
        rule.matcher.as_str().contains("/home/sandbox/.cache/tool")
            && rule.access == FsAccess::ReadWrite
    }));
}

#[test]
fn tooldirs_adds_nonempty_documented_environment_relocations() {
    let policy = compile(
        &json!({"fs": {"$tooldirs": "r"}}),
        &ctx(&[
            ("npm_config_cache", "/relocated/npm-cache"),
            ("PNPM_HOME", "/relocated/pnpm-home"),
            ("BUN_INSTALL_CACHE_DIR", "/relocated/bun-cache"),
            ("XDG_DATA_HOME", "/relocated/data"),
            ("YARN_CACHE_FOLDER", ""),
        ]),
    )
    .expect("documented relocations compile without tool discovery");
    let rules: Vec<_> = policy
        .fs
        .rules
        .entries
        .iter()
        .map(|rule| rule.matcher.as_str())
        .collect();
    for expected in [
        "/relocated/npm-cache",
        "/relocated/pnpm-home",
        "/relocated/bun-cache",
        "/relocated/data/pnpm",
    ] {
        assert!(
            rules.iter().any(|rule| rule.contains(expected)),
            "missing {expected}: {rules:?}"
        );
        assert!(policy.fs.rules.entries.iter().any(|rule| {
            rule.matcher.as_str().contains(expected) && rule.access == FsAccess::Read
        }));
    }
    assert!(!rules.iter().any(|rule| rule.contains("YARN_CACHE_FOLDER")));
    assert!(!policy.env.constructed.contains_key("PNPM_HOME"));
}

#[test]
fn tooldirs_uses_resolved_child_environment_once_including_reused_lists() {
    for fs in [
        json!(["$tooldirs"]),
        json!({"$tooldirs": "rw"}),
        json!(["...:#/shared/array"]),
        json!({"...:#/shared/object": true}),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut ctx = ctx(&[("UV_CACHE_DIR", "/previous/cache")]).with_document(json!({
            "shared": {"array": ["$tooldirs"], "object": {"$tooldirs": "rw"}}
        }));
        ctx.runner = Box::new(ToolPathRunner {
            path: "/resolved/cache".into(),
            calls: calls.clone(),
        });
        let policy = compile(
            &json!({"fs": fs, "vars": {"UV_CACHE_DIR": "$(tool-cache-location)"}}),
            &ctx,
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(policy.env.constructed["UV_CACHE_DIR"], "/resolved/cache");
        let rules: Vec<_> = policy
            .fs
            .rules
            .entries
            .iter()
            .map(|r| r.matcher.as_str())
            .collect();
        assert!(
            rules.iter().any(|p| p.contains("/resolved/cache")),
            "{rules:?}"
        );
        assert!(!rules.iter().any(|p| p.contains("/previous/cache")));
        assert!(!rules.contains(&"/resolved/**"));
    }
}

#[test]
fn tooldirs_keeps_substitution_scope_and_root_validation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = ctx(&[]);
    ctx.runner = Box::new(ToolPathRunner {
        path: "/".into(),
        calls: calls.clone(),
    });
    let value = json!({"fs": ["$tooldirs"], "vars": {"UV_CACHE_DIR": "$(tool-cache-location)"}});
    ctx.caps = ScopeCapabilities::dependency();
    assert!(matches!(
        compile(&value, &ctx),
        Err(nub_sandbox::CompileError::UntrustedSubstitution { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    ctx.caps = ScopeCapabilities::approved();
    let error = compile(&value, &ctx).unwrap_err().to_string();
    assert!(error.contains("filesystem root"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[cfg(windows)]
#[test]
fn tooldirs_honors_case_insensitive_windows_environment_overrides() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = ctx(&[
        ("UV_CACHE_DIR", "C:/previous/cache"),
        ("LocalAppData", "C:/redirected/local"),
    ]);
    ctx.runner = Box::new(ToolPathRunner {
        path: "C:/resolved/cache".into(),
        calls,
    });
    let policy = compile(
        &json!({"fs": ["$tooldirs"], "vars": {"uv_cache_dir": "$(tool-cache-location)"}}),
        &ctx,
    )
    .unwrap();
    let rules: Vec<_> = policy
        .fs
        .rules
        .entries
        .iter()
        .map(|r| r.matcher.as_str())
        .collect();
    assert!(
        rules.iter().any(|p| p.contains("C:/resolved/cache")),
        "{rules:?}"
    );
    assert!(
        rules
            .iter()
            .any(|p| p.contains("C:/redirected/local/npm-cache"))
    );
    assert!(!rules.iter().any(|p| p.contains("C:/previous/cache")));
}

#[test]
fn tooldirs_covers_neutral_pm_storage_overrides() {
    for variable in [
        "NUB_CACHE_DIR",
        "npm_config_cache_dir",
        "NPM_CONFIG_CACHE_DIR",
        "npm_config_store_dir",
        "NPM_CONFIG_STORE_DIR",
        "npm_config_virtual_store_dir",
        "NPM_CONFIG_VIRTUAL_STORE_DIR",
        "npm_config_global_virtual_store_dir",
        "NPM_CONFIG_GLOBAL_VIRTUAL_STORE_DIR",
    ] {
        let policy = compile(
            &json!({"fs": {"$tooldirs": "rw"}}),
            &ctx(&[(variable, "/relocated/pm-storage")]),
        )
        .unwrap();
        assert!(
            policy.fs.rules.entries.iter().any(|rule| {
                rule.matcher.as_str() == "/relocated/pm-storage/**"
                    && rule.access == FsAccess::ReadWrite
            }),
            "missing {variable}"
        );
        assert!(
            !policy
                .fs
                .rules
                .entries
                .iter()
                .any(|rule| { rule.matcher.as_str() == "/relocated/**" })
        );
    }
}

#[test]
fn tooldirs_preserves_literal_whitespace_and_anchors_relative_relocations_once() {
    let policy = compile(
        &json!({"fs": ["$tooldirs"]}),
        &ctx(&[("NPM_CONFIG_CACHE", " cache with spaces ")]),
    )
    .expect("a literal relative environment path compiles");
    let rules: Vec<_> = policy
        .fs
        .rules
        .entries
        .iter()
        .map(|rule| rule.matcher.as_str())
        .collect();
    assert!(
        rules
            .iter()
            .any(|rule| rule.contains("/project/ cache with spaces "))
    );
    assert!(
        !rules
            .iter()
            .any(|rule| rule.contains("/project/cache with spaces"))
    );
}

#[test]
fn tooldirs_splits_list_valued_gopath() {
    let gopath = std::env::join_paths(["/relocated/go-one", "/relocated/go-two"])
        .expect("test paths join on this platform")
        .into_string()
        .expect("test paths are utf-8");
    let policy = compile(&json!({"fs": ["$tooldirs"]}), &ctx(&[("GOPATH", &gopath)]))
        .expect("list-valued GOPATH compiles");
    let rules: Vec<_> = policy
        .fs
        .rules
        .entries
        .iter()
        .map(|rule| rule.matcher.as_str())
        .collect();
    for expected in ["/relocated/go-one", "/relocated/go-two"] {
        assert!(rules.iter().any(|rule| rule.contains(expected)));
    }
}

#[test]
fn tooldirs_rejects_environment_roots_instead_of_granting_the_disk() {
    for root in ["/", "C:/"] {
        assert!(
            compile(
                &json!({"fs": ["$tooldirs"]}),
                &ctx(&[("NPM_CONFIG_CACHE", root)]),
            )
            .is_err(),
            "must reject relocation root {root}"
        );
    }
}

#[cfg(windows)]
#[test]
fn tooldirs_uses_redirected_windows_profile_roots() {
    let policy = compile(
        &json!({"fs": {"$tooldirs": "r"}}),
        &ctx(&[("LOCALAPPDATA", "C:/redirected/local-app-data")]),
    )
    .expect("redirected Windows roots compile");
    assert!(policy.fs.rules.entries.iter().any(|rule| {
        rule.matcher
            .as_str()
            .contains("C:/redirected/local-app-data/npm-cache")
    }));
}

#[cfg(windows)]
#[test]
fn tooldirs_includes_nuget_under_both_redirected_windows_roots() {
    let policy = compile(
        &json!({"fs": {"$tooldirs": "r"}}),
        &ctx(&[
            ("LOCALAPPDATA", "C:/redirected/local"),
            ("APPDATA", "C:/redirected/roaming"),
        ]),
    )
    .expect("redirected NuGet roots compile");
    for root in ["C:/redirected/local", "C:/redirected/roaming"] {
        assert!(policy.fs.rules.entries.iter().any(|rule| {
            rule.matcher.as_str() == format!("{root}/NuGet/**") && rule.access == FsAccess::Read
        }));
        assert!(!policy.fs.rules.entries.iter().any(|rule| {
            rule.matcher.as_str() == root || rule.matcher.as_str() == format!("{root}/**")
        }));
        assert!(policy.fs.rules.entries.iter().any(|rule| {
            rule.matcher.as_str() == format!("{root}/nub/**") && rule.access == FsAccess::Read
        }));
    }
}

#[test]
fn managed_tmp_and_user_denies_fail_loudly() {
    for surface in [
        json!({"fs": ["$tmp/subdir"]}),
        json!({"fs": {"$tmp": "r"}}),
        json!({"fs": ["!/private"]}),
        json!({"fs": {"/private": false}}),
        json!({"fs": ["!$tooldirs"]}),
        json!({"fs": {"$tooldirs": false}}),
        json!({"fs": ["$unknown"]}),
    ] {
        assert!(
            compile(&surface, &ctx(&[])).is_err(),
            "must reject {surface}"
        );
    }
}

#[test]
fn fs_false_grants_no_authored_paths() {
    let policy = compile(&json!({"fs": false}), &ctx(&[])).expect("fs false compiles");
    assert_eq!(
        policy.fs.rules.default_effect,
        nub_sandbox::policy::Effect::Deny
    );
    assert!(policy.fs.rules.entries.is_empty());
}

#[test]
fn tooldirs_include_git_config_and_its_atomic_lock() {
    let policy = compile(&json!({"fs": {"$tooldirs": "rw"}}), &ctx(&[]))
        .expect("default tool roots compile");
    let rules: Vec<_> = policy
        .fs
        .rules
        .entries
        .iter()
        .map(|rule| rule.matcher.as_str())
        .collect();
    for expected in [".gitconfig", ".gitconfig.lock"] {
        assert!(rules.iter().any(|rule| rule.contains(expected)));
        assert!(policy.fs.rules.entries.iter().any(|rule| {
            rule.matcher.as_str().contains(expected) && rule.access == FsAccess::ReadWrite
        }));
        assert!(policy.fs.rules.entries.iter().any(|rule| {
            rule.matcher.as_str().ends_with(expected) && !rule.matcher.as_str().contains("/**")
        }));
    }
}

#[test]
fn tooldirs_include_the_current_os_js_package_manager_layouts() {
    let policy = compile(&json!({"fs": ["$tooldirs"]}), &ctx(&[]))
        .expect("current static tool layouts compile");
    let rules: Vec<_> = policy
        .fs
        .rules
        .entries
        .iter()
        .map(|rule| rule.matcher.as_str())
        .collect();
    let expected: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/.npm",
            "/Library/pnpm",
            "/Library/Caches/Yarn",
            "/.bun/install",
        ]
    } else if cfg!(windows) {
        &[
            "/AppData/Local/npm-cache",
            "/AppData/Local/pnpm",
            "/AppData/Local/Yarn",
            "/.bun/install",
        ]
    } else {
        &[
            "/.npm",
            "/.local/share/pnpm",
            "/.cache/yarn",
            "/.bun/install",
        ]
    };
    for expected in expected {
        assert!(rules.iter().any(|rule| rule.contains(expected)));
    }
}

/// An authored filesystem policy subtracts the secret floor and the policy file, and NOTHING
/// else. Both are the deliberate exceptions to an otherwise positive-only axis: a read grant is
/// meant to hand over the source, not the credentials sitting in it, and a confined command has
/// no business reading the rules that confine it.
///
/// This replaces an assertion that the axis carried no deny AT ALL, which pinned a 2026-09-08
/// narrowing of the floor to the (since-deleted) secure preset. Restored per `sandbox-epic.md`
/// 0c, which calls the floor load-bearing for `nub sandbox` specifically.
///
/// Asserted as an exact set rather than "some deny exists", because the risk worth guarding is a
/// future change quietly subtracting something ELSE from a policy the author wrote positively.
#[test]
fn an_authored_filesystem_policy_subtracts_the_secret_floor_and_the_policy_file_only() {
    let policy = compile(
        &json!({"fs": ["."]}),
        &ctx(&[]).with_policy_files(vec![PathBuf::from("/project/policy.jsonc")]),
    )
    .expect("an authored filesystem policy compiles");
    let denied: Vec<_> = policy
        .fs
        .rules
        .entries
        .iter()
        .filter(|rule| rule.effect == nub_sandbox::policy::Effect::Deny)
        .map(|rule| rule.matcher.as_str().to_string())
        .collect();
    assert_eq!(
        denied,
        vec![
            "/project/policy.jsonc",
            "**/.[eE][nN][vV]*",
            ".[eE][nN][vV]*",
            "**/.[nN][pP][mM][rR][cC]",
            ".[nN][pP][mM][rR][cC]",
            "**/[nN][oO][dD][eE]_[mM][oO][dD][uU][lL][eE][sS]/[nN][pP][mM]/[nN][pP][mM][rR][cC]",
            "[nN][oO][dD][eE]_[mM][oO][dD][uU][lL][eE][sS]/[nN][pP][mM]/[nN][pP][mM][rR][cC]",
            "**/.[eE][nN][vV]*/**",
            ".[eE][nN][vV]*/**",
            "/proc/*/environ",
            "/proc/*/mem",
            "/proc/*/maps",
            "/proc/*/smaps",
            "/proc/*/task/*/environ",
            "/proc/*/task/*/mem",
            "/proc/*/task/*/maps",
        ],
        "the policy file is subtracted first, then the case-folded secret floor (leaf, then \
         subtree), then the cross-process /proc secret band",
    );
    assert!(
        policy
            .fs
            .rules
            .entries
            .iter()
            .filter(|rule| rule.effect == nub_sandbox::policy::Effect::Allow)
            .any(|rule| rule.matcher.as_str().contains("project")),
        "the authored grant itself must survive: {:?}",
        policy.fs.rules.entries,
    );
}
