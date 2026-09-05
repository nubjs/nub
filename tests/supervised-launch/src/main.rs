//! Proves the zero-privilege supervised launch path enforces per-host egress through
//! nub-sandbox's REAL public API: a net policy is compiled, applied, and run, and the
//! confined child (curl) reaches only the allowlisted host.
//!
//! Three arms, so the block is attributable to the allowlist and not to a broken path:
//!   1. compat — allow `example.com`, hit `example.com`      → expect success
//!   2. attack — allow `example.com`, hit `cloudflare.com`   → expect FAILURE (blocked at connect)
//!   3. control — allow `cloudflare.com`, hit `cloudflare.com` → expect success
//! Arms 1 and 3 prove the supervised path DOES reach a host when the policy allows it, so arm 2's
//! failure is the deny, not a path defect. `example.com` and `cloudflare.com` have distinct IPs, so
//! the DNS-attribution allowlist cannot conflate them.

use nub_sandbox::{apply, compile, CommandSpec, CompileCtx, Homes, SandboxPolicy, ScopeCapabilities};
use serde_json::json;
use std::collections::BTreeMap;

fn policy_allowing(hosts: &[&str]) -> SandboxPolicy {
    let homes = Homes {
        home: "/root".into(),
        tmp: "/tmp".into(),
        cache: "/tmp".into(),
        project: "/tmp".into(),
    };
    // The child env is replaced by `constructed`; carry a real PATH/HOME so curl behaves.
    let mut env = BTreeMap::new();
    for key in ["PATH", "HOME"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    let ctx = CompileCtx::new(homes, "/tmp".into(), ScopeCapabilities::approved(), env);
    // `fs: true` fully RELAXES the filesystem axis (default-allow, no rules, shared tmp), so this
    // exercises the NET axis in isolation. (`fs: false` means deny-all, and an unlisted `fs` floors
    // to secure defaults — both would be refused fail-closed, since the supervised path does not
    // yet carry an fs boundary; epic 1.1d.)
    let surface = json!({ "fs": true, "net": hosts });
    compile(&surface, &ctx).expect("compile net policy")
}

fn run(label: &str, policy: &SandboxPolicy, url: &str) -> i32 {
    eprintln!(">>> {label}: launching curl {url}");
    // Plain HTTP over IPv4: the supervisor's transparent-splice path is proven for HTTP; the TLS
    // handshake's send/recv pattern over a spliced socket is a separate, unverified path (a hang
    // observed with https — recorded as a supervisor-hardening finding, epic 1.3/1.4). `-4` because
    // the VM has no IPv6, so the supervisor's IPv6 dials stall happy-eyeballs.
    let spec = CommandSpec::new("/usr/bin/curl")
        .arg("-sS")
        .arg("-4")
        .arg("-o")
        .arg("/dev/null")
        .arg("--connect-timeout")
        .arg("8")
        .arg("--max-time")
        .arg("15")
        .arg(url);
    let prepared = apply(policy, spec).expect("apply policy");
    let code = prepared
        .status()
        .expect("run supervised child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: curl exited {code}");
    code
}

fn main() {
    let allow_example = policy_allowing(&["example.com"]);
    let allow_google = policy_allowing(&["www.google.com"]);

    // example.com and www.google.com are on distinct networks (distinct IPs), so the
    // DNS-attribution allowlist cannot conflate them. Same hosts the linux-supervisor harness proved.
    let compat = run("compat", &allow_example, "http://example.com/");
    let attack = run("attack", &allow_example, "http://www.google.com/");
    let control = run("control", &allow_google, "http://www.google.com/");

    println!("compat  (allow example.com, GET example.com)     -> exit {compat}  [want 0]");
    println!("attack  (allow example.com, GET www.google.com)  -> exit {attack}  [want != 0]");
    println!("control (allow www.google.com, GET google)       -> exit {control}  [want 0]");

    let pass = compat == 0 && attack != 0 && control == 0;
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
