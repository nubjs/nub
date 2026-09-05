//! Phase-2-exit probe (epic 2.1): the compiler + CATALOG + matcher drive the new Landlock
//! build-jail primitive (`apply_landlock`) end-to-end, through the REAL public API
//! (`compile_build_jail` -> `apply` -> `status`). supervised-fs proved the separate
//! `confine_without_landlock` fork path; this proves the build-jail Command+pre_exec path.
//!
//! Two attack/control pairs, each making the block attributable, not incidental:
//!   FS (allow-only Landlock base):
//!     1. compat  — jail(pkg, package_dir=A), write A/out          -> 0 (A is write-granted)
//!     2. attack  — jail(pkg, package_dir=A), write B/evil         -> != 0 (B not in allow-set)
//!     3. control — jail(pkg, package_dir=B), write B/evil         -> 0  (B now the granted dir)
//!   NET (the compiled catalog verdict enforced by the build_seccomp family ceiling):
//!     4. compat — jail("esbuild"), curl example.com + raw AF_INET socket -> 0 (IP egress admitted)
//!     5. attack — jail("esbuild"), raw AF_UNIX socket                    -> != 0 (ceiling denies it)
//! Arm 3 runs the SAME write as arm 2 under a policy that grants B, so arm 2 is the deny, not a
//! broken path. Arms 4/5 are the discriminating net pair on ONE granted package: the compiled
//! `["*"]` verdict (`enforce=true`) admits AF_INET but the family ceiling still refuses AF_UNIX
//! (a host-daemon path neither Landlock nor a netns scopes). Under the shipped v2 catalog the
//! build jail is GENEROUS — `baseline.network=true`, zero `network:false` entries — so a
//! deny-all-egress build-jail package is not catalog-reachable; the egress-DENY mechanism itself
//! is proven on the supervised path (supervised-fs `net_attack`). Here the family ceiling is the
//! build-jail path's real, catalog-reachable egress boundary.

use nub_sandbox::{apply, compile_build_jail, CommandSpec, Homes, SandboxPolicy};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn jail(base: &Path, project: &Path, pkg: &str, package_dir: &Path) -> SandboxPolicy {
    compile_build_jail(
        Homes {
            home: base.join("home"),
            tmp: base.join("tmp"),
            cache: base.join("cache"),
            project: project.to_path_buf(),
        },
        package_dir,
        Some(pkg),
        None,
        Vec::new(), // interpreter: the child is /bin/sh, covered by the system read floor
        Vec::new(), // extra_reads
        BTreeMap::new(),
    )
    .expect("the build-jail preset compiles")
}

/// Run `/bin/sh -c <script>` under `policy` with cwd pinned to `cwd`, return the child exit code.
fn run(label: &str, policy: &SandboxPolicy, cwd: &Path, script: &str) -> i32 {
    eprintln!(">>> {label}: sh -c {script:?}");
    let spec = CommandSpec::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .cwd(cwd.to_path_buf());
    let code = apply(policy, spec)
        .expect("apply build-jail policy")
        .status()
        .expect("run confined child")
        .code()
        .unwrap_or(-1);
    eprintln!("<<< {label}: exited {code}");
    code
}

fn main() {
    let base = PathBuf::from(format!("/tmp/nub-bjl-{}", std::process::id()));
    let project = base.join("proj");
    // A: a package's own dir inside the project's node_modules — write-granted by the jail.
    let pkg_a = project.join("node_modules/esbuild");
    // B: a dir OUTSIDE project/home/cache/tmp — reachable to write outside the sandbox, but
    // never in the jail's allow-set.
    let escape = base.join("outside");
    for d in [&pkg_a, &escape, &base.join("home"), &base.join("cache"), &base.join("tmp")] {
        std::fs::create_dir_all(d).expect("mkdir fixture dir");
    }
    let _ = std::fs::remove_file(pkg_a.join("out"));
    let _ = std::fs::remove_file(escape.join("evil"));

    let esbuild_a = jail(&base, &project, "esbuild", &pkg_a);
    let esbuild_escape = jail(&base, &project, "esbuild", &escape);

    // The compiled net verdict — the catalog's job on this axis reaches the IR: a catalogued
    // package is `enforce=true` with one catch-all Allow rule (`["*"]`), never relaxed.
    eprintln!(
        "esbuild net: enforce={} default={:?} rules={}",
        esbuild_a.net.enforce, esbuild_a.net.default_effect, esbuild_a.net.rules.len()
    );

    // ---- FS: allow-only Landlock base ----
    let compat = run(
        "fs compat  (write package_dir)",
        &esbuild_a,
        &pkg_a,
        &format!("echo ok > {}/out", pkg_a.display()),
    );
    let attack = run(
        "fs attack  (write outside allow-set)",
        &esbuild_a,
        &pkg_a,
        &format!("echo evil > {}/evil", escape.display()),
    );
    let escaped = escape.join("evil").exists();
    let _ = std::fs::remove_file(escape.join("evil"));
    let control = run(
        "fs control (same write, B granted)",
        &esbuild_escape,
        &escape,
        &format!("echo ok > {}/evil", escape.display()),
    );

    // ---- NET: the compiled catalog verdict, enforced by the family ceiling ----
    // IP egress is admitted (the catalogued `["*"]` verdict); the family ceiling still refuses
    // AF_UNIX on the SAME package — the discriminating pair.
    let curl = "curl -sS -4 -o /dev/null --connect-timeout 8 --max-time 20 https://example.com/";
    let net_curl = run("net compat (esbuild: curl over IP)", &esbuild_a, &pkg_a, curl);
    let inet = "python3 -c \"import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM); print('OK')\"";
    let net_inet = run("net compat (esbuild: AF_INET socket)", &esbuild_a, &pkg_a, inet);
    let unix = "python3 -c \"import socket; socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); print('OK')\"";
    let net_unix = run("net attack (esbuild: AF_UNIX socket)", &esbuild_a, &pkg_a, unix);

    let out_written = pkg_a.join("out").exists();
    let _ = std::fs::remove_dir_all(&base);

    println!();
    println!("fs  compat  (write package_dir)          -> exit {compat}   [want 0]");
    println!("fs  attack  (write outside allow-set)    -> exit {attack}   [want != 0], escaped={escaped} [want false]");
    println!("fs  control (same write, B granted)      -> exit {control}   [want 0]");
    println!("net compat  (esbuild: curl over IP)      -> exit {net_curl}   [want 0]");
    println!("net compat  (esbuild: AF_INET socket)    -> exit {net_inet}   [want 0]");
    println!("net attack  (esbuild: AF_UNIX ceiling)   -> exit {net_unix}   [want != 0]");
    println!("oracle package_dir/out exists={out_written} [want true]");

    let pass = compat == 0
        && attack != 0
        && !escaped
        && control == 0
        && net_curl == 0
        && net_inet == 0
        && net_unix != 0
        && out_written;
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
