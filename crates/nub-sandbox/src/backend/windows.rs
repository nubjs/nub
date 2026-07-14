//! Windows backend: launch the child into an AppContainer (LowBox token) via a
//! custom `CreateProcessW` + `STARTUPINFOEX`/`SECURITY_CAPABILITIES`, confined by
//! the ALLOWLIST / default-deny model. CI-validated design (probe run 28276213658,
//! `tests/sandbox-win-probes/`); see design.md §2.4 and .fray/sandbox.md.
//!
//! THE ALLOWLIST MODEL (why NOT a deny-ACE denylist): a LowBox token can reach an
//! object ONLY where the object's ACL grants its AppContainer SID, a capability SID,
//! or `ALL APPLICATION PACKAGES`. Everything else is denied BY DEFAULT. So read-
//! confine = grant the AppContainer SID read-execute on ONLY the allowed dirs; every
//! other path fails closed with no per-file deny-ACE. The deny-ACE denylist is
//! ABANDONED — it is defeated whenever a secret sits under a dir carrying an
//! inherited `ALL APPLICATION PACKAGES` read grant (the AAP grant satisfies the
//! lowbox check before the file deny is reached). We grant a UNIQUE per-run
//! AppContainer SID and never grant AAP, so no inherited AAP can widen the allow-set.
//!
//! AXES:
//!   - fs read-confine: inheritable allow-ACE (AC SID, read+execute) on each allowed
//!     read subtree. Only the *default-deny* (read-confine) posture is expressible;
//!     a generous-read (`default_effect == Allow`) policy degrades — the allowlist
//!     cannot say "read everything except secrets" (see [`derive_grants`]).
//!   - fs write-confine: inheritable allow-ACE (AC SID, modify) on each write subtree.
//!   - env-scrub: the child env IS the policy's constructed map (`lpEnvironment`),
//!     built by construction exactly as the mac/linux backends do.
//!   - coarse egress: no `internetClient` capability ⇒ ALL egress (incl. loopback)
//!     is blocked; the capability is granted only when net is unconfined. Per-host is
//!     the egress proxy's job (S6) — reported degraded until then.
//!   - process-reap: a Job Object with `KILL_ON_JOB_CLOSE`; the whole tree dies when
//!     the job handle closes (after the child exits, or if nub does).
//!
//! ASCENDANT-ENV READ IS OS-CLOSED (design.md §2.4): a LowBox child CANNOT
//! `OpenProcess(PROCESS_VM_READ)` the parent to read nub's environ — the AppContainer
//! access check needs the target's DACL to grant the child's package SID / a capability /
//! `ALL APPLICATION PACKAGES`, which a normal parent process does not, so the open is
//! DENIED (`ERROR_ACCESS_DENIED`), integrity-level-independent. CI-proven on windows-latest
//! (run 29043151805) with the parent BOTH elevated AND de-elevated; an unconfined control
//! recovers the secret (negative control). So no dedicated-account backend is needed for
//! this axis. (Bound: the VM_READ-inclusive open is proven denied; a QUERY_LIMITED-only
//! handle wasn't separately probed but cannot read the env block.) [`apply`] therefore
//! emits NO `env-read-ascendant` `Degradation`.
//!
//! THE LAUNCH SEAM: unlike mac/linux, this backend cannot hand the caller a pre-built
//! `std::process::Command` — the AppContainer launch needs a custom CreateProcess, a
//! Job assigned at creation, and per-run ACL grants TORN DOWN after the child exits.
//! So [`apply`] returns a [`WindowsLaunch`] plan on [`Prepared::launch`], and
//! `Prepared::status()` calls [`WindowsLaunch::run`], which owns setup → spawn → wait
//! → RAII teardown.

use crate::policy::{Effect, FsAccess, FsPolicy, FsRule, NetPolicy};
// Referenced only by the Windows-gated `apply`; the host build (module-under-test)
// never names it.
#[cfg(target_os = "windows")]
use crate::policy::SandboxPolicy;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A resolved AppContainer launch plan. All fields are OS-agnostic plain data so the
/// IR→plan derivation is unit-tested on the dev host; [`WindowsLaunch::run`] (the FFI)
/// is `#[cfg(windows)]`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct WindowsLaunch {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    /// Subtrees the AppContainer SID is granted inheritable read-execute.
    read_grants: Vec<PathBuf>,
    /// Subtrees the AppContainer SID is granted inheritable modify (read+write).
    write_grants: Vec<PathBuf>,
    /// `Some` ⇒ enforce env by construction (the child env IS this map). `None` ⇒
    /// inherit the ambient env untouched.
    env: Option<BTreeMap<String, String>>,
    /// Grant the `internetClient` capability (egress allowed). `false` ⇒ coarse deny.
    allow_internet: bool,
    /// Strict-Windows Tier 1: register a machine-wide loopback exemption for the per-run
    /// AC SID before spawn (so the child can reach nub's loopback egress proxy — its SOLE
    /// egress, since `allow_internet` stays `false`), torn down when the child exits.
    /// Requires elevation; `apply` sets it only when [`plan_net`] chose [`WinNetPlan::Tier1`].
    register_loopback_exemption: bool,
}

/// What the allowlist model could NOT express for a policy, so the caller can be told.
#[derive(Debug, Default, PartialEq)]
struct FsDegrade {
    /// A generous-read base (`default_effect == Allow`, OR a whole-fs `**` Allow entry
    /// — the shape the compiler actually emits for `"..."`/`sandbox: true`). The
    /// allowlist can't express read-all-minus-secrets; reads are confined to the
    /// explicit allow-set instead.
    generous_read: bool,
    /// An embedded-glob read allow — can't be a single inheritable ACE; skipped
    /// (fail-safe over-confinement rather than widening a grant to its literal prefix,
    /// which could expose a sibling secret).
    glob_read_unenforced: bool,
}

/// Derive the AppContainer read/write grants from the fs IR. Only LITERAL subtrees can
/// be expressed as an inheritable ACE; the read-confine (`default_effect == Deny`)
/// posture maps faithfully, while a generous-read base or an embedded-glob allow can't
/// and is reported via [`FsDegrade`] (fail-safe: over-confine + name it, never widen).
/// (The deny-shadowing check is done by [`deny_shadows_grant`] in `apply`, AFTER the
/// program-dir grant is folded into the read set.)
fn derive_grants(fs: &FsPolicy) -> (Vec<PathBuf>, Vec<PathBuf>, FsDegrade) {
    let mut read = Vec::new();
    let mut write = Vec::new();
    let mut degrade = FsDegrade {
        generous_read: fs.rules.default_effect == Effect::Allow,
        ..Default::default()
    };

    for rule in &fs.rules.entries {
        // Denies are implicit in the allowlist (ungranted = denied); their one hole (a
        // deny inside a granted subtree) is checked in `apply` post-program-dir.
        if rule.effect == Effect::Deny {
            continue;
        }
        match literal_subtree(rule.matcher.as_str()) {
            Some(dir) => {
                if !read.contains(&dir) {
                    read.push(dir.clone());
                }
                if rule.access == FsAccess::ReadWrite
                    && !is_dangerous_write_root(&dir)
                    && !write.contains(&dir)
                {
                    write.push(dir);
                }
            }
            // A whole-fs `**` Allow is the generous-read base (what the compiler emits
            // for `"..."`/`sandbox: true` alongside a Deny base) — the allowlist can't
            // express it, so degrade and confine to the explicit allow-set. A NON-whole-
            // fs embedded glob is a distinct over-confinement (skipped, not widened).
            None if is_whole_fs(rule.matcher.as_str()) => degrade.generous_read = true,
            None if has_glob_meta(rule.matcher.as_str()) => degrade.glob_read_unenforced = true,
            None => {}
        }
    }
    (read, write, degrade)
}

