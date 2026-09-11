#![cfg(windows)]

#[path = "common/tool_msys.rs"]
mod tool_msys;
#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "requires provisioned Node and NUB_SANDBOX_TOOL_MSYS_ROOT"]
fn msys_preserves_native_arguments_and_denied_file_paths() {
    assert!(std::env::var_os("NUB_SANDBOX_TOOL_MSYS_ROOT").is_some());
    let node = Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .unwrap();
    assert!(node.status.success());
    let node = PathBuf::from(String::from_utf8(node.stdout).unwrap().trim());
    let root = tempfile::Builder::new()
        .prefix("sandbox-msys-")
        .tempdir_in(std::env::var_os("USERPROFILE").unwrap())
        .unwrap();
    let project = root.path().join("project with spaces");
    std::fs::create_dir(&project).unwrap();
    let canary = root.path().join("withheld-secret");
    std::fs::write(&canary, "withheld").unwrap();
    let ambient: BTreeMap<String, String> = std::env::vars().collect();
    let context = CompileCtx::new(
        Homes {
            home: root.path().join("home"),
            cache: root.path().join("cache"),
            tmp: root.path().join("tmp"),
            project: project.clone(),
        },
        project.clone(),
        ScopeCapabilities::approved(),
        ambient,
    );
    let mut grants =
        json!({"./": "rw", "$tmp": "rw", node.parent().unwrap().to_str().unwrap(): "r"});
    tool_msys::grant(grants.as_object_mut().unwrap());
    let policy = compile(&json!({"fs": grants, "net": false}), &context).unwrap();
    let expected = [
        "",
        "spaces inside",
        "double \" and single '",
        "line\nbreak",
        r"C:\Users\fixture\cache",
        r#"{"path":"C:\\Users\\fixture"}"#,
        "$HOME;$(exit 99)",
        "/not-a-native-path",
    ];
    for confined in [false, true] {
        let source = format!(
            "const fs=require('fs');let denied=false;try{{fs.readFileSync({})}}catch(e){{if(!['EACCES','EPERM'].includes(e.code))throw e;denied=true}};console.log(JSON.stringify({{args:process.argv.slice(1),denied}}))",
            serde_json::to_string(&canary).unwrap()
        );
        let args = ["-e".to_owned(), source]
            .into_iter()
            .chain(expected.iter().map(|arg| (*arg).to_owned()))
            .collect();
        let (shell, args) = tool_msys::command(&node, args, &project);
        let output = if confined {
            let sandbox = Sandbox::with_windows_native_compat(&policy).unwrap();
            let prepared = sandbox
                .prepare(
                    CommandSpec::new(shell)
                        .args(args)
                        .cwd(&project)
                        .redact_stdout(true)
                        .redact_stderr(true),
                )
                .unwrap();
            assert!(prepared.degradation.lost.is_empty());
            let output = tool_output::output(prepared);
            sandbox.close();
            output
        } else {
            Command::new(shell)
                .args(args)
                .current_dir(&project)
                .env_clear()
                .envs(&policy.env.constructed)
                .output()
                .unwrap()
        };
        assert!(output.status.success(), "confined={confined}: {output:?}");
        let observed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(observed, json!({"args": expected, "denied": confined}));
    }
    nub_sandbox::cleanup().unwrap();
}
