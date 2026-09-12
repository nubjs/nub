//! Proves PER-HOST egress through the loopback SNI-inspecting proxy (epic 5.1), driving
//! nub-sandbox's REAL public API (`compile` → `apply` → `status`) on both enforcement OSes. A
//! fine-grained `net` allowlist derives `ProxyMode::Auto`, so `apply` starts the proxy; how the
//! child reaches it differs by OS, so the arms do too:
//!
//! LINUX (transparent redirect). The child is NON-cooperative — no `HTTP_PROXY`, `--noproxy '*'`
//! besides — and the seccomp supervisor redirects every non-loopback connect through the proxy by
//! speaking the cooperative CONNECT on its behalf. A block is the OS interception, never client
//! good-behavior. The SNI gate is isolated by a same-IP discriminator (arms 3 vs 4).
//!
//! MACOS (deny-all-but-proxy). Seatbelt allows the child ONLY `localhost:<proxy_port>`; a
//! cooperative client honors the injected `https_proxy` and reaches the proxy, a non-cooperative
//! one dials direct and Seatbelt denies it — for EVERY host, allowed or not (the accepted
//! compatibility cost of having no transparent redirect on macOS). Arms 3+4 both fail ⇒
//! non-cooperative egress is blocked regardless of host = never leaked (A1).

use nub_sandbox::{
    apply, compile, CommandSpec, CompileCtx, Homes, SandboxPolicy, ScopeCapabilities,
};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeMap;

