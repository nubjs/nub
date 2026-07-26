//! macOS env-read closure — REAL enforcement test for the load-bearing security
//! default: a confined child must not recover a scrubbed secret from a co-resident
//! same-uid process's environment via `sysctl KERN_PROCARGS2`.
//!
//! The threat this closes: env confinement is CONSTRUCTION (the withheld var is simply
//! absent from the child's own environ), which is worthless if the child can turn
//! around and read the secret out of a SIBLING/parent process that still holds it.
//! `KERN_PROCARGS2` returns any same-uid pid's argv+environ; the Seatbelt closure
//! (`deny process-info*` + self-restore) shuts it. See `backend/macos.rs`.
//!
//! Every assertion is paired with a NEGATIVE CONTROL (the closure lifted → the read
//! succeeds) so a pass cannot be hollow. The confined reader runs under a policy that
//! wraps ONLY because env withholds a secret (fs + net relaxed) — exercising the
//! `env_needs_closure` wrap gate end-to-end against the kernel, not just the builder.
//! macOS-only; other OSes skip cleanly.
#![cfg(target_os = "macos")]

use nub_sandbox::policy::{Effect, EnvPolicy, SandboxPolicy};
use nub_sandbox::{CommandSpec, apply};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// A value that appears in the victim's environment and nowhere else, so finding it in
/// a procargs2 blob is unambiguous proof the reader recovered the victim's env.
const SECRET_MARKER: &str = "HUNTER2xENVxLEAKxMARKER";

/// A confined C reader that `sysctl(KERN_PROCARGS2)`s a target pid and reports whether
/// it recovered the marker. Prints exactly one of: BLOCKED / LEAK / NOSECRET.
const READER_SRC: &str = r#"
#define _DARWIN_C_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <sys/sysctl.h>
int main(int argc, char **argv) {
  if (argc < 2) { puts("NOARG"); return 3; }
  pid_t pid = (pid_t)atoi(argv[1]);
  size_t cap = 1 << 18; char *b = malloc(cap); size_t n = cap;
  int mib[3] = { CTL_KERN, KERN_PROCARGS2, pid };
  if (sysctl(mib, 3, b, &n, NULL, 0) != 0) { printf("BLOCKED errno=%d\n", errno); return 0; }
  puts(memmem(b, n, "HUNTER2xENVxLEAKxMARKER", 23) ? "LEAK" : "NOSECRET");
  return 0;
}
"#;

/// The victim: a plain compiled binary that parks on `pause()` holding the marker in
/// its env. It MUST be a freshly-compiled (non-system) binary — a system/hardened
/// binary like `/bin/sleep` has a truncated procargs2 that never exposes its env, so
/// the negative control would spuriously find nothing.
const VICTIM_SRC: &str = r#"
#include <unistd.h>
int main() { pause(); return 0; }
"#;

/// A dual-mode "kin" binary that DISCRIMINATES `(target self)` from `(target
/// same-sandbox)` at the kernel level — the case a plain external victim cannot test
/// (an unsandboxed victim is not same-sandbox, so a buggy same-sandbox grant would
/// still deny it). Run under the closure with no arg, it forks a victim CHILD (its own
/// same-sandbox kin) execd with the marker in its exec-time env, then reads that
/// child's procargs2. Under `(target self)` the read is denied (the child is a
/// different pid); under the `(target same-sandbox)` footgun it would LEAK. Prints
/// BLOCKED / LEAK / NOSECRET. (`argv[1] == "victim"` is the child's park mode.)
const KIN_SRC: &str = r#"
#define _DARWIN_C_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <signal.h>
#include <sys/sysctl.h>
int main(int argc, char **argv) {
  if (argc >= 2 && strcmp(argv[1], "victim") == 0) { pause(); return 0; }
  pid_t pid = fork();
  if (pid == 0) {
    char *env[] = { "SECRET_MARKER_VAR=HUNTER2xENVxLEAKxMARKER", NULL };
    char *av[] = { argv[0], "victim", NULL };
    execve(argv[0], av, env);
    _exit(127);
  }
  usleep(300000);
  size_t cap = 1 << 18; char *b = malloc(cap); size_t n = cap;
  int mib[3] = { CTL_KERN, KERN_PROCARGS2, pid };
  if (sysctl(mib, 3, b, &n, NULL, 0) != 0) printf("BLOCKED errno=%d\n", errno);
  else printf(memmem(b, n, "HUNTER2xENVxLEAKxMARKER", 23) ? "LEAK\n" : "NOSECRET\n");
  kill(pid, 9);
  return 0;
}
"#;

