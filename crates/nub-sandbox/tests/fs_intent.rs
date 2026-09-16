//! The filesystem syscalls the broker mediates BEYOND `open`-for-write, against a real confined
//! child: the x86_64 legacy entry points, the metadata writes, and `access`.
//!
//! Each case here closes a hole that no other layer covers, and each one was invisible to the
//! rest of the suite because Rust and glibc never emit the calls that expose it.
//!
//! - **Legacy numbers.** A seccomp filter matches a syscall NUMBER. x86_64 keeps `open`,
//!   `rename`, `chmod` and friends alongside the `*at` forms, so a static musl build, a Go
//!   runtime or hand-written asm walked straight past a filter listing only `openat`. Landlock
//!   does not cover the gap — it is syscall-agnostic, but it cannot express a DENY at all, so
//!   the `.env*` floor is broker-only and `open("/project/.env", O_RDONLY)` read the secret.
//! - **Metadata writes.** Landlock has no metadata hook at any ABI, so `chmod`, `chown` and
//!   `utimensat` took a path and were governed by nothing: a confined command could `chmod
//!   0777` any file its uid owns, anywhere on the host.
//! - **`access`.** It answered from the real filesystem while an open of the same path was
//!   refused, so `test -r` reported a secret-floor file as readable.
//!
//! Every case carries a POSITIVE CONTROL in the same child — the same syscall, on a path the
//! policy grants. Without it a test would pass just as well if the broker refused everything,
//! which is the failure mode that makes an enforcement test worthless.
#![cfg(target_os = "linux")]

#[path = "common/tool_output.rs"]
mod tool_output;

use nub_sandbox::{CommandSpec, CompileCtx, Homes, Sandbox, ScopeCapabilities, compile};
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const CASE: &str = "NUB_FS_INTENT_CASE";
const ROOT: &str = "NUB_FS_INTENT_ROOT";

/// The confined half. `root` holds three trees: `project` granted read-write, `readonly`
/// granted read-only, and `outside` granted nothing at all.
#[test]
fn fs_intent_child() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).expect("fixture root"));
    let project = root.join("project");
    let readonly = root.join("readonly");
    let outside = root.join("outside");

    match case.as_str() {
        // The floor's deny has to bite on the LEGACY number too, or every `.env` guarantee
        // holds only for programs that happen to use `openat`.
        #[cfg(target_arch = "x86_64")]
        "legacy-open" => {
            assert!(
                raw_open(&project.join("index.js")) >= 0,
                "the granted project tree must stay readable through SYS_open",
            );
            let fd = raw_open(&project.join(".env"));
            assert!(
                fd < 0,
                "SYS_open read a secret-floor file the broker refuses through SYS_openat",
            );
        }
        #[cfg(not(target_arch = "x86_64"))]
        "legacy-open" => {}
        // `chmod` is the residual `build_seccomp`'s own comment calls "the part with teeth":
        // host-wide mode rewriting on anything the uid owns. The control is the case that
        // falsified a blanket ceiling deny — node-gyp chmods its own built addon.
        "chmod" => {
            // BOTH SPELLINGS, independently. glibc's `chmod()` emits the x86_64 legacy
            // `SYS_chmod`, so a test written only against it passes with `SYS_fchmodat`
            // untrapped — a falsification run proved exactly that, and the modern number went
            // unguarded until this arm was added.
            for (label, call) in [
                ("chmod", &chmod as &dyn Fn(&Path, libc::mode_t) -> i32),
                ("fchmodat", &raw_fchmodat),
            ] {
                assert_eq!(
                    call(&project.join("writable.txt"), 0o600),
                    0,
                    "{label} inside a write grant must still run",
                );
                assert!(
                    call(&readonly.join("keep.txt"), 0o777) < 0,
                    "{label} rewrote a mode inside a read-only grant",
                );
                assert!(
                    call(&outside.join("victim.txt"), 0o777) < 0,
                    "{label} rewrote a mode on an ungranted path",
                );
            }
        }
        // The fd form carries no path, so the descriptor is read back through
        // `/proc/<tid>/fd/<n>`. A read-only grant hands out a read-only fd that would
        // otherwise rewrite the file's mode through it.
        "fchmod" => {
            let ok = fs::File::open(project.join("writable.txt")).expect("granted file opens");
            assert_eq!(
                unsafe { libc::fchmod(ok.as_raw_fd(), 0o600) },
                0,
                "fchmod inside a write grant must still run",
            );
            let ro = fs::File::open(readonly.join("keep.txt")).expect("read grant opens");
            assert!(
                unsafe { libc::fchmod(ro.as_raw_fd(), 0o777) } < 0,
                "fchmod must not escape a read-only grant through the descriptor",
            );
        }
        // Arbitrary mtime rewriting, the other named residual.
        "utimes" => {
            assert_eq!(
                utimes(&project.join("writable.txt")),
                0,
                "a timestamp write inside a write grant must still run",
            );
            assert!(
                utimes(&readonly.join("keep.txt")) < 0,
                "a read-only grant must not permit rewriting timestamps",
            );
        }
        // `access` now answers from the same rule set the open would consult, so the two can no
        // longer disagree about a secret-floor file. THE ERRNO IS PART OF THE CONTRACT: callers
        // branch on it, and git's `access_or_die` treats ENOENT and EACCES as "no" but anything
        // else as fatal — answering EPERM for a `~/.gitconfig` that does not exist is what broke
        // `git config --global`.
        "access" => {
            assert_eq!(
                access(&project.join("index.js")),
                0,
                "a granted file must report readable",
            );
            assert_eq!(
                access_errno(&project.join(".env")),
                Some(libc::EACCES),
                "a secret-floor file that EXISTS must refuse the way a permission failure does",
            );
            assert_eq!(
                access_errno(&outside.join("absent.txt")),
                Some(libc::ENOENT),
                "an ungranted path that is not even there must say so, not claim EPERM",
            );
        }
        other => panic!("unknown fs-intent case: {other}"),
    }
}

