//! The working root whose DACL the EFFECTIVE-RIGHTS API refuses to evaluate.
//!
//! `verify_clean_root` used to ask `GetEffectiveRightsFromAclW` whether
//! `ALL APPLICATION PACKAGES` could reach the jail root. That API returns
//! `ERROR_INVALID_ACL` (1336) on DACLs that are perfectly legal and entirely ordinary, so on
//! a machine carrying one the build jail could not confine ANY lifecycle script — it is on by
//! default, so that is a total failure, not a degradation. Measured on the Windows VM: 552
//! real directories under a single `%LOCALAPPDATA%\nub` returned 1336.
//!
//! THIS SUITE IS HERMETIC BY CONSTRUCTION. It does not depend on the host profile carrying
//! Store-app aces — a fresh CI runner image does not, which is why the corpus never caught
//! this — it BUILDS the hostile DACL itself and verifies the hostility before asserting
//! anything (`assert_api_chokes`). Without that positive control a green run here would be
//! indistinguishable from a run against a directory that was never hard.
//!
//! The two triggers, MEASURED 2026-08-06 by assembling acls in memory one ace at a time:
//!
//!   1. a DENY ace positioned AFTER an ALLOW ace — MSDN documents this as "fails if the acl
//!      contains an inherited access-denied ace", an inherited deny landing after the
//!      explicit allows being the common way to get there;
//!   2. explicit and INHERITED allow aces ALTERNATING past ~3 pairs: `EIEIEI` fails while
//!      `EIEIE` passes and `EEEIII` — THE SAME SIX ACES REGROUPED — passes.
//!
//! Refuted as triggers, each against a control that still passed: unresolvable AppContainer
//! package sids (well-known sids fail identically), GENERIC rights bits, `OI`/`CI`/`IO`
//! flags, acl revision, ace count alone (48 canonical aces pass), and the `\\?\` verbatim
//! path form. Trigger 1 is what this suite builds, because a test can set an ace ORDER
//! directly while the INHERITED flag is the OS's to assign.
//!
//! `harness = false`: a runner, matching its sibling Windows suites.

#[cfg(not(target_os = "windows"))]
fn main() {}

