//! Real-kernel acceptance for the build-jail's Landlock ABI-3 write floor.
//!
//! Run this fixture only on a host whose raw Landlock ABI is asserted by the
//! caller: ABI 2 must refuse before a child is launched; ABI 3+ must permit
//! truncation only in the writable package directory.  The latter verifies both
//! `truncate(2)` and the read-only-open `O_RDONLY | O_TRUNC` edge that is not
//! covered by `WRITE_FILE`.

use nub_sandbox::{apply, compile_build_jail, CommandSpec, Homes};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;

fn raw_landlock_abi() -> Option<u32> {
    // `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` is
    // the documented ABI query.  This deliberately bypasses nub-sandbox's
    // admission helper so an ABI-2 host remains identifiable after the floor
    // makes that helper return None.
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    u32::try_from(result).ok()
}

fn jail(base: &Path, writable_package: &Path, readonly: &Path) -> nub_sandbox::SandboxPolicy {
    compile_build_jail(
        Homes {
            home: base.join("user-home"),
            tmp: base.join("tmp"),
            cache: base.join("cache"),
            project: base.join("project"),
        },
        writable_package,
        Some("abi-floor-probe"),
        None,
        vec![std::env::current_exe().expect("fixture executable")],
        vec![readonly.to_path_buf()],
        BTreeMap::new(),
    )
    .expect("compile build-jail policy")
}

fn payload(operation: &str, target: &Path) -> ! {
    let target = CString::new(target.as_os_str().as_bytes()).expect("target has no NUL");
    let result = unsafe {
        match operation {
            "truncate" => libc::truncate(target.as_ptr(), 0),
            "readonly-open-truncate" => {
                let fd = libc::open(target.as_ptr(), libc::O_RDONLY | libc::O_TRUNC);
                if fd >= 0 {
                    libc::close(fd);
                    0
                } else {
                    -1
                }
            }
            _ => panic!("unknown payload operation: {operation}"),
        }
    };
    if result == 0 {
        println!("operation={operation} result=ok");
        std::process::exit(0);
    }
    println!(
        "operation={operation} result=err errno={}",
        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    );
    std::process::exit(1);
}

fn execute(
    policy: &nub_sandbox::SandboxPolicy,
    operation: &str,
    target: &Path,
) -> std::process::Output {
    apply(
        policy,
        CommandSpec::new(std::env::current_exe().expect("fixture executable"))
            .arg("--payload")
            .arg(operation)
            .arg(target),
    )
    .expect("ABI 3+ build-jail application")
    .output()
    .expect("run confined payload")
}

fn reset(path: &Path) {
    std::fs::write(path, b"four").expect("reset four-byte fixture");
}

fn check(
    policy: &nub_sandbox::SandboxPolicy,
    label: &str,
    target: &Path,
    operation: &str,
    allow: bool,
) -> bool {
    reset(target);
    let output = execute(policy, operation, target);
    let text = String::from_utf8_lossy(&output.stdout);
    let size = std::fs::metadata(target).expect("target survives").len();
    let correct = if allow {
        output.status.success() && size == 0
    } else {
        !output.status.success() && text.contains("errno=13") && size == 4
    };
    println!(
        "{label:28} {operation:24} status={:?} size={size} output={}",
        output.status.code(),
        text.trim(),
    );
    correct
}

fn main() {
    if let [_, flag, operation, target] = std::env::args().collect::<Vec<_>>().as_slice() {
        if flag == "--payload" {
            payload(operation, Path::new(target));
        }
    }

    let abi = raw_landlock_abi();
    println!("raw_landlock_abi={abi:?}");
    let Some(abi) = abi else {
        eprintln!("SKIP: this host has no enabled Landlock ABI");
        std::process::exit(77);
    };

    let base = PathBuf::from(format!(
        "/tmp/nub-landlock-abi-floor-{}",
        std::process::id()
    ));
    let package = base.join("project/node_modules/abi-floor-probe");
    let readonly = base.join("readonly");
    let withheld = base.join("withheld");
    for directory in [
        &package,
        &readonly,
        &withheld,
        &base.join("user-home"),
        &base.join("tmp"),
        &base.join("cache"),
    ] {
        std::fs::create_dir_all(directory).expect("create fixture directory");
    }
    let allowed_file = package.join("allowed.txt");
    let readonly_file = readonly.join("readonly.txt");
    let withheld_file = withheld.join("withheld.txt");
    for target in [&allowed_file, &readonly_file, &withheld_file] {
        reset(target);
    }
    let policy = jail(&base, &package, &readonly);

    if abi < 3 {
        let refusal = apply(
            &policy,
            CommandSpec::new(std::env::current_exe().expect("fixture executable"))
                .arg("--should-not-run"),
        );
        let text = refusal
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        println!("ABI-{abi} refusal={text:?}");
        let pass = text.contains("Landlock ABI 3+");
        let _ = std::fs::remove_dir_all(&base);
        std::process::exit(if pass { 0 } else { 1 });
    }

    let mut pass = true;
    for operation in ["truncate", "readonly-open-truncate"] {
        for target in [&allowed_file, &readonly_file, &withheld_file] {
            reset(target);
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--payload")
                .arg(operation)
                .arg(target)
                .output()
                .expect("run unconfined control");
            let size = std::fs::metadata(target)
                .expect("control target survives")
                .len();
            pass &= output.status.success() && size == 0;
            println!(
                "unconfined {operation} target={} status={:?} size={size}",
                target.display(),
                output.status.code()
            );
        }
        pass &= check(
            &policy,
            "allowed writable package",
            &allowed_file,
            operation,
            true,
        );
        pass &= check(
            &policy,
            "readonly extra read",
            &readonly_file,
            operation,
            false,
        );
        pass &= check(
            &policy,
            "withheld no grant",
            &withheld_file,
            operation,
            false,
        );
    }
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    let _ = std::fs::remove_dir_all(&base);
    std::process::exit(if pass { 0 } else { 1 });
}
