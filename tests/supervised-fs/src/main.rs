//! Proves the zero-privilege supervised launch enforces an ALLOW-ONLY filesystem boundary
//! through nub-sandbox's REAL public API: a policy granting rw on one directory is compiled,
//! applied, and run, and the confined child (`/bin/sh`) can write only inside the allowlist.
//!
//! The three arms make the block attributable to the Landlock ruleset, not to a broken path:
//!   1. compat  — allow rw on <allowed>, write <allowed>/f   → expect success (0)
//!   2. attack  — allow rw on <allowed>, write <denied>/f    → expect FAILURE (Landlock EACCES)
//!   3. control — allow rw on <denied>,  write <denied>/f    → expect success (0)
//! Arm 3 runs the SAME command and path as arm 2 under a policy that grants <denied>, so arm 2's
//! failure is the deny and not a missing directory or a broken launch. Both dirs pre-exist, so a
//! failure is EACCES (Landlock), never ENOENT.
//!
//! A fourth arm confirms fs and net COMPOSE in one supervised launch: allow <allowed> + example.com,
//! write inside the allowlist AND reach the allowed host → expect success.

use nub_sandbox::{apply, compile, CommandSpec, CompileCtx, Homes, SandboxPolicy, ScopeCapabilities};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

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
    compile(&surface, &ctx).expect("compile fs policy")
}

/// Run `/bin/sh -c <script>` under `policy`, return the child exit code.
fn run(label: &str, policy: &SandboxPolicy, script: &str) -> i32 {
    eprintln!(">>> {label}: sh -c {script:?}");
    let spec = CommandSpec::new("/bin/sh").arg("-c").arg(script);
    let prepared = apply(policy, spec).expect("apply policy");
    let code = prepared
        .status()
        .expect("run supervised child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

fn main() {
    // Distinct, pre-existing dirs under the shared host tmp. Both must exist so an fs failure is
    // an access denial, never a missing path.
    let base = format!("/tmp/nub-sbxfs-{}", std::process::id());
    let allowed = format!("{base}/allowed");
    let denied = format!("{base}/denied");
    for d in [&allowed, &denied] {
        std::fs::create_dir_all(d).expect("mkdir test dir");
    }
    // Clean any prior probe files so a stale success cannot mask a deny.
    let _ = std::fs::remove_file(format!("{allowed}/f"));
    let _ = std::fs::remove_file(format!("{denied}/f"));

    let write_allowed = format!("echo hi > {allowed}/f");
    let write_denied = format!("echo hi > {denied}/f");

    let allow_allowed = policy(json!({ "fs": { &allowed: "rw" } }));
    let allow_denied = policy(json!({ "fs": { &denied: "rw" } }));
    let allow_both_net = policy(json!({ "fs": { &allowed: "rw" }, "net": ["example.com"] }));

    let compat = run("compat  fs", &allow_allowed, &write_allowed);
    let attack = run("attack  fs", &allow_allowed, &write_denied);
    let control = run("control fs", &allow_denied, &write_denied);

    // fs+net compose: write inside the allowlist and reach the allowed host, one launch.
    let compose = run(
        "compose fs+net",
        &allow_both_net,
        &format!("{write_allowed} && curl -sS -4 -o /dev/null --connect-timeout 8 --max-time 15 https://example.com/"),
    );
    // net deny still holds under the fs-confining policy: an unlisted host is refused at connect.
    let net_attack = run(
        "attack  net",
        &allow_both_net,
        "curl -sS -4 -o /dev/null --connect-timeout 8 --max-time 15 https://www.google.com/",
    );

    // ---- deny-inside-allow WRITE carve-out (the USER_NOTIF write broker) ----
    // A repo the child may write, with `.git/hooks` carved out even though it sits inside the
    // granted subtree — the self-protection Landlock's union cannot express.
    let repo = format!("{base}/repo");
    let hooks = format!("{repo}/.git/hooks");
    std::fs::create_dir_all(format!("{repo}/src")).expect("mkdir repo/src");
    std::fs::create_dir_all(&hooks).expect("mkdir repo/.git/hooks");
    let carve = policy(json!({ "fs": { &repo: "rw", &hooks: false } }));
    let nocarve = policy(json!({ "fs": { &repo: "rw" } }));
    // Show the compiled Deny rules so a false negative (broker never armed) is visible.
    let denies: Vec<&str> = carve
        .fs
        .rules
        .entries
        .iter()
        .filter(|r| r.effect == nub_sandbox::policy::Effect::Deny)
        .map(|r| r.matcher.as_str())
        .collect();
    eprintln!("carve policy Deny rules: {denies:?}");

    let carve_compat = run("carve   compat", &carve, &format!("echo ok > {repo}/src/a"));
    let carve_attack = run(
        "carve   attack",
        &carve,
        &format!("echo evil > {hooks}/pre-commit"),
    );
    let hook_after_attack = Path::new(&format!("{hooks}/pre-commit")).exists();
    let _ = std::fs::remove_file(format!("{hooks}/pre-commit"));
    let carve_control = run(
        "carve   control",
        &nocarve,
        &format!("echo evil > {hooks}/pre-commit"),
    );
    let hook_after_control = Path::new(&format!("{hooks}/pre-commit")).exists();

    // Independent oracle: the allowlisted file exists, the denied one does not.
    let allowed_written = Path::new(&format!("{allowed}/f")).exists();
    let denied_written = Path::new(&format!("{denied}/f")).exists();

    println!();
    println!("compat  (allow <allowed>, write <allowed>)  -> exit {compat}   [want 0]");
    println!("attack  (allow <allowed>, write <denied>)   -> exit {attack}   [want != 0]");
    println!("control (allow <denied>,  write <denied>)   -> exit {control}   [want 0]");
    println!("compose (allow <allowed>+example.com)       -> exit {compose}   [want 0]");
    println!("attack  (allow example.com, GET google)     -> exit {net_attack}   [want != 0]");
    println!("carve   compat  (write repo/src)            -> exit {carve_compat}   [want 0]");
    println!("carve   attack  (write .git/hooks, carved)  -> exit {carve_attack}   [want != 0], hook_written={hook_after_attack} [want false]");
    println!("carve   control (write .git/hooks, no deny) -> exit {carve_control}   [want 0], hook_written={hook_after_control} [want true]");
    println!("oracle  <allowed>/f exists={allowed_written} (want true)");

    let _ = std::fs::remove_dir_all(&base);

    let pass = compat == 0
        && attack != 0
        && control == 0
        && compose == 0
        && net_attack != 0
        && allowed_written
        && denied_written // denied/f exists because the control arm (allowed) wrote it
        && carve_compat == 0
        && carve_attack != 0
        && !hook_after_attack // the broker refused the hook write
        && carve_control == 0
        && hook_after_control; // same write lands when the carve-out is absent
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
