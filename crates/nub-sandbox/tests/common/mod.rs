//! Shared test scaffolding: a deterministic `$(…)` runner and a `CompileCtx`
//! builder over fixed home anchors + a controlled ambient env.

use nub_sandbox::CommandRunner;
use nub_sandbox::compiler::{CompileCtx, ScopeCapabilities};
use nub_sandbox::matcher::Homes;
use serde_json::Value;
use std::collections::BTreeMap;

/// A `$(…)` runner that echoes a deterministic marker so substitution is testable
/// without spawning a real shell.
pub struct StubRunner;
impl CommandRunner for StubRunner {
    fn run(&self, command: &str) -> Result<String, String> {
        // A couple of recognized commands return fixed values; everything else
        // echoes a marker so a test can assert the command reached the runner.
        match command {
            "echo hi" => Ok("hi\n".to_string()),
            "fail" => Err("stub failure".to_string()),
            // Path-shaped stubs for fs `$(…)` tests: a clean single-line path (with
            // the usual trailing newline), empty output, and multi-line output.
            "store path" => Ok("/home/u/.store\n".to_string()),
            "empty path" => Ok("\n".to_string()),
            "two paths" => Ok("/home/u/a\n/home/u/b\n".to_string()),
            other => Ok(format!("STUB[{other}]")),
        }
    }
}

/// Fixed, OS-agnostic home anchors for compiler/IR tests. Absolute so the glob
/// prefix-canonicalization has something to anchor on.
pub fn homes() -> Homes {
    #[cfg(windows)]
    {
        Homes {
            home: "C:/Users/u".into(),
            tmp: "C:/Temp".into(),
            cache: "C:/Users/u/.cache".into(),
            project: "C:/proj".into(),
        }
    }
    #[cfg(not(windows))]
    {
        Homes {
            home: "/home/u".into(),
            tmp: "/tmp".into(),
            cache: "/home/u/.cache".into(),
            project: "/proj".into(),
        }
    }
}

/// Build a ctx with the given trust + ambient env pairs. `trusted` maps to the
/// single-block scope capabilities: approved (full) vs dependency-controlled (none).
/// `document` is `Null` — a policy with no `...:#/pointer` reuse; use
/// [`ctx_with_document`] to supply the whole document for a reuse test.
pub fn ctx(trusted: bool, env: &[(&str, &str)]) -> CompileCtx {
    ctx_with_document(trusted, env, Value::Null)
}

/// Build a ctx whose `document` is the whole policy document — required to resolve a
/// `...:#/pointer` reuse token (`#` = document root). The surface passed to `compile`
/// is typically `document["sandbox"]` (or a sub-node).
pub fn ctx_with_document(trusted: bool, env: &[(&str, &str)], document: Value) -> CompileCtx {
    let ambient: BTreeMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    CompileCtx {
        homes: homes(),
        cwd: homes().project,
        policy_file: None,
        caps: caps(trusted),
        ambient_env: ambient,
        document,
        runner: Box::new(StubRunner),
    }
}

/// The `ScopeCapabilities` for a trusted/untrusted scope — approved user config gets
/// the full set, a dependency-controlled scope none. Exposed so chain tests can label
/// each scope's identity independently.
pub fn caps(trusted: bool) -> ScopeCapabilities {
    if trusted {
        ScopeCapabilities::approved()
    } else {
        ScopeCapabilities::dependency()
    }
}
