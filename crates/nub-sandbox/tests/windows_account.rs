//! Windows dedicated-account (B3 bit 1) RUNTIME probe — `nub-win` VM only.
//!
//! `harness = false`; a role is dispatched on argv[1]. NOT a CI test: CI runs elevated and
//! masks the unelevated run path (design §10), so this is validated on the VM (design §11).
//! Roles:
//!   setup       — ELEVATED: `windows_setup()`; prints the resolved SetupInfo.
//!   detect      — UNELEVATED-safe: `detect()`; prints account/sid + password length (never
//!                 the password itself).
//!   detect-none — asserts `detect()` == `NotSetUp` (fail-closed when unconfigured).
//!   logon       — `detect()` → `LogonUserW(BATCH)` with the stored credential → the logon
//!                 token's User SID. Proves the account exists, the password is correct, and
//!                 SeBatchLogonRight works; asserts token SID == account SID AND != this
//!                 process's SID (the design's "child SID ≠ user SID" acceptance gate at the
//!                 account level, without yet needing bit 2's CreateProcessWithLogonW).
//!   teardown    — ELEVATED: `windows_teardown()`.
//!   whoami      — prints this process's token User SID.

#[cfg(not(target_os = "windows"))]
fn main() {
    // Non-Windows host: nothing to probe (`harness = false` needs a `main`).
}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let role = args.get(1).map(String::as_str).unwrap_or("");
    std::process::exit(win::run(role));
}

#[cfg(target_os = "windows")]
mod win {
    use nub_sandbox::{DetectError, detect, windows_setup, windows_teardown};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, LogonUserW, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    use windows_sys::Win32::System::Threading::{
        CreateProcessWithLogonW, GetCurrentProcess, INFINITE, OpenProcessToken, PROCESS_INFORMATION,
        STARTUPINFOW, WaitForSingleObject,
    };

    const LOGON32_LOGON_BATCH: u32 = 4;
    const LOGON32_PROVIDER_DEFAULT: u32 = 0;
    const LOGON_WITH_PROFILE: u32 = 0x1;
    // File/startup constants defined locally (stable Win32 values; avoids extra feature deps).
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const CREATE_ALWAYS: u32 = 2;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const STARTF_USESTDHANDLES: u32 = 0x100;

