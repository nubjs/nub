//! Windows clean-DACL working root — the ORDINARY-DIRECTORY regression guard.
//!
//! Every other Windows suite hands `apply` a fixture root it first secured itself with
//! `icacls /inheritance:r`, so all of them passed while `apply` was rejecting ordinary
//! directories in production: the precondition used to demand an `SE_DACL_PROTECTED`
//! ancestor, which only `%USERPROFILE%` paths have, so a project on a second volume or at
//! a volume root could never be confined. This file is the case the suite never had — it
//! hands `apply` directories exactly as the OS made them, on both volumes.
//!
//! It also prints the ancestor survey the predicate now rests on (every `ALL APPLICATION
//! PACKAGES` ace found on a stock machine is NON-INHERITABLE, so it cannot reach the tree
//! the child runs in) and the measurement that ruled out the alternative fix — protecting
//! the working root severs inheritance INTO it, which would strand the build jail.
//!
//! `harness = false`: this is a runner, not a libtest case, matching its sibling Windows
//! suites (they self-exec as the confined child; this one keeps the same shape so the
//! whole Windows set is invoked identically).

#[cfg(not(target_os = "windows"))]
fn main() {}

#[cfg(target_os = "windows")]
fn main() {
    match win::run() {
        Ok(()) => println!("ALL WINDOWS CLEAN-ROOT PROBES PASSED"),
        Err(n) => {
            eprintln!("{n} WINDOWS CLEAN-ROOT PROBE(S) FAILED");
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
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
        GetAce, GetSecurityDescriptorControl, INHERIT_ONLY_ACE, OBJECT_INHERIT_ACE,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    };

    const AAP_SID: &str = "S-1-15-2-1";

    // windows-sys does not export the ace-header type discriminants, so the engine declares
    // them locally too. Allow/deny and their `_CALLBACK_` variants are byte-identical up to
    // `SidStart`; the OBJECT forms interpose two GUIDs, so they are left undecoded.
    const ALLOWED_ACE: u8 = 0;
    const DENIED_ACE: u8 = 1;
    const ALLOWED_CALLBACK_ACE: u8 = 9;
    const DENIED_CALLBACK_ACE: u8 = 10;

    // ── read-only DACL inspection (the write side is exercised through `apply`) ──────

    /// What one path's DACL says about AppContainer reach. The engine's own
    /// `verify_clean_root` reads exactly these two facts; re-deriving them here keeps the
    /// probe independent of the code under test.
    struct DaclFacts {
        /// Effective `ALL APPLICATION PACKAGES` rights (0 = no AppContainer reach).
        aap_rights: u32,
        /// Whether any AAP ace on this path is inheritable — i.e. whether the grant
        /// reaches CHILDREN or stops at this directory. This is the fact that decides
        /// whether a volume root's AAP ace is a real hazard for the tree beneath it.
        aap_inheritable: bool,
        protected: bool,
    }

    fn dacl_facts(path: &Path) -> std::io::Result<DaclFacts> {
        let wide: Vec<u16> = path
            .to_string_lossy()
            .replace('/', "\\")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: standard named-security read; both out-pointers are freed below.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        let _sd = FreeGuard(sd);
        if dacl.is_null() {
            return Err(std::io::Error::other("null DACL"));
        }

        let sid_text: Vec<u16> = AAP_SID
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut aap: PSID = std::ptr::null_mut();
        // SAFETY: converts a literal SID string; freed by the guard.
        if unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut aap) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let _aap = FreeGuard(aap.cast());

        // Both facts come from ONE direct ace walk.
        //
        // `GetEffectiveRightsFromAclW` used to supply `aap_rights`, and it is the wrong
        // instrument twice over. It returns `ERROR_INVALID_ACL` on ordinary DACLs — which is
        // exactly why the engine stopped calling it (`windows_noncanonical_dacl.rs`) — and on a
        // real Windows Server 2022 host it does so for every freshly-created fixture dir here,
        // panicking the runner before any later case reports. It also EXPANDS GROUPS, answering a
        // broader question than the engine asks: a LowBox token reaches an object only where the
        // acl names an AppContainer sid, so group expansion can only add rights that mean nothing.
        // The walk stays an independent re-derivation — it is this file's own, not a call into the
        // code under test.
        //
        // SAFETY: AceCount bounds the GetAce index; each returned ace begins with an
        // ACE_HEADER, and an ALLOW/DENY ace's SidStart is the trustee SID.
        let mut aap_rights = 0u32;
        let mut denied = 0u32;
        let mut aap_inheritable = false;
        unsafe {
            for i in 0..(*dacl).AceCount as u32 {
                let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                if GetAce(dacl, i, &mut ace) == 0 {
                    continue;
                }
                let header = *ace.cast::<ACE_HEADER>();
                let sid: PSID = std::ptr::addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart)
                    .cast_mut()
                    .cast();
                if !sids_equal(sid, aap) {
                    continue;
                }
                let flags = u32::from(header.AceFlags);
                if flags & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) != 0 {
                    aap_inheritable = true;
                }
                // Rights ON THIS OBJECT, so an inherit-only ace contributes none — it grants
                // children, which `aap_inheritable` above is what reports.
                if flags & INHERIT_ONLY_ACE != 0 {
                    continue;
                }
                let mask = (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask;
                match header.AceType {
                    ALLOWED_ACE | ALLOWED_CALLBACK_ACE => aap_rights |= mask & !denied,
                    DENIED_ACE | DENIED_CALLBACK_ACE => denied |= mask & !aap_rights,
                    _ => {}
                }
            }
        }

        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: `sd` is a live self-relative descriptor until `_sd` drops.
        if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(DaclFacts {
            aap_rights,
            aap_inheritable,
            protected: control & SE_DACL_PROTECTED != 0,
        })
    }

    fn sids_equal(a: PSID, b: PSID) -> bool {
        use windows_sys::Win32::Security::GetLengthSid;
        // SAFETY: both are valid SIDs; GetLengthSid reports each one's exact length.
        unsafe {
            let (la, lb) = (GetLengthSid(a), GetLengthSid(b));
            la == lb
                && std::slice::from_raw_parts(a.cast::<u8>(), la as usize)
                    == std::slice::from_raw_parts(b.cast::<u8>(), lb as usize)
        }
    }

    struct FreeGuard(*mut std::ffi::c_void);
    impl Drop for FreeGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: every pointer wrapped here came from a LocalAlloc'ing Win32 call.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// The engine's clean-root predicate, re-derived independently so a broken
    /// `verify_clean_root` cannot make this probe agree with it: the WORKING ROOT must have
    /// no AAP reach at all; a STRICT ANCESTOR disqualifies only when its AAP ace is
    /// inheritable (a this-folder-only grant cannot reach the tree the child runs in); a
    /// protected ancestor is an early accept; running out of ancestors is an accept.
    ///
    /// It deliberately omits the published-subtree exemption, so it is only valid for a root
    /// OUTSIDE one — which every caller here is. `published_root_case` covers the exemption,
    /// and drives `apply` rather than this.
    fn qualifies(start: &Path) -> bool {
        for (i, p) in start.ancestors().enumerate() {
            let Ok(f) = dacl_facts(p) else { return false };
            if f.aap_rights != 0 && (i == 0 || f.aap_inheritable) {
                return false;
            }
            if f.protected {
                return true;
            }
        }
        true
    }

    // ── the survey that explains WHY the precondition was unsatisfiable ──────────────

    fn survey(label: &str, start: &Path) {
        println!("SURVEY {label}: {}", start.display());
        for p in start.ancestors() {
            match dacl_facts(p) {
                Ok(f) => println!(
                    "  {:<60} aap_rights=0x{:08x} aap_inheritable={} protected={}",
                    p.display(),
                    f.aap_rights,
                    f.aap_inheritable,
                    f.protected
                ),
                Err(e) => println!("  {:<60} <unreadable: {e}>", p.display()),
            }
        }
    }

    // ── policy + helpers ────────────────────────────────────────────────────────────

    fn canon(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    fn read_confine(read: &[&Path], write: &[&Path]) -> SandboxPolicy {
        let mut entries = Vec::new();
        for r in read {
            entries.push(rule(r, FsAccess::Read));
        }
        for w in write {
            entries.push(rule(w, FsAccess::ReadWrite));
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
            env: EnvPolicy::resolved(essential_env()),
            pid: PidPolicy::default(),
            // These probes drive the `nub sandbox` scope, not the dependency build jail.
            build_jail: false,
        }
    }

    fn rule(p: &Path, access: FsAccess) -> FsRule {
        rule_from(p, access, FsOrigin::Authored)
    }

    fn rule_from(p: &Path, access: FsAccess, origin: FsOrigin) -> FsRule {
        FsRule {
            matcher: CanonGlob(canon(p)),
            effect: Effect::Allow,
            access,
            origin,
        }
    }

    fn policy_of(entries: Vec<FsRule>) -> SandboxPolicy {
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries,
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

    /// An AppContainer's CreateProcessW resolves its per-container storage from the passed
    /// environment, so a too-minimal block fails with ERROR_ENVVAR_NOT_FOUND.
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

    /// Hand `apply` a directory exactly as the OS made it — no `icacls`, no pre-securing —
    /// and require the launch plan to build, the predicate to hold on disk, and the DACL to
    /// be exactly as it was: `apply` is a planner, so a plan that is never launched must
    /// leave no trace.
    fn ordinary_root_case(fails: &mut u32, label: &str, root: &Path) {
        survey(label, root);
        let before = dacl_facts(root).expect("read the untouched DACL");
        println!(
            "  BEFORE aap_rights=0x{:08x} protected={} qualifies={}",
            before.aap_rights,
            before.protected,
            qualifies(root)
        );

        let policy = read_confine(&[root], &[root]);
        let spec = CommandSpec::new("cmd.exe")
            .args(["/c", "exit", "0"])
            .cwd(root);
        match apply(&policy, spec) {
            Ok(_) => println!("PASS {label}: apply accepted an ordinary directory"),
            Err(d) => {
                *fails += 1;
                eprintln!("FAIL {label}: apply rejected an ordinary directory: {d:?}");
                return;
            }
        }

        let after = dacl_facts(root).expect("read the DACL after apply");
        println!(
            "  AFTER  aap_rights=0x{:08x} protected={}",
            after.aap_rights, after.protected
        );
        check(
            fails,
            after.aap_rights == 0,
            &format!("{label}: no AppContainer reach"),
        );
        check(
            fails,
            qualifies(root),
            &format!("{label}: the clean-root predicate holds"),
        );
        check(
            fails,
            after.protected == before.protected && after.aap_rights == before.aap_rights,
            &format!("{label}: apply left the DACL untouched"),
        );
        // The engine keeps writing to this tree after the jail exits, and so does the user.
        let probe = root.join("owner-access.txt");
        check(
            fails,
            std::fs::write(&probe, b"ok").is_ok() && std::fs::read(&probe).is_ok(),
            &format!("{label}: calling user keeps read+write on the root"),
        );
        // A file created under the root must not pick up an AppContainer grant.
        match dacl_facts(&probe) {
            Ok(f) => check(
                fails,
                f.aap_rights == 0,
                &format!("{label}: a file created under the root inherits no AAP"),
            ),
            Err(e) => {
                *fails += 1;
                eprintln!("FAIL {label}: could not inspect a child file: {e}");
            }
        }
    }

    /// A work root INSIDE a subtree nub has published to `ALL APPLICATION PACKAGES`.
    ///
    /// This is the shape a native addon that builds IN PLACE actually has: nub publishes the PM
    /// store once (the `FsOrigin::NubOwnedPublic` optimisation, which removes 76% of the fixed
    /// per-launch cost), that ace is inheritable, and the script's cwd canonicalizes to
    /// `store/<pkg>@<ver>-<hash>/node_modules/<pkg>` — inside it. Nub's own optimisation thereby
    /// made its own precondition unsatisfiable and `nub install` refused outright: measured on
    /// Windows Server 2022 as 6 of 86 corpus records, via `unix-dgram@2.0.7` and `ref@1.3.5`.
    ///
    /// The publish goes through `windows_publish_appcontainer_read` rather than `icacls` so the
    /// fixture pins the ace nub REALLY writes; an `icacls` stand-in would keep passing if that
    /// mask ever changed. The two negative controls are what make the accept meaningful — the
    /// exemption is keyed on the subtree being nub's own AND on the rights being no wider than
    /// nub publishes, so a foreign ace and a wider ace must both still refuse.
    fn published_root_case(fails: &mut u32) {
        let base = std::env::temp_dir();
        let store = base.join(format!("nub-store-{:x}", nonce()));
        let cell = store.join("unix-dgram@2.0.7-06ff5b6c30398b2d");
        let work = cell.join("node_modules").join("unix-dgram");
        let foreign = base.join(format!("nub-foreign-{:x}", nonce()));
        let foreign_work = foreign.join("pkg");
        for d in [&work, &foreign_work] {
            if std::fs::create_dir_all(d).is_err() {
                println!(
                    "SKIP published-root: cannot create a fixture under {}",
                    base.display()
                );
                return;
            }
        }

        // Publish BOTH roots identically. The only difference between them is the origin the
        // policy declares below, which is exactly the variable under test.
        for d in [&store, &foreign] {
            if let Err(e) = nub_sandbox::windows_publish_appcontainer_read(d) {
                println!(
                    "SKIP published-root: could not publish {}: {e}",
                    d.display()
                );
                let _ = std::fs::remove_dir_all(&store);
                let _ = std::fs::remove_dir_all(&foreign);
                return;
            }
        }
        let inherited = dacl_facts(&work).map(|f| f.aap_rights).unwrap_or(0);
        println!("PROBE published-root work_aap_rights=0x{inherited:08x}");
        // Without this the whole case is vacuous: the accept below would prove nothing if the
        // publish never reached the work root in the first place.
        check(
            fails,
            inherited != 0,
            "published-root: the publish reached the work root (the trap is real)",
        );

        let accepted = apply(
            &policy_of(vec![
                rule_from(&store, FsAccess::Read, FsOrigin::NubOwnedPublic),
                rule_from(&work, FsAccess::ReadWrite, FsOrigin::Authored),
            ]),
            CommandSpec::new("cmd.exe")
                .args(["/c", "exit", "0"])
                .cwd(&work),
        );
        match &accepted {
            Ok(_) => println!(
                "PASS published-root: apply accepted a work root inside nub's own published store"
            ),
            Err(d) => {
                *fails += 1;
                eprintln!("FAIL published-root: apply refused nub's own published store: {d:?}");
            }
        }

        // CONTROL 1 — same ace, same layout, but the subtree is NOT marked as nub's own. An
        // unexplained AAP ace still refuses, so the exemption cannot be read as "AAP under any
        // granted subtree is fine".
        let unmarked = apply(
            &policy_of(vec![
                rule_from(&foreign, FsAccess::Read, FsOrigin::Speculative),
                rule_from(&foreign_work, FsAccess::ReadWrite, FsOrigin::Authored),
            ]),
            CommandSpec::new("cmd.exe")
                .args(["/c", "exit", "0"])
                .cwd(&foreign_work),
        );
        check(
            fails,
            refused_for_aap(&unmarked),
            "published-root control: an AAP ace on a subtree nub does not publish still refuses",
        );

        // CONTROL 2 — nub's own published subtree, but carrying an ace WIDER than nub publishes.
        // Only the read-execute bits are excused; a write/full-control ace is somebody else's.
        let widened = std::process::Command::new("icacls")
            .arg(&work)
            .arg("/grant")
            .arg("*S-1-15-2-1:(OI)(CI)F")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if widened {
            let wide = apply(
                &policy_of(vec![
                    rule_from(&store, FsAccess::Read, FsOrigin::NubOwnedPublic),
                    rule_from(&work, FsAccess::ReadWrite, FsOrigin::Authored),
                ]),
                CommandSpec::new("cmd.exe")
                    .args(["/c", "exit", "0"])
                    .cwd(&work),
            );
            check(
                fails,
                refused_for_aap(&wide),
                "published-root control: an AAP ace wider than nub publishes still refuses",
            );
        } else {
            println!("SKIP published-root control: icacls could not widen the work root");
        }

        let _ = std::fs::remove_dir_all(&store);
        let _ = std::fs::remove_dir_all(&foreign);
    }

    /// Whether `apply` refused, and refused over AppContainer reach rather than for some
    /// unrelated reason that would make a negative control pass for free.
    fn refused_for_aap(r: &Result<nub_sandbox::Prepared, nub_sandbox::Degradation>) -> bool {
        match r {
            Ok(_) => false,
            Err(d) => d
                .reason
                .as_deref()
                .is_some_and(|s| s.contains("ALL APPLICATION PACKAGES")),
        }
    }

    /// Does protecting a directory sever a LATER inheritable grant placed on its parent?
    ///
    /// This is the measurement that RULED OUT the alternative fix. Rather than relax the
    /// predicate, `apply` could have CREATED the precondition by giving each working root a
    /// protected DACL — unprivileged, and it does close the AAP hole. But the build jail
    /// grants the dependency-tree READ on `<project>/node_modules` while each lifecycle
    /// script's cwd is `node_modules/<pkg>`. If protecting `<pkg>` blocks propagation from
    /// `node_modules`, then once package `a`'s script has run, every later package's script
    /// is granted `node_modules` and still cannot read `node_modules/a` — in that install
    /// and every install after it, because the protection is permanent.
    ///
    /// `b` is the control: same parent, never protected, so a "severed" verdict cannot be an
    /// artefact of the parent grant failing to apply at all. Reported, never asserted — it
    /// documents a rejected design, and the ACL model it measures is the platform's.
    fn inheritance_severing_case() {
        let base = std::env::temp_dir();
        let nm = base.join(format!("nub-nm-{:x}", nonce()));
        let (a, b) = (nm.join("a"), nm.join("b"));
        for d in [&a, &b] {
            if std::fs::create_dir_all(d).is_err() {
                println!(
                    "SKIP inheritance-severing: cannot create a fixture under {}",
                    nm.display()
                );
                return;
            }
        }

        // Model the rejected design directly: protect `a` exactly as `secure_clean_root`
        // would have. Driving it through `apply` is not an option — the shipped predicate
        // writes nothing, which is the whole point.
        let user = std::env::var("USERNAME").unwrap_or_default();
        let protected = std::process::Command::new("icacls")
            .arg(&a)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:(OI)(CI)F"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !protected {
            println!("SKIP inheritance-severing: icacls could not protect the child");
            let _ = std::fs::remove_dir_all(&nm);
            return;
        }

        // Now place an inheritable AAP grant on the PARENT, exactly as a later launch would
        // place its inheritable read grant on `node_modules`. AAP is the trustee because the
        // probe can already measure its effective rights on any path.
        let granted = std::process::Command::new("icacls")
            .arg(&nm)
            .arg("/grant")
            .arg("*S-1-15-2-1:(OI)(CI)RX")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !granted {
            println!("SKIP inheritance-severing: icacls could not grant on the parent");
            let _ = std::fs::remove_dir_all(&nm);
            return;
        }

        let a_rights = dacl_facts(&a).map(|f| f.aap_rights).unwrap_or(0);
        let b_rights = dacl_facts(&b).map(|f| f.aap_rights).unwrap_or(0);
        println!(
            "PROBE inheritance-severing protected_child_inherits={} control_sibling_inherits={} \
             (a=0x{a_rights:08x} b=0x{b_rights:08x})",
            a_rights != 0,
            b_rights != 0
        );
        match (a_rights != 0, b_rights != 0) {
            (false, true) => println!(
                "VERDICT inheritance-severing CONFIRMED: a protected directory does NOT \
                 receive a later inheritable grant on its parent, so protecting each working \
                 root would strand every previously-confined package dir"
            ),
            (true, true) => println!(
                "VERDICT inheritance-severing REFUTED: the protected child still received the \
                 parent's inheritable grant"
            ),
            (_, false) => println!(
                "VERDICT inheritance-severing INCONCLUSIVE: the control did not inherit \
                 either, so the parent grant never propagated at all"
            ),
        }
        let _ = std::fs::remove_dir_all(&nm);
    }

    pub fn run() -> Result<(), u32> {
        let mut fails = 0u32;
        println!(
            "HOST cwd={} temp={}",
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            std::env::temp_dir().display()
        );

        // The stock-machine survey, for the record: every volume root, the profile, the
        // temp tree, and the checkout the CI job runs in.
        for (label, path) in [
            ("system-volume-root", PathBuf::from("C:\\")),
            ("temp", std::env::temp_dir()),
            (
                "userprofile",
                PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into())),
            ),
            (
                "cwd",
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\")),
            ),
        ] {
            survey(label, &path);
        }

        // Where a case's own fixture cannot be created the host, not the fix, is the reason
        // — this runs as a standard user too, who can write neither `C:\` nor an arbitrary
        // work volume. Skip rather than panic, so the cases that CAN run still report.
        let mut case = |label: &str, base: PathBuf| {
            let root = base.join(format!("nub-cleanroot-{:x}", nonce()));
            match std::fs::create_dir_all(&root) {
                Ok(()) => {
                    ordinary_root_case(&mut fails, label, &root);
                    let _ = std::fs::remove_dir_all(&root);
                }
                Err(e) => println!(
                    "SKIP {label}: cannot create a fixture under {}: {e}",
                    base.display()
                ),
            }
        };

        // The build jail's private-tmp shape.
        case("temp-root", std::env::temp_dir());
        // The CI work volume — the exact shape that produced
        // `\\?\D:\ grants ALL APPLICATION PACKAGES access` on a windows-latest runner.
        case(
            "workdir-root",
            std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
        );
        // The system volume, which the earlier investigation also measured failing
        // (`C:\nubfx`) — so the fix is shown to be volume-independent, not a D:-only patch.
        case("system-volume-root-dir", PathBuf::from("C:\\"));

        published_root_case(&mut fails);
        inheritance_severing_case();

        if fails == 0 { Ok(()) } else { Err(fails) }
    }
}
