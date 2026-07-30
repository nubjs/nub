// Probe: does an application silo (JobObjectCreateSilo) + per-silo bindflt mapping give a
// private filesystem view under a NORMAL token, and does that dissolve the three measured
// AppContainer blockers (realpath EPERM, piped spawnSync hang, MSVC)?
//
// Call shapes transcribed from Microsoft's own sources, not derived:
//   hcsshim internal/jobobject/jobobject.go   — PromoteToSilo = SetInformationJobObject(h,35,0,0);
//                                               ApplyFileBinding(root=virt, target=backing)
//   hcsshim internal/jobobject/jobobject_test.go — TestSiloFileBinding, incl. its host-side control
//   go-winio pkg/bindfilter/bind_filter.go    — BfSetupFilter/BfRemoveMapping, jobHandle=0 = GLOBAL
//   BuildXL NativeContainerUtilities.cs       — the full BINDFLT_FLAG_* table
// `bindfltapi.dll` has no import library, so both entry points come from GetProcAddress.
//
// Zero external crates on purpose: builds on a bare box with no network.

#[cfg(not(windows))]
fn main() {
    eprintln!("silo-probe is windows-only");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    type Handle = isize;
    type Bool32 = i32;

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
    const JOB_OBJECT_CREATE_SILO: u32 = 35;
    // hcsshim jobobject.go Create(): "This is a required setting for upgrading to a silo."
    // Without it the promote returns ERROR_INVALID_PARAMETER, which reads like a bad call shape
    // rather than a missing prerequisite — measured on 10.0.20348 before this was added.
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    // BuildXL NativeContainerUtilities.cs
    const BINDFLT_FLAG_READ_ONLY_MAPPING: u32 = 0x0000_0001;
    const BINDFLT_FLAG_USE_CURRENT_SILO_MAPPING: u32 = 0x0000_0004;
    const BINDFLT_FLAG_NO_MULTIPLE_TARGETS: u32 = 0x0000_0040;
    const BINDFLT_FLAG_EMPTY_VIRT_ROOT: u32 = 0x0000_0200;

    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    const PROCESS_DUP_HANDLE: u32 = 0x0040;
    const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;

    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
    const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;
    const TOKEN_USER_CLASS: u32 = 1;
    const TOKEN_PRIVILEGES_CLASS: u32 = 3;
    const TOKEN_ELEVATION_CLASS: u32 = 20;

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        lp_reserved: *mut u16,
        lp_desktop: *mut u16,
        lp_title: *mut u16,
        dw_x: u32,
        dw_y: u32,
        dw_x_size: u32,
        dw_y_size: u32,
        dw_x_count_chars: u32,
        dw_y_count_chars: u32,
        dw_fill_attribute: u32,
        dw_flags: u32,
        w_show_window: u16,
        cb_reserved2: u16,
        lp_reserved2: *mut u8,
        h_std_input: Handle,
        h_std_output: Handle,
        h_std_error: Handle,
    }

    #[repr(C)]
    struct ProcessInformation {
        h_process: Handle,
        h_thread: Handle,
        dw_process_id: u32,
        dw_thread_id: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobExtendedLimitInformation {
        basic: JobBasicLimitInformation,
        io_counters: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[repr(C)]
    struct OsVersionInfoW {
        dw_os_version_info_size: u32,
        dw_major_version: u32,
        dw_minor_version: u32,
        dw_build_number: u32,
        dw_platform_id: u32,
        sz_csd_version: [u16; 128],
    }

    #[repr(C)]
    struct Luid {
        low_part: u32,
        high_part: i32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: Bool32, pid: u32) -> Handle;
        fn DuplicateHandle(
            src_proc: Handle,
            src: Handle,
            dst_proc: Handle,
            dst: *mut Handle,
            access: u32,
            inherit: Bool32,
            options: u32,
        ) -> Bool32;
        fn CreateJobObjectW(sa: *const c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: u32, info: *const c_void, len: u32)
            -> Bool32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool32;
        fn IsProcessInJob(process: Handle, job: Handle, result: *mut Bool32) -> Bool32;
        #[allow(clippy::too_many_arguments)]
        fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attrs: *const c_void,
            thread_attrs: *const c_void,
            inherit: Bool32,
            flags: u32,
            environment: *const c_void,
            cwd: *const u16,
            startup: *const StartupInfoW,
            info: *mut ProcessInformation,
        ) -> Bool32;
        fn ResumeThread(thread: Handle) -> u32;
        fn TerminateProcess(process: Handle, code: u32) -> Bool32;
        fn WaitForSingleObject(handle: Handle, ms: u32) -> u32;
        fn GetExitCodeProcess(process: Handle, code: *mut u32) -> Bool32;
        fn CloseHandle(handle: Handle) -> Bool32;
        fn GetLastError() -> u32;
        fn QueryDosDeviceW(name: *const u16, buf: *mut u16, max: u32) -> u32;
        fn LoadLibraryW(name: *const u16) -> Handle;
        fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
        fn GetCurrentProcess() -> Handle;
        fn GetCurrentProcessId() -> u32;
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> Bool32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> Bool32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            info: *mut c_void,
            len: u32,
            ret_len: *mut u32,
        ) -> Bool32;
        fn LookupAccountSidW(
            system: *const u16,
            sid: *const c_void,
            name: *mut u16,
            name_len: *mut u32,
            domain: *mut u16,
            domain_len: *mut u32,
            use_: *mut u32,
        ) -> Bool32;
        fn LookupPrivilegeNameW(
            system: *const u16,
            luid: *const Luid,
            name: *mut u16,
            len: *mut u32,
        ) -> Bool32;
        fn AdjustTokenPrivileges(
            token: Handle,
            disable_all: Bool32,
            new_state: *const c_void,
            len: u32,
            previous: *mut c_void,
            ret_len: *mut u32,
        ) -> Bool32;
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
        // Kept alongside the Win32 wrapper so a promote failure can be attributed: an NTSTATUS
        // distinguishes STATUS_PRIVILEGE_NOT_HELD from a rejected call shape, which the
        // kernel32 DOS-error mapping flattens.
        fn NtSetInformationJobObject(job: Handle, class: u32, info: *const c_void, len: u32)
            -> i32;
    }

    type BfSetupFilterFn = unsafe extern "system" fn(
        job: Handle,
        flags: u32,
        virt_root: *const u16,
        virt_target: *const u16,
        exceptions: *const *const u16,
        exception_count: u32,
    ) -> i32;
    type BfRemoveMappingFn =
        unsafe extern "system" fn(job: Handle, virt_root: *const u16) -> i32;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn from_w(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    fn err_name(e: u32) -> &'static str {
        match e {
            0 => "ERROR_SUCCESS",
            5 => "ERROR_ACCESS_DENIED",
            50 => "ERROR_NOT_SUPPORTED",
            87 => "ERROR_INVALID_PARAMETER",
            1314 => "ERROR_PRIVILEGE_NOT_HELD",
            _ => "?",
        }
    }

    struct Rep(Vec<String>);
    impl Rep {
        fn line(&mut self, s: impl Into<String>) {
            let s = s.into();
            println!("{s}");
            self.0.push(s);
        }
        fn head(&mut self, s: &str) {
            self.line("");
            self.line(format!("### {s}"));
        }
    }

    fn token_facts(r: &mut Rep) {
        unsafe {
            let mut tok: Handle = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                r.line(format!("token = OpenProcessToken FAILED err={}", GetLastError()));
                return;
            }
            let mut need = 0u32;
            let mut buf = vec![0u8; 512];
            if GetTokenInformation(
                tok,
                TOKEN_USER_CLASS,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut need,
            ) != 0
            {
                // TOKEN_USER == SID_AND_ATTRIBUTES; the PSID is the first field.
                let sid = *(buf.as_ptr() as *const *const c_void);
                let mut name = [0u16; 256];
                let mut domain = [0u16; 256];
                let (mut nl, mut dl, mut kind) = (256u32, 256u32, 0u32);
                if LookupAccountSidW(
                    std::ptr::null(),
                    sid,
                    name.as_mut_ptr(),
                    &mut nl,
                    domain.as_mut_ptr(),
                    &mut dl,
                    &mut kind,
                ) != 0
                {
                    r.line(format!("token-user = {}\\{}", from_w(&domain), from_w(&name)));
                } else {
                    r.line(format!("token-user = <lookup failed {}>", GetLastError()));
                }
            }

            let mut elev = 0u32;
            if GetTokenInformation(
                tok,
                TOKEN_ELEVATION_CLASS,
                &mut elev as *mut u32 as *mut c_void,
                4,
                &mut need,
            ) != 0
            {
                r.line(format!("token-elevated = {}", elev != 0));
            }

            let mut pbuf = vec![0u8; 8192];
            if GetTokenInformation(
                tok,
                TOKEN_PRIVILEGES_CLASS,
                pbuf.as_mut_ptr() as *mut c_void,
                pbuf.len() as u32,
                &mut need,
            ) != 0
            {
                let count = *(pbuf.as_ptr() as *const u32) as usize;
                let mut names = Vec::new();
                for i in 0..count {
                    // TOKEN_PRIVILEGES: DWORD count, then LUID_AND_ATTRIBUTES[] (LUID 8 + attrs 4).
                    let off = 4 + i * 12;
                    let luid = &*(pbuf.as_ptr().add(off) as *const Luid);
                    let attrs = *(pbuf.as_ptr().add(off + 8) as *const u32);
                    let mut nm = [0u16; 128];
                    let mut nl = 128u32;
                    let ok = LookupPrivilegeNameW(std::ptr::null(), luid, nm.as_mut_ptr(), &mut nl);
                    let n = if ok != 0 {
                        from_w(&nm)
                    } else {
                        format!("<luid {}:{}>", luid.high_part, luid.low_part)
                    };
                    names.push(format!("{n}({attrs:#x})"));
                }
                r.line(format!("token-privileges[{count}] = {}", names.join(" ")));
                r.line(format!(
                    "token-has-SeTcbPrivilege = {}",
                    names.iter().any(|n| n.starts_with("SeTcbPrivilege"))
                ));
            }
            CloseHandle(tok);

            let mut sess = 0u32;
            ProcessIdToSessionId(GetCurrentProcessId(), &mut sess);
            r.line(format!("session-id = {sess}"));
        }
    }

    // A privilege granted through policy lands in the token DISABLED. Kernel-side checks
    // (SePrivilegeCheck / SeSinglePrivilegeCheck) test the ENABLED bit, so a policy grant is
    // invisible to a driver until the process enables it — without this, a "grant the right and
    // retry" arm would report a false negative.
    fn enable_all_privileges(r: &mut Rep) {
        unsafe {
            let mut tok: Handle = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES, &mut tok)
                == 0
            {
                r.line(format!("enable-privileges = OpenProcessToken err={}", GetLastError()));
                return;
            }
            let mut need = 0u32;
            let mut buf = vec![0u8; 8192];
            if GetTokenInformation(
                tok,
                TOKEN_PRIVILEGES_CLASS,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut need,
            ) == 0
            {
                CloseHandle(tok);
                return;
            }
            let count = *(buf.as_ptr() as *const u32) as usize;
            for i in 0..count {
                *(buf.as_mut_ptr().add(4 + i * 12 + 8) as *mut u32) = SE_PRIVILEGE_ENABLED;
            }
            let ok = AdjustTokenPrivileges(
                tok,
                0,
                buf.as_ptr() as *const c_void,
                need,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            r.line(format!(
                "enable-all-privileges: adjusted={} lasterr={} (1300 = some not assigned)",
                ok != 0,
                GetLastError()
            ));
            CloseHandle(tok);
        }
    }

    fn resolve_bf(r: &mut Rep) -> (Option<BfSetupFilterFn>, Option<BfRemoveMappingFn>) {
        unsafe {
            r.line(format!(
                "bindfltapi.dll present = {}",
                Path::new(r"C:\Windows\System32\bindfltapi.dll").exists()
            ));
            let m = LoadLibraryW(w("bindfltapi.dll").as_ptr());
            if m == 0 {
                r.line(format!("LoadLibraryW(bindfltapi.dll) FAILED err={}", GetLastError()));
                return (None, None);
            }
            let setup = GetProcAddress(m, b"BfSetupFilter\0".as_ptr());
            let remove = GetProcAddress(m, b"BfRemoveMapping\0".as_ptr());
            r.line(format!(
                "GetProcAddress: BfSetupFilter={} BfRemoveMapping={}",
                !setup.is_null(),
                !remove.is_null()
            ));
            (
                (!setup.is_null())
                    .then(|| std::mem::transmute::<*const c_void, BfSetupFilterFn>(setup)),
                (!remove.is_null())
                    .then(|| std::mem::transmute::<*const c_void, BfRemoveMappingFn>(remove)),
            )
        }
    }

    fn create_job() -> Handle {
        unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) }
    }

    fn set_kill_on_close(job: Handle) -> Result<(), u32> {
        let mut info = JobExtendedLimitInformation::default();
        info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            if SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JobExtendedLimitInformation>() as u32,
            ) != 0
            {
                Ok(())
            } else {
                Err(GetLastError())
            }
        }
    }

    fn promote(job: Handle) -> Result<(), u32> {
        set_kill_on_close(job)?;
        unsafe {
            if SetInformationJobObject(job, JOB_OBJECT_CREATE_SILO, std::ptr::null(), 0) != 0 {
                Ok(())
            } else {
                Err(GetLastError())
            }
        }
    }

    fn bind(bf: BfSetupFilterFn, job: Handle, virt: &str, target: &str, flags: u32) -> i32 {
        unsafe {
            bf(
                job,
                flags,
                w(virt).as_ptr(),
                w(target).as_ptr(),
                std::ptr::null(),
                0,
            )
        }
    }

    struct ChildResult {
        launched: bool,
        assigned: bool,
        in_job: Option<bool>,
        breakaway_needed: bool,
        exit_code: Option<u32>,
        hung: bool,
        detail: String,
    }

    fn run_in_silo(job: Handle, cmdline: &str, cwd: &str, wait_ms: u32) -> ChildResult {
        let mut res = ChildResult {
            launched: false,
            assigned: false,
            in_job: None,
            breakaway_needed: false,
            exit_code: None,
            hung: false,
            detail: String::new(),
        };
        // CREATE_SUSPENDED + Assign + Resume is the race-free form of hcsshim's
        // TestSiloFileBinding (which sleeps instead, to dodge an import cycle).
        for attempt in 0..2 {
            let flags = CREATE_SUSPENDED
                | if attempt == 1 { CREATE_BREAKAWAY_FROM_JOB } else { 0 };
            let mut si: StartupInfoW = unsafe { std::mem::zeroed() };
            si.cb = std::mem::size_of::<StartupInfoW>() as u32;
            let mut pi: ProcessInformation = unsafe { std::mem::zeroed() };
            let mut cl = w(cmdline);
            let ok = unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    cl.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    flags,
                    std::ptr::null(),
                    w(cwd).as_ptr(),
                    &si,
                    &mut pi,
                )
            };
            if ok == 0 {
                res.detail = format!("CreateProcessW err={}", unsafe { GetLastError() });
                continue;
            }
            res.launched = true;
            if unsafe { AssignProcessToJobObject(job, pi.h_process) } == 0 {
                let e = unsafe { GetLastError() };
                res.detail = format!("AssignProcessToJobObject err={e} ({})", err_name(e));
                unsafe {
                    TerminateProcess(pi.h_process, 9);
                    CloseHandle(pi.h_thread);
                    CloseHandle(pi.h_process);
                }
                res.launched = false;
                if attempt == 0 {
                    res.breakaway_needed = true;
                    continue;
                }
                return res;
            }
            res.assigned = true;
            let mut inj: Bool32 = 0;
            if unsafe { IsProcessInJob(pi.h_process, job, &mut inj) } != 0 {
                res.in_job = Some(inj != 0);
            }
            unsafe { ResumeThread(pi.h_thread) };
            if unsafe { WaitForSingleObject(pi.h_process, wait_ms) } == 0 {
                let mut code = 0u32;
                unsafe { GetExitCodeProcess(pi.h_process, &mut code) };
                res.exit_code = Some(code);
            } else {
                res.hung = true;
                res.detail = format!("WaitForSingleObject TIMEOUT after {wait_ms} ms");
                unsafe { TerminateProcess(pi.h_process, 9) };
            }
            unsafe {
                CloseHandle(pi.h_thread);
                CloseHandle(pi.h_process);
            }
            return res;
        }
        res
    }

    fn jsq(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "\\\\")
    }

    fn write(p: &Path, s: &str) {
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        fs::write(p, s).unwrap_or_else(|e| panic!("write {}: {e}", p.display()));
    }

    fn volume_device() -> String {
        let mut buf = [0u16; 512];
        let n = unsafe { QueryDosDeviceW(w("C:").as_ptr(), buf.as_mut_ptr(), 512) };
        if n == 0 {
            String::new()
        } else {
            from_w(&buf)
        }
    }

    pub fn run() {
        let mut args: HashMap<String, String> = HashMap::new();
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i + 1 < raw.len() {
            if let Some(k) = raw[i].strip_prefix("--") {
                args.insert(k.to_string(), raw[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
        }
        let arm = args.get("arm").cloned().unwrap_or_else(|| "default".into());
        let root = PathBuf::from(
            args.get("root")
                .cloned()
                .unwrap_or_else(|| r"C:\silo-probe\default".into()),
        );
        let node = args.get("node").cloned().unwrap_or_else(|| "node.exe".into());
        let outfile = args
            .get("out")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("report.txt"));

        if let Some(role) = args.get("role") {
            let sync = PathBuf::from(
                args.get("sync").cloned().unwrap_or_else(|| r"C:\silo-probe\handoff".into()),
            );
            match role.as_str() {
                "server" => handoff_server(&sync, &outfile),
                "client" => handoff_client(&sync, &node, &outfile),
                other => eprintln!("unknown role {other}"),
            }
            return;
        }

        let mut r = Rep(Vec::new());
        r.line(format!("=========== SILO+BINDFLT PROBE — arm={arm} ==========="));

        r.head("ENVIRONMENT");
        let mut v = OsVersionInfoW {
            dw_os_version_info_size: std::mem::size_of::<OsVersionInfoW>() as u32,
            dw_major_version: 0,
            dw_minor_version: 0,
            dw_build_number: 0,
            dw_platform_id: 0,
            sz_csd_version: [0; 128],
        };
        unsafe { RtlGetVersion(&mut v) };
        r.line(format!(
            "os-version = {}.{}.{}",
            v.dw_major_version, v.dw_minor_version, v.dw_build_number
        ));
        r.line(format!("arch = {}", std::env::consts::ARCH));
        r.line(format!("probe-root = {}", root.display()));
        r.line(format!("node = {node}"));
        r.line(format!(
            "USERPROFILE = {}",
            std::env::var("USERPROFILE").unwrap_or_default()
        ));
        enable_all_privileges(&mut r);
        token_facts(&mut r);
        let dev = volume_device();
        r.line(format!("QueryDosDevice(C:) = {dev}"));

        let (bf, bf_remove) = resolve_bf(&mut r);

        // ---------------------------------------------------------------- Q1
        r.head("Q1 — PRIVILEGE: SetInformationJobObject(job, 35 /*JobObjectCreateSilo*/, NULL, 0)");
        let job0 = create_job();
        if job0 == 0 {
            r.line(format!("CreateJobObjectW FAILED err={}", unsafe { GetLastError() }));
        } else {
            // Control: the KILL_ON_JOB_CLOSE prerequisite doubles as proof the handle and the
            // SetInformationJobObject path are good, so a promote failure is about the promote.
            r.line(format!(
                "CONTROL SetInformationJobObject(class=9 ExtendedLimit, KILL_ON_JOB_CLOSE) = {:?}",
                set_kill_on_close(job0)
            ));
            match promote(job0) {
                Ok(()) => r.line("PROMOTE-TO-SILO = OK".to_string()),
                Err(e) => {
                    r.line(format!("PROMOTE-TO-SILO = FAILED err={e} ({})", err_name(e)));
                    let j = create_job();
                    let _ = set_kill_on_close(j);
                    let st = unsafe {
                        NtSetInformationJobObject(j, JOB_OBJECT_CREATE_SILO, std::ptr::null(), 0)
                    };
                    r.line(format!(
                        "  NtSetInformationJobObject(35) NTSTATUS={st:#010x} ({})",
                        match st as u32 {
                            0 => "STATUS_SUCCESS",
                            0xC000_0061 => "STATUS_PRIVILEGE_NOT_HELD",
                            0xC000_000D => "STATUS_INVALID_PARAMETER",
                            0xC000_0022 => "STATUS_ACCESS_DENIED",
                            0xC000_00BB => "STATUS_NOT_SUPPORTED",
                            _ => "?",
                        }
                    ));
                    unsafe { CloseHandle(j) };
                }
            }
            unsafe { CloseHandle(job0) };
        }

        // ------------------------------------------------------------ FIXTURE
        let backing = root.join("backing");
        let virt_dir = root.join("virt");
        let secrets = root.join("secrets");
        let secrets2 = root.join("secrets2");
        let secrets3 = root.join("secrets3");
        let emptydir = root.join("emptydir");
        let realproj = root.join("realproj");
        let linkedproj = root.join("linkedproj");
        let deep = root.join(r"proj\data\proj\node_modules\dep");
        let _ = fs::remove_dir_all(&root);
        for d in [
            &backing, &virt_dir, &secrets, &secrets2, &secrets3, &emptydir, &realproj, &linkedproj,
            &deep,
        ] {
            fs::create_dir_all(d).unwrap();
        }
        write(&backing.join("bind-test.txt"), "BACKING_FILE_CONTENT");
        write(&secrets.join("CANARY-SECRET.txt"), "TOP_SECRET");
        write(&secrets2.join("CANARY-SECRET.txt"), "TOP_SECRET");
        write(&secrets3.join("CANARY-SECRET.txt"), "TOP_SECRET");
        write(&emptydir.join("MARKER-INSIDE.txt"), "MARKER");
        write(&realproj.join("probe.js"), "// linked project entry\n");

        let canary = secrets.join("CANARY-SECRET.txt");
        let marker = secrets.join("MARKER-INSIDE.txt"); // resolvable only INSIDE the silo
        let virtfile = virt_dir.join("silo-path.txt"); // resolvable only INSIDE the silo
        let linkedjs = linkedproj.join("probe.js"); // resolvable only INSIDE the silo
        let child_out = root.join("child-main.txt");
        let entry = deep.join("index.js");

        let gr = |p: &Path| {
            format!(
                r"\\?\GLOBALROOT{}{}",
                dev,
                p.to_string_lossy().trim_start_matches("C:")
            )
        };
        let globalroot_canary = gr(&canary);
        let globalroot_secrets = gr(&secrets);

        let script = format!(
            r#"
const fs = require('fs');
const cp = require('child_process');
const out = [];
function say(k, v) {{ out.push('  ' + k + ' = ' + v); }}
function t(k, fn) {{ try {{ say(k, fn()); }} catch (e) {{ say(k, 'ERR:' + (e.code || '') + ' ' + e.message); }} }}

const CANARY   = "{canary}";
const MARKER   = "{marker}";
const VIRTFILE = "{virtfile}";
const LINKEDJS = "{linkedjs}";
const GR_CANARY  = "{gr_canary}";
const GR_SECRETS = "{gr_secrets}";
const SECRETS  = "{secrets}";
const SECRETS2 = "{secrets2}";
const SECRETS3 = "{secrets3}";

// Reaching this line at all IS the Q3 answer: `node <deep file>` unflagged means
// resolveMainPath -> _findPath -> toRealPath -> realpathSync already ran on the entry.
say('child-reached-user-code', 'YES');
say('child-pid', process.pid);
say('child-filename', __filename);
say('child-cwd', process.cwd());

// --- isolation: proves this process really is inside the silo -----------------
t('canary-exists  (silo: expect FALSE)', () => fs.existsSync(CANARY));
t('marker-exists  (silo: expect TRUE)',  () => fs.existsSync(MARKER));
t('canary-read',   () => JSON.stringify(fs.readFileSync(CANARY, 'utf8')));
t('marker-read',   () => JSON.stringify(fs.readFileSync(MARKER, 'utf8')));
t('secrets-readdir  (default flags)',            () => JSON.stringify(fs.readdirSync(SECRETS)));
t('secrets2-readdir (+NO_MULTIPLE_TARGETS)',     () => JSON.stringify(fs.readdirSync(SECRETS2)));
t('secrets3-readdir (+EMPTY_VIRT_ROOT)',         () => JSON.stringify(fs.readdirSync(SECRETS3)));
t('virtfile-read (file-level binding)', () => JSON.stringify(fs.readFileSync(VIRTFILE, 'utf8')));
t('write into shadowed dir', () => {{ fs.writeFileSync(SECRETS + '\\WROTE-INSIDE.txt', 'x'); return 'OK'; }});

// --- Q3: the AppContainer realpath blocker ------------------------------------
t('realpathSync(C:\\)',            () => fs.realpathSync('C:\\'));
t('lstatSync(C:\\).isDirectory',   () => fs.lstatSync('C:\\').isDirectory());
t('statSync(C:\\).isDirectory',    () => fs.statSync('C:\\').isDirectory());
t('readdirSync(C:\\).length',      () => fs.readdirSync('C:\\').length);
t('realpathSync(__filename)',      () => fs.realpathSync(__filename));
t('realpathSync.native(__filename)', () => fs.realpathSync.native(__filename));
t('require(./sibling)',            () => {{ fs.writeFileSync(__dirname + '\\sib.js', 'module.exports=42;'); return require('./sib.js'); }});

// --- Q5: what realpath RETURNS for a bind-linked path -------------------------
t('realpathSync(LINKED/probe.js)',        () => fs.realpathSync(LINKEDJS));
t('realpathSync.native(LINKED/probe.js)', () => fs.realpathSync.native(LINKEDJS));
t('realpathSync(VIRTFILE)',               () => fs.realpathSync(VIRTFILE));
t('realpathSync.native(VIRTFILE)',        () => fs.realpathSync.native(VIRTFILE));
t('realpathSync(SECRETS dir)',            () => fs.realpathSync(SECRETS));

// --- Q5: is the mapping a BOUNDARY, or can a device path walk around it? ------
t('ESCAPE \\\\?\\ + canary',         () => JSON.stringify(fs.readFileSync('\\\\?\\' + CANARY, 'utf8')));
t('ESCAPE \\\\?\\ + secrets dir',    () => JSON.stringify(fs.readdirSync('\\\\?\\' + SECRETS)));
t('ESCAPE GLOBALROOT + canary',      () => JSON.stringify(fs.readFileSync(GR_CANARY, 'utf8')));
t('ESCAPE GLOBALROOT + secrets dir', () => JSON.stringify(fs.readdirSync(GR_SECRETS)));

// --- Q4: piped spawnSync, the measured libuv hang under AppContainer ----------
function timed(k, fn) {{
  const t0 = Date.now();
  let res;
  try {{ res = fn(); }} catch (e) {{ say(k + ' THREW', e.message); say(k + ' elapsed_ms', Date.now() - t0); return; }}
  say(k + ' elapsed_ms', Date.now() - t0);
  say(k + ' status', res.status);
  say(k + ' signal', res.signal);
  say(k + ' stdout', JSON.stringify(res.stdout));
  say(k + ' stderr', JSON.stringify((res.stderr || '').slice(0, 200)));
  say(k + ' error', res.error ? res.error.message : 'none');
}}
timed('spawnSync-node-piped', () => cp.spawnSync(process.execPath,
  ['-e', 'process.stdout.write("PIPED_OK")'],
  {{ stdio: 'pipe', timeout: 60000, encoding: 'utf8' }}));
timed('spawnSync-cmd-piped', () => cp.spawnSync('cmd.exe', ['/c', 'echo CMD_OK'],
  {{ stdio: 'pipe', timeout: 60000, encoding: 'utf8' }}));
// Grandchild: does the silo view survive one more process hop (npm -> node-gyp -> MSBuild)?
timed('spawnSync-grandchild-canary', () => cp.spawnSync(process.execPath,
  ['-e', 'process.stdout.write(require("fs").existsSync(process.argv[1]) ? "CANARY_VISIBLE" : "CANARY_HIDDEN")', CANARY],
  {{ stdio: 'pipe', timeout: 60000, encoding: 'utf8' }}));
t('execSync(node -p 1+1)', () => cp.execSync(JSON.stringify(process.execPath) + ' -p "1+1"', {{ timeout: 60000 }}).toString().trim());

out.push('  CHILD-DONE');
const text = out.join('\n');
console.log(text);
try {{ fs.writeFileSync("{childout}", text); }} catch (e) {{ console.log('  child-writeout-failed = ' + e.message); }}
"#,
            canary = jsq(&canary),
            marker = jsq(&marker),
            virtfile = jsq(&virtfile),
            linkedjs = jsq(&linkedjs),
            gr_canary = globalroot_canary.replace('\\', "\\\\"),
            gr_secrets = globalroot_secrets.replace('\\', "\\\\"),
            secrets = jsq(&secrets),
            secrets2 = jsq(&secrets2),
            secrets3 = jsq(&secrets3),
            childout = jsq(&child_out),
        );
        write(&entry, &script);

        let bf = match bf {
            Some(f) => f,
            None => {
                r.line("bindflt unavailable — stopping".to_string());
                finish(&r, &outfile);
                return;
            }
        };

        // --------------------------------------------------------------- Q1b
        // The differential the public write-ups conflate: go-winio's GLOBAL binding
        // (jobHandle = 0) is guarded by requireElevated; the silo binding is not.
        r.head("Q1b — GLOBAL binding (jobHandle=0), the arm prior art says needs admin");
        let gvirt = root.join("globalvirt");
        let _ = fs::create_dir_all(&gvirt);
        let hr = bind(
            bf,
            0,
            &gvirt.to_string_lossy(),
            &emptydir.to_string_lossy(),
            BINDFLT_FLAG_NO_MULTIPLE_TARGETS,
        );
        r.line(format!("GLOBAL BfSetupFilter hr={hr:#010x}"));
        if hr == 0 {
            r.line(format!(
                "GLOBAL mapping visible to THIS (unassigned) process = {}",
                gvirt.join("MARKER-INSIDE.txt").exists()
            ));
            if let Some(rm) = bf_remove {
                let rhr = unsafe { rm(0, w(&gvirt.to_string_lossy()).as_ptr()) };
                r.line(format!("BfRemoveMapping hr={rhr:#010x}"));
            }
        }

        // ---------------------------------------------------------------- Q2
        r.head("Q2 — ISOLATION (mirrors hcsshim TestSiloFileBinding, with its host-side control)");
        r.line(format!(
            "HOST before: canary visible={} | virtfile visible={} | linked probe.js visible={}",
            canary.exists(),
            virtfile.exists(),
            linkedjs.exists()
        ));

        let job = create_job();
        let promoted = promote(job).is_ok();
        r.line(format!("silo promoted = {promoted}"));
        if !promoted {
            r.line("cannot bind without a silo — stopping".to_string());
            unsafe { CloseHandle(job) };
            finish(&r, &outfile);
            return;
        }

        let silo = BINDFLT_FLAG_USE_CURRENT_SILO_MAPPING;
        for (v, t, flags, note) in [
            (virtfile.clone(), backing.join("bind-test.txt"), silo, "file-level"),
            (secrets.clone(), emptydir.clone(), silo, "shadow, default flags"),
            (
                secrets2.clone(),
                emptydir.clone(),
                silo | BINDFLT_FLAG_NO_MULTIPLE_TARGETS,
                "shadow, +NO_MULTIPLE_TARGETS",
            ),
            (
                secrets3.clone(),
                emptydir.clone(),
                silo | BINDFLT_FLAG_EMPTY_VIRT_ROOT,
                "shadow, +EMPTY_VIRT_ROOT",
            ),
            (
                linkedproj.clone(),
                realproj.clone(),
                silo | BINDFLT_FLAG_READ_ONLY_MAPPING,
                "read-only project link",
            ),
        ] {
            let hr = bind(bf, job, &v.to_string_lossy(), &t.to_string_lossy(), flags);
            r.line(format!(
                "BfSetupFilter hr={hr:#010x} flags={flags:#x} [{note}]  {}  <-  {}",
                v.display(),
                t.display()
            ));
        }

        // The control: the mapping must be invisible OUTSIDE the job.
        r.line(format!(
            "HOST after bind: canary visible={} (expect true) | virtfile visible={} (expect FALSE) | linked probe.js visible={} (expect FALSE)",
            canary.exists(),
            virtfile.exists(),
            linkedjs.exists()
        ));

        // ------------------------------------------------------------- Q3/4/5
        r.head("Q3/Q4/Q5 — UNFLAGGED `node <deep file>` INSIDE THE SILO");
        let cmd = format!("\"{}\" \"{}\"", node, entry.display());
        r.line(format!("cmdline = {cmd}"));
        let res = run_in_silo(job, &cmd, "C:\\", 180_000);
        r.line(format!(
            "launched={} assigned={} in_job={:?} breakaway_needed={} exit={:?} hung={} {}",
            res.launched,
            res.assigned,
            res.in_job,
            res.breakaway_needed,
            res.exit_code,
            res.hung,
            res.detail
        ));
        match fs::read_to_string(&child_out) {
            Ok(s) => r.line(s),
            Err(e) => r.line(format!("  <no child report: {e}>")),
        }

        // Baseline: identical script, identical node, NO silo. Separates "the silo broke it"
        // from "the fixture is wrong".
        r.head("BASELINE — same script, same node, NO silo (host)");
        let host_out = root.join("child-host.txt");
        let host_script = script.replace(&jsq(&child_out), &jsq(&host_out));
        let host_entry = root.join(r"hostproj\data\proj\node_modules\dep\index.js");
        write(&host_entry, &host_script);
        match std::process::Command::new(&node)
            .arg(&host_entry)
            .current_dir("C:\\")
            .output()
        {
            Ok(o) => {
                r.line(format!("host exit = {:?}", o.status.code()));
                r.line(String::from_utf8_lossy(&o.stdout).to_string());
                if !o.stderr.is_empty() {
                    r.line(format!("host stderr = {}", String::from_utf8_lossy(&o.stderr)));
                }
            }
            Err(e) => r.line(format!("host baseline failed to spawn: {e}")),
        }

        unsafe { CloseHandle(job) };
        r.head("RESIDUE after the silo handle closes");
        r.line(format!(
            "HOST: canary visible={} (expect true) | virtfile visible={} (expect false) | linked probe.js visible={} (expect false)",
            canary.exists(),
            virtfile.exists(),
            linkedjs.exists()
        ));
        r.line(format!(
            "HOST: WROTE-INSIDE.txt landed in the BACKING dir = {} | in the shadowed dir = {}",
            emptydir.join("WROTE-INSIDE.txt").exists(),
            secrets.join("WROTE-INSIDE.txt").exists()
        ));

        // -------------------------------------------------- USERPROFILE SHADOW
        r.head("SHADOWING THE REAL %USERPROFILE% (the actual proposed use)");
        if let Ok(up) = std::env::var("USERPROFILE") {
            let up_canary = PathBuf::from(&up).join("NUB-PROBE-HOME-SECRET.txt");
            let up_empty = root.join("emptyhome");
            let _ = fs::create_dir_all(&up_empty);
            write(&up_empty.join("EMPTY-HOME-MARKER.txt"), "EMPTY_HOME");
            let wrote_canary = fs::write(&up_canary, "HOME_SECRET").is_ok();
            let up_out = root.join("child-up.txt");
            let up_script = format!(
                r#"
const fs = require('fs');
const out = [];
function t(k, fn) {{ try {{ out.push('  ' + k + ' = ' + fn()); }} catch (e) {{ out.push('  ' + k + ' = ERR:' + (e.code || '') + ' ' + e.message); }} }}
out.push('  up-child-reached-user-code = YES');
t('home-secret-exists (expect FALSE)', () => fs.existsSync("{c}"));
t('empty-home-marker-exists (expect TRUE)', () => fs.existsSync("{m}"));
t('userprofile-readdir', () => JSON.stringify(fs.readdirSync("{u}")));
t('realpathSync(USERPROFILE)', () => fs.realpathSync("{u}"));
t('os.homedir()', () => require('os').homedir());
const text = out.join('\n');
console.log(text);
fs.writeFileSync("{o}", text);
"#,
                c = jsq(&up_canary),
                m = jsq(&up_empty.join("EMPTY-HOME-MARKER.txt")),
                u = up.replace('\\', "\\\\"),
                o = jsq(&up_out),
            );
            let up_entry = root.join("up-check.js");
            write(&up_entry, &up_script);
            r.line(format!("USERPROFILE = {up} | canary written = {wrote_canary}"));
            let job2 = create_job();
            let p2 = promote(job2).is_ok();
            let hr = bind(
                bf,
                job2,
                &up,
                &up_empty.to_string_lossy(),
                silo | BINDFLT_FLAG_NO_MULTIPLE_TARGETS,
            );
            r.line(format!(
                "promoted={p2} BfSetupFilter({up} <- {}) hr={hr:#010x}",
                up_empty.display()
            ));
            let cmd2 = format!("\"{}\" \"{}\"", node, up_entry.display());
            let res2 = run_in_silo(job2, &cmd2, "C:\\", 120_000);
            r.line(format!(
                "launched={} assigned={} exit={:?} hung={} {}",
                res2.launched, res2.assigned, res2.exit_code, res2.hung, res2.detail
            ));
            match fs::read_to_string(&up_out) {
                Ok(s) => r.line(s),
                Err(e) => r.line(format!("  <no up-child report: {e}>")),
            }
            unsafe { CloseHandle(job2) };
            let _ = fs::remove_file(&up_canary);
        }

        // ------------------------------------------------------------- TIMING
        r.head("TIMING — silo create + promote + one directory binding (vs ~20 ms leaf ACE today)");
        let mut create_us = Vec::new();
        let mut promote_us = Vec::new();
        let mut bind_us = Vec::new();
        for _ in 0..25 {
            let t0 = Instant::now();
            let j = create_job();
            create_us.push(t0.elapsed().as_micros());
            let t1 = Instant::now();
            let ok = promote(j).is_ok();
            promote_us.push(t1.elapsed().as_micros());
            if ok {
                let t2 = Instant::now();
                bind(
                    bf,
                    j,
                    &secrets.to_string_lossy(),
                    &emptydir.to_string_lossy(),
                    silo | BINDFLT_FLAG_NO_MULTIPLE_TARGETS,
                );
                bind_us.push(t2.elapsed().as_micros());
            }
            unsafe { CloseHandle(j) };
        }
        for (name, mut v) in [
            ("CreateJobObjectW", create_us),
            ("PromoteToSilo", promote_us),
            ("BfSetupFilter(1 dir)", bind_us),
        ] {
            if v.is_empty() {
                continue;
            }
            v.sort_unstable();
            r.line(format!(
                "{name}: n={} min={}us median={}us max={}us",
                v.len(),
                v[0],
                v[v.len() / 2],
                v[v.len() - 1]
            ));
        }

        r.line("");
        r.line(format!("=========== END arm={arm} ==========="));
        finish(&r, &outfile);
    }

    // ---------------------------------------------------------------- HANDOFF
    // The product question: the bindflt gate is admin, and a binding is per-silo, so if the
    // privileged step can be done ONCE by an installed helper and the job handle handed to an
    // ordinary process, nub ships as a one-time setup. If not, every install needs elevation.
    // Server = elevated (stands in for the service); client = the standard-user account.

    fn poll_file(p: &Path, secs: u64) -> Option<String> {
        let deadline = Instant::now() + std::time::Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(s) = fs::read_to_string(p) {
                if !s.trim().is_empty() {
                    return Some(s.trim().to_string());
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        None
    }

    fn handoff_fixture(sync: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let secrets = sync.join("secrets");
        let emptydir = sync.join("emptydir");
        fs::create_dir_all(&secrets).unwrap();
        fs::create_dir_all(&emptydir).unwrap();
        write(&secrets.join("CANARY-SECRET.txt"), "TOP_SECRET");
        write(&emptydir.join("MARKER-INSIDE.txt"), "MARKER");
        (secrets, emptydir, sync.join("check.js"))
    }

    pub fn handoff_server(sync: &Path, outfile: &Path) {
        let mut r = Rep(Vec::new());
        r.line("=========== HANDOFF SERVER (elevated stand-in for an installed helper) ===========");
        token_facts(&mut r);
        let (bf, _) = resolve_bf(&mut r);
        let bf = bf.expect("bindflt");

        let _ = fs::remove_dir_all(sync);
        let (secrets, emptydir, checkjs) = handoff_fixture(sync);
        let report = sync.join("child-report.txt");
        write(
            &checkjs,
            &format!(
                r#"
const fs = require('fs'); const o = [];
function t(k, f) {{ try {{ o.push('  ' + k + ' = ' + f()); }} catch (e) {{ o.push('  ' + k + ' = ERR:' + (e.code || '') + ' ' + e.message); }} }}
o.push('  handoff-child-reached-user-code = YES');
t('canary-exists (silo: expect FALSE)', () => fs.existsSync("{c}"));
t('marker-exists (silo: expect TRUE)',  () => fs.existsSync("{m}"));
t('secrets-readdir', () => JSON.stringify(fs.readdirSync("{s}")));
t('realpathSync(C:\\)', () => fs.realpathSync('C:\\'));
const txt = o.join('\n'); console.log(txt); fs.writeFileSync("{o}", txt);
"#,
                c = jsq(&secrets.join("CANARY-SECRET.txt")),
                m = jsq(&secrets.join("MARKER-INSIDE.txt")),
                s = jsq(&secrets),
                o = jsq(&report),
            ),
        );
        write(&sync.join("fixture-ready.txt"), "1");

        let pid: u32 = match poll_file(&sync.join("pid.txt"), 240) {
            Some(s) => s.parse().unwrap_or(0),
            None => {
                r.line("no client pid — giving up".to_string());
                finish(&r, outfile);
                return;
            }
        };
        r.line(format!("client pid = {pid}"));

        let job = create_job();
        let promoted = promote(job).is_ok();
        let hr = bind(
            bf,
            job,
            &secrets.to_string_lossy(),
            &emptydir.to_string_lossy(),
            BINDFLT_FLAG_USE_CURRENT_SILO_MAPPING | BINDFLT_FLAG_NO_MULTIPLE_TARGETS,
        );
        r.line(format!("server: promoted={promoted} BfSetupFilter hr={hr:#010x}"));

        unsafe {
            let target = OpenProcess(PROCESS_DUP_HANDLE, 0, pid);
            if target == 0 {
                r.line(format!("OpenProcess(PROCESS_DUP_HANDLE) FAILED err={}", GetLastError()));
                finish(&r, outfile);
                return;
            }
            let mut dup: Handle = 0;
            let ok = DuplicateHandle(
                GetCurrentProcess(),
                job,
                target,
                &mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            );
            r.line(format!(
                "DuplicateHandle -> client = {} (handle {dup}) err={}",
                ok != 0,
                GetLastError()
            ));
            write(&sync.join("job.txt"), &dup.to_string());
            CloseHandle(target);
        }

        match poll_file(&sync.join("client-done.txt"), 240) {
            Some(s) => r.line(s),
            None => r.line("client never reported".to_string()),
        }
        r.line(format!(
            "SERVER-SIDE CONTROL — host still sees the real canary = {}",
            secrets.join("CANARY-SECRET.txt").exists()
        ));
        unsafe { CloseHandle(job) };
        finish(&r, outfile);
    }

    pub fn handoff_client(sync: &Path, node: &str, outfile: &Path) {
        let mut r = Rep(Vec::new());
        r.line("=========== HANDOFF CLIENT (unprivileged) ===========");
        enable_all_privileges(&mut r);
        token_facts(&mut r);
        let (bf, _) = resolve_bf(&mut r);

        poll_file(&sync.join("fixture-ready.txt"), 240);

        // Control, in this very process: the client must be unable to bind on its own. Without
        // it, a successful handoff could just mean the client was privileged after all.
        if let Some(bf) = bf {
            let own = create_job();
            let p = promote(own).is_ok();
            let hr = bind(
                bf,
                own,
                &sync.join("client-own-virt").to_string_lossy(),
                &sync.join("emptydir").to_string_lossy(),
                BINDFLT_FLAG_USE_CURRENT_SILO_MAPPING | BINDFLT_FLAG_NO_MULTIPLE_TARGETS,
            );
            r.line(format!(
                "CLIENT CONTROL: own promote={p}, own BfSetupFilter hr={hr:#010x} (expect 0x80070005)"
            ));
            unsafe { CloseHandle(own) };
        }

        write(&sync.join("pid.txt"), &std::process::id().to_string());
        let handle: Handle = match poll_file(&sync.join("job.txt"), 240) {
            Some(s) => s.parse().unwrap_or(0),
            None => {
                r.line("no job handle arrived".to_string());
                finish(&r, outfile);
                return;
            }
        };
        r.line(format!("received job handle = {handle}"));

        let cmd = format!("\"{}\" \"{}\"", node, sync.join("check.js").display());
        let res = run_in_silo(handle, &cmd, "C:\\", 120_000);
        r.line(format!(
            "CLIENT LAUNCH INTO HANDED-OVER SILO: launched={} assigned={} in_job={:?} exit={:?} hung={} {}",
            res.launched, res.assigned, res.in_job, res.exit_code, res.hung, res.detail
        ));
        match fs::read_to_string(sync.join("child-report.txt")) {
            Ok(s) => r.line(s),
            Err(e) => r.line(format!("  <no child report: {e}>")),
        }
        finish(&r, outfile);
        let _ = fs::write(sync.join("client-done.txt"), r.0.join("\r\n"));
    }

    fn finish(r: &Rep, out: &Path) {
        if let Some(d) = out.parent() {
            let _ = fs::create_dir_all(d);
        }
        let _ = fs::write(out, r.0.join("\r\n"));
    }
}
