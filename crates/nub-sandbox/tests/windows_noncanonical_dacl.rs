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
    /// host profile propagates. Each entry is `(allow, ace_flags, mask, sid)`.
    ///
    /// The `…AceEx` variants are load-bearing rather than incidental: the plain
    /// `AddAccessAllowedAce` writes `AceFlags = 0`, so no fixture could ever be INHERITABLE
    /// and `has_inheritable_grant` — the only branch that disqualifies on inheritance — had
    /// no coverage at all.
    fn set_dacl(dir: &Path, entries: &[(bool, u32, u32, &str)]) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSidToSidW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            ACL, AddAccessAllowedAceEx, AddAccessDeniedAceEx, DACL_SECURITY_INFORMATION,
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
            for (allow, flags, mask, sid_text) in entries {
                let wide = to_wide(sid_text);
                let mut sid: PSID = std::ptr::null_mut();
                if ConvertStringSidToSidW(wide.as_ptr(), &mut sid) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let ok = if *allow {
                    AddAccessAllowedAceEx(acl, ACL_REVISION, *flags, *mask, sid)
                } else {
                    AddAccessDeniedAceEx(acl, ACL_REVISION, *flags, *mask, sid)
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
    /// ACE_FLAGS. `NONE` applies to this object only; `OICI` also propagates to children;
    /// `OICI_IO` propagates but grants NOTHING on the object itself.
    const NONE: u32 = 0x0;
    const OICI: u32 = 0x3;
    const OICI_IO: u32 = 0xB;

    pub fn run() -> Result<(), u32> {
        let mut fails = 0u32;
        let base = std::env::temp_dir().join(format!("nub-noncanon-{:x}", nonce()));
        std::fs::create_dir_all(&base).expect("create the fixture base");

        // (1) THE REGRESSION. A dacl the legacy api cannot evaluate, granting AAP nothing.
        // The deny sits after the allow, which is legal and which Explorer can produce.
        let hostile = make_dir(&base, "hostile");
        set_dacl(
            &hostile,
            &[(true, NONE, FULL, EVERYONE), (false, NONE, FULL, GUESTS)],
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
                (true, NONE, FULL, EVERYONE),
                (true, NONE, READ_EXEC, AAP),
                (false, NONE, FULL, GUESTS),
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
            &[(true, NONE, FULL, EVERYONE), (true, NONE, READ_EXEC, AAP)],
        )
        .expect("write the plain AAP-reachable dacl");
        check(
            &mut fails,
            !confines(&reachable_plain),
            "an evaluable root granting AAP is refused too",
        );

        // (3b) INHERIT-ONLY. The ace grants AAP nothing on the directory object, so the
        // object-rights arm reads zero and ONLY `has_inheritable_grant` can catch it. This is
        // the branch the old effective-rights check had no equivalent of, and the reason the
        // cwd test is now stricter: the confined script CREATES files here, and every one of
        // them would inherit the grant.
        let inherit_only = make_dir(&base, "aap-inherit-only");
        set_dacl(
            &inherit_only,
            &[
                (true, NONE, FULL, EVERYONE),
                (true, OICI_IO, READ_EXEC, AAP),
            ],
        )
        .expect("write the inherit-only AAP dacl");
        check(
            &mut fails,
            !confines(&inherit_only),
            "a root whose AAP grant is INHERIT-ONLY is refused (children would inherit it)",
        );

        // (3c) An inheritable AAP grant that DOES also apply to the object.
        let inheritable = make_dir(&base, "aap-inheritable");
        set_dacl(
            &inheritable,
            &[(true, NONE, FULL, EVERYONE), (true, OICI, READ_EXEC, AAP)],
        )
        .expect("write the inheritable AAP dacl");
        check(
            &mut fails,
            !confines(&inheritable),
            "a root with an inheritable AAP grant is refused",
        );

        // (3d) DENY BEFORE ALLOW. Canonical evaluation lets the deny win, so AAP ends up with
        // nothing and the root is genuinely safe. Accepting it is correct, and it proves the
        // walk honours deny aces rather than being allow-only.
        let denied_first = make_dir(&base, "aap-denied-first");
        set_dacl(
            &denied_first,
            &[
                (true, NONE, FULL, EVERYONE),
                (false, NONE, FULL, AAP),
                (true, NONE, READ_EXEC, AAP),
            ],
        )
        .expect("write the deny-before-allow dacl");
        check(
            &mut fails,
            confines(&denied_first),
            "an AAP allow fully neutralised by a preceding deny does not disqualify the root",
        );

        // (3e) The mirror image: the allow comes FIRST, so those bits are granted and a later
        // deny cannot take them back. Must be refused. Without this, (3d) alone would also be
        // satisfied by an implementation that lets any deny mask an allow.
        let allowed_first = make_dir(&base, "aap-allowed-first");
        set_dacl(
            &allowed_first,
            &[
                (true, NONE, FULL, EVERYONE),
                (true, NONE, READ_EXEC, AAP),
                (false, NONE, FULL, AAP),
            ],
        )
        .expect("write the allow-before-deny dacl");
        check(
            &mut fails,
            !confines(&allowed_first),
            "an AAP allow preceding a deny still disqualifies the root",
        );

        // (4) The clean control: an ordinary evaluable dacl with no AAP ace is confined.
        let clean = make_dir(&base, "clean");
        set_dacl(&clean, &[(true, NONE, FULL, EVERYONE)]).expect("write the clean dacl");
        check(
            &mut fails,
            legacy_effective_rights_rc(&clean) != 1336,
            "control: the clean fixture is evaluable, so (1) isolates the ordering",
        );
        check(&mut fails, confines(&clean), "a clean root is confined");

        // (5) THE REAL HOST. The synthetic cases above prove the mechanism; this proves the
        // mechanism is the one that bites on a developer's actual machine. Gated on the host
        // genuinely exhibiting the condition — a fresh CI runner image does not carry the
        // Store-app aces that produce it, and asserting there would fail for the wrong
        // reason. When the gate does not fire the case reports and skips; it never passes
        // vacuously, because the gate IS the legacy api returning 1336.
        host_survey(&mut fails);

        // (6) THE PREMISE THE WHOLE FIX RESTS ON. The walk counts only aces whose trustee is
        // ALL APPLICATION PACKAGES; the replaced api additionally EXPANDED GROUPS, and would
        // report an `Everyone` ace as AAP rights (measured: an acl whose only ace is
        // `allow Everyone 0x1200A9` reports 0x1200A9 for trustee S-1-15-2-1). Dropping that
        // expansion is only sound if an `Everyone` grant really does confer nothing on a
        // LowBox token. If it ever did, case (4) above would be accepting a root the child
        // can read and this suite would be certifying a hole. So prove it directly rather
        // than citing the AppContainer model: put a secret behind an `Everyone`-only dacl
        // OUTSIDE the allow-set and require the confined child to fail to read it.
        appcontainer_ignores_everyone(&mut fails, &base);

        let _ = std::fs::remove_dir_all(&base);
        if fails == 0 { Ok(()) } else { Err(fails) }
    }

    /// Find a real directory under the user profile that the legacy api cannot evaluate, and
    /// require the engine to confine it anyway.
    fn host_survey(fails: &mut u32) {
        let Ok(local) = std::env::var("LOCALAPPDATA") else {
            println!("SKIP host survey: no LOCALAPPDATA");
            return;
        };
        // BREADTH-first, and seeded at nub's own tree first. A depth-first walk of the whole
        // of %LOCALAPPDATA% wanders into an unrelated sibling and exhausts its budget without
        // ever reaching `nub\` — it reported "none found" on a host carrying 552 of them.
        // The jail home and the package store both live under `nub\`, so that is where the
        // condition actually bites.
        let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
        let nub_dir = PathBuf::from(&local).join("nub");
        if nub_dir.is_dir() {
            queue.push_back(nub_dir);
        }
        queue.push_back(PathBuf::from(&local));

        let mut hostile: Option<PathBuf> = None;
        let mut scanned = 0usize;
        while let Some(dir) = queue.pop_front() {
            if scanned > 4000 || hostile.is_some() {
                break;
            }
            scanned += 1;
            if legacy_effective_rights_rc(&dir) == 1336 {
                hostile = Some(dir);
                break;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                if e.path().is_dir() {
                    queue.push_back(e.path());
                }
            }
        }
        match hostile {
            None => println!(
                "SKIP host survey: scanned {scanned} directories under {local}, none defeat \
                 the legacy api. This host does not carry the condition, which is expected \
                 on a fresh CI runner image."
            ),
            Some(dir) => {
                println!(
                    "host survey: {} defeats GetEffectiveRightsFromAclW (1336)",
                    dir.display()
                );
                check(
                    fails,
                    confines(&dir),
                    "a REAL host directory the legacy api cannot evaluate is still confined",
                );
                // Planning is not running. Launch a script there and require it to EXECUTE,
                // because "apply returned Ok" would still be true of a plan that cannot start.
                check(
                    fails,
                    runs_confined_in(&dir),
                    "a script actually RUNS confined in that real host directory",
                );
            }
        }
    }

    /// Launch a real child in `root` under the jail and confirm it executed there — proved by
    /// the marker it writes into its own working directory, not merely by an exit code.
    fn runs_confined_in(root: &Path) -> bool {
        let marker = root.join(format!("nub-noncanon-ran-{:x}.txt", nonce()));
        let _ = std::fs::remove_file(&marker);
        let name = marker.file_name().expect("marker has a file name");
        let policy = read_confine(root);
        let spec = CommandSpec::new("cmd.exe")
            .args(["/c", &format!("echo ran> {}", name.to_string_lossy())])
            .cwd(root);
        let prepared = match apply(&policy, spec) {
            Ok(p) => p,
            Err(d) => {
                eprintln!("  [run] apply refused: {d:?}");
                return false;
            }
        };
        let status = match prepared.status() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [run] launch failed: {e} os={:?}", e.raw_os_error());
                return false;
            }
        };
        let wrote = marker.is_file();
        println!("  [run] exit={:?} marker_written={wrote}", status.code());
        let _ = std::fs::remove_file(&marker);
        status.success() && wrote
    }

    /// An `Everyone` grant must confer nothing on the confined child.
    ///
    /// The control matters as much as the assertion: the SAME child, in the SAME launch,
    /// reads a file it IS granted. Without it a "could not read" result is equally consistent
    /// with a child that never started, and the case would pass while proving nothing.
    fn appcontainer_ignores_everyone(fails: &mut u32, base: &Path) {
        const SECRET: &str = "everyone-readable-secret";
        let root = make_dir(base, "premise-root");
        let outside = make_dir(base, "premise-outside");
        let secret = outside.join("secret.txt");
        if std::fs::write(&secret, SECRET).is_err() {
            *fails += 1;
            eprintln!("FAIL premise: could not write the fixture secret");
            return;
        }
        // Readable by literally everyone, and named in NO policy rule.
        if let Err(e) = set_dacl(&outside, &[(true, NONE, FULL, EVERYONE)]) {
            *fails += 1;
            eprintln!("FAIL premise: could not write the Everyone dacl: {e}");
            return;
        }
        // The positive control: same shape, but inside the granted root.
        let granted = root.join("granted.txt");
        let _ = std::fs::write(&granted, SECRET);

        let out = root.join("premise-out.txt");
        let policy = read_confine(&root);
        let spec = CommandSpec::new("cmd.exe")
            // UNQUOTED deliberately. This spec's argv is re-encoded on the way to
            // `cmd.exe`, which escapes an embedded `"` as `\"` and hands cmd a path it
            // rejects with "the filename, directory name, or volume label syntax is
            // incorrect" — both reads then fail and the case looks like a clean no-leak.
            // The fixture paths are nonce-named under the temp dir and contain no spaces.
            .args([
                "/c",
                &format!(
                    "type {} > premise-out.txt 2>&1 & type {} >> premise-out.txt 2>&1",
                    secret.display(),
                    granted.display()
                ),
            ])
            .cwd(&root);
        let prepared = match apply(&policy, spec) {
            Ok(p) => p,
            Err(d) => {
                *fails += 1;
                eprintln!("FAIL premise: apply refused the root: {d:?}");
                return;
            }
        };
        if let Err(e) = prepared.status() {
            *fails += 1;
            eprintln!("FAIL premise: launch failed: {e}");
            return;
        }
        let text = std::fs::read_to_string(&out).unwrap_or_default();
        let leaked = text.contains(SECRET);
        // The granted read proves the child ran and could read SOMETHING, so a clean "no
        // leak" cannot be a child that silently did nothing. Both files hold the same bytes,
        // so distinguish by COUNTING occurrences: exactly one means outside failed, inside
        // succeeded.
        let hits = text.matches(SECRET).count();
        println!("  [premise] occurrences={hits} raw={:?}", text.trim());
        check(
            fails,
            hits == 1,
            "an Everyone-only path outside the allow-set is NOT readable by the confined \
             child, while a granted path in the same launch IS",
        );
        if leaked && hits > 1 {
            eprintln!(
                "  [premise] an Everyone grant DOES reach a LowBox token on this build. \
                 The ace walk's AAP-only scope would then be unsound and the effective-rights \
                 group expansion was load-bearing after all."
            );
        }
    }

    /// The fixture must actually defeat the legacy api, or case (1) proves nothing.
    fn assert_api_chokes(fails: &mut u32, dir: &Path) {
        let rc = legacy_effective_rights_rc(dir);
        if rc == 1336 {
            println!(
                "PASS positive control: GetEffectiveRightsFromAclW rejects the fixture (1336)"
            );
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
