//! Proves the zero-privilege supervised launch enforces PER-HOST egress through the loopback
//! SNI-inspecting proxy (epic 5.1), driving nub-sandbox's REAL public API (`compile` → `apply` →
//! `status`). A fine-grained `net` allowlist derives `ProxyMode::Auto`, so `apply` starts the proxy
//! and the supervisor redirects each non-loopback connect through it via the cooperative HTTP
//! CONNECT — the child never cooperates (no `HTTP_PROXY`; `--noproxy '*'` too), so a block is the
//! OS-level supervisor interception, never client good-behavior.
//!
//! The arms, with the SNI gate isolated by a same-IP discriminator:
//!   1. compat        — allow example.com, reach https://example.com            → expect 0
//!   2. attack-deny   — allow example.com, reach https://www.google.com         → expect != 0 (gate 1)
//!   3. attack-sni    — connect to example.com's IP but send SNI www.google.com → expect != 0 (gate 2)
//!   4. control-sni   — connect to example.com's IP and send SNI example.com    → expect 0
//! Arms 3 and 4 hit the SAME destination IP (example.com's, via `--connect-to`/direct) and differ
//! ONLY in the ClientHello SNI, so arm 3's block is the proxy's SNI gate — not the IP, not the
//! observed DNS name (which is example.com, ALLOWED, in both). This is the CDN-shared-IP leak that
//! the coarse observed-IP path (epic 1.5) could not close.
//!
//! `SUP PROXY ... -> 127.0.0.1:<port>` on stderr (the harness sets `NUB_SANDBOX_SUP_DEBUG`) proves
//! the redirect fired — gate 1 admitted example.com's authority — so arm 3's failure is gate 2.

use nub_sandbox::{apply, compile, CommandSpec, CompileCtx, Homes, SandboxPolicy, ScopeCapabilities};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn policy(surface: Value) -> SandboxPolicy {
    let homes = Homes {
        home: "/root".into(),
        tmp: "/tmp".into(),
        cache: "/tmp".into(),
        project: "/tmp".into(),
    };
    let mut env = BTreeMap::new();
    for key in ["PATH", "HOME"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    let ctx = CompileCtx::new(homes, "/tmp".into(), ScopeCapabilities::approved(), env);
    compile(&surface, &ctx).expect("compile net policy")
}

/// Run `curl` (via `/bin/sh -c`) under `policy`; return its exit code. `--noproxy '*'` makes curl
/// dial the destination DIRECTLY (ignoring any proxy env), so the ONLY thing routing it through the
/// proxy is the supervisor's transparent redirect — the non-cooperative case A1 requires.
fn curl(label: &str, policy: &SandboxPolicy, curl_args: &str) -> i32 {
    let script = format!(
        "curl -4 -sS --noproxy '*' -o /dev/null --connect-timeout 8 --max-time 20 {curl_args}"
    );
    eprintln!(">>> {label}: sh -c {script:?}");
    let spec = CommandSpec::new("/bin/sh").arg("-c").arg(&script);
    let code = apply(policy, spec)
        .expect("apply policy")
        .status()
        .expect("run supervised child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

fn main() {
    // Turn on the supervisor decision trace so `SUP PROXY` on stderr confirms the redirect fired.
    unsafe { std::env::set_var("NUB_SANDBOX_SUP_DEBUG", "1") };

    let allow_example = policy(json!({ "fs": true, "net": ["example.com"] }));

    // 1. compat: an ALLOWED host is reachable through the redirect.
    let compat = curl("compat      ", &allow_example, "https://example.com/");

    // 2. attack-deny: a DENIED host is blocked at the proxy's host gate (gate 1).
    let attack_deny = curl("attack-deny ", &allow_example, "https://www.google.com/");

    // 3. attack-sni: connect to example.com's IP (observed name example.com, ALLOWED → gate 1
    //    passes, SUP PROXY logged) but present SNI www.google.com → the proxy's SNI gate (gate 2)
    //    denies. This is the shared-IP leak the coarse path left open.
    let attack_sni = curl(
        "attack-sni  ",
        &allow_example,
        "--connect-to www.google.com:443:example.com:443 https://www.google.com/",
    );

    // 4. control-sni: SAME destination IP as arm 3, SNI example.com → allowed. Isolates the SNI
    //    gate as arm 3's decider (identical IP + observed name; only the SNI differs).
    let control_sni = curl(
        "control-sni ",
        &allow_example,
        "--connect-to example.com:443:example.com:443 https://example.com/",
    );

    println!();
    println!("1 compat       (allow example.com, GET example.com)            -> exit={compat}   [want 0]");
    println!("2 attack-deny  (allow example.com, GET google)                 -> exit={attack_deny}   [want != 0]");
    println!("3 attack-sni   (example.com IP, SNI=google)                    -> exit={attack_sni}   [want != 0]");
    println!("4 control-sni  (example.com IP, SNI=example.com)               -> exit={control_sni}   [want 0]");

    let pass = compat == 0 && attack_deny != 0 && attack_sni != 0 && control_sni == 0;
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
