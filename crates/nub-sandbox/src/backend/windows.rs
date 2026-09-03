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
//!     is blocked. An AppContainer with `internetClient` has public outbound access,
//!     not full host networking. Per-host requests fail closed because the available
//!     exemption exposes every loopback listener, including local forwarders.
//!   - process-reap: a Job Object with `KILL_ON_JOB_CLOSE`; the whole tree dies when
//!     the job handle closes (after the child exits, or if nub does).
//!   - process-count: the same Job carries `ACTIVE_PROCESS` (see
//!     [`active_process_cap`]) so a fork bomb from confined code is bounded — a
//!     zero-privilege limit the LowBox token cannot break away from.
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

use crate::policy::{Effect, FsAccess, FsOrigin, FsPolicy, FsRule, NetPolicy};
// Referenced only by the Windows-gated `apply`; the host build (module-under-test)
// never names it.
#[cfg(target_os = "windows")]
use crate::policy::SandboxPolicy;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Normalize an environment entry sequence into Windows's case-insensitive key
/// space. The last entry wins when a direct caller supplies aliases; compiler
/// construction has already selected the literal value before this final guard.
/// Kept outside the FFI module so this contract is unit-tested on non-Windows hosts.
fn dedupe_windows_env_pairs<'a>(
    pairs: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Vec<(&'a String, &'a String)> {
    let mut folded = BTreeMap::new();
    for (key, value) in pairs {
        folded.insert(key.to_ascii_uppercase(), (key, value));
    }
    folded.into_values().collect()
}

/// A resolved AppContainer launch plan. All fields are OS-agnostic plain data so the
/// IR→plan derivation is unit-tested on the dev host; [`AppContainerLaunch::run`] (the FFI)
/// is `#[cfg(windows)]`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct AppContainerLaunch {
    program: OsString,
    args: super::CommandArgs,
    cwd: Option<PathBuf>,
    /// Subtrees the AppContainer SID is granted inheritable read-execute.
    read_grants: Vec<PathBuf>,
    /// Subtrees the AppContainer SID is granted inheritable modify (read+write).
    write_grants: Vec<PathBuf>,
    /// The subset of `read_grants` marked [`FsOrigin::NubOwnedPublic`] — nub's OWN public
    /// caches. Published ONCE to `ALL APPLICATION PACKAGES` instead of re-granted per run,
    /// which is what makes the store grant free after the first launch; see
    /// [`FsOrigin::NubOwnedPublic`] for the measured cost and the exposure it trades.
    publishable_grants: Vec<PathBuf>,
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

/// Which Windows mechanism owns this launch. Exactly one applies, which is why this is an
/// enum rather than two `Option` fields: build-jail's admin-free AppContainer and
/// agent-sandbox's dedicated account are alternatives, never a combination.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) enum WindowsLaunch {
    /// Per-run AppContainer (LowBox) — the pure-allowlist path. No elevation, ever.
    AppContainer(AppContainerLaunch),
    /// Dedicated local account + WFP — the full-grammar path. Needs a one-time elevated setup.
    Account(super::windows_account::AccountLaunch),
}

#[cfg(target_os = "windows")]
impl WindowsLaunch {
    pub(crate) fn run(self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            WindowsLaunch::AppContainer(l) => l.run(),
            WindowsLaunch::Account(l) => l.run(),
        }
    }
}

/// Active-process ceiling applied to every confined launch's Job Object
/// (`JOB_OBJECT_LIMIT_ACTIVE_PROCESS`), bounding a fork bomb from confined code without
/// any privilege. Sized from what a LEGITIMATE build actually needs: node-gyp emits no
/// `-j`, so `make` runs serial, and the measured structural ceiling of a parallel native
/// build is `2 * cores + 5` (23 at 8 cores, 69 at 32). `8 * cores` is ~4x that headroom;
/// the 64 floor keeps a low-core runner from getting a cap tighter than a JS-only
/// script tree ever needs. The scope is deliberately PER LAUNCH (one Job per confined
/// spawn = one script tree), not per install — a per-install cap would have to be summed
/// across concurrent scripts and would land ABOVE the ~1,440-process incident it exists
/// to bound, protecting nothing.
///
/// Over-cap failure is an observable spawn error in the child (`ERROR_NOT_ENOUGH_QUOTA`,
/// 1816), NOT a kill of the tree — a legitimate build that brushes the ceiling reports a
/// spawn failure through its own toolchain rather than dying silently.
pub(super) fn active_process_cap() -> u32 {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    u32::try_from(cores.saturating_mul(8))
        .unwrap_or(u32::MAX)
        .max(64)
}

/// What the allowlist model could NOT express for a policy, so the caller can be told.
#[derive(Debug, Default, PartialEq)]
pub(super) struct FsDegrade {
    /// A generous-read base (`default_effect == Allow`, OR a whole-fs `**` Allow entry
    /// — the shape the compiler actually emits for `sandbox: true`). The
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
/// The deny-shadowing check runs against these policy-derived subtree grants before the
/// program-file grant is added; an unrepresentable nested deny is rejected, not degraded.
///
/// Consults the real filesystem, unlike the pure carve above: whether a grant whose source
/// is MISSING survives depends on its [`FsOrigin`], the same split the Linux bind plan makes
/// (see the arm inside).
///
/// `pub(super)` so the dedicated-account backend can reuse the same derivation for its
/// own grant/deny plan rather than restating it.
///
/// A STRUCT rather than a tuple because `publishable` is a SUBSET of `read` rather than a
/// fourth independent list, and a bare 4-tuple gives a reader no way to see that.
pub(super) struct DerivedGrants {
    pub(super) read: Vec<PathBuf>,
    pub(super) write: Vec<PathBuf>,
    /// The subset of `read` marked [`FsOrigin::NubOwnedPublic`] — nub's own public caches,
    /// which a backend may satisfy with one persistent machine-wide read instead of an ACE
    /// minted and revoked per launch. Still present in `read`: publishing is an OPTIMISATION
    /// the launch path may decline, never a substitute for the grant.
    ///
    /// Read only by the AppContainer launch path, so a non-Windows build derives it and never
    /// consults it — the derivation stays compiled everywhere on purpose, so a change to it is
    /// type-checked on the dev host rather than only in CI's Windows leg.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(super) publishable: Vec<PathBuf>,
    pub(super) degrade: FsDegrade,
}

pub(super) fn derive_grants(fs: &FsPolicy) -> DerivedGrants {
    let mut read = Vec::new();
    let mut write = Vec::new();
    let mut publishable = Vec::new();
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
                // An ACE can only be installed on a path that exists, and `set_ace` fails
                // the launch when it is not there. What that failure MEANS depends on who
                // named the path, exactly as it does for the Linux bind plan
                // (`linux_grants::compile_mount_plan`): an AUTHORED path is a specific
                // location someone named, so its absence is an authoring mistake worth
                // refusing, while a SPECULATIVE one is a guess across ecosystems and
                // layouts that is absent on most machines by construction. Windows was
                // dropping `FsOrigin` on the floor here and refusing both, which made the
                // build jail — whose project and PM-cache roots are speculated — unable to
                // launch at all whenever one of them had yet to be created. Skipping opens
                // no hole: a path that does not exist grants nothing, and an authored rule
                // naming the same path still pushes it and still fails hard.
                if rule.origin.tolerates_absent() && !dir.exists() {
                    continue;
                }
                if !read.contains(&dir) {
                    read.push(dir.clone());
                }
                // A subtree nub OWNS and that holds only public bytes can be satisfied by a
                // persistent machine-wide read instead of an ACE minted and destroyed every
                // launch. Recorded here, where the origin is still in hand — `derive_grants`
                // otherwise returns bare paths and the distinction is gone. See
                // [`FsOrigin::NubOwnedPublic`] for the measured cost this avoids.
                if rule.origin == FsOrigin::NubOwnedPublic && !publishable.contains(&dir) {
                    publishable.push(dir.clone());
                }
                if rule.access == FsAccess::ReadWrite
                    && !is_dangerous_write_root(&dir)
                    && !write.contains(&dir)
                {
                    write.push(dir);
                }
            }
            // A whole-fs `**` Allow is the generous-read base (what the compiler emits
            // for `sandbox: true` alongside a Deny base) — the allowlist can't
            // express it, so degrade and confine to the explicit allow-set. A NON-whole-
            // fs embedded glob is a distinct over-confinement (skipped, not widened).
            None if is_whole_fs(rule.matcher.as_str()) => degrade.generous_read = true,
            None if has_glob_meta(rule.matcher.as_str()) => degrade.glob_read_unenforced = true,
            None => {}
        }
    }
    DerivedGrants {
        read,
        write,
        publishable,
        degrade,
    }
}

