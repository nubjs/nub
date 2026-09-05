//! Phase-3 probe (epic 3.2): the compiler + matcher drive the Windows AppContainer backend
//! end-to-end through the REAL public API (`compile` / `compile_build_jail` -> `apply` -> `status`).
//! The Windows analog of seatbelt-fs / build-jail-landlock, and the running proof for 3.2's
//! coarse-net + allow-only-fs deliverable plus the resurrected window-station ace.
//!
//! MUST run on a NON-INTERACTIVE station (an SSH/service session, `Service-0x0-...$`), never an
//! interactive `WinSta0` — seclogon auto-grants the station ace on `WinSta0`, which would let a
//! BROKEN ace port still pass the ACE arm. The whole point of that arm is the station where the
//! auto-grant does not reach.
//!
//! ARMS (each attack has a failing control, so a block is attributable, not incidental):
//!   FS (allow-only, via the PURE-ALLOWLIST build jail — the shipping Windows fs shape):
//!     1. compat  — jail(pkg, package_dir=A), write A\out   -> 0
//!     2. attack  — jail(pkg, package_dir=A), write B\evil  -> != 0 (B not in the allow-set)
//!     3. control — jail(pkg, package_dir=B), write B\evil  -> 0  (same write, B now granted)
//!   ACE (the load-bearing 3.2 proof):
//!     4. node.exe --version under an AppContainer policy -> 0 (a USER32 importer LOADS on the
//!        non-interactive station; without the ace it dies 0xC0000142 = 3221225794).
//!   NET (coarse, no proxy — per-host rides Phase 5.1):
//!     5. compat  — net:true  (internetClient granted), curl example.com -> 0
//!     6. attack  — net:false (no internetClient), curl example.com      -> != 0
//!
//! Why the build jail and not a raw `{"fs":{dir:"rw"}}`: a general policy's secret read-deny floor
//! (`**/.env*`, `**/.npmrc`, ...) is depth-independent, so it nests inside ANY grant, and Windows
//! cannot carve a read-deny inside a granted subtree (an inheritable allow wins). The backend
//! fail-closes such a policy (`fs-read-deny`) — correct, and by design. A pure-allowlist build jail
//! emits no deny at all, so it is the Windows-enforceable allow-only shape.

use nub_sandbox::{
    CommandSpec, CompileCtx, Homes, SandboxPolicy, ScopeCapabilities, apply, compile,
    compile_build_jail,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The OS-essential ambient env a Windows child needs to launch (SystemRoot/PATH/ComSpec/...).
/// An EMPTY ambient env makes `CreateProcessW` fail with `ERROR_ENVVAR_NOT_FOUND` (os error 203).
fn os_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in [
        "PATH", "SystemRoot", "windir", "SystemDrive", "USERPROFILE", "LOCALAPPDATA", "APPDATA",
        "TEMP", "TMP", "ProgramFiles", "ProgramFiles(x86)", "ProgramData", "ComSpec", "PATHEXT",
        "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    env
}

/// A general (non-build-jail) policy — used only for the coarse-net + ace arms, whose surfaces
/// (`fs: true`) carry no subtree grant for a secret deny to shadow.
fn policy(base: &Path, surface: Value) -> SandboxPolicy {
    let homes = Homes {
        home: base.join("home"),
        tmp: base.join("tmp"),
        cache: base.join("cache"),
        project: base.join("proj"),
    };
    let ctx = CompileCtx::new(homes, base.join("proj"), ScopeCapabilities::approved(), os_env());
    compile(&surface, &ctx).expect("compile policy")
}

/// The pure-allowlist build jail: grants `package_dir` rw + system reads, emits no deny.
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
        Vec::new(),
        Vec::new(),
        os_env(),
    )
    .expect("the build-jail preset compiles")
}

fn run(label: &str, policy: &SandboxPolicy, spec: CommandSpec) -> i32 {
    eprintln!(">>> {label}");
    match apply(policy, spec) {
        Ok(prepared) => {
            let code = prepared
                .status()
                .expect("run confined child")
                .code()
                .unwrap_or(-1);
            eprintln!("<<< {label}: exited {code}");
            code
        }
        // A probe reports an apply-time rejection rather than aborting the whole run, so one pass
        // shows every arm's disposition (sentinel -3 = rejected before launch).
        Err(deg) => {
            eprintln!("<<< {label}: apply REJECTED lost={:?} reason={:?}", deg.lost, deg.reason);
            -3
        }
    }
}