#[cfg(target_arch = "x86_64")]
fn raw_open(path: &Path) -> i64 {
    let c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
    unsafe { libc::syscall(libc::SYS_open, c.as_ptr(), libc::O_RDONLY, 0) }
}

fn chmod(path: &Path, mode: libc::mode_t) -> i32 {
    let c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
    unsafe { libc::chmod(c.as_ptr(), mode) }
}

/// The modern number, issued directly. glibc never emits it on x86_64, so nothing else here
/// reaches it.
fn raw_fchmodat(path: &Path, mode: libc::mode_t) -> i32 {
    let c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
    unsafe {
        libc::syscall(
            libc::SYS_fchmodat,
            libc::AT_FDCWD,
            c.as_ptr(),
            mode as libc::c_uint,
        ) as i32
    }
}

fn utimes(path: &Path) -> i32 {
    let c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
    let times = [
        libc::timespec {
            tv_sec: 1_000_000,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: 1_000_000,
            tv_nsec: 0,
        },
    ];
    unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) }
}

fn access(path: &Path) -> i32 {
    let c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
    unsafe { libc::access(c.as_ptr(), libc::R_OK) }
}

fn access_errno(path: &Path) -> Option<i32> {
    assert!(access(path) < 0, "{} unexpectedly readable", path.display());
    std::io::Error::last_os_error().raw_os_error()
}

#[test]
fn the_legacy_open_number_still_meets_the_secret_floor() {
    run("legacy-open");
}

#[test]
fn a_mode_change_outside_a_write_grant_is_refused() {
    run("chmod");
}

#[test]
fn a_mode_change_through_a_read_only_descriptor_is_refused() {
    run("fchmod");
}

#[test]
fn a_timestamp_write_outside_a_write_grant_is_refused() {
    run("utimes");
}

#[test]
fn access_answers_from_the_policy_rather_than_the_filesystem() {
    run("access");
}

fn ctx(root: &Path) -> CompileCtx {
    let project = root.join("project");
    CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: project.clone(),
        },
        project,
        ScopeCapabilities::approved(),
        BTreeMap::new(),
    )
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("fixture root");
    for dir in ["project", "readonly", "outside"] {
        fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    fs::write(root.path().join("project/index.js"), "ordinary-source").unwrap();
    fs::write(root.path().join("project/writable.txt"), "scratch").unwrap();
    fs::write(root.path().join("project/.env"), "TOKEN=secret").unwrap();
    fs::write(root.path().join("readonly/keep.txt"), "read-me").unwrap();
    fs::write(root.path().join("outside/victim.txt"), "not yours").unwrap();
    root
}

fn run(case: &str) {
    let root = fixture();
    let project = root.path().join("project");
    let readonly = root.path().join("readonly");
    let mut policy = compile(
        &json!({
            "fs": {
                (project.to_string_lossy()): "rw",
                (readonly.to_string_lossy()): "r",
            },
            "net": false,
        }),
        &ctx(root.path()),
    )
    .expect("the fixture policy compiles");
    policy.env.constructed.insert(CASE.into(), case.into());
    policy
        .env
        .constructed
        .insert(ROOT.into(), root.path().to_string_lossy().into_owned());

    let sandbox = Sandbox::new(&policy).expect("the supervised backend acquires");
    let prepared = sandbox
        .prepare(
            CommandSpec::new(std::env::current_exe().unwrap())
                .args(["--exact", "fs_intent_child", "--nocapture"])
                .cwd(&project)
                .redact_stdout(true)
                .redact_stderr(true),
        )
        .expect("the child prepares");
    assert!(prepared.degradation.is_full(), "{:?}", prepared.degradation);
    let output = tool_output::output(prepared);
    assert!(
        output.status.success(),
        "fs-intent case `{case}` failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