/// Whether any read DENY could match a path inside a granted read subtree — an
/// inheritable read-allow on the grant DEFEATS such a deny on Windows (the same class
/// of trap the AAP denylist hits), so it cannot be carved and must be reported. The
/// rule is sound and conservative: a depth-independent glob deny (`**/.env`) shadows
/// EVERY grant, and a deny whose literal prefix is inside a grant (or vice-versa)
/// shadows it. Matching is case-insensitive (Windows paths are). Run against the
/// policy-derived SUBTREE grants only — the caller excludes the program-file grant (a
/// single leaf with no subtree, an exec necessity), which cannot host a deny "inside" it.
fn deny_shadows_grant(entries: &[FsRule], read_grants: &[PathBuf]) -> bool {
    if read_grants.is_empty() {
        return false;
    }
    for rule in entries {
        if rule.effect != Effect::Deny {
            continue;
        }
        let g = rule.matcher.as_str();
        // A depth-independent glob deny (no literal prefix before the first `**`, e.g.
        // `**/.env`) can match inside any granted subtree.
        let prefix = literal_prefix(g);
        if prefix.is_empty() {
            return true;
        }
        let dp = PathBuf::from(prefix);
        if read_grants
            .iter()
            .any(|grant| path_prefixes(grant, &dp) || path_prefixes(&dp, grant))
        {
            return true;
        }
    }
    false
}

