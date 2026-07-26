//! nub-sandbox-probe — the black-box probe the sandbox conformance matrix runs
//! UNDER `nub run --sandbox`. It attempts ONE action and reports the outcome via its
//! EXIT CODE so the conformance runner can diff actual vs expected without parsing
//! output:
//!   0 → the action SUCCEEDED (the sandbox ALLOWED it),
//!   7 → the action was DENIED / failed (a distinctive code so the runner can tell a
//!       genuine denial apart from a nub-side compile/apply error, which exits 1),
//!   2 → a usage error.
//!
//! std-only by construction: no dependency links vcruntime on Windows, so under
//! `-C target-feature=+crt-static` the binary starts cleanly inside the AppContainer
//! LowBox (a dynamic vcruntime dep would fail to load there and read as a false
//! denial — the same reason the Windows enforcement probe is crt-static).

use std::io::{Read, Write};
use std::process::ExitCode;

const DENIED: u8 = 7;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let action = args.first().map(String::as_str).unwrap_or("");
    // `env-print` prints each named var's value to stdout (`KEY=VALUE` per line) so the
    // output-redaction e2e can assert a granted secret's value is scrubbed on the wire
    // while a non-secret var passes through. As a non-Node binary it also proves the
    // scrub is language-agnostic. Its verdict is the printed bytes, not an exit code.
    if action == "env-print" {
        return do_env_print(&args[1..]);
    }
    let allowed = match action {
        "read" => do_read(arg(&args, 1)),
        "write" => do_write(arg(&args, 1)),
        "connect" => do_connect(arg(&args, 1), arg(&args, 2)),
        "env" => do_env(arg(&args, 1)),
        _ => {
            eprintln!("usage: nub-sandbox-probe <read|write|connect|env|env-print> <arg...>");
            return ExitCode::from(2);
        }
    };
    // A stderr marker aids CI triage; the verdict is the exit code, not this line.
    eprintln!(
        "probe {action} {} -> {}",
        arg(&args, 1),
        if allowed { "ALLOWED" } else { "DENIED" }
    );
    if allowed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(DENIED)
    }
}

fn arg(args: &[String], i: usize) -> &str {
    args.get(i).map(String::as_str).unwrap_or("")
}

/// Open-for-read is the read-permission gate on all three backends, so a successful
/// open means allowed. The
/// one-byte read confirms a usable descriptor; an empty file (0 bytes) still counts.
fn do_read(path: &str) -> bool {
    match std::fs::File::open(path) {
        Ok(mut f) => f.read(&mut [0u8; 1]).is_ok(),
        Err(_) => false,
    }
}

/// Create-or-open-for-write + one byte. A denied directory fails at open.
fn do_write(path: &str) -> bool {
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
    {
        Ok(mut f) => f.write_all(b"x").is_ok(),
        Err(_) => false,
    }
}

/// TCP connect with a bounded timeout. Any failure to reach the peer — a blocked
/// `socket()` (Linux seccomp), a blocked `connect()` (Seatbelt / WFP), or a blocked
/// DNS lookup — surfaces as "not allowed", which is the correct DENIED verdict.
fn do_connect(host: &str, port: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(5)).is_ok())
}

/// Present-and-non-empty. Env access has no OS gate; this reports what nub actually
/// placed in the child's constructed environment (the env-scrub verdict).
fn do_env(key: &str) -> bool {
    std::env::var(key).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Print each named var's value as `KEY=VALUE` to stdout — the child's raw output the
/// Nub-side redactor scrubs. A missing var prints `KEY=<unset>`.
fn do_env_print(keys: &[String]) -> ExitCode {
    let mut out = std::io::stdout();
    for key in keys {
        let value = std::env::var(key).unwrap_or_else(|_| "<unset>".to_string());
        let _ = writeln!(out, "{key}={value}");
    }
    let _ = out.flush();
    ExitCode::SUCCESS
}