fn cmd_write(path: &Path, cwd: &Path) -> CommandSpec {
    CommandSpec::new(r"C:\Windows\System32\cmd.exe")
        .arg("/c")
        .arg(format!("echo hi > {}", path.display()))
        .cwd(cwd.to_path_buf())
}

fn first_existing(paths: &[&str]) -> Option<String> {
    paths.iter().find(|p| Path::new(p).exists()).map(|p| p.to_string())
}

fn main() {
    let base = PathBuf::from(format!(r"C:\nub-wac-{}", std::process::id()));
    let project = base.join("proj");
    // A: a package's own dir inside the project node_modules — write-granted by the jail.
    let pkg_a = project.join("node_modules").join("esbuild");
    // B: a dir OUTSIDE project/home/cache/tmp — never in the jail's allow-set.
    let escape = base.join("outside");
    for d in [
        &pkg_a,
        &escape,
        &base.join("home"),
        &base.join("tmp"),
        &base.join("cache"),
    ] {
        std::fs::create_dir_all(d).expect("mkdir fixture dir");
    }
    let _ = std::fs::remove_file(pkg_a.join("out"));
    let _ = std::fs::remove_file(escape.join("evil"));

    let jail_a = jail(&base, &project, "esbuild", &pkg_a);
    let jail_b = jail(&base, &project, "esbuild", &escape);
    let net_on = policy(&base, json!({ "fs": true, "net": true }));
    let net_off = policy(&base, json!({ "fs": true, "net": false }));

    // ---- FS: allow-only via the pure-allowlist build jail ----
    let compat = run("fs compat  (write package_dir)", &jail_a, cmd_write(&pkg_a.join("out"), &pkg_a));
    let attack = run("fs attack  (write outside allow-set)", &jail_a, cmd_write(&escape.join("evil"), &pkg_a));
    let escaped = escape.join("evil").exists();
    let _ = std::fs::remove_file(escape.join("evil"));
    let control = run("fs control (same write, B granted)", &jail_b, cmd_write(&escape.join("evil"), &escape));

    // ---- ACE: a USER32 importer must LOAD on the non-interactive station ----
    let node = first_existing(&[
        r"C:\Program Files\nodejs\node.exe",
        r"C:\Program Files (x86)\nodejs\node.exe",
    ]);
    let (ace_code, ace_note) = match &node {
        Some(node) => (
            run(
                "ace       (node --version under AppContainer)",
                &net_off,
                CommandSpec::new(node).arg("--version").cwd(base.clone()),
            ),
            "node found",
        ),
        None => (-2, "node.exe NOT FOUND — ace arm skipped"),
    };

    // ---- NET: coarse (per-host rides Phase 5.1) ----
    let curl = r"C:\Windows\System32\curl.exe";
    let curl_spec = || {
        CommandSpec::new(curl)
            .arg("-sS")
            .arg("-4")
            .arg("-o")
            .arg("NUL")
            .arg("--connect-timeout")
            .arg("8")
            .arg("--max-time")
            .arg("20")
            .arg("https://example.com/")
            .cwd(base.clone())
    };
    let net_compat = run("net compat (net:true)", &net_on, curl_spec());
    let net_attack = run("net attack (net:false)", &net_off, curl_spec());

    let out_written = pkg_a.join("out").exists();
    let _ = std::fs::remove_dir_all(&base);

    println!();
    println!("fs  compat  (write package_dir)       -> exit {compat}   [want 0]");
    println!("fs  attack  (write outside allow-set) -> exit {attack}   [want != 0], escaped={escaped} [want false]");
    println!("fs  control (same write, B granted)   -> exit {control}   [want 0]");
    println!("ace        (node --version)           -> exit {ace_code}   [want 0]  ({ace_note})");
    println!("net compat  (net:true)                -> exit {net_compat}   [want 0]");
    println!("net attack  (net:false)               -> exit {net_attack}   [want != 0]");
    println!("oracle package_dir\\out exists={out_written} [want true]");

    let ace_ok = ace_code == 0 || ace_code == -2;
    let pass = compat == 0
        && attack != 0
        && !escaped
        && control == 0
        && ace_ok
        && net_compat == 0
        && net_attack != 0
        && out_written;
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