/// The literal directory prefix of a glob — the leading run of full, glob-free path
/// components (e.g. `C:/proj/*.pem` → `C:/proj`, `**/.env` → ``, `C:/x` → `C:/x`).
fn literal_prefix(glob: &str) -> String {
    if !has_glob_meta(glob) {
        return glob.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    for comp in glob.split('/') {
        if has_glob_meta(comp) {
            break;
        }
        kept.push(comp);
    }
    kept.join("/")
}

/// Whether `a` is a path-prefix of `b` (component-wise, case-insensitive).
fn path_prefixes(a: &Path, b: &Path) -> bool {
    let mut bc = b.components();
    for ac in a.components() {
        match bc.next() {
            Some(bcomp) => {
                if !ac.as_os_str().eq_ignore_ascii_case(bcomp.as_os_str()) {
                    return false;
                }
            }
            None => return false,
        }
    }
    true
}

/// Whether a canonical IR glob contains glob metacharacters.
fn has_glob_meta(glob: &str) -> bool {
    glob.contains(['*', '?', '[', ']', '{', '}'])
}

/// Whether a glob addresses the whole filesystem (the generous-read base spellings).
fn is_whole_fs(glob: &str) -> bool {
    matches!(glob, "**" | "/**" | "/")
}

/// The literal directory subtree a matcher grants, or `None` if it can't be expressed
/// as one inheritable ACE. A plain absolute literal, or a literal + trailing `/**`
/// subtree twin, yields that directory; anything with embedded globs (or the whole-fs
/// spellings) yields `None`. Mirrors the macOS backend's `to_match_term` subpath case.
fn literal_subtree(glob: &str) -> Option<PathBuf> {
    if is_whole_fs(glob) {
        return None;
    }
    if !has_glob_meta(glob) {
        // A canonical IR path is absolute + forward-slashed; accept a Windows drive
        // path (`C:/…`) or a UNC/rooted path.
        return Some(PathBuf::from(glob));
    }
    if let Some(prefix) = glob.strip_suffix("/**")
        && !has_glob_meta(prefix)
    {
        return Some(PathBuf::from(prefix));
    }
    None
}

/// Top-level roots a WRITE grant must never cover — a `..`-collapsed surface path can
/// resolve to a system root, and an inheritable modify ACE there would be a
/// filesystem-wide write hole. The Windows twin of the macOS `is_dangerous_write_root`
/// (reads are exempt; a generous read is a legitimate posture, and read is separately
/// allowlist-confined here anyway). Matches on the forward-slashed canonical form.
fn is_dangerous_write_root(dir: &Path) -> bool {
    let Some(s) = dir.to_str() else { return false };
    let s = s.trim_end_matches('/');
    // Drive root (`C:`), the Windows dir, and Program Files are the roots a stray `..`
    // could land on. Case-insensitive: Windows paths are case-insensitive.
    let low = s.to_ascii_lowercase();
    if low.is_empty() || low == "/" {
        return true;
    }
    // `C:` / `C:/` — a bare drive root (2 chars + optional slash).
    let bytes = low.as_bytes();
    if bytes.len() <= 3 && bytes.get(1) == Some(&b':') {
        return true;
    }
    matches!(
        low.as_str(),
        "c:/windows"
            | "c:/windows/system32"
            | "c:/program files"
            | "c:/program files (x86)"
            | "c:/programdata"
            | "c:/users"
    )
}

/// Whether the fs axis confines anything (mirrors the mac/linux `fs_confines`). A
/// relaxed axis (`default_effect == Allow` with no entries) is not a confinement.
fn fs_confines(fs: &FsPolicy) -> bool {
    fs.rules.default_effect != Effect::Allow || !fs.rules.entries.is_empty()
}

/// The Windows net posture the backend can achieve for a policy, given whether nub runs
/// elevated. THE WINDOWS DIFFERENCE (design.md; `wiki/research/sandbox-windows-net-parity.md`):
/// per-host + MITM ride nub's loopback egress proxy, but an AppContainer child is blocked
/// from ALL loopback by WFP regardless of capability, and the only lift —
/// `NetworkIsolationSetAppContainerConfig` — is admin-only. So the per-host/MITM tier is
/// reachable ONLY when elevated; coarse on/off needs no proxy and stays unprivileged. Pure
/// fn ⇒ host-unit-tested (the `is_elevated` FFI is factored out into the caller).
#[derive(Debug, PartialEq, Eq)]
enum WinNetPlan {
    /// Net unconfined — grant `internetClient`, no proxy.
    Unconfined,
    /// Coarse egress-deny — withhold `internetClient`, no proxy (deny-all; unprivileged).
    CoarseDeny,
    /// Strict-Windows Tier 1 — register the per-run AC-SID loopback exemption so the child
    /// reaches nub's proxy (its SOLE egress, `internetClient` withheld). Per-host + MITM
    /// enforce. Requires elevation.
    Tier1,
    /// Fail-CLOSED: the policy needs per-host/MITM but nub is not elevated, so the loopback
    /// exemption can't be registered. The maintainer requirement — surface a clear error,
    /// NEVER silently coarse-degrade an allow-list into a deny-all.
    FailUnelevated,
}

/// Decide the net posture. Per-host is signalled by any Allow rule (matches
/// `backend::start_proxy_if_needed`, which is what actually starts the proxy). A pure
/// deny-all is coarse (no proxy, no elevation). `elevated` is consulted only on the
/// per-host branch, so the caller may pass `false` elsewhere without changing the verdict.
fn plan_net(net: &NetPolicy, elevated: bool) -> WinNetPlan {
    if !net.enforce {
        return WinNetPlan::Unconfined;
    }
    let needs_proxy = net.rules.iter().any(|r| r.effect == Effect::Allow);
    if !needs_proxy {
        return WinNetPlan::CoarseDeny;
    }
    if elevated {
        WinNetPlan::Tier1
    } else {
        WinNetPlan::FailUnelevated
    }
}

// TRAVERSE MODEL (why a LEAF grant alone suffices — no ancestor traverse grants): a
// LowBox token retains SeChangeNotifyPrivilege (Bypass Traverse Checking), and standard
// local NTFS volumes carry FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL on the VOLUME DEVICE
// object, so intermediate-directory ACLs are NOT access-checked during path resolution
// on C: — only the final leaf object's ACL is. Granting the AC SID read/modify on the
// allowed leaves is therefore sufficient regardless of where they live (an ordinary
// `%TEMP%`/profile/project dir); nub never needs WRITE_DAC on a shared ancestor like
// `C:\Users`, and confined work dirs need NOT live under a nub-owned store at `C:\`.
// (CI-proven on real windows-latest, run 29033024137: leaf-only grant under ungranted
// `%TEMP%` ancestors reachable, ungranted sibling denied. Traverse would only be enforced
// on the rare device LACKING the volume flag — a custom filter-driver/redirector device,
// not where user/build files live.)

// ── the apply() entry (Windows-only: constructs Prepared.launch) ────────────────

#[cfg(target_os = "windows")]
pub(crate) fn apply(
    policy: &SandboxPolicy,
    spec: super::CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    // The MITM child CA-bundle. On the Tier-1 elevated per-host path it is injected into
    // the child env (CA-trust) AND added to the read allow-set so the confined child can
    // read it; on the plain path it rides `set_ca_env`.
    ca_bundle: Option<&std::path::Path>,
    // Private-tmp fresh dir. CUT-1: enforcement (redirect TEMP/TMP + hide the shared tmp
    // without breaking the OS-essential TEMP floor) is a follow-up decision, so the env is
    // pointed best-effort and the axis is reported lost (never a silent under-enforce).
    tmp_dir: Option<&std::path::Path>,
) -> Result<super::Prepared, super::Degradation> {
    use super::{Degradation, Prepared};

    let confine_fs = fs_confines(&policy.fs);
    let sandboxing = confine_fs || policy.net.enforce;
    let tmp_lost = super::tmp_lost_axis(policy);

    // ── net posture (strict-Windows tier decision) ──────────────────────────────
    // Per-host + MITM ride nub's loopback proxy, which an AppContainer child can reach
    // ONLY through an admin-registered loopback exemption. `is_elevated` is queried lazily
    // (only when a per-host rule is present) so the coarse/unconfined paths pay nothing.
    let net_plan = plan_net(
        &policy.net,
        policy.net.enforce
            && policy.net.rules.iter().any(|r| r.effect == Effect::Allow)
            && launch::is_elevated(),
    );
    // INFORMATIVE FAIL (maintainer requirement): a per-host / MITM config on an unelevated
    // Windows host cannot register the exemption, so FAIL CLOSED with a clear message —
    // never silently collapse an allow-list into a coarse deny-all. Coarse on/off is
    // unaffected (it never reaches here).
    if net_plan == WinNetPlan::FailUnelevated {
        let mut lost = vec!["net-per-host".to_string()];
        if !policy.net.brokers.is_empty() {
            lost.push("net-per-request".to_string());
        }
        return Err(Degradation {
            lost,
            reason: Some(
                "per-host network rules (and TLS inspection / credential brokering) require \
                 nub to register a loopback network exemption, which on Windows needs \
                 administrator elevation. Re-run nub from an elevated (Run as administrator) \
                 prompt, or use a coarse net policy — allow-all or deny-all — which needs no \
                 elevation."
                    .to_string(),
            ),
        });
    }
    let tier1 = net_plan == WinNetPlan::Tier1;
    // Tier 1 is meaningless without the running proxy the child routes through; if the
    // proxy failed to start (CA/TLS build or bind failure) fail closed rather than launch a
    // child that can reach nothing under a per-host promise.
    if tier1 && proxy_port.is_none() {
        return Err(Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(
                "the egress proxy required for per-host / TLS-inspect enforcement could not \
                 start"
                    .to_string(),
            ),
        });
    }

    // Nothing needs the AppContainer: only env-scrub (or nothing). Use the plain
    // command path — identical contract to the mac/linux relaxed case.
    if !sandboxing && tmp_lost.is_none() {
        let mut command = std::process::Command::new(&spec.program);
        command.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command.env_clear();
        for (k, v) in &policy.env.constructed {
            command.env(k, v);
        }
        if let Some(port) = proxy_port {
            super::set_proxy_env(&mut command, port, proxy_token);
        }
        if let Some(bundle) = ca_bundle {
            super::set_ca_env(&mut command, bundle);
        }
        if let Some(dir) = tmp_dir {
            super::set_tmp_env(&mut command, dir);
        }
        return Ok(Prepared {
            command,
            degradation: Degradation::full(),
            proxy: None,
            launch: None,
            _private_tmp: None,
        });
    }

    let (read_grants, write_grants, fs_degrade) = derive_grants(&policy.fs);

    // The deny-shadow degradation is judged against the POLICY-derived subtree grants
    // ONLY — captured before the program file is folded in below. The program-file grant
    // is a single leaf with no subtree and is an exec necessity, so no user data-policy
    // deny can "land inside" it; including it would spuriously flag `fs-read-deny` whenever
    // the program merely lives under a deny'd dir.
    let policy_read_grants = read_grants.clone();

    // Auto-grant read+execute on the program FILE ITSELF (not its parent dir) so the
    // LowBox child can exec — with traverse-bypass the leaf-object ACL is what gates the
    // image open, so a file grant suffices. This mirrors the macOS backend's file-only
    // program grant and CLOSES the neighbor-read leak the old parent-dir grant carried (a
    // `.env` next to a tool is no longer swept into the allow-set). A build-jail toolchain
    // (e.g. node.exe) is self-contained and needs nothing more; a program that loads
    // SIBLING DLLs from its own dir needs the FRONT-END to supply that toolchain dir in
    // the read allow-set — the exact launcher contract the macOS "toolchain read-confine
    // for a non-system interpreter" residual defines. The engine no longer auto-widens to
    // the whole program dir.
    let mut read_grants = read_grants;
    if let Some(prog) = resolve_program(&spec.program, spec.cwd.as_deref())
        && !read_grants.contains(&prog)
    {
        read_grants.push(prog);
    }

    // Tier 1 + MITM: the confined child must READ the ephemeral CA bundle to trust the
    // proxy's minted leaves. Grant it as nub infra (not user config), mirroring the
    // mac/linux ca-bundle read grant. Only under a real per-host tier (`tier1`); the plain
    // path handles CA-trust via `set_ca_env` on an unconfined fs.
    if tier1 && let Some(bundle) = ca_bundle {
        let b = bundle.to_path_buf();
        if !read_grants.contains(&b) {
            read_grants.push(b);
        }
    }

    // ── degradation (fail-safe-not-silent) ──────────────────────────────────────
    let mut deg = Degradation::full();
    let mut reason: Option<String> = None;
    if fs_degrade.generous_read {
        deg.lost.push("fs-read".to_string());
        reason.get_or_insert_with(|| {
            "AppContainer enforces an allowlist — a generous read-all-minus-secrets \
             policy is not expressible; reads confined to the explicit allow-set"
                .to_string()
        });
    }
    if fs_degrade.glob_read_unenforced {
        deg.lost.push("fs-read-glob".to_string());
        reason.get_or_insert_with(|| {
            "an embedded-glob read allow can't be an inheritable ACE — that path is \
             not read-granted (over-confined)"
                .to_string()
        });
    }
    // A read deny landing inside a granted subtree can't be carved on Windows — the
    // inheritable read-allow defeats it. Checked against the policy-derived subtree grants
    // only (the program-file grant is excluded — see `policy_read_grants` above).
    if deny_shadows_grant(&policy.fs.rules.entries, &policy_read_grants) {
        deg.lost.push("fs-read-deny".to_string());
        reason.get_or_insert_with(|| {
            "a read deny landing inside a granted subtree can't be carved on Windows \
             (inheritable allow wins) — deny not enforced"
                .to_string()
        });
    }
    // Net per-host / MITM is NOT a degradation here: an unelevated per-host config already
    // returned the informative fail-closed above, and an elevated one (`tier1`) ENFORCES
    // via the loopback exemption registered in `run()` — so there is nothing to report lost.
    // Coarse deny-all and unconfined net are fully honored with no proxy.
    // (Ascendant-env read is OS-CLOSED — the AppContainer denies the parent
    // OpenProcess(PROCESS_VM_READ), run 29043151805 — so NO `env-read-ascendant`
    // Degradation is emitted. Reporting it would falsely tell a frontend Windows is
    // degraded when it isn't. See the module doc.)
    // Private/deny tmp is not yet enforced on Windows — hiding the shared tmp while keeping
    // the OS-essential TEMP floor the child needs to start is a follow-up decision. Report
    // the axis lost (fail-safe honesty) rather than silently run on the shared tmp.
    if let Some(axis) = tmp_lost {
        deg.lost.push(axis.to_string());
        reason.get_or_insert_with(|| {
            "private/deny tmp not yet enforced on Windows (shared tmp visible)".to_string()
        });
    }
    deg.reason = reason;

    let launch = WindowsLaunch {
        program: spec.program,
        args: spec.args,
        cwd: spec.cwd,
        read_grants,
        write_grants,
        env: build_child_env(&policy.env, tier1, proxy_port, proxy_token, ca_bundle),
        // Grant internetClient only when net is unconfined; an enforced net (coarse deny
        // OR Tier 1) withholds it. For Tier 1 this is LOAD-BEARING: the loopback exemption
        // opens loopback but withholding internetClient keeps external egress blocked, so
        // nub's proxy is the child's SOLE egress (matches mac/linux `remote ip localhost`).
        allow_internet: !policy.net.enforce,
        register_loopback_exemption: tier1,
    };

    // The `command` field is unused on the launch path (status() runs `launch`); it
    // holds a benign never-spawned placeholder so the struct stays uniform.
    Ok(Prepared {
        command: std::process::Command::new(&launch.program),
        degradation: deg,
        proxy: None,
        launch: Some(launch),
        _private_tmp: None,
    })
}