fn policy(surface: Value) -> SandboxPolicy {
    let root = std::env::temp_dir();
    let homes = Homes {
        home: root.clone(),
        tmp: root.clone(),
        cache: root.clone(),
        project: root.clone(),
    };
    let mut env = BTreeMap::new();
    // The Windows co-package helper receives the compiled environment rather than the parent
    // environment. Keep only OS startup roots in this fixture; no secret-bearing ambient values
    // are relevant to the proxy contract.
    for key in [
        "PATH",
        "HOME",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "LOCALAPPDATA",
        "USERPROFILE",
        "ComSpec",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    let ctx = CompileCtx::new(homes, root, ScopeCapabilities::approved(), env);
    compile(&surface, &ctx).expect("compile net policy")
}

/// Run `curl` under `policy`, return its exit code. `noproxy` adds `--noproxy '*'` so curl dials
/// the destination DIRECTLY, ignoring any proxy env — the non-cooperative case.
fn curl(label: &str, policy: &SandboxPolicy, noproxy: bool, curl_args: &str) -> i32 {
    let np = if noproxy { "--noproxy '*'" } else { "" };
    let script =
        format!("curl -4 -sS {np} -o /dev/null --connect-timeout 8 --max-time 20 {curl_args}");
    eprintln!(">>> {label}: sh -c {script:?}");
    let spec = CommandSpec::new("/bin/sh").arg("-c").arg(&script);
    let code = apply(policy, spec)
        .expect("apply policy")
        .status()
        .expect("run confined child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

#[cfg(target_os = "linux")]
fn run() -> bool {
    unsafe { std::env::set_var("NUB_SANDBOX_SUP_DEBUG", "1") };
    let allow = policy(json!({ "fs": true, "net": ["example.com"] }));
    // Non-cooperative throughout: the supervisor's transparent redirect is the only thing routing.
    let compat = curl("compat      ", &allow, true, "https://example.com/");
    let attack_deny = curl("attack-deny ", &allow, true, "https://www.google.com/");
    let attack_sni = curl(
        "attack-sni  ",
        &allow,
        true,
        "--connect-to www.google.com:443:example.com:443 https://www.google.com/",
    );
    let control_sni = curl(
        "control-sni ",
        &allow,
        true,
        "--connect-to example.com:443:example.com:443 https://example.com/",
    );
    println!();
    println!("1 compat      (allow example.com, GET example.com)  -> exit={compat}   [want 0]");
    println!(
        "2 attack-deny (allow example.com, GET google)       -> exit={attack_deny}   [want != 0]"
    );
    println!(
        "3 attack-sni  (example.com IP, SNI=google)          -> exit={attack_sni}   [want != 0]"
    );
    println!(
        "4 control-sni (example.com IP, SNI=example.com)     -> exit={control_sni}   [want 0]"
    );
    compat == 0 && attack_deny != 0 && attack_sni != 0 && control_sni == 0
}

#[cfg(target_os = "macos")]
fn run() -> bool {
    let allow = policy(json!({ "fs": true, "net": ["example.com"] }));
    // Cooperative (honors the injected https_proxy) — the proxy's per-host gate decides.
    let coop_allow = curl("coop-allow  ", &allow, false, "https://example.com/");
    let coop_deny = curl("coop-deny   ", &allow, false, "https://www.google.com/");
    // Non-cooperative (dials direct) — Seatbelt denies ALL direct egress, allowed or not. Names
    // fail at DNS (the resolver is off-limits too); the hardcoded-IP arm proves the block is at
    // connect, not merely resolution — a client that needs no DNS still cannot leave.
    let noncoop_deny = curl("noncoop-deny", &allow, true, "https://www.google.com/");
    let noncoop_allow = curl("noncoop-allw", &allow, true, "https://example.com/");
    let noncoop_ip = curl("noncoop-ip  ", &allow, true, "https://1.1.1.1/");
    println!();
    println!("1 coop-allow   (proxy env, GET example.com)   -> exit={coop_allow}   [want 0]");
    println!("2 coop-deny    (proxy env, GET google)        -> exit={coop_deny}   [want != 0]");
    println!("3 noncoop-deny (--noproxy, GET google)        -> exit={noncoop_deny}   [want != 0]");
    println!("4 noncoop-allw (--noproxy, GET example.com)   -> exit={noncoop_allow}   [want != 0]");
    println!("5 noncoop-ip   (--noproxy, GET 1.1.1.1)       -> exit={noncoop_ip}   [want != 0]");
    // 1 vs 2: the proxy's per-host gate works for a cooperative client. 3/4/5 all blocked:
    // non-cooperative egress is denied regardless of host or DNS — never leaked.
    coop_allow == 0 && coop_deny != 0 && noncoop_deny != 0 && noncoop_allow != 0 && noncoop_ip != 0
}

#[cfg(target_os = "windows")]
fn windows_curl(label: &str, policy: &SandboxPolicy, noproxy: bool, url: &str) -> i32 {
    let mut spec = CommandSpec::new(r"C:\Windows\System32\curl.exe")
        .args([
            "-4",
            "-sS",
            "-o",
            "NUL",
            "--connect-timeout",
            "8",
            "--max-time",
            "20",
        ])
        .cwd(r"C:\Windows\System32");
    if noproxy {
        spec = spec.args(["--noproxy", "*"]);
    }
    spec = spec.arg(url);
    eprintln!(">>> {label}: curl {url} (noproxy={noproxy})");
    let code = apply(policy, spec)
        .expect("apply Windows host-filter policy with registered helper")
        .status()
        .expect("run confined Windows curl")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

#[cfg(target_os = "windows")]
fn run() -> bool {
    // Mirror the nub-cli embedder: the helper is this co-package binary's hidden re-entry. The
    // same executable serves the proxy below, so this probes the actual registered-helper path
    // rather than the library's intentionally fail-closed unregistered state.
    nub_sandbox::set_windows_egress_helper_command(vec![
        std::env::current_exe()
            .expect("current proxy probe executable")
            .into_os_string(),
        "--windows-egress-helper".into(),
    ]);
    // Keep the filesystem axis relaxed so a failed curl cannot be misattributed to an absent file
    // grant. The network axis is still an AppContainer funnel: the command has no Internet
    // capability and can reach egress only through the same-SID helper's injected proxy.
    let allow = policy(json!({ "fs": false, "net": ["example.com"] }));
    let coop_allow = windows_curl("coop-allow  ", &allow, false, "https://example.com/");
    let coop_deny = windows_curl("coop-deny   ", &allow, false, "https://www.google.com/");
    // These deliberately ignore the helper-injected proxy environment. An allowlisted hostname
    // still must not direct-dial, and an IP literal has no host rule to admit it.
    let noncoop_allow = windows_curl("noncoop-allw", &allow, true, "https://example.com/");
    let noncoop_ip = windows_curl("noncoop-ip  ", &allow, true, "https://1.1.1.1/");
    println!();
    println!("1 coop-allow   (helper proxy, GET example.com)  -> exit={coop_allow}   [want 0]");
    println!("2 coop-deny    (helper proxy, GET google)       -> exit={coop_deny}   [want != 0]");
    println!(
        "3 noncoop-allw (--noproxy, GET example.com)    -> exit={noncoop_allow}   [want != 0]"
    );
    println!("4 noncoop-ip   (--noproxy, GET 1.1.1.1)         -> exit={noncoop_ip}   [want != 0]");
    coop_allow == 0 && coop_deny != 0 && noncoop_allow != 0 && noncoop_ip != 0
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn run() -> bool {
    eprintln!("per-host egress enforcement is unsupported on this OS");
    true
}

fn main() {
    #[cfg(target_os = "windows")]
    if std::env::args().nth(1).as_deref() == Some("--windows-egress-helper") {
        nub_sandbox::serve_windows_egress_helper();
    }
    let pass = run();
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