    pub fn run(role: &str) -> i32 {
        match role {
            "setup" => match windows_setup() {
                Ok(info) => {
                    println!("SETUP-OK account={} sid={}", info.account_name, info.account_sid);
                    0
                }
                Err(e) => {
                    eprintln!("SETUP-FAIL: {e}");
                    1
                }
            },
            "detect" => match detect() {
                Ok(c) => {
                    println!(
                        "DETECT-OK account={} sid={} pw_len={}",
                        c.account_name,
                        c.account_sid,
                        c.password.len()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("DETECT-FAIL: {e}");
                    1
                }
            },
            "detect-none" => match detect() {
                Err(DetectError::NotSetUp) => {
                    println!("DETECT-NONE-OK (fail-closed as expected)");
                    0
                }
                Err(other) => {
                    eprintln!("DETECT-NONE-WRONG-ERR: {other}");
                    1
                }
                Ok(_) => {
                    eprintln!("DETECT-NONE-FAIL: detect() succeeded but setup should be absent");
                    1
                }
            },
            "logon" => logon_check(),
            "launchtest" => launch_test(),
            "teardown" => match windows_teardown() {
                Ok(()) => {
                    println!("TEARDOWN-OK");
                    0
                }
                Err(e) => {
                    eprintln!("TEARDOWN-FAIL: {e}");
                    1
                }
            },
            "whoami" => {
                match current_process_sid() {
                    Ok(sid) => println!("WHOAMI-SID {sid}"),
                    Err(e) => {
                        eprintln!("WHOAMI-FAIL: {e}");
                        return 1;
                    }
                }
                0
            }
            _ => {
                eprintln!("unknown role: {role:?}");
                2
            }
        }
    }

    fn logon_check() -> i32 {
        let creds = match detect() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("LOGON-FAIL detect: {e}");
                return 1;
            }
        };
        let user = to_wide(&creds.account_name);
        let domain = to_wide(".");
        let pass = to_wide(&creds.password);
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated wide inputs; token received out-param.
        let ok = unsafe {
            LogonUserW(
                user.as_ptr(),
                domain.as_ptr(),
                pass.as_ptr(),
                LOGON32_LOGON_BATCH,
                LOGON32_PROVIDER_DEFAULT,
                &mut token,
            )
        };
        if ok == 0 {
            eprintln!(
                "LOGON-FAIL LogonUserW: {}",
                std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
            );
            return 1;
        }
        let logon_sid = token_user_sid(token);
        unsafe { CloseHandle(token) };
        let logon_sid = match logon_sid {
            Ok(s) => s,
            Err(e) => {
                eprintln!("LOGON-FAIL token sid: {e}");
                return 1;
            }
        };
        let self_sid = match current_process_sid() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("LOGON-FAIL self sid: {e}");
                return 1;
            }
        };
        println!("LOGON token_sid={logon_sid} expected_sid={} self_sid={self_sid}", creds.account_sid);
        if logon_sid != creds.account_sid {
            eprintln!("LOGON-FAIL: logon token SID != recorded account SID");
            return 1;
        }
        if logon_sid == self_sid {
            eprintln!("LOGON-FAIL: logon token SID == this process's SID (not a distinct principal)");
            return 1;
        }
        println!("LOGON-OK account token SID is distinct from the user's SID");
        0
    }

    /// Bit-2 core-mechanism probe: launch a child AS the dedicated account via
    /// `CreateProcessWithLogonW` (the unelevated secondary-logon path) and confirm the child
    /// runs as a DISTINCT principal. The child (`cmd /c whoami /user`) writes its own SID to a
    /// world-writable file. Also the acid test for bit 1's rights model: CreateProcessWithLogonW
    /// performs an INTERACTIVE-type logon, so if the account is denied interactive logon this
    /// fails with 1385 (ERROR_LOGON_TYPE_NOT_GRANTED) — the exact bit-1/bit-2 reconciliation.
    fn launch_test() -> i32 {
        let creds = match detect() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("LAUNCH-FAIL detect: {e}");
                return 1;
            }
        };
        let out = r"C:\Users\Public\childsid.out";
        let _ = std::fs::remove_file(out);
        // The child runs as nub-sandbox, which cannot write arbitrary host paths. Redirect its
        // stdout to a PARENT-OWNED inheritable file handle instead: CreateProcessWithLogonW
        // ignores bInheritHandles but honors STARTF_USESTDHANDLES, and seclogon duplicates the
        // std handles into the child (design §10 — the mechanism bit 2 relies on; verified here).
        let wout = to_wide(out);
        let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = 1;
        let hfile = unsafe {
            CreateFileW(
                wout.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ,
                &sa,
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if hfile == INVALID_HANDLE_VALUE {
            eprintln!(
                "LAUNCH-FAIL CreateFileW: {}",
                std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
            );
            return 1;
        }
        let user = to_wide(&creds.account_name);
        let domain = to_wide(".");
        let pass = to_wide(&creds.password);
        let mut cmdline = to_wide("cmd.exe /c whoami /user");
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = hfile;
        si.hStdError = hfile;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: NUL-terminated wide inputs; cmdline is a writable buffer; env/appname/cwd NULL.
        let ok = unsafe {
            CreateProcessWithLogonW(
                user.as_ptr(),
                domain.as_ptr(),
                pass.as_ptr(),
                LOGON_WITH_PROFILE,
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            unsafe { CloseHandle(hfile) };
            let err = unsafe { GetLastError() };
            eprintln!(
                "LAUNCH-FAIL CreateProcessWithLogonW: err={err} ({})",
                std::io::Error::from_raw_os_error(err as i32)
            );
            if err == 1385 {
                eprintln!(
                    "  → ERROR_LOGON_TYPE_NOT_GRANTED: the account is denied the logon type \
                     CreateProcessWithLogonW uses (interactive). Bit 1 must grant it."
                );
            }
            return 1;
        }
        unsafe {
            WaitForSingleObject(pi.hProcess, INFINITE);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            CloseHandle(hfile); // flush + release so the parent can read the sink
        }
        let child_bytes = std::fs::read(out).unwrap_or_default();
        let child = String::from_utf8_lossy(&child_bytes);
        println!("LAUNCH child whoami output: {}", child.trim());
        if child.contains(&creds.account_sid) {
            println!("LAUNCH-OK child ran as the dedicated account (SID {})", creds.account_sid);
            0
        } else {
            eprintln!("LAUNCH-FAIL: child SID does not match the account SID");
            1
        }
    }

    fn current_process_sid() -> std::io::Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sid = token_user_sid(token);
        unsafe { CloseHandle(token) };
        sid
    }

    /// The token's User SID as a string. Two-call sizing on `GetTokenInformation`.
    fn token_user_sid(token: HANDLE) -> std::io::Result<String> {
        let mut len = 0u32;
        // Sizing call — fails with ERROR_INSUFFICIENT_BUFFER, setting `len`.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = vec![0u8; len as usize];
        if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // The buffer begins with a TOKEN_USER whose User.Sid points inside it.
        let tu = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        sid_to_string(tu.User.Sid)
    }

    fn sid_to_string(sid: PSID) -> std::io::Result<String> {
        let mut out: *mut u16 = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut len = 0usize;
        unsafe {
            while *out.add(len) != 0 {
                len += 1;
            }
        }
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, len) });
        unsafe { LocalFree(out.cast()) };
        Ok(s)
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}