/// The child's resolved env block.
///
/// The compiler snapshots both relaxed and enforced environments into `constructed`.
/// Apply never re-reads the ambient process environment.
/// - Tier 1 folds in the cooperative proxy hint (clients route through the loopback proxy)
///   and the MITM CA-trust vars (the child trusts the proxy's minted leaves). A non-Tier-1
///   enforced env stays the plain scrub — no proxy is running to route to.
#[cfg(target_os = "windows")]
fn build_child_env(
    env: &crate::policy::EnvPolicy,
    tier1: bool,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<&std::path::Path>,
) -> Option<BTreeMap<String, String>> {
    let mut m = env.constructed.clone();
    if tier1 {
        if let Some(port) = proxy_port {
            let url = match proxy_token {
                Some(t) => format!("http://{t}@127.0.0.1:{port}"),
                None => format!("http://127.0.0.1:{port}"),
            };
            for k in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "http_proxy",
                "https_proxy",
                "ALL_PROXY",
            ] {
                m.insert(k.to_string(), url.clone());
            }
            m.insert("NODE_USE_ENV_PROXY".to_string(), "1".to_string());
        }
        if let Some(bundle) = ca_bundle {
            // The same tool-convention CA-trust keys as `backend::set_ca_env`.
            let p = bundle.to_string_lossy().into_owned();
            for k in [
                "NODE_EXTRA_CA_CERTS",
                "SSL_CERT_FILE",
                "REQUESTS_CA_BUNDLE",
                "CURL_CA_BUNDLE",
                "GIT_SSL_CAINFO",
                "PIP_CERT",
                "NPM_CONFIG_CAFILE",
                "npm_config_cafile",
                "CARGO_HTTP_CAINFO",
                "AWS_CA_BUNDLE",
                "DENO_CERT",
            ] {
                m.insert(k.to_string(), p.clone());
            }
        }
    }
    Some(m)
}