#[cfg(target_os = "windows")]
fn main() {
    match win::run() {
        Ok(()) => println!("ALL NON-CANONICAL DACL PROBES PASSED"),
        Err(n) => {
            eprintln!("{n} NON-CANONICAL DACL PROBE(S) FAILED");
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

    const EVERYONE: &str = "S-1-1-0";
    const GUESTS: &str = "S-1-5-32-546";
    const AAP: &str = "S-1-15-2-1";

    // ── building a hostile DACL ──────────────────────────────────────────────────────

    /// Write `entries` onto `dir` as a PROTECTED dacl, in exactly the order given.
    ///
    /// Protection is what makes the fixture deterministic: it severs inheritance, so the acl
    /// on disk is precisely the one written and the assertions do not depend on whatever the
    /// host profile propagates. Each entry is `(allow, mask, sid)`.
    fn set_dacl(dir: &Path, entries: &[(bool, u32, &str)]) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSidToSidW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            ACL, AddAccessAllowedAce, AddAccessDeniedAce, DACL_SECURITY_INFORMATION,
            InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
        };
        const ACL_REVISION: u32 = 2;
        let mut buf = vec![0u8; 8192];
        let acl = buf.as_mut_ptr().cast::<ACL>();
        // SAFETY: `buf` is an 8 KiB writable allocation, well above the acl this builds.
        unsafe {
            if InitializeAcl(acl, 8192, ACL_REVISION) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            for (allow, mask, sid_text) in entries {
                let wide = to_wide(sid_text);
                let mut sid: PSID = std::ptr::null_mut();
                if ConvertStringSidToSidW(wide.as_ptr(), &mut sid) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let ok = if *allow {
                    AddAccessAllowedAce(acl, ACL_REVISION, *mask, sid)
                } else {
                    AddAccessDeniedAce(acl, ACL_REVISION, *mask, sid)
                };
                LocalFree(sid.cast());
                if ok == 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let mut wpath = to_wide(&dir.to_string_lossy().replace('/', "\\"));
            let rc = SetNamedSecurityInfoW(
                wpath.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null_mut(),
            );
            if rc != 0 {
                return Err(std::io::Error::from_raw_os_error(rc as i32));
            }
        }
        Ok(())
    }

    /// What `GetEffectiveRightsFromAclW` returns for `ALL APPLICATION PACKAGES` on `dir`.
    ///
    /// Retained ONLY as this suite's positive control. It is the call the engine no longer
    /// makes, and the whole point is that its answer here is 1336 rather than a rights mask.
    fn legacy_effective_rights_rc(dir: &Path) -> u32 {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSidToSidW, GetEffectiveRightsFromAclW, GetNamedSecurityInfoW,
            NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
        };
        use windows_sys::Win32::Security::{
            ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        };
        let wide = to_wide(&dir.to_string_lossy().replace('/', "\\"));
        let sid_text = to_wide(AAP);
        // SAFETY: standard named-security read; both LocalAlloc'd out-pointers are freed.
        unsafe {
            let mut sid: PSID = std::ptr::null_mut();
            if ConvertStringSidToSidW(sid_text.as_ptr(), &mut sid) == 0 {
                return u32::MAX;
            }
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            );
            if rc != 0 {
                LocalFree(sid.cast());
                return u32::MAX;
            }
            let trustee = TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.cast(),
            };
            let mut rights = 0u32;
            let erc = GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights);
            LocalFree(sd);
            LocalFree(sid.cast());
            erc
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // ── the cases ────────────────────────────────────────────────────────────────────

    const FULL: u32 = 0x001F_01FF;
    const READ_EXEC: u32 = 0x0012_00A9;

    pub fn run() -> Result<(), u32> {
        let mut fails = 0u32;
        let base = std::env::temp_dir().join(format!("nub-noncanon-{:x}", nonce()));
        std::fs::create_dir_all(&base).expect("create the fixture base");

        // (1) THE REGRESSION. A dacl the legacy api cannot evaluate, granting AAP nothing.
        // The deny sits after the allow, which is legal and which Explorer can produce.
        let hostile = make_dir(&base, "hostile");
        set_dacl(
            &hostile,
            &[(true, FULL, EVERYONE), (false, FULL, GUESTS)],
        )
        .expect("write the non-canonical dacl");
        assert_api_chokes(&mut fails, &hostile);
        check(
            &mut fails,
            confines(&hostile),
            "a root the effective-rights api cannot evaluate is still confined",
        );

        // (2) THE SECURITY PROPERTY. Same shape, but AAP really is granted: must be REFUSED.
        let reachable = make_dir(&base, "aap-reachable");
        set_dacl(
            &reachable,
            &[
                (true, FULL, EVERYONE),
                (true, READ_EXEC, AAP),
                (false, FULL, GUESTS),
            ],
        )
        .expect("write the AAP-reachable dacl");
        check(
            &mut fails,
            !confines(&reachable),
            "a root ALL APPLICATION PACKAGES can reach is refused",
        );

        // (3) The same AAP grant WITHOUT the awkward ordering, so the refusal in (2) cannot be
        // an artefact of the unevaluable dacl rather than of the grant itself.
        let reachable_plain = make_dir(&base, "aap-reachable-plain");
        set_dacl(
            &reachable_plain,
            &[(true, FULL, EVERYONE), (true, READ_EXEC, AAP)],
        )
        .expect("write the plain AAP-reachable dacl");
        check(
            &mut fails,
            !confines(&reachable_plain),
            "an evaluable root granting AAP is refused too",
        );

        // (4) The clean control: an ordinary evaluable dacl with no AAP ace is confined.
        let clean = make_dir(&base, "clean");
        set_dacl(&clean, &[(true, FULL, EVERYONE)]).expect("write the clean dacl");
        check(
            &mut fails,
            legacy_effective_rights_rc(&clean) != 1336,
            "control: the clean fixture is evaluable, so (1) isolates the ordering",
        );
        check(
            &mut fails,
            confines(&clean),
            "a clean root is confined",
        );

        let _ = std::fs::remove_dir_all(&base);
        if fails == 0 { Ok(()) } else { Err(fails) }
    }

    /// The fixture must actually defeat the legacy api, or case (1) proves nothing.
    fn assert_api_chokes(fails: &mut u32, dir: &Path) {
        let rc = legacy_effective_rights_rc(dir);
        if rc == 1336 {
            println!("PASS positive control: GetEffectiveRightsFromAclW rejects the fixture (1336)");
        } else {
            *fails += 1;
            eprintln!(
                "FAIL positive control: the fixture is NOT hostile — \
                 GetEffectiveRightsFromAclW returned {rc}, expected 1336. \
                 Windows may have canonicalised the dacl on write; this suite is \
                 asserting nothing until that is fixed."
            );
        }
    }

    /// Whether `apply` produces a plan that still confines the filesystem — i.e. whether
    /// `verify_clean_root` accepted this working root.
    fn confines(root: &Path) -> bool {
        let policy = read_confine(root);
        let spec = CommandSpec::new("cmd.exe")
            .args(["/c", "exit", "0"])
            .cwd(root);
        match apply(&policy, spec) {
            Ok(p) => {
                let lost = p.degradation.lost.iter().any(|a| a == "fs-root");
                if lost {
                    eprintln!("  [{}] fs-root reported lost", root.display());
                }
                !lost
            }
            Err(d) => {
                eprintln!("  [{}] apply refused: {d:?}", root.display());
                false
            }
        }
    }

    fn make_dir(base: &Path, name: &str) -> PathBuf {
        let d = base.join(name);
        std::fs::create_dir_all(&d).expect("create a fixture directory");
        d
    }

    fn read_confine(root: &Path) -> SandboxPolicy {
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries: vec![FsRule {
                        matcher: CanonGlob(root.to_string_lossy().replace('\\', "/")),
                        effect: Effect::Allow,
                        access: FsAccess::ReadWrite,
                        origin: FsOrigin::Authored,
                    }],
                    default_effect: Effect::Deny,
                },
                tmp: TmpMode::Private,
            },
            net: NetPolicy::default(),
            env: EnvPolicy::resolved(essential_env()),
            pid: PidPolicy::default(),
            build_jail: false,
        }
    }

    fn essential_env() -> BTreeMap<String, String> {
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
            "NUMBER_OF_PROCESSORS",
            "OS",
            "PROCESSOR_ARCHITECTURE",
            "USERNAME",
            "COMPUTERNAME",
        ] {
            if let Ok(v) = std::env::var(k) {
                m.insert(k.to_string(), v);
            }
        }
        m
    }

    fn nonce() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn check(fails: &mut u32, ok: bool, label: &str) {
        if ok {
            println!("PASS {label}");
        } else {
            *fails += 1;
            eprintln!("FAIL {label}");
        }
    }
}