/// Whether any read DENY could match a path inside a granted read subtree — an
/// inheritable read-allow on the grant DEFEATS such a deny on Windows (the same class
/// of trap the AAP denylist hits), so it cannot be carved and must be rejected. The
/// rule is sound and conservative: a depth-independent glob deny (`**/.env`) shadows
/// EVERY grant, and a deny whose literal prefix is inside a grant (or vice-versa)
/// shadows it. Matching is case-insensitive (Windows paths are). Run against the
/// policy-derived SUBTREE grants only — the caller excludes the program-file grant (a
/// single leaf with no subtree, an exec necessity), which cannot host a deny "inside" it.
pub(super) fn deny_shadows_grant(entries: &[FsRule], read_grants: &[PathBuf]) -> bool {
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
pub(super) fn has_glob_meta(glob: &str) -> bool {
    glob.contains(['*', '?', '[', ']', '{', '}'])
}

/// Whether a glob addresses the whole filesystem (the generous-read base spellings).
pub(super) fn is_whole_fs(glob: &str) -> bool {
    matches!(glob, "**" | "/**" | "/")
}

/// The literal directory subtree a matcher grants, or `None` if it can't be expressed
/// as one inheritable ACE. A plain absolute literal, or a literal + trailing `/**`
/// subtree twin, yields that directory; anything with embedded globs (or the whole-fs
/// spellings) yields `None`. Mirrors the macOS backend's `to_match_term` subpath case.
pub(super) fn literal_subtree(glob: &str) -> Option<PathBuf> {
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
pub(super) fn is_dangerous_write_root(dir: &Path) -> bool {
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

/// The child command for a launch that takes no LowBox token — the relaxed case and the
/// build jail's full-disk tier.
///
/// The env axis is still ENFORCED here, which is the half worth stating: it is carried by
/// constructing the child's environment rather than by the token, so declining the
/// AppContainer costs the fs and net axes and nothing else. `env_clear` first, so the
/// constructed map is the whole environment and an ambient credential cannot survive by
/// simply not being named.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn plain_command(
    policy: &crate::policy::SandboxPolicy,
    spec: super::CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<&std::path::Path>,
    tmp_dir: Option<&std::path::Path>,
) -> std::process::Command {
    let mut command = std::process::Command::new(&spec.program);
    spec.args.apply_to(&mut command);
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
    command
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

/// Drop the `\\?\` prefix `std::fs::canonicalize` puts on a Windows path, when the result
/// is still a plain drive path a normal API accepts.
///
/// `\\?\UNC\server\share` is left ALONE: its non-verbatim spelling is `\\server\share`,
/// a genuine network path, and rewriting it would change which host is addressed. A real
/// network working directory is not something cmd.exe supports anyway, so stripping there
/// would trade one failure for a less obvious one.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
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

    let mut spec = spec;

    let confine_fs = fs_confines(&policy.fs);
    let sandboxing = confine_fs || policy.net.enforce;
    let tmp_lost = super::tmp_lost_axis(policy);

    // Which of the two Windows backends owns this launch. Decided BEFORE the working-root
    // work below because the clean-DACL precondition is an AppContainer-model requirement
    // that the dedicated-account route does not share; the dispatch itself happens after.
    let account_route = sandboxing
        && super::windows_account::needs_account_backend(policy)
        && super::windows_account::is_provisioned();

    // Derived HERE rather than beside its other consumers below because `verify_clean_root`
    // needs `publishable` — the subtrees nub publishes to `ALL APPLICATION PACKAGES` — to tell
    // its own ace from a foreign one. Pure over the policy apart from an `exists()` per rule,
    // so the paths that return before the launch plan pay nothing that matters.
    let derived = derive_grants(&policy.fs);

    if confine_fs {
        let Some(cwd) = spec.cwd.as_deref() else {
            return Err(Degradation {
                lost: vec!["fs-root".to_string()],
                reason: Some(
                    "Windows filesystem confinement requires an explicit working directory"
                        .to_string(),
                ),
            });
        };
        // Resolve once against the apply-time parent cwd, then use the same absolute
        // directory for both DACL preflight and the eventual CreateProcessW launch.
        // Otherwise `work` is inspected as a one-component lexical path (never reaching
        // its protected ancestor), and a later ambient-cwd change can launch elsewhere.
        let effective_cwd = std::fs::canonicalize(cwd).map_err(|error| Degradation {
            lost: vec!["process-cwd".to_string()],
            reason: Some(format!(
                "resolving sandbox working directory {}: {error}",
                cwd.display()
            )),
        })?;
        // The dedicated-account route is exempt: its child is a separate local principal,
        // never an AppContainer, so no `ALL APPLICATION PACKAGES` grant can widen it and no
        // clean root is required (see `windows_account`). Previously this ran ahead of that
        // dispatch, so `--sandbox-admin setup` could not rescue a host it had no reason to
        // fail on.
        if !account_route
            && let Err(error) = launch::timed("verify_clean_root", || {
                launch::verify_clean_root(&effective_cwd, &derived.publishable)
            })
        {
            return Err(Degradation {
                lost: vec!["fs-root".to_string()],
                reason: Some(format!(
                    "Windows filesystem confinement requires a working root that no \
                     AppContainer can already reach: {error}"
                )),
            });
        }
        // The DACL checks above want the canonical form; the CHILD must not receive it.
        // `canonicalize` returns an extended-length `\\?\C:\…` path, and cmd.exe rejects
        // one as a working directory — it prints "UNC paths are not supported" and silently
        // runs in the Windows directory instead. Every dependency lifecycle script on
        // Windows is a cmd.exe invocation, so handing the verbatim form through meant each
        // one started in the wrong directory and could not find its own package's files.
        spec.cwd = Some(strip_verbatim_prefix(effective_cwd));
    }

    // ── the catalog's full-disk tier ─────────────────────────────────────────────
    // A build-jail policy whose fs axis confines NOTHING is what a `fullDisk` catalog grant
    // compiles to, and Windows is the platform that cannot render it inside the sandbox.
    //
    // THERE IS NO CHEAP ACE FOR "EVERYTHING", and no expensive one either. A LowBox token
    // reaches an object only where that object's own ACL names its AppContainer SID, so a
    // whole-disk grant means an ACE on each drive root — which `is_dangerous_write_root`
    // refuses outright (an inheritable modify ACE on `C:\` is a filesystem-wide write hole
    // for the SID it names, outliving this launch's teardown if anything goes wrong — one
    // derivable per-run container, NOT every AppContainer on the machine, since nub never
    // grants `ALL APPLICATION PACKAGES`; see this module's doc), and which `set_ace` would
    // pay for by re-propagating inheritance across
    // the entire volume on a launch whose ACEs are written and revoked EVERY TIME. Nor does
    // the non-propagating variant help: Windows inheritance is static, copied into a child's
    // DACL when the child is created, so an inheritable ACE written without propagation
    // grants nothing to a single file that already exists. The cheapest correct form is
    // therefore not an ACE at all — it is not taking the LowBox token, which costs zero.
    //
    // WHAT THAT COSTS, and the loss is the OS-LEVEL half only. Egress is an AppContainer
    // CAPABILITY here (`internetClient`), so declining the token declines OS egress
    // confinement with it. What survives is the USERLAND gate: `net_gate_shim.js` rides
    // `NODE_OPTIONS`, which this path preserves by construction — `plain_command` replays
    // `policy.env.constructed`, and the env allowlist admits `NODE_OPTIONS` on Windows
    // precisely because the jail stamps it (`build_jail_env_allowed`). So a full-disk package
    // the catalog does not admit to the network still has its `net`/`dns`/`dgram` and
    // `child_process` seams patched. TRACED, NOT MEASURED: no one has executed this path on
    // Windows, and the stamp is skipped outright when the interpreter predates `--import`
    // (Node 20.6), which leaves no gate at all.
    //
    // THE RESIDUAL IS THE MODAL PACKAGE HERE, not an edge case, and that is why the loss is
    // still reported rather than talked down. The gate is userland: a native addon opening a
    // raw socket walks past it, and full-disk is overwhelmingly what native-addon and
    // download-a-binary packages ask for. The proxy blackhole below covers the one case the
    // preload can never reach — a non-Node top-level lifecycle script — opportunistically, on
    // the same terms the shim states: additive, not a boundary.
    //
    // The env axis is unaffected — it is enforced by constructing the child's environment,
    // which needs no token — so the credential scrub and the `HOME` redirect still hold.
    if policy.build_jail && !confine_fs {
        let mut deg = Degradation::full();
        let mut command = plain_command(policy, spec, proxy_port, proxy_token, ca_bundle, tmp_dir);
        if policy.net.enforce {
            deg.lost.push("net".to_string());
            deg.reason = Some(
                "a full-disk build-jail grant cannot run inside an AppContainer on Windows \
                 (the allowlist has no spelling for the whole filesystem), and egress is an \
                 AppContainer capability — so this package's network access is not confined \
                 by the OS. nub's userland gate still applies inside Node, but it does not \
                 stop a native addon opening a raw socket"
                    .to_string(),
            );
            // ⛔ GATED ON THE NET AXIS, WHICH IS INVERTED FROM THE OBVIOUS READING. A coarse
            // ALLOW compiles to `enforce == false` (see `preset::build_jail_net`) — it is the
            // only spelling that reaches `internetClient` — so `enforce` is true exactly when
            // the package is DENIED egress. Every catalogued full-disk cell is network-allowed
            // today, so blackholing unconditionally here would break all of them.
            if proxy_port.is_none() {
                super::set_proxy_blackhole(&mut command);
            }
        }
        if let Some(axis) = tmp_lost {
            deg.lost.push(axis.to_string());
        }
        return Ok(Prepared {
            command,
            degradation: deg,
            proxy: None,
            launch: None,
            _private_tmp: None,
            redact_stdout: false,
            redact_stderr: false,
        });
    }

    // ── agent-sandbox route (dedicated account + WFP) ────────────────────────────
    // A policy the ALLOWLIST cannot carry — a generous-read base, a deny that must be carved
    // inside a grant, or per-host egress — goes to the dedicated-account backend, which
    // expresses all three but costs a one-time elevated setup. build-jail's shape (pure
    // default-deny allowlist, coarse or absent net) never matches, so `nub install` stays
    // admin-free. See `windows_account`'s module doc for why the split falls exactly here.
    // PROVISIONING IS PART OF THE PREDICATE, not just a precondition checked later. Without
    // it this branch subsumes the whole strict-Windows tier below — `Tier1`/`FailUnelevated`
    // require exactly `net.enforce && any(Allow)`, which is byte-identical to
    // `needs_account_backend`'s per-host arm and also forces `sandboxing` — so an
    // unprovisioned machine would take the account route, fail closed, and leave that tier
    // unreachable. Falling through instead keeps the AppContainer tier live and degrading
    // honestly (over-confined reads, a reported deny it cannot carve), which is strictly
    // better than refusing to run on a machine that never opted into the elevated setup.
    if account_route {
        return super::windows_account::apply(policy, spec, proxy_port, proxy_token, ca_bundle);
    }

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
        return Ok(Prepared {
            command: plain_command(policy, spec, proxy_port, proxy_token, ca_bundle, tmp_dir),
            degradation: Degradation::full(),
            proxy: None,
            launch: None,
            _private_tmp: None,
            redact_stdout: false,
            redact_stderr: false,
        });
    }

    let read_grants = derived.read;
    let write_grants = derived.write;
    let publishable_grants = derived.publishable;
    let fs_degrade = derived.degrade;

    // The deny-shadow rejection is judged against the POLICY-derived subtree grants
    // ONLY — captured before the program file is folded in below. The program-file grant
    // is a single leaf with no subtree and is an exec necessity, so no user data-policy
    // deny can "land inside" it; including it would spuriously flag `fs-read-deny` whenever
    // the program merely lives under a deny'd dir.
    let policy_read_grants = read_grants.clone();

    // An inheritable read allow wins over a deny nested beneath it. This is not a
    // reduced-mode policy: returning Prepared would hand direct embedders a launchable
    // plan with broader read access than requested, so reject it before any launch plan
    // or filesystem ACE can be produced.
    if deny_shadows_grant(&policy.fs.rules.entries, &policy_read_grants) {
        return Err(Degradation {
            lost: vec!["fs-read-deny".to_string()],
            reason: Some(
                "a read deny landing inside a granted subtree can't be carved on Windows \
                 (inheritable allow wins); the policy was rejected before launch"
                    .to_string(),
            ),
        });
    }

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
    // NOT reported for the build jail, whose coarse-allow IS its contract. A `nub sandbox` scope
    // that authored `net: true` asked for full host networking and got less, which is a real
    // shortfall; a catalogued dependency asked for "may reach the network" and got exactly that,
    // so a per-spawn "reduced mode" line on every one of the 181 granted packages would assert
    // something false at install-time volume. `compiler::preset::build_jail_net` is what routes
    // an admitted package here, and its doc records why this spelling is the only one Windows'
    // unprivileged lever accepts.
    if net_plan == WinNetPlan::Unconfined && !policy.build_jail {
        deg.lost.push("net-full".to_string());
        reason.get_or_insert_with(|| {
            "AppContainer internetClient permits public outbound connections but does not \
             provide full host networking: AppContainer loopback destinations remain restricted"
                .to_string()
        });
    }
    // Net per-host / MITM is NOT a degradation here: an unelevated per-host config already
    // returned the informative fail-closed above, and an elevated one (`tier1`) ENFORCES
    // via the loopback exemption registered in `run()` — so there is nothing to report lost.
    // Coarse deny-all is fully honored with no proxy. (A read deny shadowed by a grant is
    // REJECTED above rather than degraded, so no `fs-read-deny` loss is reported here.)
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

    let launch = AppContainerLaunch {
        program: spec.program,
        args: spec.args,
        cwd: spec.cwd,
        read_grants,
        write_grants,
        publishable_grants,
        env: build_child_env(&policy.env, tier1, proxy_port, proxy_token, ca_bundle),
        // Grant internetClient only when net is unconfined; an enforced net (coarse deny
        // OR Tier 1) withholds it. For Tier 1 this is LOAD-BEARING: the loopback exemption
        // opens loopback but withholding internetClient keeps external egress blocked, so
        // nub's proxy is the child's SOLE egress (matches mac/linux `remote ip localhost`).
        // The unconfined case is reported as less than full host networking above.
        allow_internet: !policy.net.enforce,
        register_loopback_exemption: tier1,
    };

    // The `command` field is unused on the launch path (status() runs `launch`); it
    // holds a benign never-spawned placeholder so the struct stays uniform.
    Ok(Prepared {
        command: std::process::Command::new(&launch.program),
        degradation: deg,
        proxy: None,
        launch: Some(WindowsLaunch::AppContainer(launch)),
        _private_tmp: None,
        redact_stdout: false,
        redact_stderr: false,
    })
}

/// The child's env block, or `None` to inherit the ambient env untouched.
///
/// - env enforced ⇒ start from the constructed scrub map; else (Tier 1 only) snapshot the
///   ambient env so the proxy/CA overrides ride an otherwise-inherited environment — the
///   Windows launch block is all-or-nothing, unlike a mac/linux `Command`'s inherit+override,
///   so "inherit + override" must be materialized here (a non-Unicode var is lossily kept).
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
    if !env.enforce && !tier1 {
        return None;
    }
    let mut m = if env.enforce {
        env.constructed.clone()
    } else {
        std::env::vars_os()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect()
    };
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
pub(super) fn resolve_program(
    program: &std::ffi::OsStr,
    child_cwd: Option<&Path>,
) -> Option<PathBuf> {
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

/// Place (`grant`) or remove the ancestor repair's non-inherited traverse ace on `dir` for
/// `sddl`, so the probe can TIME the real writer against its own copy of the propagating one —
/// same trustee, same path, same run. Nothing else attributes a cost difference to the primitive
/// rather than to the machine, and the cost is the entire claim.
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub fn windows_object_traverse_ace(
    dir: &std::path::Path,
    sddl: &str,
    grant: bool,
) -> std::io::Result<()> {
    launch::object_traverse_ace(dir, sddl, grant)
}

/// Whether `dir` already publishes read+execute to every AppContainer inheritably, i.e. whether
/// a leaf read grant on it is a no-op. Which paths do is a property of the MACHINE's default
/// ACLs, so the probe reports it rather than asserting it.
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub fn windows_leaf_grant_redundant(dir: &std::path::Path) -> bool {
    launch::leaf_read_grant_redundant(dir)
}

/// Publish `dir` to every AppContainer as read+execute, inheritably — the ONE grant an embedder
/// writes AHEAD of a launch rather than per-run, and the reason a nub-owned interpreter copy costs
/// nothing at spawn time.
///
/// CALL THIS ON AN EMPTY DIRECTORY, THEN POPULATE IT. The ace is inheritable, so children pick it
/// up AT CREATION and there is no propagation pass; writing the same ace over an already-populated
/// tree is a walk, and the two are not close (measured on `windows-latest`: 24 ms on an empty
/// directory against 426 ms re-granting a 2,435-entry Node distribution — run 30517506683). The
/// per-launch saving is the same number again: an inheritable AAP ace is exactly what
/// [`windows_leaf_grant_redundant`] looks for, so the backend's own leaf grant on this directory
/// SKIPS, and a per-run package sid — which would have to be written every spawn — is never needed.
///
/// The trustee is the STABLE `ALL APPLICATION PACKAGES` rather than a per-run profile sid, and that
/// is sound because a zero-capability LowBox token reads through it (measured — it is why System32
/// is readable at all). What it costs is that the directory becomes readable to every AppContainer
/// on the machine, so an embedder may only publish a tree whose contents are already public: the
/// intended one is a copy of the user's own Node distribution, which is public bytes from
/// nodejs.org.
///
/// Needs no elevation on any path a user owns, which is the whole point — it is the escape from
/// writing a DACL somewhere a standard user cannot (`%ProgramFiles%\nodejs`, `C:\hostedtoolcache`),
/// measured as `PrivilegeNotHeldException` there and as a clean write plus read-back under a
/// restricted token on nub's own directory.
#[cfg(target_os = "windows")]
pub fn windows_publish_appcontainer_read(dir: &std::path::Path) -> std::io::Result<()> {
    launch::publish_appcontainer_read(dir)
}

// ── the FFI launcher ────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub(super) mod launch {
    use super::{AppContainerLaunch, dedupe_windows_env_pairs};
    use std::io;
    use std::io::Write as _;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::ExitStatus;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
        SetHandleInformation, WAIT_OBJECT_0,
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
        GetSecurityDescriptorControl, GetTokenInformation, OBJECT_INHERIT_ACE,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
        TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Console::{CONSOLE_MODE, GetConsoleMode};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapFree};
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateMutexW,
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetCurrentProcess, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
        OpenProcess, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, ReleaseMutex, ResumeThread, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    // Generic access rights (avoid a Storage_FileSystem feature dep for FILE_GENERIC_*).
    // `SYNCHRONIZE` is local for the same reason: the `Win32::Foundation` surface this crate enables
    // does not re-export it, and widening the feature set for one standard-rights bit buys nothing. It
    // is needed on a tracked process handle so `drain_job_and_status` can ask whether that handle is
    // SIGNALED before trusting its exit code.
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_EXECUTE: u32 = 0x2000_0000;
    const DELETE: u32 = 0x0001_0000;
    // ACE_FLAGS: applies to this object only (no inheritance) — reached only by the
    // REVOKE_ACCESS teardown, which matches purely on the trustee and ignores inheritance.
    const NO_INHERITANCE: u32 = 0x0;
    // SE_GROUP_ENABLED — a capability SID in SECURITY_CAPABILITIES must be enabled.
    const SE_GROUP_ENABLED: u32 = 0x4;
    // SYNCHRONIZE | FILE_READ_ATTRIBUTES | FILE_TRAVERSE | FILE_LIST_DIRECTORY — byte-identical
    // to the mask Windows itself puts on `C:\` for its capability SID, so the two halves of the
    // ancestor repair grant the same thing and a path's reachability does not depend on which
    // half covered it. It admits ENUMERATING an ancestor (names only); reading anything under
    // one still requires that object's own grant, and paired with NO_INHERITANCE the reach
    // stops at the directory object.
    //
    // ⛔ THAT ENUMERATION RIGHT IS A REAL, WINDOWS-ONLY RESIDUAL, AND IT IS NOT SYMMETRIC WITH THE
    // OTHER TWO BACKENDS — which is the part this comment used to leave unsaid. MEASURED 2026-08-05
    // by the adversarial probe (corpus run 30968614273, same commit and same fixture on all three):
    //
    //     linux   list the real $HOME -> BLOCKED EACCES      (Landlock)
    //     macos   list the real $HOME -> BLOCKED EPERM       (Seatbelt)
    //     win32   list the real $HOME -> ALLOWED, 42 entries enumerated
    //
    // On the same run every CREDENTIAL READ (~/.npmrc, ~/.gitconfig, ~/.aws/credentials,
    // ~/.ssh/id_rsa) and the persistence WRITE were EPERM-denied on Windows too, with a jail-off
    // control proving all six succeed unjailed. So the exfiltration claim holds on every backend and
    // only the RECON layer differs: an attacker learns WHICH tools and configs exist (`.aws` implies
    // an AWS user, `.ssh` implies keys worth targeting elsewhere) without reading any of them.
    //
    // Kept deliberately. The mask is byte-identical to what Windows itself puts on `C:\` for its
    // capability SID, and the ancestor repair needs traverse+list for the child to REACH the paths it
    // is granted — the same reachability the window-station ACE and the profile-dir fixes exist to
    // preserve. Narrowing it to close a names-only leak would risk breaking that.
    //
    // ⛔ THE CONSEQUENCE FOR USER-FACING COPY, since this is where the asymmetry is decided: "the
    // build jail blocks a Shai-Hulud-style credential steal" is TRUE on all three platforms. "A
    // jailed install script cannot see your home directory" is true on macOS and Linux and FALSE on
    // Windows. Never ship the second claim unqualified.
    const TRAVERSE_MASK: u32 = 0x0010_00a1;
    // The well-known internetClient capability SID.
    const INTERNET_CLIENT_SID: &str = "S-1-15-3-1";
    // ALL APPLICATION PACKAGES. Any right for this SID invalidates the default-deny
    // AppContainer assumption for that path.
    const ALL_APPLICATION_PACKAGES_SID: &str = "S-1-15-2-1";

    /// Monotonic per-process counter so concurrent launches never collide on the
    /// AppContainer profile name (combined with pid + a time nonce).
    static LAUNCH_CTR: AtomicU64 = AtomicU64::new(0);

    /// Serializes the per-path DACL read-modify-write in [`set_ace`]. Concurrent launches
    /// can grant/revoke on a SHARED leaf (two runs granting a common toolchain/program
    /// dir); without this, two non-atomic RMWs race and one run's ACE is lost (its grant
    /// then missing). A single global lock is ample — ACL edits are brief and rare.
    static ACL_LOCK: Mutex<()> = Mutex::new(());

    /// Verify that `cwd` is rooted beneath a protected DACL and that neither it nor any
    /// ancestor up to that boundary grants ALL APPLICATION PACKAGES access. The LowBox
    /// allowlist is default-deny only under this precondition; inherited AAP access would
    /// otherwise let the child reach files nub never granted.
    ///
    /// `published` is [`AppContainerLaunch::publishable_grants`] — the subtrees nub ITSELF
    /// publishes to AAP, and the reason this takes an argument at all. The predicate's premise
    /// is "AAP reach ⇒ access nub never granted", and inside one of those subtrees the premise
    /// is FALSE BY CONSTRUCTION: the ace is nub's own, written to satisfy a read grant the child
    /// already holds on that very subtree, so read-execute reach there IS the grant rather than
    /// a hole. Without the exemption nub's own optimisation makes its own precondition
    /// unsatisfiable — a native addon that builds IN PLACE has its cwd inside the published PM
    /// store, so the install refused outright (measured on Windows Server 2022: 6 of 86 corpus
    /// records, via `unix-dgram@2.0.7` and `ref@1.3.5`).
    ///
    /// THE EXEMPTION IS BOUNDED BY RIGHTS, NOT BY LOCATION. Only the bits
    /// [`publish_appcontainer_read`] itself writes are excused; an AAP ace inside a published
    /// subtree carrying WRITE, DELETE or full control is not nub's and still refuses, as does
    /// any AAP ace outside one. A genuinely dirty root therefore fails closed exactly as before
    /// — the posture 5c8d168833 settled on when it rejected re-authoring the user's DACL and
    /// corrected the predicate instead.
    pub(super) fn verify_clean_root(cwd: &Path, published: &[PathBuf]) -> io::Result<()> {
        // Canonicalized ONCE, outside the ancestor walk, into the same `\\?\`-verbatim form the
        // caller resolved `cwd` into — a raw policy path (`C:\…`) never component-matches a
        // canonical one. An unresolvable entry drops out, which excuses nothing: fail-closed.
        let published: Vec<PathBuf> = published
            .iter()
            .filter_map(|dir| std::fs::canonicalize(dir).ok())
            .collect();
        let publishes = file_specific_rights(GENERIC_READ | GENERIC_EXECUTE);
        let sid_text = to_wide(ALL_APPLICATION_PACKAGES_SID);
        let mut aap_sid: PSID = std::ptr::null_mut();
        if unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut aap_sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _sid = LocalFreeGuard(aap_sid.cast());

        for path in cwd.ancestors() {
            let wpath = to_wide_path(path);
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = unsafe {
                GetNamedSecurityInfoW(
                    wpath.as_ptr(),
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
                return Err(io::Error::other(format!(
                    "could not inspect {}: {}",
                    path.display(),
                    io::Error::from_raw_os_error(rc as i32)
                )));
            }
            let _sd = LocalFreeGuard(sd);
            if dacl.is_null() {
                return Err(io::Error::other(format!(
                    "{} has a null DACL",
                    path.display()
                )));
            }

            // On the WORKING ROOT any AAP grant is disqualifying — one that applies to the
            // directory object, and equally one that is merely INHERITABLE, since the child
            // CREATES files here and each would copy that ace. On a STRICT ANCESTOR only an
            // INHERITABLE ace matters: a this-folder-only grant governs that directory object
            // alone and can never reach the tree the child runs in.
            //
            // Inside a subtree nub publishes, the rights nub publishes are excused and every
            // other bit still disqualifies (see the fn doc). `!0` outside one keeps the
            // unpublished case bit-identical to the plain "any ace at all" test.
            let disqualifying = if published
                .iter()
                .any(|root| super::path_prefixes(root, path))
            {
                !publishes
            } else {
                !0
            };
            let on_object = aap_rights_on_object(dacl, aap_sid, path)? & disqualifying;
            let inheritable = inheritable_grant_rights(dacl, aap_sid, path)? & disqualifying;
            if (path == cwd && on_object != 0) || inheritable != 0 {
                return Err(io::Error::other(format!(
                    "{} grants ALL APPLICATION PACKAGES access",
                    path.display()
                )));
            }

            let mut control = 0u16;
            let mut revision = 0u32;
            if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
                return Err(io::Error::other(format!(
                    "could not read DACL control flags on {}: {}",
                    path.display(),
                    io::Error::last_os_error()
                )));
            }
            // A protected DACL is an EARLY ACCEPT, not a requirement: nothing above it can
            // propagate in, so the ancestors beyond it cannot affect the working root.
            if control & SE_DACL_PROTECTED != 0 {
                return Ok(());
            }
        }

        Ok(())
    }

    /// The FILE-object form of the generic rights the leaf grants are expressed in. Windows
    /// applies this mapping itself when it evaluates an ace, and an effective-rights query
    /// reports the RESULT — so a comparison against a generic mask has to map first or every
    /// answer comes back "not granted". Bits that are already specific (`DELETE`) pass through.
    fn file_specific_rights(generic: u32) -> u32 {
        const FILE_GENERIC_READ: u32 = 0x0012_0089;
        const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
        const FILE_GENERIC_EXECUTE: u32 = 0x0012_00a0;
        const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
        const GENERIC_ALL: u32 = 0x1000_0000;
        let mut out = generic & !(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
        if generic & GENERIC_READ != 0 {
            out |= FILE_GENERIC_READ;
        }
        if generic & GENERIC_WRITE != 0 {
            out |= FILE_GENERIC_WRITE;
        }
        if generic & GENERIC_EXECUTE != 0 {
            out |= FILE_GENERIC_EXECUTE;
        }
        // `GA` subsumes the other three. Leaving it unmapped made an existing
        // `AAP:(OI)(CI)GA` ace fail the redundancy comparison in
        // `already_granted_to_appcontainers`, so nub re-paid the propagating write on a
        // directory that already published everything. It was never a correctness risk for
        // `verify_clean_root` — an unmapped `GA` bit is still non-zero, so such a root is
        // refused either way — but it is the same class of mistake as the one above.
        if generic & GENERIC_ALL != 0 {
            out |= FILE_ALL_ACCESS;
        }
        out
    }

    /// Whether `path` ALREADY grants every right in `access` to AppContainers generally, through
    /// an INHERITABLE ace — i.e. whether the ace [`grant_leaf_ace`] is about to write would
    /// change nothing.
    ///
    /// This is worth a DACL read because the WRITE is the expensive half. An inheritable grant
    /// legitimately propagates through the subtree, and a populated toolchain tree makes that
    /// cost real: measured on windows-latest, granting the runner's `hostedtoolcache` python took
    /// ~1000 ms against 3 ms on an empty directory, and a re-grant with the ace already present
    /// cost the same as a fresh one — the signature of a tree walk, not a descriptor write.
    /// Narrowing the grant is not an alternative: `Lib\` at 6,412 entries IS the tree, and a
    /// narrow grant fails `0xc0000135 STATUS_DLL_NOT_FOUND` because `python3.dll`,
    /// `python312.dll` and `vcruntime140*.dll` sit in the install ROOT beside the exe.
    ///
    /// It applies broadly, not just to python: `%ProgramFiles%` carries
    /// `ALL APPLICATION PACKAGES: ReadAndExecute` inheritably on both Windows images (43 of the
    /// 44 `C:\Program Files` children; `nodejs` is the known outlier), so an all-users python,
    /// node, or Visual Studio install needs no grant at all. Only per-user layouts pay, which is
    /// why `hostedtoolcache` — carrying none — is the one that measured.
    ///
    /// INHERITABLE is required rather than incidental: the ace being skipped covers the whole
    /// subtree, so a this-directory-only AAP ace does not substitute for it. Same distinction
    /// `verify_clean_root` draws above, for the same reason. Any failure to read the DACL answers
    /// "no" and the grant is written — the skip is an optimisation and must never be the reason a
    /// package cannot start.
    ///
    /// No conflict with `verify_clean_root`'s refusal to launch under an AAP-readable root: that
    /// governs the working root's own chain, this governs granted paths OUTSIDE it. A toolchain
    /// the OS already publishes to every AppContainer is not access nub is adding.
    fn already_granted_to_appcontainers(path: &Path, access: u32) -> bool {
        let sid_text = to_wide(ALL_APPLICATION_PACKAGES_SID);
        let mut aap_sid: PSID = std::ptr::null_mut();
        if unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut aap_sid) } == 0 {
            return false;
        }
        let _sid = LocalFreeGuard(aap_sid.cast());

        let wpath = to_wide_path(path);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wpath.as_ptr(),
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
            return false;
        }
        let _sd = LocalFreeGuard(sd);
        if dacl.is_null() {
            return false;
        }
        // Only an INHERITABLE grant makes the ace redundant, so the walk ignores
        // this-directory-only aces — same distinction `verify_clean_root` draws, for the same
        // reason. This used to ask `GetEffectiveRightsFromAclW`, which returns
        // `ERROR_INVALID_ACL` on ordinary DACLs (see `for_each_ace_of_sid`); that answered
        // "not granted" on exactly the machines this optimisation exists for, so every launch
        // there re-paid the propagating write it is meant to skip.
        let needed = file_specific_rights(access);
        let mut allowed = 0u32;
        let mut denied = 0u32;
        let walked = for_each_ace_of_sid(dacl, aap_sid, path, |is_allow, flags, mask| {
            if flags & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) == 0 {
                return;
            }
            let mask = file_specific_rights(mask);
            if is_allow {
                allowed |= mask & !denied;
            } else {
                denied |= mask & !allowed;
            }
        });
        // Any failure to read the DACL answers "no" and the grant is written — the skip is an
        // optimisation and must never be the reason a package cannot start.
        walked.is_ok() && allowed & needed == needed
    }

    /// Install one leaf allow-ace, reporting whether an ace was actually written.
    ///
    /// The return value is the ONLY thing the teardown list is driven from, which is what keeps
    /// grant and revoke symmetric BY CONSTRUCTION: a skipped path is never recorded, so the
    /// teardown cannot strip an ace this launch did not create. Two independent conditionals
    /// would make that a coincidence instead of a property — and stripping
    /// `ALL APPLICATION PACKAGES` off `%ProgramFiles%\nodejs` would be a lasting change to the
    /// user's own machine.
    fn grant_leaf_ace(path: &Path, sid: PSID, access: u32) -> io::Result<bool> {
        if already_granted_to_appcontainers(path, access) {
            return Ok(false);
        }
        set_ace(path, sid, access, GRANT_ACCESS, true).map(|()| true)
    }

    /// See [`super::windows_object_traverse_ace`].
    #[doc(hidden)]
    pub(super) fn object_traverse_ace(dir: &Path, sddl: &str, grant: bool) -> io::Result<()> {
        let sid = CapSid::new(sddl)?;
        let mode = if grant { GRANT_ACCESS } else { REVOKE_ACCESS };
        set_ace_on_object(dir, sid.0, TRAVERSE_MASK, mode)
    }

    /// See [`super::windows_leaf_grant_redundant`].
    #[doc(hidden)]
    pub(super) fn leaf_read_grant_redundant(dir: &Path) -> bool {
        already_granted_to_appcontainers(dir, GENERIC_READ | GENERIC_EXECUTE)
    }

    /// See [`super::windows_publish_appcontainer_read`].
    #[doc(hidden)]
    pub(super) fn publish_appcontainer_read(dir: &Path) -> io::Result<()> {
        let sid = CapSid::new(ALL_APPLICATION_PACKAGES_SID)?;
        set_ace(
            dir,
            sid.0,
            GENERIC_READ | GENERIC_EXECUTE,
            GRANT_ACCESS,
            true,
        )
    }

    /// Each granted path's STRICT ancestors, deduped and ordered shallowest-first. These are
    /// the directories Node's `realpathSync` opens as targets on its way to a granted leaf. A
    /// grant that is itself an ancestor of another grant is included, and simply takes the
    /// traverse ACE alongside its own inheritable one.
    ///
    /// ⛔ `container_profile` IS A LEAF FOR THE SAME REASON THE GRANTS ARE, AND LEAVING IT OUT WAS
    /// THE SINGLE LARGEST CAUSE OF WHOLE-DISK GRANTS ON WINDOWS. The child's temp lives at
    /// `<child %LOCALAPPDATA%>\Packages\<profile>\AC\Temp`, and step 1a creates that leaf and
    /// grants ACEs on it, on `AC` and on `AC\Temp` — but `create_dir_all` makes the intermediate
    /// `Packages` directory carrying NO ace for this container. Writing into temp therefore works
    /// while RESOLVING it does not: `realpath()` walks every component from the root and dies with
    /// `EPERM: lstat '…\AppData\Local\Packages'`.
    ///
    /// That is fatal far below the temp directory's own users, because `temp-dir` calls
    /// `fs.realpathSync(os.tmpdir())` AT MODULE LOAD and is transitively depended on by
    /// `tempfile` -> `download`/`decompress` -> `bin-build`/`bin-wrapper` — the whole
    /// download-a-binary family. Every rung below `write:"disk"` failed for them, and `write:"disk"`
    /// "fixed" it only because that rung declines the AppContainer token altogether, so there is no
    /// container temp redirect left to resolve. It is also why the platforms diverge so sharply:
    /// macOS and Linux have no AppContainer, hence no `Packages` component to walk.
    fn ancestor_chain(
        launch: &AppContainerLaunch,
        container_profile: Option<&Path>,
    ) -> Vec<PathBuf> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        let leaves: Vec<&Path> = launch
            .read_grants
            .iter()
            .chain(launch.write_grants.iter())
            .chain(launch.cwd.iter())
            .map(PathBuf::as_path)
            .chain(std::iter::once(Path::new(&launch.program)))
            .chain(container_profile)
            .collect();
        for leaf in leaves {
            let mut chain: Vec<&Path> = leaf.ancestors().skip(1).collect();
            chain.reverse();
            for dir in chain {
                if dir.as_os_str().is_empty() {
                    continue;
                }
                if seen.insert(dir.to_path_buf()) {
                    out.push(dir.to_path_buf());
                }
            }
        }
        out
    }

    /// Add or remove an ace on `path` WITHOUT re-propagating inheritance into its subtree.
    ///
    /// This is the whole reason the ancestor repair does not go through [`set_ace`].
    /// `SetNamedSecurityInfoW` re-applies the object's inheritable aces to every DESCENDANT
    /// whenever the DACL is rewritten — a full recursive walk, regardless of whether the ace
    /// being added inherits. On an ancestor like the user profile or a tool cache that is
    /// minutes of I/O per launch, and it wedged a 20-minute CI step. The handle-based
    /// `SetSecurityInfo` writes the object's own DACL and stops there, which is exactly the
    /// scope a non-inherited traverse grant wants.
    ///
    /// `FILE_FLAG_BACKUP_SEMANTICS` is what lets `CreateFileW` open a DIRECTORY at all.
    ///
    /// `SetSecurityInfo` WAS NOT ENOUGH EITHER, and that is why the writer below is the kernel
    /// one. Both `Set*SecurityInfo` entry points run advapi32's user-mode inheritance
    /// propagation before they return, so swapping the named writer for the handle-based one
    /// narrowed nothing: run 30493913027's watchdog pinned the remaining stall to the FIRST
    /// launch that writes these aces, and the only WRITE in that window is this function
    /// (`verify_clean_root` merely reads DACLs). The chain includes
    /// `%TEMP%`, which on a CI runner is enormous, so the walk took minutes and varied run to
    /// run. `SetKernelObjectSecurity` goes straight to `NtSetSecurityObject`: it writes the
    /// object's own descriptor and there is no propagation pass to skip. Measured on
    /// windows-latest, the `ace-cost` group of `tests/windows_jail_repairs.rs`, same trustee and
    /// same path in the same run — see that group's own comment for the numbers.
    ///
    /// The price is that it wants a whole SECURITY_DESCRIPTOR rather than a bare ACL, hence the
    /// hand-built one below. `SetEntriesInAclW` still does the MERGE — it only assembles an ACL
    /// in memory and propagates nothing; the cost was never there.
    ///
    /// SE_DACL_AUTO_INHERITED and SE_DACL_PROTECTED are carried across DELIBERATELY. A
    /// hand-built descriptor starts with a zero control word, and writing that back would clear
    /// both bits on a directory nub does not own — changing how the user's own ACL edits later
    /// propagate through their profile or temp dir. This repair is only ever allowed to add and
    /// remove one traverse ace.
    fn set_ace_on_object(path: &Path, sid: PSID, access: u32, mode: i32) -> io::Result<()> {
        use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
        use windows_sys::Win32::Security::{
            InitializeSecurityDescriptor, SE_DACL_AUTO_INHERITED, SECURITY_DESCRIPTOR,
            SetKernelObjectSecurity, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        const READ_CONTROL: u32 = 0x0002_0000;
        const WRITE_DAC: u32 = 0x0004_0000;
        const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
        const CARRIED_CONTROL: u16 = SE_DACL_AUTO_INHERITED | SE_DACL_PROTECTED;

        let _lock: MutexGuard<'_, ()> = ACL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let wpath = to_wide_path(path);
        // SAFETY: `wpath` is a NUL-terminated wide path; `HandleGuard` closes the handle.
        let handle = unsafe {
            CreateFileW(
                wpath.as_ptr(),
                READ_CONTROL | WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let _handle = HandleGuard(handle);

        let mut old_dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = unsafe {
            GetSecurityInfo(
                handle,
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
        let _sd = LocalFreeGuard(sd);

        let mut control = 0u16;
        let mut revision = 0u32;
        if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        ea.grfAccessPermissions = access;
        ea.grfAccessMode = mode;
        ea.grfInheritance = NO_INHERITANCE;
        ea.Trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: sid.cast(),
        };
        // Merges into the aces already there, INHERITED_ACE flags intact, so the descriptor
        // written below differs from the one read above by exactly this one ace.
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let rc = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let _new = LocalFreeGuard(new_dacl.cast());

        let mut fresh: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let psd: PSECURITY_DESCRIPTOR = std::ptr::from_mut(&mut fresh).cast();
        // SAFETY: `fresh` is a stack SECURITY_DESCRIPTOR that does not move; `new_dacl` outlives
        // the write (`_new` drops after it). The absolute form is what SetKernelObjectSecurity
        // documents for this pattern.
        unsafe {
            if InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) == 0
                || SetSecurityDescriptorDacl(psd, 1, new_dacl, 0) == 0
                || SetSecurityDescriptorControl(psd, CARRIED_CONTROL, control & CARRIED_CONTROL)
                    == 0
                || SetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, psd) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// ACE header types we can safely decode. An allow/deny ace — and its `_CALLBACK_`
    /// variant, which is byte-identical up to `SidStart` — puts the access mask at offset 4
    /// and the trustee sid at offset 8. The OBJECT forms interpose two GUIDs before the sid,
    /// so the same offsets would read a GUID as a sid.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
    const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
    /// The ace grants nothing on the object itself; it exists only to propagate to children.
    const INHERIT_ONLY_ACE: u32 = 0x8;

    /// Walk `dacl` and hand each decodable ace to `visit` as `(is_allow, flags, mask)`, but
    /// only for aces whose trustee is `sid`.
    ///
    /// ⛔ THIS EXISTS BECAUSE `GetEffectiveRightsFromAclW` CANNOT ANSWER THE QUESTION ON REAL
    /// MACHINES. It fails with `ERROR_INVALID_ACL` (1336) on DACLs that are perfectly legal,
    /// and a developer's `%USERPROFILE%` routinely carries one — which made the build jail
    /// refuse to run at all there (552 paths under one `%LOCALAPPDATA%\nub` on the Windows VM).
    /// MEASURED 2026-08-06 by building acls in memory one ace at a time, two independent
    /// sufficient triggers, each with a passing control:
    ///
    ///   * any DENY ace positioned AFTER an ALLOW ace — this is MSDN's documented "fails if
    ///     the acl contains an inherited access-denied ace", since an inherited deny lands
    ///     after the explicit allows;
    ///   * THREE OR MORE maximal BLOCKS of consecutive INHERITED aces — equivalently, the
    ///     count of explicit→inherited transitions plus one if the acl starts inherited.
    ///     `EIEIEI` fails while `EIEIE` passes, and `EEEIII` — the same six aces regrouped
    ///     into ONE inherited block — passes. Verified on 22 sequences, 12 of them predicted
    ///     before being run. NOT about interleaving or size: `EEEIIIEEEIII` (12 aces, 2
    ///     blocks) passes where `EEEIIIEEEIIIEEEIII` (3 blocks) fails, and `EIEIE` vs
    ///     `IEIEI` are the same length and alternation, differing only in whether the FIRST
    ///     ace is inherited. The real jail-home dacl carries 12 such blocks.
    ///
    /// REFUTED as triggers, each against a control that still passed: unresolvable
    /// AppContainer package sids (well-known sids fail identically), GENERIC rights bits,
    /// OI/CI/IO flags, acl revision, ace count alone (48 canonical aces pass), and the
    /// `\\?\` verbatim path form. So the SID-resolution weakness that looks like the obvious
    /// culprit is not the one; ordering is.
    ///
    /// A direct walk also answers a strictly narrower question than "effective rights": it
    /// does no group expansion, which is sound here because a LowBox token reaches an object
    /// only where that object's acl names an AppContainer sid — an `Everyone` ace grants an
    /// AppContainer nothing. And it can name the offending sid rather than reporting that the
    /// acl structure is invalid.
    fn for_each_ace_of_sid(
        dacl: *const ACL,
        sid: PSID,
        path: &Path,
        mut visit: impl FnMut(bool, u32, u32),
    ) -> io::Result<()> {
        use windows_sys::Win32::Security::{ACCESS_ALLOWED_ACE, ACE_HEADER, GetAce};
        // SAFETY: AceCount bounds the GetAce index; the ace types accepted below all place
        // AceFlags/Mask/SidStart at the offsets ACCESS_ALLOWED_ACE declares.
        unsafe {
            for i in 0..(*dacl).AceCount as u32 {
                let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                if GetAce(dacl, i, &mut ace) == 0 {
                    return Err(io::Error::other(format!(
                        "could not read ace {i} of {}: {}",
                        path.display(),
                        io::Error::last_os_error()
                    )));
                }
                let header = &*ace.cast::<ACE_HEADER>();
                let is_allow = match header.AceType {
                    ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => true,
                    ACCESS_DENIED_ACE_TYPE | ACCESS_DENIED_CALLBACK_ACE_TYPE => false,
                    // FAIL CLOSED. An object or vendor ace type cannot be decoded with these
                    // offsets, and guessing would silently under-report an AppContainer grant
                    // — the one error this check must never make. Refusing the root is the
                    // safe answer; audit/alarm types cannot legally appear in a DACL at all.
                    other => {
                        return Err(io::Error::other(format!(
                            "{} carries an ace of unsupported type {other}, so \
                             AppContainer reachability cannot be determined",
                            path.display()
                        )));
                    }
                };
                let ace_sid: PSID =
                    std::ptr::addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart)
                        .cast_mut()
                        .cast();
                if !sids_equal(ace_sid, sid) {
                    continue;
                }
                let mask = (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask;
                visit(is_allow, u32::from(header.AceFlags), mask);
            }
        }
        Ok(())
    }

    /// Rights `sid` holds on the directory OBJECT itself, in file-specific form. Deny aces
    /// subtract, and — matching how Windows evaluates a DACL — a deny only removes rights not
    /// already granted by an earlier allow. Inherit-only aces are skipped: they grant nothing
    /// here, which is what [`inheritable_grant_rights`] covers instead.
    ///
    /// The `& !allowed` term when accumulating denials states the first-ace-wins rule but does
    /// not change the answer, and it is worth saying so because a mutation test proves no test
    /// can defend it: `denied` is only ever read as `mask & !denied` to gate a LATER allow, so
    /// the bits it drops are exactly the ones already present in `allowed`, which re-granting
    /// cannot change. Kept because it makes the rule legible, not because it is load-bearing.
    fn aap_rights_on_object(dacl: *const ACL, sid: PSID, path: &Path) -> io::Result<u32> {
        let mut allowed = 0u32;
        let mut denied = 0u32;
        for_each_ace_of_sid(dacl, sid, path, |is_allow, flags, mask| {
            if flags & INHERIT_ONLY_ACE != 0 {
                return;
            }
            let mask = file_specific_rights(mask);
            if is_allow {
                allowed |= mask & !denied;
            } else {
                denied |= mask & !allowed;
            }
        })?;
        Ok(allowed)
    }

    /// The rights `sid` holds through an INHERITABLE grant here — the union of every allow ace
    /// flagged to propagate to children. This is the fact that decides whether a grant reaches
    /// the tree the confined child actually runs in.
    ///
    /// A MASK rather than the bool this used to return, because `verify_clean_root` now has to
    /// distinguish the read-execute nub publishes on its own caches from anything wider. Denies
    /// are deliberately not subtracted: over-reporting reach is the fail-closed direction, and
    /// an inherited deny does not reliably survive the ordering an inheriting child ends up with.
    fn inheritable_grant_rights(dacl: *const ACL, sid: PSID, path: &Path) -> io::Result<u32> {
        let mut allowed = 0u32;
        for_each_ace_of_sid(dacl, sid, path, |is_allow, flags, mask| {
            let inheritable = flags & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) != 0;
            if is_allow && inheritable {
                allowed |= file_specific_rights(mask);
            }
        })?;
        Ok(allowed)
    }

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
        let held = !h.is_null() && unsafe { WaitForSingleObject(h, 10_000) } == WAIT_OBJECT_0;
        let out = f();
        if !h.is_null() {
            if held {
                unsafe { ReleaseMutex(h) };
            }
            unsafe { CloseHandle(h) };
        }
        out
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
            // Free AFTER Set — `new_list` borrows these Sid pointers. The Get hands back
            // N+1 separate process-heap blocks (the array, plus one per entry's `Sid`);
            // MSDN's `FreeAppContainerConfig` sample is this exact loop. NOT
            // `NetworkIsolationFreeAppContainers` — despite the name that releases
            // `NetworkIsolationEnumAppContainers` output, a different element type, and the
            // `.cast()` that let it compile type-confused firewallapi into freeing a garbage
            // pointer (0xC0000374 at teardown on every elevated per-host run).
            if !arr.is_null() {
                // SAFETY: Set has consumed the Sid pointers; this is their last use. Iterate
                // `existing`, NOT `new_list` — the caller's `sid` is FreeSid/Rust-owned, so
                // freeing that here would be a wrong-allocator free plus a later double free.
                let heap = unsafe { GetProcessHeap() };
                for e in existing {
                    unsafe { HeapFree(heap, 0, e.Sid.cast()) };
                }
                unsafe { HeapFree(heap, 0, arr.cast()) };
            }
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

    impl AppContainerLaunch {
        /// Own the full spawn lifecycle: create a per-run AppContainer profile, grant
        /// the inheritable allow-ACEs, launch the child under the LowBox token inside a
        /// kill-on-close Job, wait, then tear everything down (RAII).
        pub(crate) fn run(self) -> io::Result<ExitStatus> {
            // 1. Per-run AppContainer profile → AC SID. `_profile` deletes it on drop
            //    (declared FIRST ⇒ dropped LAST, after the ACEs are revoked).
            let name = unique_profile_name();
            let ac_sid = timed("create_appcontainer", || create_appcontainer(&name))?;
            let _profile = ProfileGuard {
                name: to_wide(&name),
                sid: ac_sid,
            };
            // An owned copy of the SID bytes, so ACE revoke doesn't depend on the
            // profile-owned SID pointer surviving.
            let sid_copy = copy_sid(ac_sid)?;

            // 1b. ⛔ WINDOW STATION + DESKTOP ACE — WITHOUT IT, ANY CHILD THAT IMPORTS `USER32`
            //     DIES BEFORE `main`, AND THE JAIL LOOKS LIKE IT BROKE THE PACKAGE.
            //
            // `USER32`'s init attaches the process to a window station and desktop. A LowBox
            // token reaches neither unless its container SID is in their DACLs, and a DllMain
            // that fails is reported by the loader as `STATUS_DLL_INIT_FAILED` (0xC0000142) —
            // an exit code with nothing in it to suggest a sandbox, which is why this cost a
            // day to find. MEASURED 2026-08-04 over SSH (a non-interactive station): `node.exe`,
            // `git.exe` and `nub.exe` all died 0xC0000142 while a std-only crt-static probe and
            // System32's `hostname.exe` — neither of which imports USER32 — ran fine.
            //
            // ⛔ ON AN INTERACTIVE `WinSta0` THIS IS REDUNDANT: seclogon already auto-grants it,
            // which is why CI and ordinary desktop installs never saw the failure and only a
            // remoted/service session does. It is cheap and it makes the jail behave the same
            // way in both, so it is unconditional rather than gated on detecting the station.
            //
            // The same guard the dedicated-account backend uses, deliberately: it FAILS FORWARD
            // (a station whose DACL cannot be rewritten still launches, rather than losing a run
            // that worked before this existed) and RESTORES the prior DACL on drop.
            let _window =
                match unsafe { crate::backend::windows_account::account::sid_to_string(ac_sid) } {
                    Ok(sid_str) => Some(
                        crate::backend::windows_account::launch::WindowAceGuard::grant(&sid_str),
                    ),
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            "sandbox: could not stringify the container SID for the window-station \
                             ace — a USER32-importing child on a non-interactive station may fail \
                             loader init"
                        );
                        None
                    }
                };

            // 1a. ⛔ THE CHILD RESOLVES ITS PROFILE FROM `%LOCALAPPDATA%`; THE PARENT DOES NOT.
            //
            // `CreateAppContainerProfile` above runs HERE, unsandboxed, and Windows places the real
            // profile via the PARENT's known-folder location. But `defaults::OS_ESSENTIAL_ENV` hands
            // the CHILD a `LOCALAPPDATA` value, and the enforcing path resolves the per-container
            // profile dir from THAT. When the two disagree the child looks somewhere the profile was
            // never created and has no ACE to create it, so every launch dies before running:
            //
            //     npm error syscall mkdir
            //     npm error path ...\home\AppData\Local\Packages\nub_sbx_4412_18c8788d58963f90_0
            //
            // ⛔ NOTE THE PATH ENDS IN THE PROFILE NAME, NOT `Packages`. Pre-creating `Packages`
            // externally does NOT help — measured, run 30869760855, grants byte-identical to
            // baseline — because the leaf is `unique_profile_name()`, generated per launch. Only
            // this function knows it, which is why the fix has to live here.
            //
            // Reproduces wherever the child's `%LOCALAPPDATA%` differs from the parent's known
            // folder: redirected folders, enterprise profiles, anything that sets the var
            // explicitly. The measurement harness hits it on every single run, which is how it was
            // found — it drove ~17 packages to a whole-disk grant that they do not need.
            //
            // Creating it here is not a widening: it is one per-launch directory, named after this
            // container, carrying only this container's ACE, and removed on drop.
            let _child_profile = self
                .env
                .as_ref()
                .and_then(|e| {
                    e.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("LOCALAPPDATA"))
                })
                .map(|(_, v)| PathBuf::from(v).join("Packages").join(&name))
                .filter(|dir| !dir.exists())
                .and_then(|dir| {
                    // Best effort by design. A failure here is not fatal: when the parent's known
                    // folder DOES agree with the child's env the profile already exists (filtered
                    // out above), and if the directory cannot be made the launch fails exactly as
                    // it does today rather than differently.
                    std::fs::create_dir_all(&dir).ok()?;
                    // ⛔ `DELETE` FOR THE SAME REASON ITS `AC`/`AC\Temp` CHILDREN NEED IT, and this
                    // line is FALLOUT FROM THE COMMIT THAT ADDED IT THERE: that fix granted DELETE on
                    // the two leaves and left the PROFILE ROOT on READ|WRITE, so a file written
                    // directly here could be created and never removed. Windows governs unlink by
                    // DELETE on the FILE, where POSIX governs it by write permission on the
                    // DIRECTORY, so an inherited ACE carrying only GENERIC_READ|GENERIC_WRITE grants
                    // everything the write needs except removing it — the same EPERM-on-unlink the
                    // leaf grant produced for electron-chromedriver and playwright-chromium.
                    //
                    // ⛔ NOT MEASURED AGAINST A WITNESS, unlike the leaf fix. No corpus record is
                    // known to write a file directly into the profile ROOT rather than under `AC`,
                    // so this lands as CONSISTENCY with the write-grant mask used elsewhere in this
                    // file, NOT as a claimed fix. Do not credit it with any metric movement.
                    //
                    // ⛔ `GENERIC_EXECUTE` ADDED 2026-09-03, AND THIS ONE HAS A WITNESS. A package
                    // that stages an executable under the container profile could write it, read it
                    // and delete it, but not RUN it: measured jailed, `spawnSync` of a valid PE
                    // copied into `AC\Temp` returned EPERM, while the identical file launched from
                    // an ordinary write-granted directory. The mask now matches `self.write_grants`
                    // below, which has always carried execute. Download-then-exec installers are a
                    // large family, so an under-grant here is far worse than the over-grant.
                    let _ = grant_leaf_ace(
                        &dir,
                        ac_sid,
                        GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                    );
                    // ⛔ `AC` MUST EXIST TOO, AND CREATING THE PROFILE DIR ALONE DOES NOT MAKE IT.
                    // When Windows creates an AppContainer profile it also lays down the `AC`
                    // subtree, which is where it VIRTUALIZES the container's `%LOCALAPPDATA%`. We
                    // are making this directory by hand, so nothing creates `AC` and the child
                    // dies on a path INSIDE the profile it can otherwise reach:
                    //
                    //     Error: ENOENT: no such file or directory, lstat
                    //       '…\Packages\nub_sbx_1404_18c8868173b7d0c0_0\AC'
                    //
                    // MEASURED, run 30882019778: that is gifsicle@5.3.0's ENTIRE remaining blocker
                    // once the npm-prefix redirect removed the other one — its 56 cell logs mention
                    // `npm` zero times and carry this instead. The same shape appears one level
                    // deeper as `AC\npm-cache\_cacache\tmp\…` (impit) and `AC\Temp\…`
                    // (electron-chromedriver, playwright-chromium), so it is the family, not a case.
                    //
                    // Granted at the leaf like its parent rather than recursively: the child creates
                    // its own subtree beneath `AC`, and files it creates inherit from the directory
                    // it created them in — a recursive walk would cost a DACL propagation per
                    // lifecycle spawn for nothing.
                    // ⛔⛔ `AC` ALONE LEFT THIS ENTIRELY UNFIXED — `AC\Temp` IS WHERE THE FAILURES LAND.
                    // The comment above already named `AC\Temp\…` as part of the family, and the
                    // first version of this block still stopped at `AC`.
                    //
                    // MEASURED on the fixed binary (nub 8a49b39413, run 30893326426): ALL THREE
                    // witnesses — electron-chromedriver@43.2.0, playwright-chromium@0.13.0,
                    // gifsicle@5.3.0 — were UNCHANGED at 55 cells write:"disk", and 130 of their
                    // cell logs carry the same shape ONE LEVEL DEEPER:
                    //
                    //     ENOENT: no such file or directory, open
                    //       '…\Packages\nub_sbx_…_0\AC\Temp\playwright-download-chromium-win64-…zip'
                    //
                    // `AC\Temp` is where an AppContainer virtualizes the container's TEMP, so every
                    // installer that downloads to a temp file — which is most of the browser and
                    // driver family — dies there.
                    //
                    // ⛔ THE THREE-WITNESS NEGATIVE IS WHAT FORCED READING THE LOG, and the log
                    // carried an answer neither reading of "still 55 cells" allowed for: not "the fix
                    // did nothing" and not "the fix worked, something else blocks", but the SAME
                    // failure at a deeper path. A grant is only as good as the deepest path the
                    // child actually opens.
                    // ⛔⛔ `DELETE` IS LOAD-BEARING AND THIS GRANT OMITTED IT — the third level of
                    // the same family. With `AC\Temp` created and read-write granted, the child
                    // CREATES its download fine and then cannot REMOVE it:
                    //
                    //     [Error: EPERM: operation not permitted, unlink
                    //       '…\AC\Temp\electron-download-Oiubht\chromedriver-v43.2.0-win32-x64.zip']
                    //     errno: -4048
                    //
                    // MEASURED on the FIXED binary (nub 8f7d5adb67, run 30898968818): 32 cell logs
                    // for electron-chromedriver@43.2.0 and 12 for playwright-chromium@0.13.0 carry
                    // that line, with ZERO ENOENT — the directory now exists, so the failure moved
                    // from "cannot open" to "cannot unlink". gifsicle@5.3.0 narrowed 55c -> 6c in the
                    // same run because it does not unlink its download; that is the whole difference.
                    //
                    // ⛔ POSIX DOES NOT NEED THIS, which is why it was easy to miss: unlink there is
                    // governed by write permission on the DIRECTORY, so a writable temp dir is enough.
                    // Windows requires DELETE on the FILE, and an inherited ACE carrying only
                    // GENERIC_READ|GENERIC_WRITE grants everything the download needs except removing
                    // it. The ordinary write-grant path already knew this — `self.write_grants` below
                    // uses `GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE` — and this leaf
                    // grant did not match it. It does now: the missing `GENERIC_EXECUTE` was its own
                    // defect, measured 2026-09-03, and the two masks are deliberately identical.
                    //
                    // Every download-then-move installer cleans up its staging file, so this blocks
                    // the same family `AC\Temp` itself did.
                    for leaf in ["AC", "AC/Temp"] {
                        let p = dir.join(leaf);
                        if std::fs::create_dir_all(&p).is_ok() {
                            let _ = grant_leaf_ace(
                                &p,
                                ac_sid,
                                GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                            );
                        }
                    }
                    Some(ChildProfileGuard { dir })
                });

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

            // 1c. PUBLISH nub's OWN PUBLIC CACHES ONCE, BEFORE the per-run grant loop — the single
            //     largest cost in a jailed launch, removed rather than optimised.
            //
            //     A per-run AC SID means the store grant is an inheritable ACE written and revoked
            //     EVERY launch, and Windows inheritance is STATIC: setting it rewrites every
            //     existing child's DACL right then. Measured in-product, that pair is 10,553 ms of
            //     a 13,845 ms fixed per-launch cost across 25,526 store entries — 76% of it — and
            //     it scales linearly. Published to `ALL APPLICATION PACKAGES` instead, the very
            //     next `grant_leaf_ace` sees `already_granted_to_appcontainers` and SKIPS the path,
            //     so it is never granted or revoked again on this machine. Exactly the reason
            //     `%ProgramFiles%\nodejs` costs nothing today.
            //
            //     ⛔ BEST-EFFORT ON PURPOSE. A failure here is a MISSED OPTIMISATION, never a
            //     confinement change: the path stays in `read_grants`, so the loop below grants it
            //     per-run as before and the launch is slow rather than wrong. Erroring out would
            //     turn an unwritable cache DACL into "no package can build on this machine".
            //
            //     The one-time cost lands on whoever publishes first, on an already-populated
            //     store (~39 s measured for 25,526 entries). Publishing at store CREATION, while
            //     it is empty, avoids even that — the trick `stage_appcontainer_readable_copy`
            //     already uses — but that belongs to the code that makes the store, not here.
            for dir in &self.publishable_grants {
                if !dir.exists() || leaf_read_grant_redundant(dir) {
                    continue;
                }
                let _ = timed(&format!("publish.once {}", dir.display()), || {
                    publish_appcontainer_read(dir)
                });
            }

            // 2. Grant the leaf allow-ACEs; `_aces` revokes them on drop (declared before
            //    the job ⇒ revoked after the tree is reaped, before profile delete). Leaf
            //    read/write grants are INHERITABLE (cover the subtree). Ancestors are
            //    handled separately, in step 2b. A REVOKE_ACCESS teardown on the unique SID
            //    removes exactly our ACEs from every path, whatever the access mask.
            //
            //    Both kinds run through ONE loop so the teardown list is populated from a
            //    single decision — see [`grant_leaf_ace`], which may report that the path
            //    already grants AppContainers what we were about to add.
            let mut _aces = AceGuard {
                paths: Vec::new(),
                objects: Vec::new(),
                sid: sid_copy,
            };
            let leaves = self
                .read_grants
                .iter()
                .map(|d| ("read", d, GENERIC_READ | GENERIC_EXECUTE))
                .chain(self.write_grants.iter().map(|d| {
                    (
                        "write",
                        d,
                        GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                    )
                }));
            //    A REFUSED **READ** GRANT IS SKIPPED, NOT FATAL — same reasoning as the
            //    ancestor chain in 2b, which the read grants share a failure mode with.
            //    Writing a DACL needs `WRITE_DAC`, and a read grant may legitimately name a
            //    path the user does not hold it on: a toolchain outside their profile
            //    (`C:\hostedtoolcache`, an all-users Python) is granted read, and taking `?`
            //    there aborted EVERY lifecycle script on the machine over one unreachable
            //    toolchain — the loudest possible failure for the mildest cause. Skipping
            //    cannot open the jail: a grant is a REDUCTION from the unconfined lifecycle
            //    spawn's complete access, so a grant not installed leaves the child with
            //    LESS, and the worst outcome is one package failing to find one tool. The
            //    interpreter no longer relies on this — nub stages a copy it owns (see
            //    `pm_engine::jail_bin`) — which is what makes it a genuine safety net rather
            //    than the mechanism.
            //
            //    WRITE grants stay FATAL. Every one of them is nub's own private tmp or the
            //    package directory being built, both under the user's own tree, so a refusal
            //    there is not a reachable configuration — it is a broken assumption, and
            //    continuing would launch a build that silently cannot write its output.
            // THE SEAM. It was added because the fail-closed behaviour this replaced was the prime
            // suspect for a sibling lane's finding that `cmd.exe` cannot run confined at all
            // de-elevated: `resolve_program` auto-grants the program FILE (above), so a System32
            // program's own leaf grant is attempted, and if de-elevated refusal aborted the launch
            // under `?`, that would be indistinguishable from cmd misbehaving.
            //
            // ⛔ THAT SUSPICION IS NOW REFUTED, BY THE ARM ITSELF — do not re-open it from this
            // comment. Corpus run 30918296299 set this variable and re-measured bs-platform@9.0.2
            // (the witness — `spawnSync C:\Windows\system32\cmd.exe EPERM` in 51 of 54 cell logs)
            // beside optipng-bin@8.1.0 as a negative control. Fail-closed produced **ZERO**
            // `installing read grant ACE ... failed` aborts across 61 logs while the EPERM stayed at
            // 51/54. So no read-grant ACE is being skipped here, silently or otherwise: they install
            // fine and the refusal is somewhere else entirely.
            //
            // What that corpus DID establish is that the remaining Windows failures are TWO distinct
            // causes, separated by ERRNO rather than by which grants they need — grouping them by the
            // rung signature merged them twice. bs-platform is refused `cmd.exe`
            // (EPERM = ERROR_ACCESS_DENIED); jpegtran-bin@5.0.2 cannot spawn its OWN downloaded
            // vendor exe (`spawn UNKNOWN`, 26/56). UNKNOWN is libuv's `default:` arm — an error it has
            // no mapping for — which rules out ENOENT and EPERM alike (`deps/uv/src/win/error.c`
            // maps ERROR_MOD_NOT_FOUND -> ENOENT and ERROR_ACCESS_DENIED -> EPERM). Each signature is
            // ABSENT from the sibling that works, measured in the same arm.
            //
            // The seam stays: it is the only way to tell an uninstallable ACE from a live refusal, it
            // answered its question once, and it can only ever make the jail STRICTER — so it is not a
            // lever anything can be widened with.
            let fail_closed = std::env::var_os("NUB_SANDBOX_WIN_FAIL_CLOSED_READ_GRANTS").is_some();
            for (kind, dir, access) in leaves {
                let installed = match timed(&format!("grant.{kind} {}", dir.display()), || {
                    grant_leaf_ace(dir, ac_sid, access)
                }) {
                    Ok(installed) => installed,
                    Err(_) if kind == "read" && !fail_closed => continue,
                    Err(error) => {
                        return Err(io::Error::new(
                            error.kind(),
                            format!(
                                "sandbox: installing {kind} grant ACE on {} failed: {error}",
                                dir.display()
                            ),
                        ));
                    }
                };
                if installed && !_aces.paths.contains(dir) {
                    _aces.paths.push(dir.clone());
                }
            }

            // 2b. THE ANCESTOR CHAIN, which the leaf grants alone do not cover.
            //
            // Traverse-bypass exempts INTERMEDIATE components of one open; it does not make
            // an ancestor openable as a TARGET, and Node's `realpathSync` opens every prefix
            // of a path in turn — starting at the volume root. Measured, that is where an
            // absolute `require()` dies: `EPERM: lstat 'C:\'`, with `C:\Users` and the user
            // profile refused right behind it (run 30464397422).
            //
            // Two mechanisms are attempted, and ONLY THE FIRST IS MEASURED TO WORK. Writing an
            // ACE needs `WRITE_DAC`, which a standard user holds on their own profile and below
            // but not on `C:\` or `C:\Users` (measured de-elevated, same run). So:
            //
            //  - Where nub CAN write, it writes a NON-INHERITED ACE carrying exactly
            //    traverse + read-attributes. Non-inherited is not a detail: it grants the
            //    directory OBJECT and nothing under it (so an ancestor grant never becomes a
            //    subtree read), and it costs no DACL propagation, which is what keeps this
            //    affordable per lifecycle spawn. THIS is the half that holds unprivileged.
            //  - Where it cannot — `C:\` is owned by TrustedInstaller and `C:\Users` by SYSTEM,
            //    and neither grants a standard group `WRITE_DAC` — NOTHING repairs those two
            //    roots. A second mechanism was tried and is DEAD: the capability SIDs Windows
            //    already places on them are `S-1-15-3-65536-…`, and the kernel refuses that
            //    AppSilo RID class outright (`0xc000000d` from `NtCreateLowBoxToken`), measured
            //    in BOTH principals. See `wiki/design/build-jail-windows.md`.
            //
            // Both are best-effort by design. This jail is defence in depth, not a watertight
            // boundary; a package that cannot start is a worse outcome than a residual, and
            // every grant here is a REDUCTION from the unconfined lifecycle spawn's complete
            // access. A refused ACE write is therefore skipped, not fatal.
            //
            // The seam exists so the branch-scoped Windows probe can measure BOTH directions
            // in ONE run: without an arm where the defect still reproduces, a green repaired
            // arm is measuring nothing. It can only ever REMOVE grants, so it is not a lever
            // anything can be widened with.
            let ancestors = if std::env::var_os("NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR").is_some() {
                Vec::new()
            } else {
                // The container profile created in 1a rides along as a leaf, so `Packages` and
                // everything above it take the same traverse ACE the grant chain gets. `None` when
                // the parent's known folder already agreed with the child's env (1a filtered it
                // out): Windows created the profile itself and the chain is already walkable.
                ancestor_chain(&self, _child_profile.as_ref().map(|g| g.dir.as_path()))
            };
            for dir in &ancestors {
                if set_ace_on_object(dir, ac_sid, TRAVERSE_MASK, GRANT_ACCESS).is_ok() {
                    _aces.objects.push(dir.clone());
                }
            }

            // 3. Capabilities: internetClient iff egress allowed, and nothing else. The
            //    ancestor chain contributes none — see the DEAD note in 2b.
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
            let job = create_confinement_job()?;
            let _job = HandleGuard(job);

            // 5. Proc-thread attribute list: SECURITY_CAPABILITIES, plus a HANDLE_LIST
            //    scoping inheritance to EXACTLY the std handles (see `bInheritHandles`
            //    below). The list must be alive across CreateProcessW (it stores the
            //    pointer); `inherit_handles` outlives the call.
            let ChildStdio {
                triple: std_triple,
                list: inherit_handles,
                writers: relay_writers,
                relays,
            } = child_stdio();
            let n_attrs = 1 + u32::from(!inherit_handles.is_empty());
            let mut attr = ProcThreadAttrList::new(n_attrs)?;
            // The attribute list stores a POINTER to `sec_caps` rather than a copy, so it must
            // stay live until CreateProcessW returns.
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

            // The std handles are named EXPLICITLY rather than left to be copied from nub's own
            // process parameters, because `CREATE_NO_WINDOW` below gives the child a fresh console
            // and an unnamed stdout would then resolve to THAT console's buffer — invisible, and
            // the script's output gone.
            if !inherit_handles.is_empty() {
                si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
                si.StartupInfo.hStdInput = std_triple[0];
                si.StartupInfo.hStdOutput = std_triple[1];
                si.StartupInfo.hStdError = std_triple[2];
            }

            // ⛔⛔ `CREATE_NO_WINDOW` IS LOAD-BEARING, NOT COSMETIC: WITHOUT IT THE CONFINED CHILD
            // SHARES NUB'S CONSOLE, AND CONHOST REFUSES EVERY CONSOLE **READ** IT MAKES.
            //
            // A LowBox token sits below the conhost serving nub's console, and conhost rejects
            // `ReadConsoleOutput`/`ReadConsoleOutputCharacter`/`ReadConsoleOutputAttribute`/
            // `WriteConsoleInput` across that boundary with ERROR_ACCESS_DENIED so a lower-trust
            // client cannot scrape a higher-trust console's screen. It is DELIBERATE and NOT
            // ACL-driven — the console team's answer is "there's no workaround" and a world-access
            // DACL on `CONOUT$` changes nothing (microsoft/terminal#5468) — so no ACE, capability
            // or grant in this backend could ever have fixed it.
            //
            // It is not an obscure corner. PowerShell's `Write-Progress` reads the buffer to save
            // the region under the progress pane, so ANY script whose shell renders progress hits
            // it; `Expand-Archive` is the common shape. MEASURED on nub-win3, one fixture, one
            // variable, `expand-probe` running `Expand-Archive` on a local zip:
            //
            //     shared console (before)   Write-Progress ERROR_ACCESS_DENIED, ZIP NOT EXTRACTED
            //                               and, on an interactive console, `out-lineoutput` fails
            //                               the same way and the install exits 1
            //     own console (after)       no denial, archive extracted, exit 0
            //
            // The extraction is LOST, not merely un-progress-barred: the cmdlet aborts on the
            // error. Child console identity is what proves the mechanism — with nub's console
            // title set to a marker, the child read the marker back before this flag and reads its
            // own title after it.
            //
            // WHY NOT `DETACHED_PROCESS`: measured, and it fixes the denial the same way, but the
            // shell then calls `AllocConsole` for itself, which REPOINTS the std handles at that
            // new console. Every byte of script output vanished in both the piped and the
            // interactive arm. `CREATE_NO_WINDOW` gives the console up front so nothing reallocates
            // it, and unlike `CREATE_NEW_CONSOLE` it flashes no window on an interactive desktop.
            let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW;
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
            let mut launch = || unsafe {
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
            let ok = launch();
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let _ = cap_sid_owned; // backs `sec_caps` — held alive until here

            // 6a. The child now owns the inherited write end of every relay pipe, so nub drops its
            //     copy: while nub holds one, the reader never sees EOF. Then start the readers —
            //     BEFORE the resume below, so no output can be produced with nothing draining it.
            drop(relay_writers);
            let relay_threads = spawn_relays(relays);

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
                // ⛔⛔ THE DIRECT CHILD EXITING IS NOT THE SCRIPT FINISHING, AND RETURNING HERE
                // TRUNCATED THE BUILD. Waiting only on `pi.hProcess` waits on the SHELL that runs
                // the lifecycle script. When that shell hands off to a trailing external process —
                // `node-gyp rebuild`, i.e. the overwhelmingly common shape — it can exit as soon as
                // the handoff is made, and this wait then returns while the real work is still
                // running. `_job` drops moments later, and the job carries KILL_ON_JOB_CLOSE, so the
                // build is KILLED mid-flight and its exit status is whatever the shell reported.
                //
                // MEASURED on nub-win3, one fixture, one variable: a script that is
                // `node -e "setTimeout(()=>process.exit(42), 20000)"` took 20s and reported exit 1
                // with the jail OFF, and **3-4 seconds reporting SUCCESS** with the jail ON. A
                // twenty-second script was declared successful in three. That is the mechanism
                // behind every symptom filed against this path: lost stdout (nothing had flushed),
                // a lost exit code (the shell's 0 is what got read), and Windows corpus records
                // whose artifact gate failed for no attributable reason — which is how a ladder cell
                // passes with no artifact and the search climbs to `write:"disk"`.
                //
                // So drain the JOB before letting it close. Polled rather than event-driven because
                // a completion port needs `Win32_System_IO`, which this crate does not enable, and
                // widening the feature set to avoid a 50 ms poll would buy nothing.
                let handed_off = timed("drain_job", || drain_job_and_status(job, pi.dwProcessId));
                let mut code: u32 = 0;
                GetExitCodeProcess(pi.hProcess, &mut code);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                // The direct child's status WINS WHEN IT IS NON-ZERO — a shell that reports its own
                // failure is authoritative and must not be overwritten. Only when it says success do
                // we consult what it handed off to, which is the case that was silently passing.
                if code == 0 {
                    handed_off.unwrap_or(0)
                } else {
                    code
                }
            };

            // Join the relays before reporting the status, so the caller never prints its own
            // "done" line ahead of output the script already produced. `drain_job_and_status`
            // above has already waited for the whole tree, so every write end is closed and each
            // reader is at EOF — this cannot block on a live descendant.
            for thread in relay_threads {
                let _ = thread.join();
            }

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
    /// Removes the child-visible AppContainer profile directory created in `run()` step 1a.
    ///
    /// Separate from [`ProfileGuard`] because they clean up DIFFERENT things: that one calls
    /// `DeleteAppContainerProfile`, which removes the profile Windows registered at the PARENT's
    /// known-folder location. This one removes the mirror created under the CHILD's
    /// `%LOCALAPPDATA%`, which Windows knows nothing about and would otherwise leak one directory
    /// per launch.
    struct ChildProfileGuard {
        dir: PathBuf,
    }
    impl Drop for ChildProfileGuard {
        fn drop(&mut self) {
            // Best effort: the child may still hold a handle under it, and a leaked temp dir is
            // not worth failing a completed run over.
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

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
        /// Ancestor directories carrying a non-inherited traverse ace. Kept apart from
        /// `paths` because they must be revoked through the OBJECT-scoped writer: the named
        /// one would re-propagate inheritance across each ancestor's whole subtree on the way
        /// out, which is the cost the grant side goes out of its way to avoid.
        objects: Vec<std::path::PathBuf>,
        sid: Vec<u8>,
    }
    impl Drop for AceGuard {
        fn drop(&mut self) {
            let sid = self.sid.as_ptr() as PSID;
            for p in &self.paths {
                let _ = timed(&format!("revoke.leaf {}", p.display()), || {
                    revoke_ace(p, sid)
                });
            }
            for p in &self.objects {
                let _ = timed(&format!("revoke.object {}", p.display()), || {
                    set_ace_on_object(p, sid, TRAVERSE_MASK, REVOKE_ACCESS)
                });
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

    /// Which of nub's own streams a relay thread copies a pipe into.
    #[derive(Copy, Clone)]
    enum RelayTarget {
        Stdout,
        Stderr,
    }

    /// The stdio to hand the confined child, plus everything nub must hold to keep it flowing.
    ///
    /// ⛔ THE CHILD RUNS ON ITS OWN CONSOLE (`CREATE_NO_WINDOW`), SO A CONSOLE HANDLE IS NOT A
    /// USABLE STDOUT FOR IT — WriteFile SUCCEEDS AND THE BYTES ARE DISCARDED. That silent drop is
    /// why a console std handle is replaced by a pipe nub relays here, rather than passed through:
    /// see the console note at the `CREATE_NO_WINDOW` flag for the measurement and for why the
    /// child cannot stay on nub's console in the first place.
    struct ChildStdio {
        /// hStdInput / hStdOutput / hStdError, in that order. Null ⇒ the child gets no handle
        /// for that stream, which is what an unusable parent handle produced before this existed.
        triple: [HANDLE; 3],
        /// `triple` deduplicated — the PROC_THREAD_ATTRIBUTE_HANDLE_LIST contents. Empty ⇒ the
        /// caller inherits nothing (bInheritHandles FALSE).
        list: Vec<HANDLE>,
        /// nub's own copy of each relay pipe's WRITE end. Dropped immediately after
        /// CreateProcessW: while nub still holds one, the matching reader never sees EOF and
        /// the relay thread never finishes.
        writers: Vec<std::io::PipeWriter>,
        /// One relay per pipe: read what the child wrote, write it to nub's real stream.
        relays: Vec<(std::io::PipeReader, RelayTarget)>,
    }

    /// True only for a real console screen/input buffer. `GetConsoleMode` is the precise test —
    /// it fails for a pipe, a file and `NUL`, which are exactly the pass-through cases.
    /// `GetFileType == FILE_TYPE_CHAR` would not do: it also matches `NUL`.
    fn is_console_handle(h: HANDLE) -> bool {
        let mut mode: CONSOLE_MODE = 0;
        unsafe { GetConsoleMode(h, &mut mode) != 0 }
    }

    /// Builds [`ChildStdio`]. A non-console handle is passed straight through and marked
    /// inheritable, which is what `std`'s own inherited-stdio spawn does and widens nothing the
    /// child can reach beyond its stdio.
    fn child_stdio() -> ChildStdio {
        let raws = [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ];
        let mut triple: [HANDLE; 3] = [std::ptr::null_mut(); 3];
        let mut writers: Vec<std::io::PipeWriter> = Vec::new();
        let mut relays: Vec<(std::io::PipeReader, RelayTarget)> = Vec::new();
        // stdout and stderr are usually THE SAME console handle. One shared pipe for both keeps
        // the child's own interleaving byte-exact and costs one relay instead of two; two pipes
        // copied by two threads would tear each other's lines.
        let mut reused: Vec<(HANDLE, HANDLE)> = Vec::new();
        for (i, raw) in raws.into_iter().enumerate() {
            let h: HANDLE = raw.cast();
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                continue;
            }
            let target = match i {
                1 => Some(RelayTarget::Stdout),
                2 => Some(RelayTarget::Stderr),
                // stdin is passed through as-is, console or not. A confined lifecycle script that
                // waits on the user's console is a hang either way, so there is no behaviour a
                // relay would buy — a console stdin simply stops answering once the child is on
                // its own console, which turns that hang into an EOF.
                _ => None,
            };
            if let Some(target) = target
                && is_console_handle(h)
            {
                if let Some(&(_, w)) = reused.iter().find(|(dest, _)| *dest == h) {
                    triple[i] = w;
                    continue;
                }
                if let Ok((reader, writer)) = std::io::pipe() {
                    let w: HANDLE = writer.as_raw_handle().cast();
                    // Only a handle nub can mark inheritable may go in the HANDLE_LIST — a
                    // non-inheritable member makes CreateProcessW fail the whole spawn.
                    if unsafe { SetHandleInformation(w, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                        != 0
                    {
                        triple[i] = w;
                        reused.push((h, w));
                        relays.push((reader, target));
                        writers.push(writer);
                        continue;
                    }
                }
                // Pipe creation or the inherit mark failed. Fall through to the console handle:
                // the child's output is then dropped, but a script that cannot START is a worse
                // outcome than one whose output is lost, which is this jail's standing rule.
            }
            let marked =
                unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
            if marked != 0 {
                triple[i] = h;
            }
        }
        let mut list: Vec<HANDLE> = Vec::new();
        for h in triple {
            if !h.is_null() && !list.contains(&h) {
                list.push(h);
            }
        }
        ChildStdio {
            triple,
            list,
            writers,
            relays,
        }
    }

    /// Starts one thread per relay pipe. These MUST run concurrently with the wait on the child:
    /// a full pipe blocks the writer, so draining only after the child exits would deadlock a
    /// script that produces more output than the pipe buffer holds.
    fn spawn_relays(
        relays: Vec<(std::io::PipeReader, RelayTarget)>,
    ) -> Vec<std::thread::JoinHandle<()>> {
        relays
            .into_iter()
            .map(|(mut reader, target)| {
                std::thread::spawn(move || match target {
                    RelayTarget::Stdout => {
                        let mut out = std::io::stdout();
                        let _ = std::io::copy(&mut reader, &mut out);
                        let _ = out.flush();
                    }
                    RelayTarget::Stderr => {
                        let mut err = std::io::stderr();
                        let _ = std::io::copy(&mut reader, &mut err);
                        let _ = err.flush();
                    }
                })
            })
            .collect()
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

    /// Wait until the confinement Job holds no live process, so a lifecycle script that handed its
    /// work to a trailing process is not killed by `KILL_ON_JOB_CLOSE` the instant its shell exits.
    ///
    /// ⛔⛔ THE BOUND IS 90 SECONDS BECAUSE 30 MINUTES BROKE THE CORPUS — MEASURED, NOT FEARED.
    ///
    /// A script that leaves a daemon behind never drains, so this has to be bounded. The first version
    /// picked 30 minutes on the reasoning that a cold `node-gyp` build is minutes, not seconds. That
    /// reasoning was wrong about WHO WAITS: the corpus harness gives each arm a 600 000 ms deadline, so
    /// a 30-minute drain does not produce a slow measurement, it produces NO measurement. Observed on
    /// `nub-win3` immediately after the fix landed — `measure-windows.mjs` on `@posthog/cli@0.7.34`
    /// reached `VERIFY[fb1] TIMED-OUT in 'approve-builds' after 600000 ms -- no verdict; check for
    /// surviving children`, and abandoned the ladder. The `synth` and `fb0` arms had passed rc=0; only
    /// the NARROWER rung hung, which is the tell that the survivor is a process STUCK against a denied
    /// operation rather than useful work still running.
    ///
    /// 90s is chosen against that: comfortably longer than any trailing process that is actually going
    /// to finish (the shell normally waits for its own build, so this path is reached only when it
    /// handed off and left), and far enough inside every harness deadline that a stuck child costs a
    /// measurement its precision rather than its existence. Hitting the cap falls through to the
    /// pre-existing reap, which is exactly the old behaviour.
    ///
    /// BEST-EFFORT ON QUERY FAILURE, deliberately: if the job cannot be interrogated, the honest
    /// response is to stop waiting rather than to spin on a call that will keep failing. The caller's
    /// status handling is unchanged either way.
    /// ⛔ AND RECOVER THE STATUS THE DIRECT CHILD CANNOT REPORT. Sampling the live tree 10s into a
    /// jailed 30s script shows what nub is actually waiting on:
    ///
    /// ```text
    ///   5376 2780 nub.exe      <- nub itself
    ///   2636 4104 node.exe     <- the script's node, PPID 4104
    ///   (no sh.exe present)
    /// ```
    ///
    /// `node`'s parent is neither nub nor any live process and no shell remains, so the shell
    /// SPAWNED NODE AND EXITED — the shape is nub → `sh` (exits early) → `node` (orphan, still in the
    /// job). That is why draining is necessary, and why `GetExitCodeProcess(pi.hProcess)` answers
    /// with the departed shell's 0 for a script whose work exited 42.
    ///
    /// The shell is not at fault and was checked rather than assumed: both arms report `SHELL0=sh`,
    /// jail-OFF reports `exited with code 42` correctly, and an explicit `node …; RC=$?; exit $RC`
    /// propagates 42 under the jail too. `sh` simply does not wait when the node invocation is the
    /// script's LAST command.
    ///
    /// So handles are opened for every non-direct-child job member WHILE IT IS STILL ALIVE. That
    /// ordering is load-bearing: an exit code is unreadable once the last handle to the process
    /// closes, so a status recovered after the drain must have been opened during it.
    ///
    /// ANY NON-ZERO WINS, and that is the safe direction rather than a guess at npm's semantics. A
    /// build that failed must not read as success — that is the whole reason the Windows records
    /// could not be trusted. The cost is that a script deliberately backgrounding a failing process
    /// now surfaces as a failure; for a build jail that is the right way to be wrong.
    fn drain_job_and_status(job: HANDLE, direct_child_pid: u32) -> Option<u32> {
        const POLL: std::time::Duration = std::time::Duration::from_millis(50);
        const CAP: std::time::Duration = std::time::Duration::from_secs(90);
        // Bounded so a runaway script cannot make this allocate without limit. Far above any real
        // lifecycle script's process count; anything beyond it is simply not tracked.
        const MAX_TRACKED: usize = 64;

        let mut tracked: Vec<HANDLE> = Vec::new();
        let mut seen: Vec<u32> = Vec::new();
        let start = std::time::Instant::now();
        loop {
            let mut buf = vec![
                0u8;
                std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
                    + MAX_TRACKED * std::mem::size_of::<usize>()
            ];
            let listed = unsafe {
                QueryInformationJobObject(
                    job,
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    std::ptr::null_mut(),
                )
            };
            if listed != 0 {
                let list = buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
                // SAFETY: `buf` is sized for MAX_TRACKED ids past the header, and the count is
                // clamped to that; the ids follow the header contiguously.
                let ids = unsafe {
                    let n = ((*list).NumberOfProcessIdsInList as usize).min(MAX_TRACKED);
                    std::slice::from_raw_parts(
                        std::ptr::addr_of!((*list).ProcessIdList).cast::<usize>(),
                        n,
                    )
                };
                for &raw in ids {
                    let pid = raw as u32;
                    if pid == 0 || pid == direct_child_pid || seen.contains(&pid) {
                        continue;
                    }
                    seen.push(pid);
                    // SYNCHRONIZE is needed as well as the query right: the status read below decides whether a
                    // handle is SIGNALED before trusting its exit code, and `WaitForSingleObject` fails on a
                    // handle opened for query alone.
                    let h = unsafe {
                        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid)
                    };
                    if !h.is_null() {
                        tracked.push(h);
                    }
                }
            }

            let mut acct: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                QueryInformationJobObject(
                    job,
                    JobObjectBasicAccountingInformation,
                    std::ptr::from_mut(&mut acct).cast(),
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || acct.ActiveProcesses == 0 || start.elapsed() >= CAP {
                break;
            }
            std::thread::sleep(POLL);
        }

        // ⛔⛔ THE LAST MEMBER TO EXIT IS THE OUTCOME, and neither simpler rule survived measurement.
        //
        // "ANY NON-ZERO WINS" (the original) fabricates failures: a jailed `cypress@15.20.1` install
        // printed `✔ Finished Installation` and was reported failed, because of its eight trailing
        // members four exited 1 as ordinary helpers it never waits on.
        //
        // "override only if NO member succeeded" (tried next) breaks the case the rule exists for: a
        // fixture that detaches a single failing process still has a sibling exiting 0, so the override
        // stopped firing and a failed hand-off read as SUCCESS. Measured, both arms, on nub-win3.
        //
        // Exit ORDER separates them. Where the trailing tree IS the work — `node-gyp rebuild` as the
        // script's last command, where `sh` does not wait — the work is what the shell handed off to and
        // therefore what finishes last. Where the tree is a fan of helpers, the helpers drain while the
        // real work has already completed. Polling the handles each round is what the drain loop is
        // already doing for the job accounting, so this costs one wait per member per round.
        let mut last_exit: Option<u32> = None;
        let mut pending: Vec<HANDLE> = tracked.clone();
        let order_deadline = std::time::Instant::now() + CAP;
        while !pending.is_empty() && std::time::Instant::now() < order_deadline {
            let mut still = Vec::with_capacity(pending.len());
            for h in pending {
                if unsafe { WaitForSingleObject(h, 0) } == WAIT_OBJECT_0 {
                    let mut code: u32 = 0;
                    if unsafe { GetExitCodeProcess(h, &mut code) } != 0 {
                        if std::env::var_os("NUB_JAIL_DUMP_POLICY").is_some() {
                            eprintln!("JAILDUMP drain exited code={code}");
                        }
                        // ⛔⛔ STATUS_BREAKPOINT IS TEARDOWN NOISE, NOT AN OUTCOME, AND LETTING IT WIN
                        // MADE THE HIGHEST-WEIGHT PACKAGE ON WINDOWS FAIL AT RANDOM. Measured on
                        // `puppeteer@25.8.0` (~11.9M installs/week) with two runs that differ in
                        // nothing but scheduling. Its postinstall COMPLETES in both — chrome and
                        // chrome-headless-shell are both downloaded — and `0x80000003` appears in
                        // both drains. Only the ORDER differs:
                        //
                        //   failed: 0, 0, 0, 0, 2147483651   <- breakpoint exits LAST, so it wins
                        //   passed: 0, 0, 0, 2147483651, 0   <- a member follows it, so it does not
                        //
                        // The exit-ORDER rule above is unchanged and still right; this only removes a
                        // member that was never a candidate for "the outcome". A debug break is what a
                        // process reports when it is broken into or torn down abnormally, so it says
                        // nothing about whether the script's work succeeded — unlike `0xC0000142`
                        // (loader init), which IS a real failure and must keep counting.
                        //
                        // Deliberately narrow. The two rules this loop already rejected both failed by
                        // being general, and the note above records what each one broke.
                        const STATUS_BREAKPOINT: u32 = 0x8000_0003;
                        if code != STATUS_BREAKPOINT {
                            last_exit = Some(code);
                        }
                    }
                } else {
                    still.push(h);
                }
            }
            pending = still;
            if pending.is_empty() {
                break;
            }
            std::thread::sleep(POLL);
        }
        // A member still running at the cap contributes nothing: it has no exit code, and inventing one
        // is the STILL_ACTIVE defect this path already paid for once.
        let status = match last_exit {
            Some(0) | None => None,
            other => other,
        };
        status
    }

    /// The confinement Job: whole-tree reap on handle close, plus the active-process
    /// ceiling (see [`super::active_process_cap`]).
    fn create_confinement_job() -> io::Result<HANDLE> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // ACTIVE_PROCESS is transitive to grandchildren and refuses CREATE_BREAKAWAY_FROM_JOB,
        // so confined code cannot escape it; it needs no privilege, which is why it is the
        // containment lever the zero-privilege jail can actually use.
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        info.BasicLimitInformation.ActiveProcessLimit = super::active_process_cap();
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

    /// Per-step wall-clock for the jailed launch, emitted only when `NUB_SANDBOX_WIN_TIMING`
    /// is set. Diagnostic seam, never a behaviour switch.
    ///
    /// WHY IT EXISTS. A jailed launch on Windows costs a FIXED ~14 s regardless of what the
    /// script does — measured on Server 2022 with an empty script: 406 ms unconfined against
    /// 14,644 ms under the LowBox token, split 7.8 s before the script and 6.5 s after it, to
    /// run a 1 ms script. Per-operation cost is NOT the cause (every file op measured at or
    /// below its unconfined rate), and `RUST_LOG=debug` attributes none of it, so the only
    /// way to localise it was to guess. Inheritable-ACE propagation is the leading suspect:
    /// `icacls` measured ~1.9 ms per entry on the real store tree (77,339 entries ⇒ 147 s),
    /// so a grant landing on a few thousand entries is seconds, and it is paid TWICE — the
    /// grant/revoke rate ratio (1.11) matches the observed setup/teardown ratio (1.20).
    ///
    /// The cost is PER PACKAGE, so a project with 20 install-script packages pays ~280 s on a
    /// default-on feature. That is what this seam exists to attribute and then delete.
    pub(super) fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
        if std::env::var_os("NUB_SANDBOX_WIN_TIMING").is_none() {
            return f();
        }
        let start = std::time::Instant::now();
        let out = f();
        eprintln!("WIN_JAIL_TIMING {label} {} ms", start.elapsed().as_millis());
        out
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
    pub(in crate::backend) fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A path as a NUL-terminated wide string with backslash separators (canonical IR
    /// paths are forward-slashed; the Win32 security APIs want native separators).
    fn to_wide_path(p: &Path) -> Vec<u16> {
        let s = p.to_string_lossy().replace('/', "\\");
        to_wide(&s)
    }

    /// Build a mutable UTF-16 command line from program + args, quoting each argv token
    /// per the CommandLineToArgvW rules std uses. lpApplicationName is NULL, so the
    /// child gets a conventional argv.
    ///
    /// THE INVARIANT A VERBATIM TAIL PROTECTS: `cmd.exe` does not parse its line by
    /// those rules, so re-encoding a line already built for it (aube's `raw_arg` tail —
    /// see `spawn_shell_with_settings` in `aube-scripts`) escapes `"` as `\"` and hands
    /// `cmd.exe` a first token of `\""`. Every dependency lifecycle script under the
    /// build jail passes through here, so that re-encoding broke all of them on Windows.
    /// A [`CommandArgs::Verbatim`](crate::backend::CommandArgs) tail is therefore copied
    /// through untouched; the gate that keeps this from being a general quoting bypass
    /// lives in `validate_apply_inputs`, not here.
    pub(in crate::backend) fn build_command_line(
        program: &std::ffi::OsStr,
        args: &crate::backend::CommandArgs,
    ) -> Vec<u16> {
        let mut line: Vec<u16> = Vec::new();
        append_quoted(&mut line, program);
        match args {
            crate::backend::CommandArgs::Argv(v) => {
                for a in v {
                    line.push(u16::from(b' '));
                    append_quoted(&mut line, a);
                }
            }
            crate::backend::CommandArgs::Verbatim(tail) => {
                line.push(u16::from(b' '));
                line.extend(tail.encode_wide());
            }
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
    pub(in crate::backend) fn build_env_block(
        env: &std::collections::BTreeMap<String, String>,
    ) -> Vec<u16> {
        // Folds case-insensitively, so this both DEDUPES colliding keys and yields the
        // case-insensitive ordering Windows expects.
        let pairs = dedupe_windows_env_pairs(env.iter());
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
    use crate::policy::{CanonGlob, FsOrigin, FsRule, FsRuleSet, TmpMode};

    /// A dependency lifecycle script on Windows is a cmd.exe invocation, and cmd.exe
    /// REFUSES an extended-length working directory — it prints "UNC paths are not
    /// supported" and runs in the Windows directory instead, so the script cannot find its
    /// own package's files. `canonicalize` produces exactly that spelling, so the child's
    /// cwd has to be handed over in its ordinary form.
    #[test]
    fn the_child_cwd_sheds_the_verbatim_prefix_canonicalize_adds() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\r\pkg")),
            PathBuf::from(r"C:\Users\r\pkg")
        );
        // Already ordinary, and POSIX-shaped paths (the test host): unchanged.
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"C:\Users\r\pkg")),
            PathBuf::from(r"C:\Users\r\pkg")
        );
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from("/tmp/pkg")),
            PathBuf::from("/tmp/pkg")
        );
    }

    /// A verbatim UNC path must survive INTACT. Stripping `\\?\` from `\\?\UNC\srv\share`
    /// yields `UNC\srv\share`, a relative path naming a different location entirely — a
    /// silently wrong working directory rather than a loud failure.
    #[test]
    fn a_verbatim_unc_cwd_is_left_alone() {
        let unc = PathBuf::from(r"\\?\UNC\server\share\pkg");
        assert_eq!(strip_verbatim_prefix(unc.clone()), unc);
    }

    /// The cap must clear the measured structural ceiling of a legitimate parallel
    /// native build (`2 * cores + 5`) on this host with real headroom, and never fall
    /// below the 64 floor — the two properties that make it defence-in-depth rather
    /// than a build-breaking limit.
    #[test]
    fn active_process_cap_clears_a_legitimate_build_ceiling() {
        let cores = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        let cap = active_process_cap();
        assert!(cap >= 64, "cap {cap} fell below the floor");
        let build_ceiling = u32::try_from(2 * cores + 5).unwrap();
        assert!(
            cap >= build_ceiling * 3,
            "cap {cap} leaves under 3x headroom over the {build_ceiling}-process build ceiling"
        );
    }

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
            origin: FsOrigin::Authored,
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
        let __g = derive_grants(&p);
        let read = __g.read;
        let write = __g.write;
        let deg = __g.degrade;
        assert_eq!(read, vec![PathBuf::from("C:/proj/pkg")]);
        assert_eq!(write, vec![PathBuf::from("C:/proj/pkg")]);
        assert_eq!(deg, FsDegrade::default());
    }

    /// Both directions of the origin-aware existence check, on one fixture so the ONLY
    /// difference between the two arms is `FsOrigin`. The speculative arm is what lets the
    /// build jail launch before its guessed roots exist; the authored arm is the control
    /// that keeps a named-but-missing grant a hard launch failure (the promise
    /// `windows_enforcement`'s `missing-grant` probe pins end-to-end).
    #[test]
    fn a_missing_source_is_skipped_only_when_speculative() {
        let dir = tempfile::tempdir().expect("tempdir");
        let present = dir.path().join("present");
        std::fs::create_dir(&present).expect("create present");
        let missing = dir.path().join("missing");
        let canon = |p: &Path| p.to_string_lossy().replace('\\', "/");

        let with_origin = |origin: FsOrigin| FsRule {
            matcher: CanonGlob(canon(&missing)),
            effect: Effect::Allow,
            access: FsAccess::Read,
            origin,
        };

        let __g = derive_grants(&fs(
            Effect::Deny,
            vec![
                rule(&canon(&present), Effect::Allow, FsAccess::Read),
                with_origin(FsOrigin::Speculative),
            ],
        ));
        let read = __g.read;
        assert_eq!(
            read,
            vec![present.clone()],
            "an absent speculative grant must not reach the ACE plan"
        );

        let __g = derive_grants(&fs(
            Effect::Deny,
            vec![
                rule(&canon(&present), Effect::Allow, FsAccess::Read),
                with_origin(FsOrigin::Authored),
            ],
        ));
        let read = __g.read;
        assert_eq!(
            read,
            vec![present, missing],
            "an absent AUTHORED grant must still be planned, so set_ace fails the launch"
        );
    }

    /// A speculative grant is skipped for being ABSENT, never for being speculative — the
    /// failure mode that would silently hollow out the build jail's whole read set.
    #[test]
    fn a_present_speculative_source_is_still_granted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canon = dir.path().to_string_lossy().replace('\\', "/");
        let __g = derive_grants(&fs(
            Effect::Deny,
            vec![FsRule {
                matcher: CanonGlob(format!("{canon}/**")),
                effect: Effect::Allow,
                access: FsAccess::ReadWrite,
                origin: FsOrigin::Speculative,
            }],
        ));
        let read = __g.read;
        let write = __g.write;
        assert_eq!(read, vec![dir.path().to_path_buf()]);
        assert_eq!(write, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn read_only_allow_yields_no_write_grant() {
        let p = fs(
            Effect::Deny,
            vec![rule("C:/tools", Effect::Allow, FsAccess::Read)],
        );
        let __g = derive_grants(&p);
        let read = __g.read;
        let write = __g.write;
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
        let __g = derive_grants(&p);
        let _read = __g.read;
        let _write = __g.write;
        let deg = __g.degrade;
        assert!(
            deg.generous_read,
            "a default-Allow base must degrade fs-read"
        );
    }

    #[test]
    fn whole_fs_allow_entry_degrades_generous_read() {
        // The shape the compiler ACTUALLY emits for `sandbox: true`: a Deny
        // base + a whole-fs `**` Allow ENTRY (+ secret denies). It must degrade, not be
        // silently dropped as a no-op grant.
        let p = fs(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                rule("**/.env", Effect::Deny, FsAccess::Read),
            ],
        );
        let __g = derive_grants(&p);
        let read = __g.read;
        let _write = __g.write;
        let deg = __g.degrade;
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
        let __g = derive_grants(&p);
        let read = __g.read;
        let _write = __g.write;
        let deg = __g.degrade;
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

    /// THE defect this change exists to fix. Every read-granting build-jail policy was
    /// rejected on Windows: SIX of the fold's eight secret-file globs (`**/.env*`, `.env*`,
    /// `**/.npmrc`, `**/node_modules/npm/npmrc`, `**/.env*/**`, `.env*/**` — measured) are
    /// depth-independent, so their `literal_prefix` is empty and each shadows EVERY grant.
    /// One is enough: `deny_shadows_grant` fires, `apply` returns
    /// `Degradation{lost:["fs-read-deny"]}`, and `pm_engine::build_jail` turns that into
    /// "build-jail could not be applied (fail-closed)" — no lifecycle script runs at all. The
    /// other two (`.npmrc`, `node_modules/npm/npmrc`) are the relative rootless twins, whose
    /// prefixes never match an absolute grant. A pure allowlist emits no deny at all, so the
    /// predicate has nothing to fire on.
    #[test]
    fn a_pure_allowlist_build_jail_is_no_longer_rejected() {
        use crate::compiler::compile_build_jail;
        use crate::matcher::Homes;
        use std::collections::BTreeMap;

        let homes = Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        };
        let policy = compile_build_jail(
            homes,
            Path::new("/proj/node_modules/somepkg"),
            None,
            None,
            vec![PathBuf::from("/testhome/.cache/nub/node/v26/bin/node")],
            vec![PathBuf::from(
                "/testhome/.cache/nub/node/v26/lib/node_modules",
            )],
            BTreeMap::new(),
        )
        .expect("build-jail compiles");
        let __g = derive_grants(&policy.fs);
        let grants = __g.read;

        assert!(
            !grants.is_empty(),
            "the control is only meaningful against a policy that actually grants reads"
        );
        assert!(
            !deny_shadows_grant(&policy.fs.rules.entries, &grants),
            "a pure-allowlist build-jail policy must be accepted on Windows"
        );
        // CONTROL: re-attach the band this change removed and the rejection comes straight
        // back — so the pass above is the removal doing the work, not an empty rule set.
        let mut with_floor = policy.fs.rules.entries.clone();
        with_floor.extend(
            crate::compiler::ENV_DENY_LEAF_GLOBS
                .iter()
                .map(|g| rule(g, Effect::Deny, FsAccess::Read)),
        );
        assert!(
            deny_shadows_grant(&with_floor, &grants),
            "control arm must reproduce the shipping rejection"
        );
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
            let __g = derive_grants(&p);
            let _read = __g.read;
            let write = __g.write;
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
        let __g = derive_grants(&p);
        let _r = __g.read;
        let write = __g.write;
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
    fn windows_env_serialization_deduplicates_case_aliases() {
        let path = "Path".to_string();
        let ambient = "ambient".to_string();
        let literal_key = "PATH".to_string();
        let literal = "literal".to_string();
        let pairs = dedupe_windows_env_pairs([(&path, &ambient), (&literal_key, &literal)]);
        assert_eq!(pairs.len(), 1, "Windows has one logical PATH key");
        assert_eq!(pairs[0].0, "PATH");
        assert_eq!(pairs[0].1, "literal");
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
            // Allow-base keeps `confine_fs` false: this test passes no cwd, and the
            // merged `apply` fail-closes with `fs-root` when fs confines without one.
            fs: fs(Effect::Allow, vec![]),
            net,
            ..Default::default()
        };

        // ⛔ THE PROXY PORT IS NOT A FREE PARAMETER ON A PROVISIONED MACHINE, and the hardcoded
        // one this used to pass asserted the OPPOSITE of the contract there. Provisioning is part
        // of `account_route`'s predicate, so the two hosts take different routes: a stock runner
        // has no sandbox account and falls through to the AppContainer tier, which never looks at
        // the port; a machine someone ran `nub setup-sandbox` on takes the dedicated-account
        // route, which fail-CLOSES unless the proxy bound inside the loopback window that setup
        // pre-authorized in WFP — a proxy the child cannot reach is silent no-net rather than a
        // degradation. MEASURED on a provisioned Windows Server 2022 host: the literal `9999` is
        // outside the installed 59080-59089 window, so `apply` refused and the elevated arm
        // panicked on a machine behaving exactly as designed.
        let marker = crate::backend::windows_account::state::read_marker()
            .ok()
            .flatten();

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

        // On a provisioned machine an off-window port is an ERROR rather than a degradation.
        // Pinned here so the refusal that produced the measurement above stays asserted instead
        // of being rediscovered as a mystery failure.
        if let Some(marker) = &marker {
            let Err(off_window) = apply(
                &per_host,
                crate::CommandSpec::new("cmd.exe"),
                Some(marker.port_high.wrapping_add(1)),
                None,
                None,
                None,
            ) else {
                panic!("a proxy bound outside the WFP window must fail-closed, not apply");
            };
            assert!(
                off_window.lost.iter().any(|s| s == "net-per-host"),
                "the off-window refusal must name net-per-host (got {:?})",
                off_window.lost
            );
        }

        // In the window where one applies; an unprovisioned host never reads this.
        let port = marker.as_ref().map_or(59080, |marker| marker.port_low);
        let res = apply(
            &per_host,
            crate::CommandSpec::new("cmd.exe"),
            Some(port),
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
            // `expect_err` would need `Prepared: Debug`, which it is not (it owns a live
            // proxy and a launch plan) — so destructure instead.
            let Err(err) = res else {
                panic!("unelevated per-host must fail-closed, not degrade");
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