/// Resolve a program to an absolute path (best-effort) so its parent dir can be
/// read-granted and so CreateProcess needn't PATH-search under the LowBox token.
/// Absolute → itself; a path with a separator → joined against the child cwd; a bare
/// name → PATH search trying the name and common executable extensions. Windows-only
/// (its PATHEXT search is Windows semantics; the host build never calls it).
#[cfg(target_os = "windows")]
fn resolve_program(program: &std::ffi::OsStr, child_cwd: Option<&Path>) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    if p.components().count() > 1 {
        let base = match child_cwd {
            Some(c) => c.to_path_buf(),
            None => std::env::current_dir().ok()?,
        };
        return Some(base.join(p));
    }
    let has_ext = p.extension().is_some();
    let exts = ["exe", "cmd", "bat", "com"];
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if has_ext {
            let cand = dir.join(p);
            if cand.is_file() {
                return Some(cand);
            }
        } else {
            for ext in exts {
                let cand = dir.join(format!("{}.{ext}", program.to_string_lossy()));
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

// ── the FFI launcher ────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod launch {
    use super::WindowsLaunch;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::ExitStatusExt;
    use std::path::Path;
    use std::process::ExitStatus;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
        SetHandleInformation, WAIT_ABANDONED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::NetworkManagement::WindowsFirewall::{
        NetworkIsolationGetAppContainerConfig, NetworkIsolationSetAppContainerConfig,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
        NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeleteAppContainerProfile,
    };
    use windows_sys::Win32::Security::{
        ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, FreeSid, GetLengthSid,
        GetTokenInformation, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES,
        SID_AND_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapFree};
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateMutexW, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, OpenProcessToken,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
        PROCESS_INFORMATION, ReleaseMutex, ResumeThread, STARTUPINFOEXW, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };

    // Generic access rights (avoid a Storage_FileSystem feature dep for FILE_GENERIC_*).
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_EXECUTE: u32 = 0x2000_0000;
    const DELETE: u32 = 0x0001_0000;
    // ACE_FLAGS: applies to this object only (no inheritance) — reached only by the
    // REVOKE_ACCESS teardown, which matches purely on the trustee and ignores inheritance.
    const NO_INHERITANCE: u32 = 0x0;
    // SE_GROUP_ENABLED — a capability SID in SECURITY_CAPABILITIES must be enabled.
    const SE_GROUP_ENABLED: u32 = 0x4;
    // The well-known internetClient capability SID.
    const INTERNET_CLIENT_SID: &str = "S-1-15-3-1";

    /// Monotonic per-process counter so concurrent launches never collide on the
    /// AppContainer profile name (combined with pid + a time nonce).
    static LAUNCH_CTR: AtomicU64 = AtomicU64::new(0);

    /// Serializes the per-path DACL read-modify-write in [`set_ace`]. Concurrent launches
    /// can grant/revoke on a SHARED leaf (two runs granting a common toolchain/program
    /// dir); without this, two non-atomic RMWs race and one run's ACE is lost (its grant
    /// then missing). A single global lock is ample — ACL edits are brief and rare.
    static ACL_LOCK: Mutex<()> = Mutex::new(());

    /// Machine-wide named mutex serializing the loopback-exemption RMW (below). The
    /// exemption list is MACHINE-WIDE state, so a process-local lock is insufficient — two
    /// concurrent elevated nub processes would race the get→set and lose each other's
    /// entry. `Global\` needs `SeCreateGlobalPrivilege`, which the elevated Tier-1 path
    /// holds. Versioned so a future format change can't collide with an old holder.
    const EXEMPTION_MUTEX_NAME: &str = "Global\\nub_sbx_loopback_exempt_v1";

    /// Whether nub runs with an ELEVATED (full admin) token — the exact condition under
    /// which the loopback-exemption write (`NetworkIsolationSetAppContainerConfig`) succeeds
    /// (a standard user or an admin's filtered Medium-IL token both report `false` and both
    /// get ACCESS_DENIED on the write — empirically confirmed on the nub-win VM). So this is
    /// the honest gate for whether the strict-Windows per-host/MITM tier is available.
    pub(super) fn is_elevated() -> bool {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: query-only handle into our own process token.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len: u32 = 0;
        // SAFETY: `elevation` is a correctly-sized TOKEN_ELEVATION out-buffer.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                std::ptr::from_mut(&mut elevation).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            )
        };
        unsafe { CloseHandle(token) };
        ok != 0 && elevation.TokenIsElevated != 0
    }

    /// Run `f` holding the machine-wide loopback-exemption mutex, so the get→modify→set of
    /// the firewall's shared exemption list is atomic across concurrent nub processes.
    /// Best-effort: on a create/timeout failure `f` still runs (the RMW race is fail-SAFE —
    /// a lost entry only REMOVES a child's loopback reach, never widens egress). A 10s cap
    /// keeps a wedged holder from deadlocking teardown.
    fn with_exemption_lock<T>(f: impl FnOnce() -> T) -> T {
        let name = to_wide(EXEMPTION_MUTEX_NAME);
        // SAFETY: standard named-mutex create; NULL security attrs, not initially owned.
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        // WAIT_ABANDONED also means WE now OWN the mutex (a prior holder died mid-RMW): treat
        // it as held so we RELEASE it afterwards rather than re-abandon it on CloseHandle. The
        // protected state may be inconsistent, but our RMW re-reads the whole list, so a
        // crashed predecessor is self-healing — the lock is only about atomicity, not the data.
        let held = !h.is_null()
            && matches!(
                unsafe { WaitForSingleObject(h, 10_000) },
                WAIT_OBJECT_0 | WAIT_ABANDONED
            );
        let out = f();
        if !h.is_null() {
            if held {
                unsafe { ReleaseMutex(h) };
            }
            unsafe { CloseHandle(h) };
        }
        out
    }

    /// Free the buffer `NetworkIsolationGetAppContainerConfig` hands back. That buffer is a
    /// process-heap allocation — the `SID_AND_ATTRIBUTES` array plus a separate heap block per
    /// entry `Sid` — so the matching deallocator is `HeapFree` on the process heap for each
    /// `Sid` then the array (the MSDN `FreeAppContainerConfig` sample). It is NOT freed with
    /// `NetworkIsolationFreeAppContainers`: that is the deallocator for
    /// `NetworkIsolationEnumAppContainers`'s much larger `INET_FIREWALL_APP_CONTAINER` records,
    /// and pointing it at a `SID_AND_ATTRIBUTES` array type-confuses the walk into freeing
    /// garbage interior pointers → STATUS_HEAP_CORRUPTION (windows-latest MSVC; #433).
    fn free_app_container_config(arr: *mut SID_AND_ATTRIBUTES, count: u32) {
        if arr.is_null() {
            return;
        }
        // SAFETY: `arr`/`count` come from a successful `NetworkIsolationGetAppContainerConfig`,
        // which allocates the array and each entry's `Sid` on the process heap.
        unsafe {
            let heap = GetProcessHeap();
            for i in 0..count as usize {
                let sid = (*arr.add(i)).Sid;
                if !sid.is_null() {
                    HeapFree(heap, 0, sid.cast());
                }
            }
            HeapFree(heap, 0, arr.cast());
        }
    }

    /// Add or remove `sid` in the machine-wide AppContainer loopback-exemption list via a
    /// read-modify-write (`NetworkIsolationSetAppContainerConfig` REPLACES the whole list,
    /// so the current entries must be preserved — never clobber other apps' exemptions).
    /// Held under [`with_exemption_lock`]. A stale copy of `sid` is always dropped first, so
    /// register is idempotent and remove is exact.
    fn set_loopback_exemption(sid: PSID, add: bool) -> io::Result<()> {
        with_exemption_lock(|| {
            let mut count: u32 = 0;
            let mut arr: *mut SID_AND_ATTRIBUTES = std::ptr::null_mut();
            // SAFETY: out-params for the current exemption list (count + heap array).
            let rc = unsafe { NetworkIsolationGetAppContainerConfig(&mut count, &mut arr) };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let existing: &[SID_AND_ATTRIBUTES] = if arr.is_null() || count == 0 {
                &[]
            } else {
                // SAFETY: `arr` points at `count` entries per the successful Get above.
                unsafe { std::slice::from_raw_parts(arr, count as usize) }
            };
            let mut new_list: Vec<SID_AND_ATTRIBUTES> = existing
                .iter()
                .filter(|e| !sids_equal(e.Sid, sid))
                .copied()
                .collect();
            if add {
                new_list.push(SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: 0,
                });
            }
            // SAFETY: `new_list` outlives the Set call; its Sid pointers reference either
            // the still-live `arr` allocation or the caller's `sid` (freed only after).
            let set_rc = unsafe {
                NetworkIsolationSetAppContainerConfig(
                    new_list.len() as u32,
                    if new_list.is_empty() {
                        std::ptr::null()
                    } else {
                        new_list.as_ptr()
                    },
                )
            };
            // Free the got list AFTER Set (new_list borrowed its Sid pointers).
            free_app_container_config(arr, count);
            if set_rc != 0 {
                return Err(io::Error::from_raw_os_error(set_rc as i32));
            }
            Ok(())
        })
    }

    /// Byte-equality of two SIDs (both are self-relative fixed-length structures).
    fn sids_equal(a: PSID, b: PSID) -> bool {
        if a.is_null() || b.is_null() {
            return false;
        }
        let (la, lb) = unsafe { (GetLengthSid(a), GetLengthSid(b)) };
        if la != lb {
            return false;
        }
        // SAFETY: GetLengthSid reports each SID's exact byte length.
        let sa = unsafe { std::slice::from_raw_parts(a.cast::<u8>(), la as usize) };
        let sb = unsafe { std::slice::from_raw_parts(b.cast::<u8>(), lb as usize) };
        sa == sb
    }

    /// Removes the per-run loopback exemption on drop. Owned SID copy (independent of the
    /// profile-owned SID pointer). Best-effort remove — a failure only leaves an ORPHANED
    /// exemption for a now-deleted AC SID (harmless: it grants nothing, but accretes a list
    /// entry; the crash-leak is documented in LIMITATIONS.md).
    struct ExemptionGuard {
        sid: Vec<u8>,
    }
    impl Drop for ExemptionGuard {
        fn drop(&mut self) {
            let sid = self.sid.as_ptr() as PSID;
            let _ = set_loopback_exemption(sid, false);
        }
    }

    impl WindowsLaunch {
        /// Own the full spawn lifecycle: create a per-run AppContainer profile, grant
        /// the inheritable allow-ACEs, launch the child under the LowBox token inside a
        /// kill-on-close Job, wait, then tear everything down (RAII).
        pub(crate) fn run(self) -> io::Result<ExitStatus> {
            // 1. Per-run AppContainer profile → AC SID. `_profile` deletes it on drop
            //    (declared FIRST ⇒ dropped LAST, after the ACEs are revoked).
            let name = unique_profile_name();
            let ac_sid = create_appcontainer(&name)?;
            let _profile = ProfileGuard {
                name: to_wide(&name),
                sid: ac_sid,
            };
            // An owned copy of the SID bytes, so ACE revoke doesn't depend on the
            // profile-owned SID pointer surviving.
            let sid_copy = copy_sid(ac_sid)?;

            // 1b. Strict-Windows Tier 1: register the machine-wide loopback exemption for
            //     this per-run AC SID so the child can reach nub's loopback egress proxy
            //     (its SOLE egress — internetClient stays withheld). `_exemption` removes it
            //     on drop (RAII, owned SID copy so it's independent of the profile SID).
            //     FAIL-CLOSED: a failed register aborts the launch rather than spawn a child
            //     that can't reach its proxy under a per-host promise. TRADEOFF (bounded +
            //     documented, LIMITATIONS.md): the exemption widens the child to ALL loopback
            //     services for the run's lifetime — scoped to this ephemeral SID and torn
            //     down on exit, but not narrowable to only the proxy port without admin WFP.
            let _exemption = if self.register_loopback_exemption {
                let owned = copy_sid(ac_sid)?;
                set_loopback_exemption(ac_sid, true).map_err(|e| {
                    io::Error::other(format!(
                        "sandbox: could not register the loopback network exemption required \
                         for per-host net / TLS inspection (needs elevation): {e}"
                    ))
                })?;
                Some(ExemptionGuard { sid: owned })
            } else {
                None
            };

            // 2. Grant the leaf allow-ACEs; `_aces` revokes them on drop (declared before
            //    the job ⇒ revoked after the tree is reaped, before profile delete). Leaf
            //    read/write grants are INHERITABLE (cover the subtree). NO ancestor
            //    traverse grants: a LowBox token bypasses traverse checking on a standard
            //    NTFS volume (the TRAVERSE MODEL note above), so the leaf ACL alone gates
            //    access — no WRITE_DAC on a shared ancestor, no C:\-owned store. A
            //    REVOKE_ACCESS teardown on the unique SID removes exactly our ACEs from
            //    every path, whatever the access mask.
            let mut granted: Vec<std::path::PathBuf> = Vec::new();
            for dir in &self.read_grants {
                // A failed grant (e.g. a >MAX_PATH path, or a dir we can't WRITE_DAC) is
                // skipped, not fatal — the child is then denied that leaf. Log at debug so
                // a "child can't read its own dir" report is diagnosable, not silent.
                match set_ace(
                    dir,
                    ac_sid,
                    GENERIC_READ | GENERIC_EXECUTE,
                    GRANT_ACCESS,
                    true,
                ) {
                    Ok(()) => granted.push(dir.clone()),
                    Err(e) => tracing::debug!(
                        path = %dir.display(),
                        error = %e,
                        "sandbox: read-grant ACE failed — leaf unreachable to the child"
                    ),
                }
            }
            for dir in &self.write_grants {
                if let Err(e) = set_ace(
                    dir,
                    ac_sid,
                    GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                    GRANT_ACCESS,
                    true,
                ) {
                    tracing::debug!(
                        path = %dir.display(),
                        error = %e,
                        "sandbox: write-grant ACE failed — leaf not writable by the child"
                    );
                }
                if !granted.contains(dir) {
                    granted.push(dir.clone());
                }
            }
            let _aces = AceGuard {
                paths: granted,
                sid: sid_copy,
            };

            // 3. Capabilities: internetClient iff egress allowed.
            let mut cap_sid_owned: Option<CapSid> = None;
            let mut caps: Vec<SID_AND_ATTRIBUTES> = Vec::new();
            if self.allow_internet {
                let cs = CapSid::new(INTERNET_CLIENT_SID)?;
                caps.push(SID_AND_ATTRIBUTES {
                    Sid: cs.0,
                    Attributes: SE_GROUP_ENABLED,
                });
                cap_sid_owned = Some(cs);
            }
            let mut sec_caps = SECURITY_CAPABILITIES {
                AppContainerSid: ac_sid,
                Capabilities: if caps.is_empty() {
                    std::ptr::null_mut()
                } else {
                    caps.as_mut_ptr()
                },
                CapabilityCount: caps.len() as u32,
                Reserved: 0,
            };

            // 4. Job with KILL_ON_JOB_CLOSE; `_job` closes the handle on drop (declared
            //    LAST ⇒ dropped FIRST ⇒ reaps any lingering tree before ACE revoke).
            let job = create_kill_on_close_job()?;
            let _job = HandleGuard(job);

            // 5. Proc-thread attribute list: SECURITY_CAPABILITIES, plus a HANDLE_LIST
            //    scoping inheritance to EXACTLY the std handles (see `bInheritHandles`
            //    below). The list must be alive across CreateProcessW (it stores the
            //    pointer); `inherit_handles` outlives the call.
            let inherit_handles = inheritable_std_handles();
            let n_attrs = 1 + u32::from(!inherit_handles.is_empty());
            let mut attr = ProcThreadAttrList::new(n_attrs)?;
            attr.update(
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                std::ptr::from_mut(&mut sec_caps).cast(),
                std::mem::size_of::<SECURITY_CAPABILITIES>(),
            )?;
            if !inherit_handles.is_empty() {
                attr.update(
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    inherit_handles.as_ptr().cast_mut().cast(),
                    std::mem::size_of::<HANDLE>() * inherit_handles.len(),
                )?;
            }

            // 6. Build the command line + env block + cwd (kept alive across the call).
            let mut cmdline = build_command_line(&self.program, &self.args);
            let env_block = self.env.as_ref().map(build_env_block);
            let cwd_wide = self.cwd.as_ref().map(|c| to_wide(&c.to_string_lossy()));

            let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si.lpAttributeList = attr.as_ptr();
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

            let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED;
            let env_ptr: *const std::ffi::c_void = match &env_block {
                Some(b) => {
                    flags |= CREATE_UNICODE_ENVIRONMENT;
                    b.as_ptr().cast()
                }
                None => std::ptr::null(),
            };
            let cwd_ptr = cwd_wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());

            // SAFETY: cmdline/env_block/cwd_wide/attr/sec_caps/caps all outlive this
            // call; lpCommandLine is a writable UTF-16 buffer as CreateProcessW requires.
            let ok = unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    cmdline.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    // bInheritHandles must be TRUE for the PROC_THREAD_ATTRIBUTE_HANDLE_LIST
                    // above to take effect — and WITH that list, the child inherits ONLY the
                    // std handles in it (its output still reaches the user), not every
                    // inheritable handle nub holds. If there was no valid std handle to pass,
                    // the list is absent and we set FALSE (inherit nothing) — fail-safe.
                    i32::from(!inherit_handles.is_empty()),
                    flags,
                    env_ptr as *const _,
                    cwd_ptr,
                    std::ptr::from_mut(&mut si).cast(),
                    &mut pi,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let _ = cap_sid_owned; // held alive until here

            // 7. Assign to the job while the child is still SUSPENDED, and only resume
            //    once it is contained — so a child that spawns a descendant can never do
            //    so outside the Job. On assign failure, terminate the still-suspended
            //    child (it never ran) and fail closed.
            let assign_ok = unsafe { AssignProcessToJobObject(job, pi.hProcess) };
            if assign_ok == 0 {
                unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                }
                return Err(io::Error::other("AssignProcessToJobObject failed"));
            }
            unsafe { ResumeThread(pi.hThread) };

            let code = unsafe {
                if WaitForSingleObject(pi.hProcess, INFINITE) != WAIT_OBJECT_0 {
                    let e = io::Error::last_os_error();
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                    return Err(e);
                }
                let mut code: u32 = 0;
                GetExitCodeProcess(pi.hProcess, &mut code);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                code
            };

            Ok(ExitStatus::from_raw(code))
            // `_job` (reap) → `_aces` (revoke) → `_profile` (delete) drop here, reverse.
        }
    }

    /// A capability SID string converted to a PSID (LocalFree'd on drop).
    struct CapSid(PSID);
    impl CapSid {
        fn new(sid_str: &str) -> io::Result<Self> {
            let wide = to_wide(sid_str);
            let mut sid: PSID = std::ptr::null_mut();
            let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(CapSid(sid))
        }
    }
    impl Drop for CapSid {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    /// Deletes the per-run AppContainer profile and frees the AC SID on drop.
    struct ProfileGuard {
        name: Vec<u16>,
        sid: PSID,
    }
    impl Drop for ProfileGuard {
        fn drop(&mut self) {
            // DeleteAppContainerProfile removes the profile (registry/on-disk state) but
            // does NOT free the SID buffer; per MSDN the SID from
            // CreateAppContainerProfile must be released with FreeSid. Independent calls,
            // no double-free.
            unsafe {
                DeleteAppContainerProfile(self.name.as_ptr());
                FreeSid(self.sid);
            }
        }
    }

    /// Revokes the per-run allow-ACEs on drop. Uses an owned SID copy so it does not
    /// depend on the profile SID pointer. REVOKE_ACCESS removes every ACE for the SID;
    /// since the SID is unique per run and appears nowhere else, exactly our ACE goes.
    struct AceGuard {
        paths: Vec<std::path::PathBuf>,
        sid: Vec<u8>,
    }
    impl Drop for AceGuard {
        fn drop(&mut self) {
            let sid = self.sid.as_ptr() as PSID;
            for p in &self.paths {
                let _ = revoke_ace(p, sid);
            }
        }
    }

    /// Closes a raw handle on drop. For the Job handle this triggers
    /// KILL_ON_JOB_CLOSE — reaping any process still in the tree.
    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// An initialized PROC_THREAD_ATTRIBUTE_LIST, backed by a pointer-aligned buffer
    /// (a `Vec<usize>`, not `Vec<u8>`, so the opaque list is suitably aligned), freed on
    /// drop.
    struct ProcThreadAttrList {
        buf: Vec<usize>,
    }
    impl ProcThreadAttrList {
        fn new(count: u32) -> io::Result<Self> {
            let mut size: usize = 0;
            // First call sizes the list (expected to "fail" setting size).
            unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
            let words = size.div_ceil(std::mem::size_of::<usize>()).max(1);
            let mut buf = vec![0usize; words];
            let ok = unsafe {
                InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), count, 0, &mut size)
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { buf })
        }
        fn update(
            &mut self,
            attr: usize,
            value: *mut std::ffi::c_void,
            size: usize,
        ) -> io::Result<()> {
            let ok = unsafe {
                UpdateProcThreadAttribute(
                    self.buf.as_mut_ptr().cast(),
                    0,
                    attr,
                    value,
                    size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        fn as_ptr(&mut self) -> *mut std::ffi::c_void {
            self.buf.as_mut_ptr().cast()
        }
    }
    impl Drop for ProcThreadAttrList {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.buf.as_mut_ptr().cast()) };
        }
    }

    /// The std handles (stdin/stdout/stderr) to hand the child, deduplicated. Each is
    /// marked inheritable — every member of a PROC_THREAD_ATTRIBUTE_HANDLE_LIST must be,
    /// or CreateProcessW fails. An invalid/NULL std handle (a parent with no console) is
    /// skipped; an empty result ⇒ the caller inherits nothing (bInheritHandles FALSE).
    /// Marking the parent's own std handles inheritable is what `std`'s own inherited-stdio
    /// spawn does; it does not widen anything the child can reach beyond its stdio.
    fn inheritable_std_handles() -> Vec<HANDLE> {
        let raws = [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ];
        let mut out: Vec<HANDLE> = Vec::new();
        for r in raws {
            let h: HANDLE = r.cast();
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                continue;
            }
            // Only keep a handle we could actually mark inheritable — a non-inheritable
            // member would make CreateProcessW fail the whole spawn, so omit it (the child
            // loses that one stream) rather than take the process down.
            let marked =
                unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
            if marked != 0 && !out.contains(&h) {
                out.push(h);
            }
        }
        out
    }

    fn unique_profile_name() -> String {
        let pid = std::process::id();
        let ctr = LAUNCH_CTR.fetch_add(1, Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // AppContainer names are <= 64 chars, alnum/underscore.
        format!("nub_sbx_{pid}_{nonce:x}_{ctr}")
    }

    fn create_appcontainer(name: &str) -> io::Result<PSID> {
        let wname = to_wide(name);
        let mut sid: PSID = std::ptr::null_mut();
        // hr is an HRESULT; 0 == S_OK. Display name + description reuse the name.
        let hr = unsafe {
            CreateAppContainerProfile(
                wname.as_ptr(),
                wname.as_ptr(),
                wname.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if hr != 0 {
            return Err(io::Error::other(format!(
                "CreateAppContainerProfile failed hr=0x{hr:08x}"
            )));
        }
        Ok(sid)
    }

    /// Copy a PSID's bytes into an owned buffer (GetLengthSid).
    fn copy_sid(sid: PSID) -> io::Result<Vec<u8>> {
        let len = unsafe { GetLengthSid(sid) } as usize;
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; len];
        unsafe { std::ptr::copy_nonoverlapping(sid.cast::<u8>(), buf.as_mut_ptr(), len) };
        Ok(buf)
    }

    fn create_kill_on_close_job() -> io::Result<HANDLE> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            unsafe { CloseHandle(job) };
            return Err(e);
        }
        Ok(job)
    }

    /// Remove every ACE for `sid` on `path` (teardown). REVOKE_ACCESS ignores the
    /// access mask + inheritance and matches purely on the trustee, so a unique per-run
    /// SID's ACEs go cleanly wherever we placed them.
    fn revoke_ace(path: &Path, sid: PSID) -> io::Result<()> {
        set_ace(path, sid, 0, REVOKE_ACCESS, false)
    }

    /// Add/remove an ACE granting `sid` `access` on `path`. `inherit` ⇒ the ACE is
    /// container+object inheritable (a leaf subtree grant); otherwise it applies to
    /// `path` alone (reached only by the REVOKE_ACCESS teardown, which matches on the
    /// trustee and ignores inheritance). Additive — reads the existing DACL and merges,
    /// never clobbering other ACEs.
    fn set_ace(path: &Path, sid: PSID, access: u32, mode: i32, inherit: bool) -> io::Result<()> {
        // Serialize the DACL RMW across concurrent launches (see ACL_LOCK). Poison-
        // tolerant: a prior panicked holder left no invariant broken here.
        let _lock: MutexGuard<'_, ()> = ACL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wpath = to_wide_path(path);
        let mut old_dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // Read the existing DACL so the grant is additive (never clobber existing ACEs).
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut old_dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let sd_guard = LocalFreeGuard(sd);

        let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        ea.grfAccessPermissions = access;
        ea.grfAccessMode = mode;
        ea.grfInheritance = if inherit {
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
        } else {
            NO_INHERITANCE
        };
        ea.Trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: sid.cast(),
        };

        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let rc = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let new_guard = LocalFreeGuard(new_dacl.cast());

        let rc = unsafe {
            SetNamedSecurityInfoW(
                wpath.as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null_mut(),
            )
        };
        drop(new_guard);
        drop(sd_guard);
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }

    struct LocalFreeGuard(*mut std::ffi::c_void);
    impl Drop for LocalFreeGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// UTF-16, NUL-terminated.
    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A path as a NUL-terminated wide string with backslash separators (canonical IR
    /// paths are forward-slashed; the Win32 security APIs want native separators).
    fn to_wide_path(p: &Path) -> Vec<u16> {
        let s = p.to_string_lossy().replace('/', "\\");
        to_wide(&s)
    }

    /// Build a mutable UTF-16 command line from program + args, quoting each token per
    /// the CommandLineToArgvW rules std uses. lpApplicationName is NULL, so the child
    /// gets a conventional argv.
    fn build_command_line(program: &std::ffi::OsStr, args: &[std::ffi::OsString]) -> Vec<u16> {
        let mut line: Vec<u16> = Vec::new();
        append_quoted(&mut line, program);
        for a in args {
            line.push(u16::from(b' '));
            append_quoted(&mut line, a);
        }
        line.push(0);
        line
    }

    fn append_quoted(out: &mut Vec<u16>, arg: &std::ffi::OsStr) {
        let wide: Vec<u16> = arg.encode_wide().collect();
        let needs_quote = wide.is_empty()
            || wide
                .iter()
                .any(|&c| c == u16::from(b' ') || c == u16::from(b'\t') || c == u16::from(b'"'));
        if !needs_quote {
            out.extend_from_slice(&wide);
            return;
        }
        out.push(u16::from(b'"'));
        let mut backslashes = 0usize;
        for &c in &wide {
            if c == u16::from(b'\\') {
                backslashes += 1;
            } else if c == u16::from(b'"') {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push(u16::from(b'\\'));
                }
                out.push(u16::from(b'"'));
                backslashes = 0;
            } else {
                for _ in 0..backslashes {
                    out.push(u16::from(b'\\'));
                }
                backslashes = 0;
                out.push(c);
            }
        }
        for _ in 0..(backslashes * 2) {
            out.push(u16::from(b'\\'));
        }
        out.push(u16::from(b'"'));
    }

    /// Build a UTF-16 double-NUL-terminated environment block from the constructed map.
    /// Entries are ordered case-INSENSITIVELY by key — the block ordering Windows
    /// expects (the source `BTreeMap` is case-sensitive, so a lowercase key like
    /// `windir` would otherwise sort after all-uppercase keys and violate the
    /// convention).
    fn build_env_block(env: &std::collections::BTreeMap<String, String>) -> Vec<u16> {
        let mut pairs: Vec<(&String, &String)> = env.iter().collect();
        pairs.sort_by_key(|a| a.0.to_ascii_uppercase());
        let mut block: Vec<u16> = Vec::new();
        for (k, v) in pairs {
            block.extend(k.encode_utf16());
            block.push(u16::from(b'='));
            block.extend(v.encode_utf16());
            block.push(0);
        }
        // An empty block still needs the terminating double-NUL.
        block.push(0);
        if block.len() == 1 {
            block.push(0);
        }
        block
    }
}

