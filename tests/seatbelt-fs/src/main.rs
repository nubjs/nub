//! Phase-3 probe (epic 3.1): the compiler + matcher drive the macOS Seatbelt primitive
//! (`macos::apply` -> `/usr/bin/sandbox-exec`) end-to-end through the REAL public API
//! (`compile` -> `apply` -> `status`). The macOS analog of supervised-fs; runs natively on the
//! dev Mac, the only host that can enforce Seatbelt.
//!
//! FS (allow-only, Seatbelt path-rule base):
//!   1. compat  — allow rw on A, write A/f       -> 0
//!   2. attack  — allow rw on A, write B/f       -> != 0 (Seatbelt EPERM)
//!   3. control — allow rw on B, write B/f       -> 0  (same write, B granted)
//! NET (coarse, no proxy):
//!   4. compat  — net:true  (not enforced -> allow network*), curl example.com -> 0
//!   5. attack  — net:false (deny-all base, loopback closed), curl example.com -> != 0
//! Arm 3 makes arm 2 the deny, not a broken path. Arm 4 is the discriminating control for arm 5:
//! identical curl, the only difference is the compiled net verdict.

use nub_sandbox::{apply, compile, CommandSpec, CompileCtx, Homes, SandboxPolicy, ScopeCapabilities};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

fn policy(base: &Path, surface: Value) -> SandboxPolicy {
    let homes = Homes {
        home: base.join("home"),
        tmp: base.join("tmp"),
        cache: base.join("cache"),
        project: base.join("proj"),
    };
    let mut env = BTreeMap::new();
    for key in ["PATH", "HOME"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    let ctx = CompileCtx::new(homes, base.join("proj"), ScopeCapabilities::approved(), env);
    compile(&surface, &ctx).expect("compile policy")
}

fn run(label: &str, policy: &SandboxPolicy, script: &str) -> i32 {
    eprintln!(">>> {label}: sh -c {script:?}");
    let spec = CommandSpec::new("/bin/sh").arg("-c").arg(script);
    let code = apply(policy, spec)
        .expect("apply policy")
        .status()
        .expect("run confined child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

fn main() {
    // NOT under $TMPDIR: the macOS backend grants the real DARWIN confstr temp dir
    // (/var/folders/.../T) wholesale, so a fixture there would be writable by that grant, not the
    // policy — masking the allow-only attack. Anchor under $HOME instead.
    let home = std::env::var("HOME").expect("HOME set");
    let base = Path::new(&home).join(format!("nub-sbfs-{}", std::process::id()));
    let allowed = base.join("allowed");
    let denied = base.join("denied");
    for d in [&allowed, &denied] {
        std::fs::create_dir_all(d).expect("mkdir test dir");
    }
    let _ = std::fs::remove_file(allowed.join("f"));
    let _ = std::fs::remove_file(denied.join("f"));

    let allow_a = policy(&base, json!({ "fs": { allowed.to_str().unwrap(): "rw" } }));
    let allow_b = policy(&base, json!({ "fs": { denied.to_str().unwrap(): "rw" } }));
    let net_on = policy(&base, json!({ "fs": true, "net": true }));
    let net_off = policy(&base, json!({ "fs": true, "net": false }));

    let write_a = format!("echo hi > {}/f", allowed.display());
    let write_b = format!("echo hi > {}/f", denied.display());
    let curl = "curl -sS -4 -o /dev/null --connect-timeout 8 --max-time 20 https://example.com/";

    let compat = run("fs compat  (allow A, write A)", &allow_a, &write_a);
    let attack = run("fs attack  (allow A, write B)", &allow_a, &write_b);
    let b_written_by_attack = denied.join("f").exists();
    let _ = std::fs::remove_file(denied.join("f"));
    let control = run("fs control (allow B, write B)", &allow_b, &write_b);
    let net_compat = run("net compat (net:true)", &net_on, curl);
    let net_attack = run("net attack (net:false)", &net_off, curl);

    let a_written = allowed.join("f").exists();
    let _ = std::fs::remove_dir_all(&base);

    println!();
    println!("fs  compat  (allow A, write A)  -> exit {compat}   [want 0]");
    println!("fs  attack  (allow A, write B)  -> exit {attack}   [want != 0], b_written={b_written_by_attack} [want false]");
    println!("fs  control (allow B, write B)  -> exit {control}   [want 0]");
    println!("net compat  (net:true)          -> exit {net_compat}   [want 0]");
    println!("net attack  (net:false)         -> exit {net_attack}   [want != 0]");
    println!("oracle A/f exists={a_written} [want true]");

    let pass = compat == 0
        && attack != 0
        && !b_written_by_attack
        && control == 0
        && net_compat == 0
        && net_attack != 0
        && a_written;
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
