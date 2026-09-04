// Four-arm proof harness for the ported transparent egress supervisor.
//   probe allow      -> supervised curl to an ALLOWED host, expect success
//   probe deny       -> supervised curl to a DENIED host, expect connect refused
//   probe iouring    -> supervised io_uring_setup, expect EPERM (bypass closed)
//   probe control    -> UNSUPERVISED curl to the "denied" host, expect success (discriminator)
//   probe __iouring_child -> internal: attempt io_uring_setup, report via exit code
use std::env;
use std::ffi::CString;
use linux_supervisor_probe::backend::linux_supervisor::{run_supervised, EgressPolicy};

const ALLOWED: &str = "example.com";
const DENIED: &str = "www.google.com";

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn curl_argv(host: &str) -> Vec<CString> {
    // -sS quiet-but-errors, -m timeout, print the HTTP status to stdout, discard body.
    [
        "curl", "-sS", "-4", "-m", "20", "-o", "/dev/null",
        "-w", "HTTP=%{http_code}\\n", &format!("http://{host}/"),
    ]
    .iter()
    .map(|s| cstr(s))
    .collect()
}

fn main() {
    let arm = env::args().nth(1).unwrap_or_default();
    match arm.as_str() {
        "__iouring_child" => {
            // io_uring_params is 120 bytes on x86_64; a zeroed one with entries=1 is a valid
            // setup request on a kernel with io_uring enabled.
            let mut params = [0u8; 120];
            let rc = unsafe {
                libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr())
            };
            if rc >= 0 {
                unsafe { libc::close(rc as i32) };
                println!("IOURING_CHILD ok fd={rc}");
                std::process::exit(0);
            }
            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            println!("IOURING_CHILD errno={e}");
            // 42 == EPERM sentinel, 43 == other errno
            std::process::exit(if e == libc::EPERM { 42 } else { 43 });
        }
        "allow" => {
            let policy = EgressPolicy { allow_all: false, allow: vec![ALLOWED.into()] };
            let rc = run_supervised(policy, &curl_argv(ALLOWED)).expect("run");
            println!("ARM allow: host={ALLOWED} child_exit={rc}");
            std::process::exit(rc);
        }
        "deny" => {
            // ALLOWED is allowed; DENIED is not. curl to DENIED must be blocked at connect.
            let policy = EgressPolicy { allow_all: false, allow: vec![ALLOWED.into()] };
            let rc = run_supervised(policy, &curl_argv(DENIED)).expect("run");
            println!("ARM deny: host={DENIED} child_exit={rc} (nonzero == blocked)");
            std::process::exit(rc);
        }
        "iouring" => {
            let self_exe = env::current_exe().unwrap();
            let argv = vec![cstr(self_exe.to_str().unwrap()), cstr("__iouring_child")];
            let policy = EgressPolicy { allow_all: true, allow: vec![] };
            let rc = run_supervised(policy, &argv).expect("run");
            println!("ARM iouring(supervised): child_exit={rc} (42 == EPERM, bypass closed)");
            std::process::exit(rc);
        }
        "iouring-control" => {
            let self_exe = env::current_exe().unwrap();
            let status = std::process::Command::new(self_exe)
                .arg("__iouring_child")
                .status()
                .unwrap();
            let rc = status.code().unwrap_or(-1);
            println!("ARM iouring(control,unsupervised): child_exit={rc} (0 == io_uring works here)");
            std::process::exit(rc);
        }
        "control" => {
            // No supervisor at all: the "denied" host must be reachable, proving the deny
            // arm's block is the supervisor's doing and not a dead host.
            let status = std::process::Command::new("curl")
                .args(["-sS", "-4", "-m", "20", "-o", "/dev/null", "-w", "HTTP=%{http_code}\n",
                       &format!("http://{DENIED}/")])
                .status()
                .unwrap();
            let rc = status.code().unwrap_or(-1);
            println!("ARM control: host={DENIED} curl_exit={rc} (0 == reachable without supervisor)");
            std::process::exit(rc);
        }
        other => {
            eprintln!("unknown arm: {other:?}");
            std::process::exit(2);
        }
    }
}