// ── host-testable derivation tests ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CanonGlob, FsRule, FsRuleSet, TmpMode};

    fn fs(default_effect: Effect, entries: Vec<FsRule>) -> FsPolicy {
        FsPolicy {
            rules: FsRuleSet {
                entries,
                default_effect,
            },
            tmp: TmpMode::Private,
        }
    }
    fn rule(m: &str, effect: Effect, access: FsAccess) -> FsRule {
        FsRule {
            matcher: CanonGlob(m.to_string()),
            effect,
            access,
        }
    }

    #[test]
    fn read_confine_grants_only_explicit_allows_no_degrade() {
        // default-deny + a literal own-dir rw allow = the build-jail shape: one read
        // grant + one write grant, no degradation.
        let p = fs(
            Effect::Deny,
            vec![rule("C:/proj/pkg", Effect::Allow, FsAccess::ReadWrite)],
        );
        let (read, write, deg) = derive_grants(&p);
        assert_eq!(read, vec![PathBuf::from("C:/proj/pkg")]);
        assert_eq!(write, vec![PathBuf::from("C:/proj/pkg")]);
        assert_eq!(deg, FsDegrade::default());
    }

    #[test]
    fn read_only_allow_yields_no_write_grant() {
        let p = fs(
            Effect::Deny,
            vec![rule("C:/tools", Effect::Allow, FsAccess::Read)],
        );
        let (read, write, _) = derive_grants(&p);
        assert_eq!(read, vec![PathBuf::from("C:/tools")]);
        assert!(
            write.is_empty(),
            "a read-only allow must not open a write grant"
        );
    }

    #[test]
    fn subtree_twin_collapses_to_the_directory() {
        // `C:/proj/**` and `C:/proj` both mean the subtree — one grant.
        assert_eq!(
            literal_subtree("C:/proj/**"),
            Some(PathBuf::from("C:/proj"))
        );
        assert_eq!(literal_subtree("C:/proj"), Some(PathBuf::from("C:/proj")));
    }

    #[test]
    fn generous_read_base_degrades_fs_read() {
        // default-allow (generous read-all-minus-secrets) can't be an allowlist.
        let p = fs(
            Effect::Allow,
            vec![rule("**/.env", Effect::Deny, FsAccess::Read)],
        );
        let (_read, _write, deg) = derive_grants(&p);
        assert!(
            deg.generous_read,
            "a default-Allow base must degrade fs-read"
        );
    }

    #[test]
    fn whole_fs_allow_entry_degrades_generous_read() {
        // The shape the compiler ACTUALLY emits for `"..."` / `sandbox: true`: a Deny
        // base + a whole-fs `**` Allow ENTRY (+ secret denies). It must degrade, not be
        // silently dropped as a no-op grant.
        let p = fs(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                rule("**/.env", Effect::Deny, FsAccess::Read),
            ],
        );
        let (read, _write, deg) = derive_grants(&p);
        assert!(
            read.is_empty(),
            "a whole-fs `**` allow yields no literal grant"
        );
        assert!(
            deg.generous_read,
            "a whole-fs `**` Allow ENTRY must degrade fs-read (not silently drop)"
        );
    }

    #[test]
    fn embedded_glob_allow_is_skipped_not_widened() {
        // `C:/proj/*.pem` must NOT widen to a `C:/proj` read grant (would expose a
        // sibling secret); it is skipped + flagged (fail-safe over-confinement).
        let p = fs(
            Effect::Deny,
            vec![rule("C:/proj/*.pem", Effect::Allow, FsAccess::Read)],
        );
        let (read, _write, deg) = derive_grants(&p);
        assert!(
            read.is_empty(),
            "an embedded-glob allow must not be widened to a grant"
        );
        assert!(deg.glob_read_unenforced);
    }

    #[test]
    fn deny_shadowed_by_a_grant_is_detected() {
        let grants = vec![PathBuf::from("C:/proj")];
        // A LITERAL deny inside a granted subtree — inheritable allow defeats it.
        let literal = vec![rule("C:/proj/secret", Effect::Deny, FsAccess::Read)];
        assert!(deny_shadows_grant(&literal, &grants));
        // A GLOBBED deny inside the grant (`C:/proj/*.pem`) — the earlier gap: its
        // literal prefix `C:/proj` is the grant, so it's shadowed.
        let globbed = vec![rule("C:/proj/*.pem", Effect::Deny, FsAccess::Read)];
        assert!(deny_shadows_grant(&globbed, &grants));
        // A DEPTH-INDEPENDENT deny (`**/.env`) matches inside every grant.
        let depth_indep = vec![rule("**/.env", Effect::Deny, FsAccess::Read)];
        assert!(deny_shadows_grant(&depth_indep, &grants));
        // Case-insensitive: `C:/PROJ/...` still shadows the `C:/proj` grant.
        let cased = vec![rule("C:/PROJ/secret", Effect::Deny, FsAccess::Read)];
        assert!(deny_shadows_grant(&cased, &grants));
        // A deny OUTSIDE every grant is enforced by default-deny — not shadowed.
        let outside = vec![rule("C:/other/secret", Effect::Deny, FsAccess::Read)];
        assert!(!deny_shadows_grant(&outside, &grants));
        // No grants ⇒ nothing to shadow.
        assert!(!deny_shadows_grant(&depth_indep, &[]));
    }

    #[test]
    fn dangerous_write_roots_never_get_a_write_grant() {
        // A rw allow that resolves to a system root must not open an inheritable modify
        // ACE there (filesystem-wide write hole). Read of it is still fine.
        for root in ["C:", "C:/", "C:/Windows", "C:/Program Files", "C:/Users"] {
            let p = fs(
                Effect::Deny,
                vec![rule(root, Effect::Allow, FsAccess::ReadWrite)],
            );
            let (_read, write, _) = derive_grants(&p);
            assert!(
                write.is_empty(),
                "{root} must not receive a write grant (dangerous root)"
            );
        }
        // A real project dir under Users is NOT over-blocked.
        let p = fs(
            Effect::Deny,
            vec![rule("C:/Users/me/proj", Effect::Allow, FsAccess::ReadWrite)],
        );
        let (_r, write, _) = derive_grants(&p);
        assert_eq!(write, vec![PathBuf::from("C:/Users/me/proj")]);
    }

    #[test]
    fn whole_fs_globs_have_no_literal_subtree() {
        assert_eq!(literal_subtree("**"), None);
        assert_eq!(literal_subtree("/**"), None);
        assert_eq!(literal_subtree("/"), None);
    }

    #[test]
    fn fs_confines_matches_mac_linux_semantics() {
        // Relaxed (default-Allow, no entries) does NOT confine.
        assert!(!fs_confines(&fs(Effect::Allow, vec![])));
        // Any entry, or a deny base, confines.
        assert!(fs_confines(&fs(Effect::Deny, vec![])));
        assert!(fs_confines(&fs(
            Effect::Allow,
            vec![rule("C:/x", Effect::Deny, FsAccess::Read)]
        )));
    }

    #[test]
    fn plan_net_decides_windows_net_posture() {
        use crate::policy::{NetPolicy, NetRule, NetTarget};
        let allow = |h: &str| NetRule {
            target: NetTarget::Host(h.to_string()),
            effect: Effect::Allow,
        };

        // Unconfined net — grant internetClient, no proxy (elevation-irrelevant).
        let unconfined = NetPolicy::default();
        assert_eq!(plan_net(&unconfined, false), WinNetPlan::Unconfined);
        assert_eq!(plan_net(&unconfined, true), WinNetPlan::Unconfined);

        // Pure deny-all — coarse egress-deny, unprivileged (elevation-irrelevant).
        let deny_all = NetPolicy {
            enforce: true,
            default_effect: Effect::Deny,
            ..Default::default()
        };
        assert_eq!(plan_net(&deny_all, false), WinNetPlan::CoarseDeny);
        assert_eq!(plan_net(&deny_all, true), WinNetPlan::CoarseDeny);

        // Per-host (any Allow rule) needs the elevated loopback exemption: Tier 1 when
        // elevated, fail-CLOSED (never silent coarse-degrade) when not.
        let per_host = NetPolicy {
            enforce: true,
            rules: vec![allow("example.com")],
            default_effect: Effect::Deny,
            ..Default::default()
        };
        assert_eq!(plan_net(&per_host, true), WinNetPlan::Tier1);
        assert_eq!(plan_net(&per_host, false), WinNetPlan::FailUnelevated);
    }

    // `apply` is `#[cfg(windows)]`, so this test compiles + runs only on the Windows VM/CI.
    #[cfg(target_os = "windows")]
    #[test]
    fn apply_windows_net_tiers() {
        use crate::policy::{NetPolicy, NetRule, NetTarget};
        let mk = |net: NetPolicy| SandboxPolicy {
            fs: fs(Effect::Deny, vec![]),
            net,
            ..Default::default()
        };

        // Pure deny-all: coarse egress-deny, fully enforced — never a net-per-host loss.
        // Elevation-independent (no proxy, no exemption).
        let deny_all = mk(NetPolicy {
            enforce: true,
            default_effect: Effect::Deny,
            ..Default::default()
        });
        let deg = apply(
            &deny_all,
            crate::CommandSpec::new("cmd.exe"),
            None,
            None,
            None,
            None,
        )
        .expect("apply deny-all")
        .degradation;
        assert!(
            !deg.lost.iter().any(|s| s == "net-per-host"),
            "deny-all is coarse-enforced, not degraded (got {:?})",
            deg.lost
        );

        // Per-host: Tier 1 (enforced, no degradation) when elevated; fail-CLOSED with a
        // clear elevation message otherwise — NEVER a silent coarse-degrade.
        let per_host = mk(NetPolicy {
            enforce: true,
            rules: vec![NetRule {
                target: NetTarget::Host("example.com".to_string()),
                effect: Effect::Allow,
            }],
            default_effect: Effect::Deny,
            ..Default::default()
        });
        let res = apply(
            &per_host,
            crate::CommandSpec::new("cmd.exe"),
            Some(9999),
            None,
            None,
            None,
        );
        if launch::is_elevated() {
            let deg = res.expect("elevated: Tier 1 applies").degradation;
            assert!(
                !deg.lost.iter().any(|s| s == "net-per-host"),
                "Tier 1 enforces per-host — no net-per-host degradation (got {:?})",
                deg.lost
            );
        } else {
            // `expect_err` would require `Prepared: Debug` (the Ok type), which it does not
            // implement; match instead.
            let err = match res {
                Ok(_) => panic!("unelevated per-host must fail-closed, not degrade"),
                Err(d) => d,
            };
            assert!(
                err.lost.iter().any(|s| s == "net-per-host"),
                "the fail-closed Degradation must name net-per-host (got {:?})",
                err.lost
            );
            assert!(
                err.reason.as_deref().unwrap_or_default().contains("elevat"),
                "the fail message must name the elevation requirement (got {:?})",
                err.reason
            );
        }
    }
}