/// A reader of its OWN procargs2 — the legitimate self-introspection node relies on,
/// which the closure must NOT break. Prints SELF_OK / SELF_BLOCKED.
const SELF_SRC: &str = r#"
#define _DARWIN_C_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/sysctl.h>
int main() {
  size_t cap = 1 << 18; char *b = malloc(cap); size_t n = cap;
  int mib[3] = { CTL_KERN, KERN_PROCARGS2, getpid() };
  if (sysctl(mib, 3, b, &n, NULL, 0) != 0) { puts("SELF_BLOCKED"); return 0; }
  puts("SELF_OK");
  return 0;
}
"#;

/// Compile a C source to an executable in `dir`, returning its path — or `None` if no
/// C toolchain is available (a clean skip, never a hollow pass). Wherever the Seatbelt
/// suite runs, `cc` is present (Xcode CLT), so this only skips on a truly bare host.
fn compile(dir: &Path, name: &str, src: &str) -> Option<PathBuf> {
    let c = dir.join(format!("{name}.c"));
    let bin = dir.join(name);
    std::fs::write(&c, src).ok()?;
    let ok = Command::new("cc")
        .arg("-O2")
        .arg("-o")
        .arg(&bin)
        .arg(&c)
        .status()
        .ok()?
        .success();
    ok.then_some(bin)
}

/// A relaxed-fs, relaxed-net policy whose ONLY confinement is an env scrub that
/// WITHHOLDS `SECRET_MARKER`. `apply` must wrap this (to emit the env-read closure)
/// even though fs and net are open — the `env_needs_closure` gate.
fn env_scrub_policy() -> SandboxPolicy {
    let mut constructed = BTreeMap::new();
    // A minimal usable baseline (PATH so a resolved `node` can re-find tools); the
    // secret is deliberately ABSENT — that is the whole point of the scrub.
    if let Ok(path) = std::env::var("PATH") {
        constructed.insert("PATH".to_string(), path);
    }
    let mut policy = SandboxPolicy {
        env: EnvPolicy {
            resolved: true,
            enforce: true,
            constructed,
            schema: Vec::new(),
            withheld: vec!["SECRET_MARKER_VAR".to_string()],
            sensitive_keys: Vec::new(),
        },
        ..Default::default()
    };
    // Relax fs explicitly (default is deny-based); net default is already not-enforcing.
    policy.fs.rules.default_effect = Effect::Allow;
    policy
}

/// Spawn a long-lived victim (the compiled `victim` binary) holding `SECRET_MARKER` in
/// its environment. It is a SIBLING of this test process — the case where a `(target
/// others)`/`(target same-sandbox)` grant leaked. Returns the child so the caller kills it.
fn spawn_victim(victim: &Path) -> std::process::Child {
    Command::new(victim)
        .env("SECRET_MARKER_VAR", SECRET_MARKER)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn victim")
}

