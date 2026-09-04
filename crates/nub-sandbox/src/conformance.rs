//! The conformance harness scaffold: a committed fixture is a surface `sandbox`
//! block plus an assertion manifest (expected allow/deny per axis), and the
//! runner compiles the block and diffs the DECIDED verdicts against the manifest.
//!
//! STAGE 1 evaluates the COMPILER + MATCHER decisions (does the policy admit this
//! path/host/env key?) with no OS in the loop — the engine-pure half of
//! conformance, runnable on any host. The later backends reuse the SAME fixtures
//! against a real probe program (the OS-level half), so the fixture format is the
//! shared contract. Fixtures deserialize from JSON so they are pure data.

use crate::compiler::CompileCtx;
use crate::matcher::{HostMatcher, PathMatcher};
use crate::policy::{Effect, FsAccess};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

/// A conformance fixture: one surface policy + its expected verdicts.
#[derive(Debug, Clone, Deserialize)]
pub struct Fixture {
    pub name: String,
    /// The surface `sandbox` block under test.
    pub sandbox: Value,
    #[serde(default)]
    pub fs: Vec<FsCase>,
    #[serde(default)]
    pub net: Vec<NetCase>,
    #[serde(default)]
    pub env: Vec<EnvCase>,
}

/// An fs probe: a candidate path and its expected read/write verdict.
#[derive(Debug, Clone, Deserialize)]
pub struct FsCase {
    pub path: String,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
}

/// A net probe: a host/IP and whether egress is expected to be admitted.
#[derive(Debug, Clone, Deserialize)]
pub struct NetCase {
    pub host: String,
    pub admit: bool,
}

/// An env probe: a key, whether it should be present in the child env, and
/// (optionally) its expected value.
#[derive(Debug, Clone, Deserialize)]
pub struct EnvCase {
    pub key: String,
    pub present: bool,
    #[serde(default)]
    pub value: Option<String>,
}

/// A single divergence between an expected verdict and the decided one.
#[derive(Debug, Clone, PartialEq)]
pub struct Mismatch {
    pub axis: &'static str,
    pub subject: String,
    pub expected: String,
    pub actual: String,
}

/// Run a fixture through compile → decide and return every mismatch (empty = the
/// fixture passes). A compile error is surfaced as a single mismatch so a
/// fixture asserting a policy that should compile fails loudly rather than
/// panicking.
pub fn run_fixture(fixture: &Fixture, ctx: &CompileCtx) -> Vec<Mismatch> {
    let policy = match crate::compile(&fixture.sandbox, ctx) {
        Ok(p) => p,
        Err(e) => {
            return vec![Mismatch {
                axis: "compile",
                subject: fixture.name.clone(),
                expected: "a policy that compiles".to_string(),
                actual: e.to_string(),
            }];
        }
    };
    let mut out = Vec::new();

    let fs = PathMatcher::new(&policy.fs.rules);
    for case in &fixture.fs {
        let d = fs.decide(&PathBuf::from(&case.path));
        let readable = matches!(d.effect, Effect::Allow);
        let writable = readable && matches!(d.access, FsAccess::ReadWrite);
        if readable != case.read {
            out.push(Mismatch {
                axis: "fs.read",
                subject: case.path.clone(),
                expected: case.read.to_string(),
                actual: readable.to_string(),
            });
        }
        if writable != case.write {
            out.push(Mismatch {
                axis: "fs.write",
                subject: case.path.clone(),
                expected: case.write.to_string(),
                actual: writable.to_string(),
            });
        }
    }

    let net = HostMatcher::new(&policy.net);
    for case in &fixture.net {
        let admit = net.admits(&case.host);
        if admit != case.admit {
            out.push(Mismatch {
                axis: "net",
                subject: case.host.clone(),
                expected: case.admit.to_string(),
                actual: admit.to_string(),
            });
        }
    }

    for case in &fixture.env {
        let got = policy.env.constructed.get(&case.key);
        // When env does not enforce, every key is inherited → treat as present.
        let present = if policy.env.enforce {
            got.is_some()
        } else {
            ctx.ambient_env.contains_key(&case.key)
        };
        if present != case.present {
            out.push(Mismatch {
                axis: "env.present",
                subject: case.key.clone(),
                expected: case.present.to_string(),
                actual: present.to_string(),
            });
        }
        if let Some(expected_val) = &case.value {
            let actual_val = got.cloned().unwrap_or_default();
            if &actual_val != expected_val {
                out.push(Mismatch {
                    axis: "env.value",
                    subject: case.key.clone(),
                    expected: expected_val.clone(),
                    actual: actual_val,
                });
            }
        }
    }
    out
}
