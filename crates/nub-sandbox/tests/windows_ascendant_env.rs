//! Windows AppContainer — REAL ascendant-env attack probe (windows-latest CI + VM).
//!
//! The module doc + backend claim ascendant-env read is OS-CLOSED: a LowBox child
//! cannot `OpenProcess(PROCESS_VM_READ)` the parent to read nub's environ. The
//! enforcement suite only asserts `apply()` does not REPORT the degradation — it never
//! actually MOUNTS the attack. This probe does: the parent holds a secret in its OWN
//! (unscrubbed) environ, spawns a scrubbed AppContainer child, and the child tries to
//! recover the secret by opening the parent for VM read and walking its PEB → env block.
//!
//! Exit-code contract (child): 5 = OpenProcess DENIED for every VM-read mask (attack
//! blocked, OS-closed holds); 0 = secret RECOVERED from the parent env (BREAKOUT);
//! 3 = a VM-read handle was obtained but the env read failed (PARTIAL — still alarming,
//! the confinement's OpenProcess denial did not hold); 9 = other error.
//!
//! The confined leg's exit-5 is only meaningful if the SAME attack recovers the secret
//! when NOT confined — otherwise "denied" could be a vacuous pass on a broken attack
//! (wrong offsets, a PEB-walk bug, the secret not actually in the parent's env). So an
//! unconfined NEGATIVE CONTROL runs the identical attack from a normal (non-AppContainer)
//! process and asserts exit 0 (the vector is provably live); the confinement is then the
//! only difference between recovery and denial. Mirrors `macos_envread`'s LEAK control.

