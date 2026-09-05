//! Drives realistic CONCURRENT egress through the supervised launch (epic 1.5). The route.c
//! prototype was measured to handle 8 parallel curls but "not survive npm install's connection
//! concurrency" — the single-threaded blocking supervisor exits on the transient ENOENT the kernel
//! returns when a notification is reaped between wake and RECV. These arms find whether that gap
//! is real in the wired code and, once fixed, prove it closed.

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
    compile(&surface, &ctx).expect("compile policy")
}

fn run(label: &str, policy: &SandboxPolicy, script: &str) -> i32 {
    eprintln!(">>> {label}");
    let spec = CommandSpec::new("/bin/sh").arg("-c").arg(script);
    let prepared = apply(policy, spec).expect("apply policy");
    let code = prepared
        .status()
        .expect("run supervised child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exit {code}");
    code
}

fn main() {
    let net = policy(json!({ "fs": true, "net": ["example.com"] }));

    // Escalating parallel curls to the allowed host. Each arm passes only if EVERY request got 200
    // — a supervisor that died mid-run leaves later curls unable to connect.
    let mut conc_results = Vec::new();
    for n in [8, 30, 64] {
        // `-I @`, not `-I _`: the replace token must not appear in the command, and `_` occurs
        // inside `%{http_code}` (xargs turned it into `http<N>code`, corrupting every -w).
        let script = format!(
            "codes=$(seq 1 {n} | xargs -P {n} -I @ curl -sS -4 -o /dev/null -w '%{{http_code}} ' \
             --connect-timeout 10 --max-time 30 https://example.com/); \
             ok=$(echo \"$codes\" | tr ' ' '\\n' | grep -c '^200'); \
             echo \"got $ok/{n} 200s\"; test \"$ok\" = \"{n}\"",
            n = n
        );
        let code = run(&format!("conc {n}x curl"), &net, &script);
        conc_results.push((n, code));
    }

    // A real npm install of a tiny package — the canonical concurrency + real-stack workload.
    let work = format!("/tmp/nub-conc-npm-{}", std::process::id());
    std::fs::create_dir_all(&work).ok();
    // fs unconfined (`true`) to isolate the CONCURRENCY-of-net question — npm reads node/its
    // modules and writes node_modules freely; only the net axis is enforced, so the registry is
    // the one reachable host and its parallel/keepalive connections exercise the supervisor.
    let npm_policy = policy(json!({ "fs": true, "net": ["registry.npmjs.org"] }));
    let npm_script = format!(
        "export PATH=/usr/local/bin:/usr/bin:/bin HOME={work}; \
         cd {work} && npm install --no-audit --no-fund --cache {work}/.npm is-odd >{work}/npm.log 2>&1; \
         rc=$?; echo npm rc=$rc; tail -3 {work}/npm.log; test -d {work}/node_modules/is-odd"
    );
    let npm_code = run("npm install is-odd", &npm_policy, &npm_script);
    let _ = std::fs::remove_dir_all(&work);

    println!();
    for (n, code) in &conc_results {
        println!("conc {n}x curl   -> exit {code}   [want 0]");
    }
    println!("npm install     -> exit {npm_code}   [want 0]");

    let conc_ok = conc_results.iter().all(|(_, c)| *c == 0);
    let pass = conc_ok && npm_code == 0;
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