/// Run `program args…` under `policy` in `cwd`, returning trimmed stdout.
fn run(policy: &SandboxPolicy, cwd: &Path, program: &Path, args: &[&str]) -> String {
    let spec = CommandSpec::new(program)
        .args(args.iter().copied())
        .cwd(cwd);
    let out = apply(policy, spec)
        .expect("apply")
        .output()
        .expect("spawn reader");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn confined_reader_cannot_recover_a_siblings_env() {
    let dir = TempDir::new_in("/private/tmp").expect("tmp");
    let (Some(reader), Some(victim_bin)) = (
        compile(dir.path(), "reader", READER_SRC),
        compile(dir.path(), "victim", VICTIM_SRC),
    ) else {
        eprintln!("skipping: no C toolchain to build the procargs2 probe");
        return;
    };

    let mut victim = spawn_victim(&victim_bin);
    let pid = victim.id().to_string();
    // Give the victim a moment to exec so its env is recorded in procargs2.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // NEGATIVE CONTROL: unconfined, the read leaks the sibling's secret — proving the
    // vector is real and the victim genuinely holds the marker.
    let raw = Command::new(&reader)
        .arg(&pid)
        .output()
        .expect("raw reader");
    let raw_out = String::from_utf8_lossy(&raw.stdout).trim().to_string();

    // CONFINED under the env-read closure: the read is denied (procargs2 EPERM), so the
    // secret cannot be recovered — no LEAK.
    let confined = run(&env_scrub_policy(), dir.path(), &reader, &[&pid]);
    let _ = victim.kill();
    let _ = victim.wait(); // reap so no zombie is left behind

    assert_eq!(
        raw_out, "LEAK",
        "neg-control: an UNCONFINED reader must recover the sibling's env (got {raw_out:?})"
    );
    assert!(
        confined.starts_with("BLOCKED"),
        "confined reader must get EPERM from KERN_PROCARGS2, not recover the env (got {confined:?})"
    );
}

#[test]
fn confined_process_cannot_read_a_same_sandbox_child_env() {
    // The discriminating case: a confined process reads its OWN same-sandbox child's
    // env. This is what separates `(target self)` from the `(target same-sandbox)`
    // footgun — a plain external victim is NOT same-sandbox and so cannot exercise it.
    let dir = TempDir::new_in("/private/tmp").expect("tmp");
    let Some(kin) = compile(dir.path(), "kin", KIN_SRC) else {
        eprintln!("skipping: no C toolchain to build the kin probe");
        return;
    };

    // NEGATIVE CONTROL: unconfined, the parent reads its child's env — proving the
    // fork-and-read construction genuinely exposes the marker.
    let raw = Command::new(&kin).output().expect("raw kin");
    let raw_out = String::from_utf8_lossy(&raw.stdout).trim().to_string();

    // CONFINED: under the closure the child is a different pid (not self), so the read
    // is denied even though the child IS same-sandbox.
    let confined = run(&env_scrub_policy(), dir.path(), &kin, &[]);

    assert_eq!(
        raw_out, "LEAK",
        "neg-control: unconfined, a parent must read its child's env (got {raw_out:?})"
    );
    assert!(
        confined.starts_with("BLOCKED"),
        "a confined process must NOT read its same-sandbox child's env — \
         `(target self)`, never `(target same-sandbox)` (got {confined:?})"
    );
}

#[test]
fn self_env_read_survives_the_closure() {
    // node introspects its OWN process via procargs2; the closure restores `(target
    // self)`, so a self-read must still succeed under the wrapping policy.
    let dir = TempDir::new_in("/private/tmp").expect("tmp");
    let Some(selfp) = compile(dir.path(), "selfp", SELF_SRC) else {
        eprintln!("skipping: no C toolchain to build the self probe");
        return;
    };
    let out = run(&env_scrub_policy(), dir.path(), &selfp, &[]);
    assert_eq!(
        out, "SELF_OK",
        "self procargs2 read must survive the closure"
    );
}

#[test]
fn node_runs_under_the_env_read_closure() {
    // The closure must not break a real runtime: `node -e` (which self-introspects and
    // reads a set of sysctls at startup) runs to completion under the wrapping policy.
    let Some(node) = which_node() else {
        eprintln!("skipping: node not on PATH");
        return;
    };
    let dir = TempDir::new_in("/private/tmp").expect("tmp");
    let out = run(
        &env_scrub_policy(),
        dir.path(),
        &node,
        &[
            "-e",
            "process.stdout.write('NODE_OK'+(require('os').cpus().length>0))",
        ],
    );
    assert_eq!(
        out, "NODE_OKtrue",
        "node must run under the env-read closure"
    );
}

fn which_node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("node"))
        .find(|p| p.is_file())
}