#[cfg(not(target_os = "windows"))]
fn main() {}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__ascchild__") {
        std::process::exit(win::child_main(&args[2..]));
    }
    match win::run() {
        Ok(()) => println!("ASCENDANT-ENV PROBE: OS-CLOSED CONFIRMED"),
        Err(n) => {
            eprintln!("{n} ASCENDANT-ENV PROBE FAILURE(S)");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use nub_sandbox::policy::{
        CanonGlob, Effect, EnvPolicy, FsAccess, FsOrigin, FsPolicy, FsRule, FsRuleSet, NetPolicy,
        PidPolicy, SandboxPolicy, TmpMode,
    };
    use nub_sandbox::{CommandSpec, apply};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    const SECRET_VAL: &str = "sk-ascendant-leak-DO-NOT-RECOVER";

    // ── the attacker child ────────────────────────────────────────────────────────

    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            // Sanity role: is a var present in the child's OWN (scrubbed) env?
            Some("getenv") => match std::env::var(&a[1]) {
                Ok(_) => 0,
                Err(_) => 4,
            },
            // Otherwise arg0 is the parent pid to attack.
            Some(pidstr) => match pidstr.parse::<u32>() {
                Ok(pid) => attack_parent_env(pid),
                Err(_) => 9,
            },
            None => 9,
        }
    }

    /// Try every VM-read-capable OpenProcess mask on the parent; if any handle opens,
    /// walk its PEB to the environment block and scan for the secret.
    fn attack_parent_env(parent_pid: u32) -> i32 {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_VM_READ,
        };
        // Masks an attacker would try, most-capable first. Every one includes VM_READ
        // (the env block lives in the parent's address space).
        let masks: [(u32, &str); 2] = [
            (PROCESS_VM_READ | PROCESS_QUERY_INFORMATION, "VM_READ|QUERY"),
            (
                PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION,
                "VM_READ|QUERY_LIMITED",
            ),
        ];
        let mut opened: Option<(isize, &str)> = None;
        for (mask, name) in masks {
            // SAFETY: query-only OpenProcess by pid; NULL on denial.
            let h = unsafe { OpenProcess(mask, 0, parent_pid) };
            if !h.is_null() {
                opened = Some((h as isize, name));
                break;
            } else {
                let err = std::io::Error::last_os_error();
                println!("  OpenProcess({name}) DENIED err={:?}", err.raw_os_error());
            }
        }
        let Some((h_raw, name)) = opened else {
            // Every VM-read mask denied → the attack cannot even begin. OS-CLOSED.
            return 5;
        };
        println!("  OpenProcess({name}) SUCCEEDED — handle obtained on parent");
        let h = h_raw as windows_sys::Win32::Foundation::HANDLE;
        let recovered = read_parent_environment(h);
        // SAFETY: close the parent handle.
        unsafe { CloseHandle(h) };
        match recovered {
            Some(true) => {
                println!("  !!! SECRET RECOVERED from parent env block — BREAKOUT");
                0
            }
            Some(false) => {
                println!("  handle obtained; env block read but secret absent");
                3
            }
            None => {
                println!("  handle obtained; PEB/env read FAILED");
                3
            }
        }
    }

    // ntdll's NtQueryInformationProcess — windows-sys gates the typed decl (and the PEB /
    // RTL_USER_PROCESS_PARAMETERS structs redact the `Environment` field), so link it
    // raw. ProcessBasicInformation = 0 fills a PROCESS_BASIC_INFORMATION whose second
    // pointer-sized field (offset 0x8 on x64) is PebBaseAddress.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            handle: windows_sys::Win32::Foundation::HANDLE,
            class: u32,
            info: *mut std::ffi::c_void,
            len: u32,
            ret: *mut u32,
        ) -> i32;
    }

    /// Walk the parent PEB → ProcessParameters → Environment (raw x64 offsets, since
    /// windows-sys redacts the fields) and scan for the secret. Returns Some(true) if
    /// found, Some(false) if the block read but no secret, None if the chain failed.
    fn read_parent_environment(h: windows_sys::Win32::Foundation::HANDLE) -> Option<bool> {
        // x64 struct offsets (stable across modern Windows):
        //   PROCESS_BASIC_INFORMATION.PebBaseAddress          = 0x08
        //   PEB.ProcessParameters                             = 0x20
        //   RTL_USER_PROCESS_PARAMETERS.Environment           = 0x80
        //   RTL_USER_PROCESS_PARAMETERS.EnvironmentSize       = 0x3F0
        let mut pbi = [0u8; 0x30];
        let mut ret_len = 0u32;
        // SAFETY: NtQueryInformationProcess(ProcessBasicInformation) into a 0x30 buffer.
        let status = unsafe {
            NtQueryInformationProcess(
                h,
                0,
                pbi.as_mut_ptr().cast(),
                pbi.len() as u32,
                &mut ret_len,
            )
        };
        if status != 0 {
            return None;
        }
        let peb_base = usize::from_le_bytes(pbi[0x8..0x10].try_into().ok()?);
        if peb_base == 0 {
            return None;
        }
        let params_ptr = read_usize(h, peb_base + 0x20)?;
        if params_ptr == 0 {
            return None;
        }
        let env_ptr = read_usize(h, params_ptr + 0x80)?;
        if env_ptr == 0 {
            return None;
        }
        // Best-effort EnvironmentSize; fall back to a fixed 64 KiB window.
        let env_size = read_usize(h, params_ptr + 0x3F0)
            .filter(|&n| (2..=(1 << 20)).contains(&n))
            .unwrap_or(64 * 1024);
        let mut buf = vec![0u8; env_size];
        if !read_bytes(h, env_ptr as *const u8, &mut buf) {
            // A short read still lets us scan what we got.
            let _ = read_prefix(h, env_ptr as *const u8, &mut buf);
        }
        let wide: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let text = String::from_utf16_lossy(&wide);
        Some(text.contains(SECRET_VAL))
    }

    fn read_usize(h: windows_sys::Win32::Foundation::HANDLE, addr: usize) -> Option<usize> {
        let mut b = [0u8; 8];
        if read_bytes(h, addr as *const u8, &mut b) {
            Some(usize::from_le_bytes(b))
        } else {
            None
        }
    }

    /// A tolerant read that accepts a partial transfer (for the env-block window).
    fn read_prefix(
        h: windows_sys::Win32::Foundation::HANDLE,
        addr: *const u8,
        buf: &mut [u8],
    ) -> bool {
        use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                h,
                addr.cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut read,
            )
        };
        ok != 0 || read > 0
    }

    fn read_bytes(
        h: windows_sys::Win32::Foundation::HANDLE,
        addr: *const u8,
        buf: &mut [u8],
    ) -> bool {
        use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        let mut read = 0usize;
        // SAFETY: read `buf.len()` bytes from the target's address space.
        let ok = unsafe {
            ReadProcessMemory(
                h,
                addr.cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut read,
            )
        };
        ok != 0 && read == buf.len()
    }

    // ── the runner (parent) ───────────────────────────────────────────────────────

    pub fn run() -> Result<(), u32> {
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;
        let mut fails = 0u32;

        // Copy this binary somewhere the LowBox child can exec (traverse-bypass on C:);
        // the CI checkout under D:\a\… is unreadable to a LowBox token.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("nub-asc-{nonce:x}"));
        std::fs::create_dir_all(&root).unwrap();
        secure_root(&root);
        let child = root.join("child.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &child).unwrap();

        // The parent HOLDS the secret in its OWN environ (simulating nub holding an
        // ambient secret at spawn). The child's constructed env WITHHOLDS it (scrub).
        // SAFETY: single-threaded test main.
        unsafe { std::env::set_var("NUB_ASC_SECRET", SECRET_VAL) };

        let pid = unsafe { GetCurrentProcessId() };

        // An fs read-confine policy engages the AppContainer; env-scrub withholds the
        // secret from the child's constructed env.
        let mut policy = read_confine(&[&root]);
        policy.env = EnvPolicy {
            resolved: true,
            enforce: true,
            constructed: base_env(),
            schema: Vec::new(),
            withheld: vec!["NUB_ASC_SECRET".to_string()],
            sensitive_keys: Vec::new(),
        };

        // Sanity: the child's OWN env really is scrubbed — the secret is absent from
        // the child (exit 4), so the ONLY path to it is reading the PARENT's env.
        expect(
            &mut fails,
            "child's own env is scrubbed (secret withheld)",
            code(
                &policy,
                &child,
                &["__ascchild__", "getenv", "NUB_ASC_SECRET"],
            ),
            4,
        );

        // NEGATIVE CONTROL: the IDENTICAL attack from a NORMAL, unconfined process (same
        // binary, same parent pid, no AppContainer token). A same-user process may open
        // its parent for VM_READ, so this MUST recover the secret (exit 0) — proving the
        // vector is live end-to-end (OpenProcess grants, the PEB walk reaches the env
        // block, the secret is really there). Only then is the confined leg's exit-5 a
        // real closure BY CONTRAST rather than a vacuous pass on a broken attack.
        let nc = code_unconfined(&child, &["__ascchild__", &pid.to_string()]);
        match nc {
            0 => println!(
                "PASS neg-control: unconfined attack recovered the parent secret (vector live)"
            ),
            5 => {
                fails += 1;
                eprintln!(
                    "FAIL neg-control VACUOUS: unconfined OpenProcess was DENIED — the closure leg proves nothing"
                );
            }
            3 => {
                fails += 1;
                eprintln!(
                    "FAIL neg-control: unconfined attack opened the parent but could NOT recover the secret (PEB-walk/offsets broken) — the closure leg cannot distinguish a real block"
                );
            }
            other => {
                fails += 1;
                eprintln!("FAIL neg-control: unexpected unconfined child exit {other}");
            }
        }

        // The attack: child tries to recover the secret from the PARENT's env.
        let attack = code(&policy, &child, &["__ascchild__", &pid.to_string()]);
        match attack {
            5 => println!("PASS ascendant-env OS-CLOSED (OpenProcess VM_READ denied on parent)"),
            0 => {
                fails += 1;
                eprintln!(
                    "FAIL BREAKOUT: AppContainer child recovered the parent's scrubbed env secret"
                );
            }
            3 => {
                fails += 1;
                eprintln!(
                    "FAIL PARTIAL: child obtained a VM-read handle on the parent (OpenProcess NOT denied)"
                );
            }
            other => {
                fails += 1;
                eprintln!("FAIL ascendant-env probe: unexpected child exit {other}");
            }
        }

        let _ = std::fs::remove_dir_all(&root);
        if fails == 0 { Ok(()) } else { Err(fails) }
    }

    fn secure_root(root: &Path) {
        let user = std::env::var("USERNAME").expect("USERNAME set on Windows");
        let status = std::process::Command::new("icacls")
            .arg(root)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:(OI)(CI)F"))
            .status()
            .expect("run icacls");
        assert!(status.success(), "icacls failed to secure the fixture root");
    }

    fn code(policy: &SandboxPolicy, program: &Path, args: &[&str]) -> i32 {
        let spec = CommandSpec::new(program.as_os_str())
            .args(args.iter().copied())
            .cwd(program.parent().expect("fixture child has a parent"));
        let prepared = match apply(policy, spec) {
            Ok(p) => p,
            Err(d) => {
                eprintln!("  [apply Err] {d:?}");
                return -100;
            }
        };
        match prepared.status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                eprintln!("  [status Err] {e} os={:?}", e.raw_os_error());
                -101
            }
        }
    }

    /// Spawn the child WITHOUT any sandbox wrapping — a plain, unconfined process. The
    /// negative-control counterpart to `code`: same binary + args, no AppContainer
    /// token, so the confinement is the ONLY difference from the confined leg.
    fn code_unconfined(program: &Path, args: &[&str]) -> i32 {
        match std::process::Command::new(program).args(args).status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                eprintln!("  [unconfined status Err] {e} os={:?}", e.raw_os_error());
                -101
            }
        }
    }

    fn expect(fails: &mut u32, label: &str, got: i32, want: i32) {
        if got == want {
            println!("PASS {label} (exit {got})");
        } else {
            *fails += 1;
            eprintln!("FAIL {label}: exit {got}, expected {want}");
        }
    }

    fn read_confine(read: &[&Path]) -> SandboxPolicy {
        let mut entries = Vec::new();
        for r in read {
            entries.push(FsRule {
                matcher: CanonGlob(r.to_string_lossy().replace('\\', "/")),
                effect: Effect::Allow,
                access: FsAccess::Read,
                origin: FsOrigin::Authored,
            });
        }
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries,
                    default_effect: Effect::Deny,
                },
                tmp: TmpMode::Private,
            },
            net: NetPolicy::default(),
            env: EnvPolicy::resolved(base_env()),
            pid: PidPolicy::default(),
        }
    }

    fn base_env() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for k in [
            "SystemRoot",
            "SystemDrive",
            "windir",
            "ComSpec",
            "PATHEXT",
            "Path",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "APPDATA",
            "LOCALAPPDATA",
            "ProgramData",
            "ALLUSERSPROFILE",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "CommonProgramFiles",
            "PUBLIC",
            "USERNAME",
            "USERDOMAIN",
            "COMPUTERNAME",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "OS",
            "DriverData",
        ] {
            if let Ok(v) = std::env::var(k) {
                m.insert(k.to_string(), v);
            }
        }
        m
    }

    // silence unused on non-child paths
    #[allow(dead_code)]
    fn _unused(_: PathBuf) {}
}
