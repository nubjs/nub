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
//! lowbox check before the file deny is reached). Caller paths receive a policy-specific
//! AppContainer SID. Explicitly publishable Nub-owned public caches are the exception:
//! their persistent AAP read grants are not session-owned and are not revoked on close.
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
//!     not full host networking. Per-host policies use an unprivileged co-package
//!     proxy helper with `internetClient`; the confined child has no direct egress.
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
//! Job assigned at creation, and durable policy-scoped ACL grants. Acquisition
//! returns a reusable resource; each spawn returns its own native process, Job and
//! streams. A command owns a resource lease through final tree reaping.

use crate::policy::{Effect, FsAccess, FsOrigin, FsPolicy, FsRule, Inspection, NetPolicy};
// Referenced only by the Windows-gated `apply`; the host build (module-under-test)
// never names it.
#[cfg(target_os = "windows")]
use crate::policy::SandboxPolicy;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

// Kept beside the native launcher rather than exported through `backend`: this is
// Windows host-state ownership, not a cross-platform policy surface.  The pure
// identity/journal half is also compiled by host tests.
#[cfg(any(target_os = "windows", test))]
#[cfg_attr(not(windows), allow(dead_code))]
#[path = "windows_registry.rs"]
pub(super) mod windows_registry;

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
#[derive(Clone)]
pub(crate) struct AppContainerLaunch {
    program: OsString,
    args: super::CommandArgs,
    cwd: Option<PathBuf>,
    /// Subtrees the AppContainer SID is granted inheritable read-execute.
    read_grants: Vec<PathBuf>,
    /// Directory OBJECTS the AppContainer SID is granted list+traverse on, with NO
    /// inheritance — [`derive_grants`]'s `read_nodes`. Granted through the same writer as
    /// the ancestor chain, so they propagate nothing and revoke through `AceGuard::objects`.
    read_node_grants: Vec<PathBuf>,
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
    /// Zero-privilege per-host egress FUNNEL: `Some(policy)` ⇒ before spawning the (capability-
    /// free) child, launch a CO-PACKAGE helper process — SAME AppContainer SID, holding
    /// `internetClient` — running nub's egress proxy over this net policy, then point the child
    /// at it via `HTTP_PROXY`. Same-package loopback needs no administrator exemption.
    /// `apply` sets it only when
    /// [`plan_net`] chose [`WinNetPlan::Funnel`]; the proxy's port/token are known only at launch,
    /// so [`AppContainerLaunch::run`] injects the proxy env then rather than `apply` baking it in.
    egress_funnel: Option<NetPolicy>,
    /// A stable profile-owned slot, resolved only after policy identity acquisition.
    private_tmp: bool,
    pub(super) native_compat: bool,
    stdout: WindowsStdio,
    stderr: WindowsStdio,
}

/// Native command stream configuration; pipes remain owned by the submitting caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum WindowsStdio {
    Inherit,
    Piped,
    Null,
}

#[cfg(windows)]
pub(super) use launch::{WindowsChild, WindowsLease, WindowsResource};

#[cfg(windows)]
pub(crate) use launch::cleanup_resources;

/// The AppContainer launch plan and its owned enforcement resources.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) enum WindowsLaunch {
    /// Per-run AppContainer (LowBox) — the pure-allowlist path. No elevation, ever.
    AppContainer(Box<AppContainerLaunch>),
    Plain(PlainLaunch),
}

/// Final command data shared by plain and AppContainer native launches.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone)]
pub(crate) struct PlainLaunch {
    program: OsString,
    args: super::CommandArgs,
    cwd: Option<PathBuf>,
    env: Option<BTreeMap<String, String>>,
    stdout: WindowsStdio,
    stderr: WindowsStdio,
}

#[cfg(target_os = "windows")]
#[allow(dead_code)] // Retained synchronous adapters; Prepared uses the native spawn path.
impl WindowsLaunch {
    pub(crate) fn plain(spec: super::CommandSpec, env: BTreeMap<String, String>) -> Self {
        Self::Plain(PlainLaunch {
            program: spec.program,
            args: spec.args,
            cwd: spec.cwd,
            env: Some(env),
            stdout: if spec.redact_stdout {
                WindowsStdio::Piped
            } else {
                WindowsStdio::Inherit
            },
            stderr: if spec.redact_stderr {
                WindowsStdio::Piped
            } else {
                WindowsStdio::Inherit
            },
        })
    }

    pub(crate) fn is_appcontainer(&self) -> bool {
        matches!(self, Self::AppContainer(_))
    }

    pub(crate) fn run(self) -> std::io::Result<std::process::ExitStatus> {
        self.run_cancellable(&std::sync::atomic::AtomicBool::new(false))
    }

    pub(crate) fn run_cancellable(
        self,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<std::process::ExitStatus> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "sandbox launch cancelled",
            ));
        }
        let resource = self.acquire()?;
        let mut child = resource.spawn()?;
        loop {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                child.kill()?;
                child.wait()?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "sandbox launch cancelled",
                ));
            }
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    pub(super) fn acquire(self) -> std::io::Result<WindowsResource> {
        self.acquire_reusing(&BTreeMap::new())
    }

    pub(crate) fn acquire_reusing(
        self,
        retained: &BTreeMap<String, WindowsLease>,
    ) -> std::io::Result<WindowsResource> {
        match self {
            Self::AppContainer(plan) => plan.acquire_reusing(retained),
            Self::Plain(plan) => Ok(WindowsResource::plain(plan)),
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
/// A STRUCT rather than a tuple because `publishable` is a SUBSET of `read` rather than a
/// fourth independent list, and a bare 4-tuple gives a reader no way to see that.
pub(super) struct DerivedGrants {
    pub(super) read: Vec<PathBuf>,
    /// Read grants naming the directory OBJECT and not its subtree — the Windows spelling of
    /// Linux's [`MountAccess::ListOnly`]. Kept apart from `read` because the two compile to
    /// different ACEs through different writers: an inheritable read that propagates, versus a
    /// non-inherited list+traverse that does not. See [`derive_grants`] for why the distinction
    /// is both a confinement fact and the largest per-spawn saving on this backend.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(super) read_nodes: Vec<PathBuf>,
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
    let mut read_nodes = Vec::new();
    let mut write = Vec::new();
    let mut publishable = Vec::new();
    let mut degrade = FsDegrade {
        generous_read: fs.rules.default_effect == Effect::Allow,
        ..Default::default()
    };

    for (index, rule) in fs.rules.entries.iter().enumerate() {
        // Denies are implicit in the allowlist (ungranted = denied); their one hole (a
        // deny inside a granted subtree) is checked in `apply` post-program-dir.
        if rule.effect == Effect::Deny {
            continue;
        }
        // A subtree is the PAIR `[P, P/**]`, so a bare `P` whose own twin does not follow it
        // names the directory NODE. Matching `linux_grants::compile_mount_plan`, which reads
        // the same IR: the twin must agree on effect AND access, since an adjacent `P/**`
        // that denies, or grants differently, is a different rule and does not make `P` a
        // subtree head.
        let node_only = {
            let pattern = rule.matcher.as_str();
            let twin = format!("{pattern}/**");
            !pattern.ends_with("/**")
                && fs.rules.entries.get(index + 1).is_none_or(|t| {
                    t.matcher.as_str() != twin.as_str()
                        || t.effect != rule.effect
                        || t.access != rule.access
                })
        };
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
                // ⛔ A NODE-ONLY READ IS THE DIRECTORY OBJECT, NEVER ITS SUBTREE, AND WINDOWS
                // WAS THE ONE BACKEND THAT IGNORED THE DISTINCTION. `preset::project_cwd_node`
                // emits a bare rule on the CONSUMER'S PROJECT ROOT for exactly this shape, and
                // its own doc calls the node/subtree split "the entire safety argument": a
                // confined lifecycle script gets a working `getcwd` and no read of `src/`,
                // `.git/` or a root `.env`. Linux compiles it to `MountAccess::ListOnly` and
                // macOS to a Seatbelt `(literal ...)`; here `literal_subtree` answered `Some`
                // for any glob-free path and the launch granted it `inherit = true`, i.e. an
                // inheritable read over the whole project — which a pure allowlist with no
                // denies has nothing to subtract back. Measured on a Windows VM before this
                // change: a jailed script read a project-root `.env` while an out-of-project
                // control was correctly refused, so the exposure was this grant and not an
                // ambient one.
                //
                // The SAME fix is the largest single per-spawn saving on this backend.
                // `SetNamedSecurityInfoW` materializes an inheritable ace by walking every
                // existing descendant, on the set AND on the revoke, so the project root alone
                // cost 108 ms to grant plus 75 ms to revoke on a small project. A node grant
                // goes through `set_ace_on_object`, which writes the object's own descriptor
                // and propagates nothing (140 µs, measured in `windows_jail_repairs.rs`).
                //
                // Only READ diverts. A node-only ReadWrite keeps today's grant: an over-grant
                // is recoverable, and an under-grant strands a build on a laundered EPERM.
                if rule.access == FsAccess::Read && node_only && dir.is_dir() {
                    if !read_nodes.contains(&dir) {
                        read_nodes.push(dir.clone());
                    }
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
    // Fold away a read grant that a WIDER read grant already reaches. An inheritable read ace
    // on an ancestor covers every descendant, so the inner grant installs the same access a
    // second time and pays a second propagation walk for it — measured, the store cell's
    // `node_modules` nested inside `<project>/node_modules` is ~63 ms to grant and ~60 ms to
    // revoke, for access the outer grant had already given.
    //
    // ⛔ THREE THINGS IT MUST NOT FOLD, EACH A SILENT UNDER-GRANT IF IT DID:
    //   - a WRITE into a read ancestor — the read ace carries no `GENERIC_WRITE`, so `write`
    //     is not touched here at all;
    //   - anything into a NODE-ONLY ancestor — those do not inherit, which is their purpose,
    //     and they live in a separate list so they cannot be picked as an `outer`;
    //   - a descendant reached through a REPARSE POINT. Containment here is lexical while
    //     inheritance follows the REAL tree, and the virtual store is laid out as directory
    //     links: `<project>/node_modules/.store/a@1/node_modules/b` is under the outer grant
    //     by path and NOT under it by dacl. Every component from the inner path up to the
    //     outer one is stat'd, and any link — or any stat that fails — keeps today's grant.
    let read_outers: Vec<PathBuf> = read.clone();
    read.retain(|dir| {
        !read_outers.iter().any(|outer| {
            outer != dir
                && dir.starts_with(outer)
                && !publishable.contains(dir)
                && !crosses_reparse_point(outer, dir)
        })
    });

    // The same fold for writes, against WRITE outers only — never a read ancestor, whose ace
    // carries no `GENERIC_WRITE`. Every write grant asks for the identical mask, so a nested one
    // is pure duplication: measured, a store cell's package directory sits inside that cell's own
    // root and cost 53 ms to grant plus 54 ms to revoke for access the cell grant already gave.
    let write_outers: Vec<PathBuf> = write.clone();
    write.retain(|dir| {
        !write_outers.iter().any(|outer| {
            outer != dir && dir.starts_with(outer) && !crosses_reparse_point(outer, dir)
        })
    });

    DerivedGrants {
        read,
        read_nodes,
        write,
        publishable,
        degrade,
    }
}

/// Whether reaching `inner` from `outer` passes through a link, so an inheritable ace on
/// `outer` cannot be assumed to reach it. `inner` ITSELF counts: granting a directory link
/// grants its target, while inheritance from the ancestor would only ever reach the link.
///
/// Any uncertainty answers YES — an unreadable component is a reason to keep the explicit
/// grant, never to drop it.
fn crosses_reparse_point(outer: &Path, inner: &Path) -> bool {
    let mut cur = inner;
    while cur != outer {
        match std::fs::symlink_metadata(cur) {
            Ok(md) if md.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return true,
        }
    }
    false
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

#[cfg(windows)]
fn finalized_plain_launch(
    spec: super::CommandSpec,
    command: &std::process::Command,
) -> WindowsLaunch {
    // plain_command always clears the environment. Its final entries include
    // proxy/CA/temp changes, while the original spec preserves raw cmd.exe args.
    let env = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    WindowsLaunch::plain(spec, env)
}

/// The Windows network mechanisms never depend on the caller's elevation.
#[derive(Debug, PartialEq, Eq)]
enum WinNetPlan {
    Unconfined,
    CoarseDeny,
    /// A co-package proxy helper; the child itself has no internet capability.
    Funnel,
    /// No unprivileged implementation can enforce the requested policy.
    Unsupported,
}

/// Drop the `\\?\` prefix `std::fs::canonicalize` puts on a Windows path, when the result
/// is still a plain drive path a normal API accepts.
///
/// `\\?\UNC\server\share` is left ALONE: its non-verbatim spelling is `\\server\share`,
/// a genuine network path, and rewriting it would change which host is addressed. A real
/// network working directory is not something cmd.exe supports anyway, so stripping there
/// would trade one failure for a less obvious one.
pub(super) fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// Select the unprivileged co-package funnel for connection-level rules.
/// TLS inspection and credential brokering are rejected, not weakened.
fn plan_net(net: &NetPolicy, helper_available: bool) -> WinNetPlan {
    if !net.enforce {
        return WinNetPlan::Unconfined;
    }
    let needs_proxy =
        net.rules.iter().any(|r| r.effect == Effect::Allow) || !net.brokers.is_empty();
    if !needs_proxy {
        return WinNetPlan::CoarseDeny;
    }
    let connection_only = net.brokers.is_empty() && net.inspection == Inspection::Connection;
    if helper_available && connection_only {
        return WinNetPlan::Funnel;
    }
    WinNetPlan::Unsupported
}

/// Whether `apply` will route this policy through the zero-privilege co-package egress funnel —
/// the exact predicate [`plan_net`] uses to return [`WinNetPlan::Funnel`]. `backend::apply`
/// consults this to SKIP starting an in-process egress proxy on Windows: the funnel's proxy runs
/// in the helper process instead, and an in-process one would bind a port the child cannot reach
/// (a wasted bind whose failure would needlessly fail the launch closed).
#[cfg(target_os = "windows")]
pub(super) fn uses_egress_funnel(policy: &SandboxPolicy) -> bool {
    let net = &policy.net;
    net.enforce
        && (net.rules.iter().any(|r| r.effect == Effect::Allow) || !net.brokers.is_empty())
        && net.brokers.is_empty()
        && net.inspection == Inspection::Connection
        && crate::backend::windows_egress_helper_command().is_some()
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
    // Used only by the relaxed plain-command path.
    ca_bundle: Option<&std::path::Path>,
    // Used only by the explicitly unconfined compatibility path. Native private
    // storage is resolved by acquisition, never by a caller's random TempDir.
    tmp_dir: Option<&std::path::Path>,
) -> Result<super::Prepared, super::Degradation> {
    use super::{Degradation, Prepared};

    let mut spec = spec;

    let confine_fs = fs_confines(&policy.fs);
    let sandboxing = confine_fs || policy.net.enforce;
    let tmp_lost = super::tmp_lost_axis(policy);
    let private_tmp = policy.fs.tmp == crate::policy::TmpMode::Private;

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
        // The AppContainer model requires a working root no `ALL APPLICATION PACKAGES` grant
        // already reaches — otherwise an inherited AAP grant would widen the child's allow-set
        // past the policy.
        if let Err(error) = launch::timed("verify_clean_root", || {
            launch::verify_clean_root(&effective_cwd, &derived.publishable)
        }) {
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
    if policy.build_jail && !confine_fs {
        let mut deg = Degradation::full();
        let mut command = plain_command(
            policy,
            spec.clone(),
            proxy_port,
            proxy_token,
            ca_bundle,
            tmp_dir,
        );
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
        let launch = finalized_plain_launch(spec, &command);
        return Ok(Prepared {
            command,
            degradation: deg,
            proxy: None,
            launch: Some(launch),
            _private_tmp: None,
            session: None,
            redact_stdout: false,
            redact_stderr: false,
        });
    }

    // Per-host rules use only the co-package helper, never a firewall exemption.
    let helper_available = crate::backend::windows_egress_helper_command().is_some();
    let net_plan = plan_net(&policy.net, helper_available);
    if net_plan == WinNetPlan::Unsupported {
        return Err(Degradation {
            lost: vec!["net-per-host".to_string()],
            reason: Some(
                "Windows per-host network rules require a registered unprivileged egress helper; \
                 TLS inspection and credential brokering are not supported by that helper"
                    .to_string(),
            ),
        });
    }
    let funnel = net_plan == WinNetPlan::Funnel;

    // Nothing needs the AppContainer: only env-scrub (or nothing). Use the plain
    // command path — identical contract to the mac/linux relaxed case.
    if !sandboxing && tmp_lost.is_none() {
        let command = plain_command(
            policy,
            spec.clone(),
            proxy_port,
            proxy_token,
            ca_bundle,
            tmp_dir,
        );
        let launch = finalized_plain_launch(spec, &command);
        return Ok(Prepared {
            command,
            degradation: Degradation::full(),
            proxy: None,
            launch: Some(launch),
            _private_tmp: None,
            session: None,
            redact_stdout: false,
            redact_stderr: false,
        });
    }

    let read_grants = derived.read;
    let read_node_grants = derived.read_nodes;
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
    // Unsupported network policies and shadowed read denies are rejected above.
    // (Ascendant-env read is OS-CLOSED — the AppContainer denies the parent
    // OpenProcess(PROCESS_VM_READ), run 29043151805 — so NO `env-read-ascendant`
    // Degradation is emitted. Reporting it would falsely tell a frontend Windows is
    // degraded when it isn't. See the module doc.)
    // The AppContainer owns private temporary storage. A deny-all temp policy
    // remains unsupported because Windows itself grants the profile's storage.
    if let Some(axis) = tmp_lost.filter(|_| !private_tmp) {
        deg.lost.push(axis.to_string());
        reason.get_or_insert_with(|| {
            "denying all temporary storage is not supported by Windows AppContainer".to_string()
        });
    }
    deg.reason = reason;

    let launch = AppContainerLaunch {
        program: spec.program,
        args: spec.args,
        cwd: spec.cwd,
        read_grants,
        read_node_grants,
        write_grants,
        publishable_grants,
        env: build_child_env(&policy.env, funnel || private_tmp),
        // Only the helper has direct egress under a per-host policy.
        allow_internet: !policy.net.enforce,
        // `run()` launches the co-package helper over this policy and injects its proxy env.
        egress_funnel: funnel.then(|| policy.net.clone()),
        private_tmp,
        native_compat: false,
        stdout: if spec.redact_stdout {
            WindowsStdio::Piped
        } else {
            WindowsStdio::Inherit
        },
        stderr: if spec.redact_stderr {
            WindowsStdio::Piped
        } else {
            WindowsStdio::Inherit
        },
    };

    // The `command` field is unused on the launch path (status() runs `launch`); it
    // holds a benign never-spawned placeholder so the struct stays uniform.
    Ok(Prepared {
        command: std::process::Command::new(&launch.program),
        degradation: deg,
        proxy: None,
        launch: Some(WindowsLaunch::AppContainer(Box::new(launch))),
        _private_tmp: None,
        session: None,
        redact_stdout: false,
        redact_stderr: false,
    })
}

/// Materialize the scrubbed environment, or the inherited environment when the
/// funnel needs to inject its endpoint at launch.
#[cfg(target_os = "windows")]
fn build_child_env(
    env: &crate::policy::EnvPolicy,
    funnel: bool,
) -> Option<BTreeMap<String, String>> {
    if env.enforce {
        Some(env.constructed.clone())
    } else if funnel {
        Some(
            std::env::vars_os()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        )
    } else {
        None
    }
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

/// A one-line report of the CURRENT process's token security principal:
/// `il=<Low|Medium|…> is_appcontainer=<bool> ac_sid=<S-1-15-2-…|none>`.
///
/// A diagnostic for confined-launch principals — used to prove, from inside a running process, that
/// it is the Low-integrity AppContainer the sandbox intended, and (for the egress funnel) that the
/// co-package helper and the confined child carry the SAME AppContainer SID. Read-only queries on
/// the process's own token; never fails hard (returns `il=err`/`none` fields instead).
#[cfg(target_os = "windows")]
pub fn windows_token_report() -> String {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
        TOKEN_APPCONTAINER_INFORMATION, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenAppContainerSid,
        TokenIntegrityLevel, TokenIsAppContainer,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return "il=err is_appcontainer=err ac_sid=err".to_string();
        }
        // Integrity level from the mandatory-label SID's last sub-authority (RID).
        let il = {
            let mut len = 0u32;
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                std::ptr::null_mut(),
                0,
                &mut len,
            );
            let mut buf = vec![0u8; len as usize];
            let mut out = "err".to_string();
            if len > 0
                && GetTokenInformation(
                    token,
                    TokenIntegrityLevel,
                    buf.as_mut_ptr().cast(),
                    len,
                    &mut len,
                ) != 0
            {
                let tml = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
                let sid = tml.Label.Sid;
                let count = *GetSidSubAuthorityCount(sid);
                let rid = *GetSidSubAuthority(sid, u32::from(count - 1));
                out = match rid {
                    0x0000 => "Untrusted".into(),
                    0x1000 => "Low".into(),
                    0x2000 => "Medium".into(),
                    0x3000 => "High".into(),
                    0x4000 => "System".into(),
                    other => format!("rid=0x{other:04x}"),
                };
            }
            out
        };
        let mut is_ac_raw = 0u32;
        let mut len = 0u32;
        GetTokenInformation(
            token,
            TokenIsAppContainer,
            std::ptr::from_mut(&mut is_ac_raw).cast(),
            4,
            &mut len,
        );
        let ac_sid = {
            let mut len = 0u32;
            GetTokenInformation(
                token,
                TokenAppContainerSid,
                std::ptr::null_mut(),
                0,
                &mut len,
            );
            if len == 0 {
                "none".to_string()
            } else {
                let mut buf = vec![0u8; len as usize];
                if GetTokenInformation(
                    token,
                    TokenAppContainerSid,
                    buf.as_mut_ptr().cast(),
                    len,
                    &mut len,
                ) != 0
                {
                    let info = &*(buf.as_ptr() as *const TOKEN_APPCONTAINER_INFORMATION);
                    if info.TokenAppContainer.is_null() {
                        "none".to_string()
                    } else {
                        let mut s: *mut u16 = std::ptr::null_mut();
                        if ConvertSidToStringSidW(info.TokenAppContainer, &mut s) != 0 {
                            let mut n = 0usize;
                            while *s.add(n) != 0 {
                                n += 1;
                            }
                            let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
                            LocalFree(s.cast());
                            out
                        } else {
                            "err".to_string()
                        }
                    }
                } else {
                    "none".to_string()
                }
            }
        };
        CloseHandle(token);
        format!("il={il} is_appcontainer={} ac_sid={ac_sid}", is_ac_raw != 0)
    }
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
    use super::{AppContainerLaunch, PlainLaunch, WindowsStdio, dedupe_windows_env_pairs};
    use std::collections::BTreeMap;
    use std::io;
    use std::io::Write as _;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::ExitStatus;
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::{
        CloseHandle, FILETIME, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
        SetHandleInformation, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
        NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, TRUSTEE_IS_SID,
        TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeleteAppContainerProfile,
        DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{
        ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, FreeSid, GetLengthSid,
        GetSecurityDescriptorControl, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID,
        SE_DACL_PROTECTED, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
    };
    use windows_sys::Win32::System::Console::{CONSOLE_MODE, GetConsoleMode};
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
        JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
        GetProcessTimes, InitializeProcThreadAttributeList, OpenProcess,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        UpdateProcThreadAttribute, WaitForSingleObject,
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
    // internetClientServer + privateNetworkClientServer. Granted to the co-package egress-funnel
    // HELPER alongside internetClient so its loopback bind/accept is never the variable under test
    // — the exact cap set the proven funnel harness gave the helper. The confined CHILD still holds
    // ZERO capabilities; these widen the trusted helper, not the sandboxed principal.
    const INTERNET_CLIENT_SERVER_SID: &str = "S-1-15-3-2";
    const PRIVATE_NETWORK_CLIENT_SERVER_SID: &str = "S-1-15-3-3";
    // An app-package-readable working directory for a LowBox process — a LowBox cannot resolve
    // nub's own user-profile cwd. `System32` carries ALL APPLICATION PACKAGES read (measured), so
    // the egress-funnel helper (which needs no policy grants of its own) starts there.
    const APP_PACKAGE_READABLE_CWD: &str = "C:\\Windows\\System32";
    // ALL APPLICATION PACKAGES. Any right for this SID invalidates the default-deny
    // AppContainer assumption for that path.
    const ALL_APPLICATION_PACKAGES_SID: &str = "S-1-15-2-1";

    /// Verify that `cwd` is rooted beneath a protected DACL and that neither it nor any
    /// ancestor up to that boundary grants ALL APPLICATION PACKAGES access. Inherited AAP
    /// access would otherwise let the child reach files nub never granted.
    ///
    /// ⛔ AAP IS NOT THE ONLY SID THAT GRANTS A LOWBOX CHILD, SO THIS IS A NARROWER
    /// PRECONDITION THAN "the allowlist is default-deny" — which is what this comment used to
    /// claim, and it was never true. Measured on Windows Server 2022 (20348.5499), each arm
    /// with its capability-free negative control:
    ///   - `ALL RESTRICTED APPLICATION PACKAGES` (`S-1-15-2-2`) grants a plain non-LPAC
    ///     AppContainer holding ZERO capabilities. That follows from the token model — a
    ///     regular AppContainer is a member of both AAP and ARAP, and only an LPAC drops AAP —
    ///     and the kernel synthesises the match during the access check rather than
    ///     materialising either SID as a token group, so it cannot be detected by enumerating
    ///     the child's token.
    ///   - A CAPABILITY ace grants whenever the token holds that capability. nub's own token
    ///     holds `internetClient` (`S-1-15-3-1`) on every egress-allowed launch, so an
    ///     `S-1-15-3-1` ace on the working root is reach this scan does not see.
    ///
    /// ⛔ SCANNING FOR THOSE TOO WAS CONSIDERED AND REJECTED, DELIBERATELY. A hit here returns a
    /// `fs-root` degradation, which makes the install REFUSE the package — so widening the scan
    /// buys a smaller residual at the price of refusing to build on a tree nub does not
    /// understand. This jail is defence in depth, and a package that cannot install is a worse
    /// outcome than a residual. The prevalence that settles it, measured the same day: across
    /// 60 directories of a real project tree, the user profile, `%LOCALAPPDATA%`, `C:\` and
    /// `C:\Users`, the count of ARAP and `S-1-15-3-*` aces was ZERO — they appear only under
    /// `Program Files` and the OS-owned roots, which are not working roots. The same scan found
    /// 18 of 20 `Program Files` directories carrying both, so it was capable of seeing them.
    /// ⇒ Widening would refuse installs to remove a residual nothing was hitting. Revisit if a
    /// real working root is ever measured carrying one.
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
    pub(crate) fn verify_clean_root(cwd: &Path, published: &[PathBuf]) -> io::Result<()> {
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

    /// Journal a profile-SID grant before writing it through the same object handle.
    /// Existing public AAP grants are neither journaled nor revoked as profile grants.
    fn grant_recorded_ace(
        resource: &mut super::windows_registry::Acquired,
        path: &Path,
        sid: PSID,
        grant: (super::windows_registry::AclKind, u32),
        admitted: bool,
        optional: bool,
    ) -> io::Result<()> {
        use super::windows_registry::{AclKind, AclMutation, object_handle_id};
        let (kind, access) = grant;
        if kind != AclKind::Object && already_granted_to_appcontainers(path, access) {
            return Ok(());
        }
        let file = match open_acl_file(path) {
            Ok(file) => file,
            Err(_) if optional => return Ok(()),
            Err(error) => return acquisition_step("file-open", Err(error)),
        };
        let id = acquisition_step("file-identity", object_handle_id(file.as_raw_handle()))?;
        if admitted {
            acquisition_step(
                "file-admission",
                resource.validate_admitted_object(path, &id),
            )?;
        }
        acquisition_step(
            "file-journal",
            resource.record_mutation_id(
                AclMutation {
                    path: path.to_string_lossy().into_owned(),
                    kind,
                    access,
                },
                Some(id),
            ),
        )?;
        // The same open object is journaled and mutated even if its name changes.
        let result = set_ace_on_handle(
            file.as_raw_handle(),
            sid,
            access,
            GRANT_ACCESS,
            kind != AclKind::Object,
            kind != AclKind::Object,
        );
        if optional {
            Ok(())
        } else {
            acquisition_step("file-grant", result)
        }
    }

    /// See [`super::windows_object_traverse_ace`].
    #[doc(hidden)]
    pub(crate) fn object_traverse_ace(dir: &Path, sddl: &str, grant: bool) -> io::Result<()> {
        let sid = CapSid::new(sddl)?;
        let mode = if grant { GRANT_ACCESS } else { REVOKE_ACCESS };
        set_ace_on_object(dir, sid.0, TRAVERSE_MASK, mode)
    }

    /// See [`super::windows_leaf_grant_redundant`].
    #[doc(hidden)]
    pub(crate) fn leaf_read_grant_redundant(dir: &Path) -> bool {
        already_granted_to_appcontainers(dir, GENERIC_READ | GENERIC_EXECUTE)
    }

    /// See [`super::windows_publish_appcontainer_read`].
    #[doc(hidden)]
    pub(crate) fn publish_appcontainer_read(dir: &Path) -> io::Result<()> {
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
            .chain(launch.read_node_grants.iter())
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
        let file = open_acl_file(path)?;
        set_ace_on_handle(file.as_raw_handle(), sid, access, mode, false, false)
    }

    fn open_acl_file(path: &Path) -> io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        std::fs::OpenOptions::new()
            .access_mode(0x0002_0000 | 0x0004_0000) // READ_CONTROL | WRITE_DAC
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    fn acl_error(stage: impl std::fmt::Display, error: io::Error) -> io::Error {
        io::Error::new(error.kind(), format!("{stage}: {error}"))
    }

    fn set_ace_on_handle(
        handle: HANDLE,
        sid: PSID,
        access: u32,
        mode: i32,
        inherit: bool,
        propagate: bool,
    ) -> io::Result<()> {
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SetSecurityInfo};
        use windows_sys::Win32::Security::{
            InitializeSecurityDescriptor, SE_DACL_AUTO_INHERITED, SECURITY_DESCRIPTOR,
            SetKernelObjectSecurity, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        };
        const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
        const CARRIED_CONTROL: u16 = SE_DACL_AUTO_INHERITED | SE_DACL_PROTECTED;

        let _lock = super::windows_registry::OperationLock::acquire("acl")?;
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
            return Err(acl_error(
                "GetSecurityInfo",
                io::Error::from_raw_os_error(rc as i32),
            ));
        }
        let _sd = LocalFreeGuard(sd);
        // A NULL DACL is already unrestricted; replacing it with only our grant
        // would change other principals' access and cannot be undone by SID removal.
        if old_dacl.is_null() {
            return Ok(());
        }
        if mode == REVOKE_ACCESS {
            let mut found = false;
            for_each_ace_of_sid(old_dacl, sid, Path::new("<opened-object>"), |_, _, _| {
                found = true;
            })?;
            if !found {
                return Ok(());
            }
        }

        let mut control = 0u16;
        let mut revision = 0u32;
        if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
            return Err(acl_error(
                "GetSecurityDescriptorControl",
                io::Error::last_os_error(),
            ));
        }

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
        // Merges into the aces already there, INHERITED_ACE flags intact, so the descriptor
        // written below differs from the one read above by exactly this one ace.
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let rc = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
        if rc != 0 {
            return Err(acl_error(
                "SetEntriesInAclW",
                io::Error::from_raw_os_error(rc as i32),
            ));
        }
        let _new = LocalFreeGuard(new_dacl.cast());

        if propagate {
            let rc = unsafe {
                SetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    new_dacl,
                    std::ptr::null_mut(),
                )
            };
            return if rc == 0 {
                Ok(())
            } else {
                Err(acl_error(
                    "SetSecurityInfo",
                    io::Error::from_raw_os_error(rc as i32),
                ))
            };
        }

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
                return Err(acl_error(
                    "SetKernelObjectSecurity/descriptor setup",
                    io::Error::last_os_error(),
                ));
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

    struct ResourceState {
        // The command keeps this Arc alive through final Job reaping, including when
        // the caller closes its WindowsResource while commands are still running.
        _lease: super::windows_registry::Acquired,
        sid: SidGuard,
        private_tmp: Option<PathBuf>,
        window_objects: Vec<super::windows_registry::WindowObject>,
        native_compat: Option<PathBuf>,
    }

    impl Drop for ResourceState {
        fn drop(&mut self) {
            let result = (|| {
                let _operation = super::windows_registry::OperationLock::acquire("resources")?;
                self._lease.close()?;
                recover_idle_resources(false, false)
            })();
            if let Err(error) = result {
                // Drop cannot return an error, but failed cleanup must remain
                // journaled and visible rather than become silent idle retention.
                tracing::warn!(%error, "sandbox resource close requires cleanup");
            }
        }
    }

    pub(crate) struct WindowsResource {
        plan: PlainLaunch,
        state: Option<Arc<ResourceState>>,
        allow_internet: bool,
        egress_funnel: Option<super::NetPolicy>,
    }

    /// Shared identity ownership without a command, arguments or environment.
    #[derive(Clone)]
    pub(crate) struct WindowsLease {
        _state: Arc<ResourceState>,
    }

    #[cfg(test)]
    impl WindowsLease {
        pub(crate) fn is_live(&self) -> bool {
            self._state._lease.has_live_lease()
        }

        #[cfg(test)]
        pub(crate) fn shares_resource(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self._state, &other._state)
        }
    }

    fn acquisition_step<T>(stage: &str, result: io::Result<T>) -> io::Result<T> {
        result.inspect_err(|error| {
            tracing::warn!(stage, %error, "sandbox resource acquisition failed");
            #[cfg(test)]
            eprintln!(
                "WINDOWS_ACQUIRE_ERROR {} {stage}: {error:?}",
                std::process::id()
            );
        })
    }

    impl AppContainerLaunch {
        #[cfg(test)]
        pub(crate) fn acquire(self) -> io::Result<WindowsResource> {
            self.acquire_reusing(&BTreeMap::new())
        }

        pub(crate) fn acquire_reusing(
            mut self,
            retained: &BTreeMap<String, WindowsLease>,
        ) -> io::Result<WindowsResource> {
            if let Some(env) = self.env.as_mut() {
                acquisition_step("environment", ensure_appcontainer_environment(env))?;
            }
            let identity = acquisition_step(
                "identity",
                timed("resource_identity", || reusable_identity(&self)),
            )?;
            let window_objects = acquisition_step(
                "window-objects",
                crate::backend::windows_ace::current_objects(),
            )?;
            if let Some(lease) = retained.get(&identity.hash) {
                let same_windows = acquisition_step(
                    "retained-validation",
                    timed("resource_cache_hit", || {
                        super::windows_registry::validate_entry(&lease._state._lease.entry)?;
                        Ok::<_, io::Error>(lease._state.window_objects == window_objects)
                    }),
                )?;
                if same_windows {
                    return Ok(self.bind(Arc::clone(&lease._state)));
                }
            }
            // An equivalent retained resource already passed this check. Repeating
            // it on a hit would rewrite the protected registry DACL per grant.
            for path in self.read_grants.iter().chain(&self.write_grants) {
                acquisition_step(
                    "registry-grant",
                    super::windows_registry::reject_registry_grant(path),
                )?;
            }
            let _operation = acquisition_step(
                "resource-lock",
                super::windows_registry::OperationLock::acquire("resources"),
            )?;
            acquisition_step("idle-recovery", recover_idle_resources(false, true))?;
            let ancestors = if std::env::var_os("NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR").is_some() {
                Vec::new()
            } else {
                ancestor_chain(&self, None)
            };
            let identity = acquisition_step(
                "object-identities",
                identity.with_objects(
                    self.read_grants
                        .iter()
                        .chain(&self.write_grants)
                        .chain(&self.read_node_grants)
                        .chain(&ancestors)
                        .cloned(),
                ),
            )?;
            let mut resource = acquisition_step(
                "registry-acquire",
                super::windows_registry::acquire(identity),
            )?;
            if !resource.fresh {
                acquisition_step(
                    "entry-validation",
                    super::windows_registry::validate_entry(&resource.entry),
                )?;
            }
            let name = resource.entry.profile_name.clone();
            let sid = SidGuard(if resource.fresh {
                acquisition_step("profile-create", create_appcontainer(&name))?
            } else {
                acquisition_step("profile-derive", derive_appcontainer(&name))?
            });
            let ac_sid = sid.0;
            let profile_folder = acquisition_step("profile-folder", appcontainer_folder(ac_sid))?;
            #[cfg(test)]
            test_crash_transition("profile-created", &name, &profile_folder);
            let private_tmp = self.private_tmp.then(|| profile_folder.join("Temp"));
            let native_compat = self
                .native_compat
                .then(|| crate::backend::windows_native_compat::asset_path(&name))
                .transpose()?;
            if resource.fresh {
                if let Some(path) = &native_compat {
                    acquisition_step(
                        "native-assets",
                        crate::backend::windows_native_compat::install(&mut resource, path),
                    )?;
                    grant_recorded_ace(
                        &mut resource,
                        path,
                        ac_sid,
                        (
                            super::windows_registry::AclKind::Subtree,
                            GENERIC_READ | GENERIC_EXECUTE,
                        ),
                        false,
                        false,
                    )?;
                    #[cfg(test)]
                    test_crash_transition("native-assets-granted", &name, path);
                }
                acquisition_step(
                    "profile-journal",
                    resource.record_private_path(&profile_folder),
                )?;
                if let Some(path) = &private_tmp {
                    acquisition_step(
                        "profile-temp-journal",
                        resource.record_mutation(super::windows_registry::AclMutation {
                            path: path.to_string_lossy().into_owned(),
                            kind: super::windows_registry::AclKind::PrivateProfile,
                            access: GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                        }),
                    )?;
                    acquisition_step("profile-temp-create", std::fs::create_dir_all(path))?;
                    #[cfg(test)]
                    test_crash_transition("private-root-created", &name, &profile_folder);
                    grant_recorded_ace(
                        &mut resource,
                        path,
                        ac_sid,
                        (
                            super::windows_registry::AclKind::PrivateProfile,
                            GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                        ),
                        false,
                        false,
                    )?;
                }
            }
            // Window objects are session-local, whereas profiles are user-global.
            // Journal each session's station/desktop before changing either DACL.
            for object in &window_objects {
                acquisition_step(
                    "window-journal",
                    resource.record_window_object(object.clone()),
                )?;
                acquisition_step(
                    "window-grant",
                    crate::backend::windows_ace::grant_persistent(object, ac_sid),
                )?;
            }

            if resource.fresh {
                let private = self.env.as_ref().and_then(|env| {
                    env.iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("LOCALAPPDATA"))
                        .map(|(_, value)| PathBuf::from(value).join("Packages").join(&name))
                });
                if let Some(dir) = &private {
                    // This profile name is exclusively owned by this registry entry.
                    acquisition_step(
                        "redirected-profile-journal",
                        resource.record_private_path(dir),
                    )?;
                    for path in [dir.clone(), dir.join("AC"), dir.join("AC/Temp")] {
                        acquisition_step(
                            "redirected-profile-acl-journal",
                            resource.record_mutation(super::windows_registry::AclMutation {
                                path: path.to_string_lossy().into_owned(),
                                kind: super::windows_registry::AclKind::PrivateProfile,
                                access: GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                            }),
                        )?;
                        acquisition_step(
                            "redirected-profile-create",
                            std::fs::create_dir_all(&path),
                        )?;
                        grant_recorded_ace(
                            &mut resource,
                            &path,
                            ac_sid,
                            (
                                super::windows_registry::AclKind::PrivateProfile,
                                GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                            ),
                            false,
                            false,
                        )?;
                    }
                }
                for dir in &self.publishable_grants {
                    if dir.exists() && !leaf_read_grant_redundant(dir) {
                        let _ = publish_appcontainer_read(dir);
                    }
                }
                let fail_closed =
                    std::env::var_os("NUB_SANDBOX_WIN_FAIL_CLOSED_READ_GRANTS").is_some();
                for (dir, access, required) in self
                    .read_grants
                    .iter()
                    .map(|dir| (dir, GENERIC_READ | GENERIC_EXECUTE, false))
                    .chain(self.write_grants.iter().map(|dir| {
                        (
                            dir,
                            GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                            true,
                        )
                    }))
                {
                    if !dir.exists() && !required {
                        continue;
                    }
                    grant_recorded_ace(
                        &mut resource,
                        dir,
                        ac_sid,
                        (super::windows_registry::AclKind::Subtree, access),
                        true,
                        !(required || fail_closed),
                    )?;
                }
                for dir in self.read_node_grants.iter().chain(&ancestors) {
                    if !dir.exists() {
                        continue;
                    }
                    // Missing optional read/traverse rights can only over-confine.
                    grant_recorded_ace(
                        &mut resource,
                        dir,
                        ac_sid,
                        (super::windows_registry::AclKind::Object, TRAVERSE_MASK),
                        true,
                        true,
                    )?;
                }
                if std::env::var_os("NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR").is_none() {
                    for dir in ancestor_chain(&self, private.as_deref()) {
                        if !ancestors.contains(&dir) && dir.exists() {
                            grant_recorded_ace(
                                &mut resource,
                                &dir,
                                ac_sid,
                                (super::windows_registry::AclKind::Object, TRAVERSE_MASK),
                                false,
                                true,
                            )?;
                        }
                    }
                }
                #[cfg(test)]
                test_crash_transition("acl-installed-before-ready", &name, &profile_folder);
                acquisition_step("ready", resource.ready())?;
            }
            Ok(self.bind(Arc::new(ResourceState {
                _lease: resource,
                sid,
                private_tmp,
                window_objects,
                native_compat,
            })))
        }

        fn bind(self, state: Arc<ResourceState>) -> WindowsResource {
            WindowsResource {
                plan: PlainLaunch {
                    program: self.program,
                    args: self.args,
                    cwd: self.cwd,
                    env: self.env,
                    stdout: self.stdout,
                    stderr: self.stderr,
                },
                allow_internet: self.allow_internet,
                egress_funnel: self.egress_funnel,
                state: Some(state),
            }
        }
    }

    impl WindowsResource {
        pub(super) fn plain(plan: PlainLaunch) -> Self {
            Self {
                plan,
                state: None,
                allow_internet: false,
                egress_funnel: None,
            }
        }

        pub(crate) fn identity(&self) -> Option<&str> {
            self.state.as_ref().map(|state| {
                let entry = &state._lease.entry;
                entry.policy_identity.as_deref().unwrap_or(&entry.identity)
            })
        }

        pub(crate) fn lease(&self) -> Option<WindowsLease> {
            self.state.as_ref().map(|state| WindowsLease {
                _state: state.clone(),
            })
        }
        #[cfg(test)]
        pub(crate) fn private_tmp(&self) -> Option<&Path> {
            self.state
                .as_ref()
                .and_then(|state| state.private_tmp.as_deref())
        }
        #[cfg(test)]
        pub(crate) fn profile_name(&self) -> &str {
            &self
                .state
                .as_ref()
                .expect("AppContainer resource")
                ._lease
                .entry
                .profile_name
        }

        pub(crate) fn spawn(&self) -> io::Result<WindowsChild> {
            #[cfg(test)]
            if std::env::var_os("NUB_NATIVE_ADAPTER_PROBE_ENABLE").is_some() {
                return self.spawn_before_resume(
                    WindowsStdio::Inherit,
                    self.plan.stdout,
                    self.plan.stderr,
                    crate::backend::windows_native_adapter_probe::inject_probe,
                );
            }
            self.spawn_with_stdio(WindowsStdio::Inherit, self.plan.stdout, self.plan.stderr)
        }

        pub(crate) fn spawn_with_stdio(
            &self,
            stdin: WindowsStdio,
            stdout: WindowsStdio,
            stderr: WindowsStdio,
        ) -> io::Result<WindowsChild> {
            self.spawn_before_resume(stdin, stdout, stderr, |_| Ok(()))
        }

        pub(crate) fn spawn_before_resume(
            &self,
            stdin: WindowsStdio,
            stdout: WindowsStdio,
            stderr: WindowsStdio,
            before_resume: impl FnOnce(u32) -> io::Result<()>,
        ) -> io::Result<WindowsChild> {
            let mut plan = self.plan.clone();
            let confined = self.state.is_some();
            let ac_sid = self
                .state
                .as_ref()
                .map_or(std::ptr::null_mut(), |state| state.sid.0);
            if let Some(path) = self
                .state
                .as_ref()
                .and_then(|state| state.private_tmp.as_ref())
            {
                let env = plan.env.get_or_insert_with(|| std::env::vars().collect());
                env.retain(|key, _| {
                    !["TEMP", "TMP", "TMPDIR"]
                        .iter()
                        .any(|name| key.eq_ignore_ascii_case(name))
                });
                let path = super::strip_verbatim_prefix(path.clone())
                    .to_string_lossy()
                    .into_owned();
                for key in ["TEMP", "TMP", "TMPDIR"] {
                    env.insert(key.to_string(), path.clone());
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
            let job = create_process_job(confined)?;
            let job_guard = HandleGuard(job);

            // 5. Proc-thread attribute list: SECURITY_CAPABILITIES, plus a HANDLE_LIST
            //    scoping inheritance to EXACTLY the std handles (see `bInheritHandles`
            //    below). The list must be alive across CreateProcessW (it stores the
            //    pointer); `inherit_handles` outlives the call.
            let mut stdio = NativeStdio::new([stdin, stdout, stderr], confined)?;
            let std_triple = stdio.triple;
            let inherit_handles = &stdio.child_handles;
            let n_attrs = 1 + u32::from(confined) + u32::from(!inherit_handles.is_empty());
            #[cfg(test)]
            let relocation_probe =
                confined && std::env::var_os("NUB_NATIVE_RELOCATION_PROBE").is_some();
            #[cfg(test)]
            let n_attrs = n_attrs + u32::from(relocation_probe);
            let jobs = [job];
            let mut attr = ProcThreadAttrList::new(n_attrs)?;
            #[cfg(test)]
            let mut relocation_policy = 0x0000_0200_u64;
            #[cfg(test)]
            if relocation_probe {
                // Test-only discriminator for MSYS's fixed-address shared state.
                // SDK: PROCESS_CREATION_MITIGATION_POLICY_FORCE_RELOCATE_IMAGES_ALWAYS_OFF.
                // This does not change release builds or the AppContainer token.
                attr.update(
                    windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY
                        as usize,
                    std::ptr::from_mut(&mut relocation_policy).cast(),
                    std::mem::size_of_val(&relocation_policy),
                )?;
                eprintln!("MSYS_RELOCATION_PROBE root policy={relocation_policy:#x}");
            }
            // The attribute list stores a POINTER to `sec_caps` rather than a copy, so it must
            // stay live until CreateProcessW returns.
            if confined {
                attr.update(
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    std::ptr::from_mut(&mut sec_caps).cast(),
                    std::mem::size_of::<SECURITY_CAPABILITIES>(),
                )?;
            }
            attr.update(
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr().cast_mut().cast(),
                std::mem::size_of_val(&jobs),
            )?;
            if !inherit_handles.is_empty() {
                attr.update(
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    stdio.inherit_list.as_ptr().cast_mut().cast(),
                    std::mem::size_of::<HANDLE>() * inherit_handles.len(),
                )?;
            }

            // 5c. THE ZERO-PRIVILEGE EGRESS FUNNEL. Launch a CO-PACKAGE helper process — SAME
            //     AppContainer SID (`ac_sid`), holding `internetClient` — that runs nub's egress
            //     proxy over `plan.egress_funnel`'s policy, then point THIS (capability-free) child
            //     at it via `HTTP_PROXY`. The child reaches the helper by SAME-PACKAGE loopback,
            //     which needs NO admin loopback exemption (the `IsAppContainerLoopback` kernel
            //     permit), so no machine-wide firewall mutation is needed.
            //
            //     Ordered here, AFTER the window-station ACE (1b): the helper shares `ac_sid`, so
            //     that ACE is what lets a USER32-importing nub.exe survive loader init on a non-
            //     interactive station. The proxy port/token exist only now, so the child's proxy
            //     env is injected here rather than in `apply`'s `build_child_env`. `_egress_helper`
            //     holds the helper in a KILL_ON_JOB_CLOSE job dropped when `run` returns (after the
            //     child is waited + reaped below), so the helper lives exactly the child's lifetime
            //     and dies with nub even on a crash.
            let _egress_helper = if let Some(policy) = &self.egress_funnel {
                let (port, token, guard) = timed("egress_funnel_helper", || {
                    launch_egress_helper(ac_sid, policy, plan.env.as_ref())
                })?;
                if let Some(env) = plan.env.as_mut() {
                    let url = format!("http://{token}@127.0.0.1:{port}");
                    for key in [
                        "HTTP_PROXY",
                        "HTTPS_PROXY",
                        "http_proxy",
                        "https_proxy",
                        "ALL_PROXY",
                        "npm_config_proxy",
                        "npm_config_https_proxy",
                    ] {
                        env.insert(key.to_string(), url.clone());
                    }
                    // A bypass var surviving here would route the child AROUND the proxy — the OS
                    // still blocks that (no `internetClient`), but it turns a clean proxy-403 into
                    // an opaque connect failure. Drop them, exactly as `backend::set_proxy_env`.
                    for key in ["NO_PROXY", "no_proxy", "npm_config_noproxy"] {
                        env.remove(key);
                    }
                    env.insert("NODE_USE_ENV_PROXY".to_string(), "1".to_string());
                }
                Some(guard)
            } else {
                None
            };

            // 6. Build the command line + env block + cwd (kept alive across the call).
            let (application, mut cmdline) = if confined {
                (None, build_command_line(&plan.program, &plan.args))
            } else {
                let application = resolve_plain_image(&plan)?;
                let batch = application.extension().is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
                });
                if batch {
                    let cmd = windows_directory(true)?.join("cmd.exe");
                    (
                        Some(to_wide_path(&cmd)),
                        build_batch_command_line(&application, &plan.args)?,
                    )
                } else {
                    (
                        Some(to_wide_path(&application)),
                        build_command_line(&plan.program, &plan.args),
                    )
                }
            };
            let env_block = plan.env.as_ref().map(build_env_block);
            let cwd_wide = plan.cwd.as_ref().map(|c| to_wide(&c.to_string_lossy()));

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
            let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED;
            if confined {
                flags |= CREATE_NO_WINDOW;
            }
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
                    application
                        .as_ref()
                        .map_or(std::ptr::null(), |path| path.as_ptr()),
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
            // A recovery may briefly borrow this process's window station to open a recorded
            // desktop.  Creating an AppContainer child during that interval would bind USER32 to
            // the wrong station, so share the in-process station lock with that borrow.
            let result = {
                let _station = crate::backend::windows_ace::station_guard();
                timed("native_spawn", || {
                    if launch() == 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                })
            };
            if let Err(error) = result {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "CreateProcessW ({}) failed: {error}",
                        if confined { "AppContainer" } else { "plain" }
                    ),
                ));
            }
            let _ = &cap_sid_owned; // backs `sec_caps` through attribute-list destruction

            // Handles become owned before the first fallible operation after spawn.
            // JOB_LIST already attached the suspended process inside CreateProcessW.
            let process = HandleGuard(pi.hProcess);
            let thread = HandleGuard(pi.hThread);
            stdio.child_handles.clear();
            let relay_threads = spawn_relays(std::mem::take(&mut stdio.relays));
            let mut child = WindowsChild {
                process,
                job: job_guard,
                pid: pi.dwProcessId,
                stdin: stdio.stdin.take(),
                stdout: stdio.stdout.take(),
                stderr: stdio.stderr.take(),
                relays: relay_threads,
                helper: _egress_helper,
                _resource: self.state.clone(),
                status: None,
                tracked: Vec::new(),
                last_exit: None,
            };
            before_resume(child.pid)?;
            if let Some(path) = self
                .state
                .as_ref()
                .and_then(|state| state.native_compat.as_deref())
            {
                crate::backend::windows_native_compat::inject(child.process.0, path)?;
            }
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                let error = io::Error::last_os_error();
                let _ = child.kill();
                return Err(error);
            }
            Ok(child)
        }
    }

    fn windows_directory(system: bool) -> io::Result<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        unsafe extern "system" {
            fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
            fn GetWindowsDirectoryW(buffer: *mut u16, size: u32) -> u32;
        }
        let mut buffer = vec![0u16; 32768];
        let len = unsafe {
            if system {
                GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32)
            } else {
                GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32)
            }
        } as usize;
        if len == 0 || len >= buffer.len() {
            return Err(io::Error::last_os_error());
        }
        Ok(std::ffi::OsString::from_wide(&buffer[..len]).into())
    }

    fn resolve_plain_image(plan: &PlainLaunch) -> io::Result<PathBuf> {
        let name = Path::new(&plan.program);
        if name.as_os_str().is_empty() || name.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "program path has no file name",
            ));
        }
        if name.components().count() != 1 {
            let path = std::path::absolute(name)?;
            if !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
            {
                let mut exe = path.as_os_str().to_os_string();
                exe.push(".exe");
                let exe = PathBuf::from(exe);
                if exe.is_file() {
                    return Ok(exe);
                }
            }
            return Ok(path);
        }
        // Match std's Windows search order, including a command's replaced PATH.
        let mut roots = Vec::new();
        if let Some(path) = plan.env.as_ref().and_then(|env| {
            env.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
                .map(|(_, value)| value)
        }) {
            roots.extend(std::env::split_paths(path));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            roots.push(parent.to_path_buf());
        }
        roots.push(windows_directory(true)?);
        roots.push(windows_directory(false)?);
        if let Some(path) = std::env::var_os("PATH") {
            roots.extend(std::env::split_paths(&path));
        }
        for root in roots
            .into_iter()
            .filter(|root| !root.as_os_str().is_empty())
        {
            let mut path = root.join(name);
            if !plan.program.as_encoded_bytes().contains(&b'.') {
                path.set_extension("exe");
            }
            if path.is_file() {
                return std::path::absolute(path);
            }
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "program not found"))
    }

    // Match std's batch-file escaping, rather than treating cmd syntax as argv.
    // In particular, percent expansion and embedded quotes require cmd's rules.
    fn build_batch_command_line(
        script: &Path,
        args: &crate::backend::CommandArgs,
    ) -> io::Result<Vec<u16>> {
        let script = super::strip_verbatim_prefix(script.to_path_buf());
        let script: Vec<u16> = script.as_os_str().encode_wide().collect();
        if script.contains(&u16::from(b'"')) || script.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid batch script path",
            ));
        }
        let mut line: Vec<u16> = "cmd.exe /e:ON /v:OFF /d /c \"\"".encode_utf16().collect();
        line.extend(script);
        line.push(u16::from(b'"'));
        match args {
            crate::backend::CommandArgs::Verbatim(raw) => {
                line.push(u16::from(b' '));
                line.extend(raw.encode_wide());
            }
            crate::backend::CommandArgs::Argv(args) => {
                for arg in args {
                    let chars: Vec<u16> = arg.encode_wide().collect();
                    if chars.iter().any(|c| matches!(*c, 0 | 10 | 13)) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "invalid batch argument",
                        ));
                    }
                    let quote = chars.is_empty()
                        || chars.last() == Some(&u16::from(b'\\'))
                        || char::decode_utf16(chars.iter().copied())
                            .filter_map(Result::ok)
                            .any(|c| {
                                (c.is_ascii()
                                    && !(c.is_ascii_alphanumeric() || r"#$*+-./:?@\_".contains(c)))
                                    || c.is_control()
                            });
                    line.push(u16::from(b' '));
                    if quote {
                        line.push(u16::from(b'"'));
                    }
                    let mut slashes = 0;
                    for c in chars {
                        if c == u16::from(b'\\') {
                            slashes += 1;
                        } else {
                            if c == u16::from(b'"') {
                                line.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
                                line.push(u16::from(b'"'));
                            } else if c == u16::from(b'%') {
                                line.extend("%%cd:~,".encode_utf16());
                            }
                            slashes = 0;
                        }
                        line.push(c);
                    }
                    if quote {
                        line.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
                        line.push(u16::from(b'"'));
                    }
                }
            }
        }
        line.extend([u16::from(b'"'), 0]);
        Ok(line)
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

    /// Closes a raw handle on drop. For the Job handle this triggers
    /// KILL_ON_JOB_CLOSE — reaping any process still in the tree.
    struct HandleGuard(HANDLE);
    // SAFETY: an owned kernel handle has no thread affinity. Closing it requires
    // exclusive ownership; all shared operations are kernel-synchronized queries.
    unsafe impl Send for HandleGuard {}
    unsafe impl Sync for HandleGuard {}
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

    struct NativeStdio {
        triple: [HANDLE; 3],
        inherit_list: Vec<HANDLE>,
        child_handles: Vec<OwnedHandle>,
        stdin: Option<std::process::ChildStdin>,
        stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
        relays: Vec<(std::io::PipeReader, RelayTarget)>,
    }

    fn is_console_handle(h: HANDLE) -> bool {
        let mut mode: CONSOLE_MODE = 0;
        unsafe { GetConsoleMode(h, &mut mode) != 0 }
    }

    fn inheritable_duplicate(handle: HANDLE) -> io::Result<OwnedHandle> {
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut duplicate = std::ptr::null_mut();
        let current = unsafe { GetCurrentProcess() };
        if unsafe {
            DuplicateHandle(
                current,
                handle,
                current,
                &mut duplicate,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: DuplicateHandle returns a new uniquely owned handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(duplicate) })
    }

    /// Standard child streams require overlapped parent handles on Windows.
    /// `std::io::pipe` uses synchronous CreatePipe handles, which cannot satisfy
    /// ChildStdin/ChildStdout's ReadFileEx/WriteFileEx completion contract.
    fn child_stdio_pipe(ours_readable: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
        use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_PIPE_CONNECTED};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
            PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
        };
        use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
        use windows_sys::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS,
        };
        use windows_sys::Win32::System::Threading::CreateEventW;

        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = to_wide(&format!(
            r"\\.\pipe\nub-stdio-{}-{nonce}",
            std::process::id()
        ));
        let server = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                FILE_FLAG_OVERLAPPED
                    | FILE_FLAG_FIRST_PIPE_INSTANCE
                    | if ours_readable {
                        PIPE_ACCESS_INBOUND
                    } else {
                        PIPE_ACCESS_OUTBOUND
                    },
                PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null(),
            )
        };
        if server == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: each successful creation returns a uniquely owned handle.
        let ours = unsafe { OwnedHandle::from_raw_handle(server) };
        let client = unsafe {
            CreateFileW(
                name.as_ptr(),
                if ours_readable {
                    GENERIC_WRITE
                } else {
                    GENERIC_READ
                },
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if client == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let client = unsafe { OwnedHandle::from_raw_handle(client) };
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let event = HandleGuard(event);
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.0;
        if unsafe { ConnectNamedPipe(server, &mut overlapped) } == 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error().map(|code| code as u32) {
                Some(ERROR_PIPE_CONNECTED) => {}
                Some(ERROR_IO_PENDING) => {
                    let mut transferred = 0;
                    if unsafe { GetOverlappedResult(server, &overlapped, &mut transferred, 1) } == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                _ => return Err(error),
            }
        }
        let theirs = inheritable_duplicate(client.as_raw_handle())?;
        Ok((ours, theirs))
    }

    impl NativeStdio {
        fn new(modes: [WindowsStdio; 3], relay_console: bool) -> io::Result<Self> {
            let mut result = Self {
                triple: [std::ptr::null_mut(); 3],
                inherit_list: Vec::new(),
                child_handles: Vec::new(),
                stdin: None,
                stdout: None,
                stderr: None,
                relays: Vec::new(),
            };
            let parent = [
                std::io::stdin().as_raw_handle(),
                std::io::stdout().as_raw_handle(),
                std::io::stderr().as_raw_handle(),
            ];
            for (index, mode) in modes.into_iter().enumerate() {
                let raw = parent[index];
                let relay = relay_console
                    && mode == WindowsStdio::Inherit
                    && index > 0
                    && is_console_handle(raw);
                let handle = if mode == WindowsStdio::Piped {
                    let (ours, theirs) = child_stdio_pipe(index != 0)?;
                    if index == 0 {
                        result.stdin = Some(ours.into());
                    } else if index == 1 {
                        result.stdout = Some(ours.into());
                    } else {
                        result.stderr = Some(ours.into());
                    }
                    theirs
                } else if relay {
                    let (reader, writer) = std::io::pipe()?;
                    let child = inheritable_duplicate(writer.as_raw_handle())?;
                    result.relays.push((
                        reader,
                        if index == 1 {
                            RelayTarget::Stdout
                        } else {
                            RelayTarget::Stderr
                        },
                    ));
                    child
                } else if mode == WindowsStdio::Null || raw.is_null() || raw == INVALID_HANDLE_VALUE
                {
                    let file = std::fs::OpenOptions::new()
                        .read(index == 0)
                        .write(index != 0)
                        .open("NUL")?;
                    inheritable_duplicate(file.as_raw_handle())?
                } else {
                    // Never toggle inheritance on a process-global standard handle.
                    inheritable_duplicate(raw)?
                };
                let raw = handle.as_raw_handle();
                result.triple[index] = raw;
                result.inherit_list.push(raw);
                result.child_handles.push(handle);
            }
            Ok(result)
        }
    }

    pub(crate) struct WindowsChild {
        process: HandleGuard,
        job: HandleGuard,
        pid: u32,
        stdin: Option<std::process::ChildStdin>,
        stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
        relays: Vec<std::thread::JoinHandle<()>>,
        helper: Option<HelperGuard>,
        _resource: Option<Arc<ResourceState>>,
        status: Option<ExitStatus>,
        tracked: Vec<(u32, HandleGuard)>,
        last_exit: Option<(u64, u32)>,
    }

    impl WindowsChild {
        pub(crate) fn id(&self) -> u32 {
            self.pid
        }
        pub(crate) fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
            self.stdin.take()
        }
        pub(crate) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
            self.stdout.take()
        }
        pub(crate) fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
            self.stderr.take()
        }

        pub(crate) fn kill(&mut self) -> io::Result<()> {
            if self.status.is_some() {
                return Ok(());
            }
            // Keep synchronization handles before termination removes members
            // from the Job's active list. Termination itself is asynchronous.
            let snapshot = self.track_job_members();
            if let Some(helper) = &self.helper {
                helper.terminate();
            }
            if unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.0, 1) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            snapshot
        }

        fn track_job_members(&mut self) -> io::Result<()> {
            const MAX_TRACKED: usize = 4096;
            let mut ids = vec![0usize; 2 + MAX_TRACKED];
            let listed = unsafe {
                QueryInformationJobObject(
                    self.job.0,
                    JobObjectBasicProcessIdList,
                    ids.as_mut_ptr().cast(),
                    std::mem::size_of_val(ids.as_slice()) as u32,
                    std::ptr::null_mut(),
                )
            };
            if listed == 0 {
                return Err(io::Error::last_os_error());
            }
            let list = ids.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            let members = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!((*list).ProcessIdList).cast::<usize>(),
                    ((*list).NumberOfProcessIdsInList as usize).min(MAX_TRACKED),
                )
            };
            for &pid in members {
                let pid = pid as u32;
                if pid == self.pid || self.tracked.iter().any(|(seen, _)| *seen == pid) {
                    continue;
                }
                let handle =
                    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
                if !handle.is_null() {
                    self.tracked.push((pid, HandleGuard(handle)));
                } else {
                    let error = io::Error::last_os_error();
                    // The member may have exited between enumeration and open.
                    if error.raw_os_error() != Some(87) {
                        return Err(error);
                    }
                }
            }
            Ok(())
        }

        pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            if let Some(status) = self.status {
                return Ok(Some(status));
            }
            // A lifecycle shell may exit before its trailing command. Sample
            // while the root is live too, and retain handles until signaled.
            self.track_job_members()?;
            match unsafe { WaitForSingleObject(self.process.0, 0) } {
                WAIT_OBJECT_0 => {}
                windows_sys::Win32::Foundation::WAIT_TIMEOUT => return Ok(None),
                _ => return Err(io::Error::last_os_error()),
            }
            let mut error = None;
            self.tracked.retain(|(_, process)| {
                match unsafe { WaitForSingleObject(process.0, 0) } {
                    WAIT_OBJECT_0 => {}
                    windows_sys::Win32::Foundation::WAIT_TIMEOUT => return true,
                    _ => {
                        error = Some(io::Error::last_os_error());
                        return true;
                    }
                }
                let mut code = 0;
                let mut creation: FILETIME = unsafe { std::mem::zeroed() };
                let mut exit = creation;
                let mut kernel = creation;
                let mut user = creation;
                if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0
                    || unsafe {
                        GetProcessTimes(process.0, &mut creation, &mut exit, &mut kernel, &mut user)
                    } == 0
                {
                    error = Some(io::Error::last_os_error());
                    return false;
                }
                let stamp = (u64::from(exit.dwHighDateTime) << 32) | u64::from(exit.dwLowDateTime);
                if self.last_exit.is_none_or(|(latest, _)| stamp > latest) {
                    self.last_exit = Some((stamp, code));
                }
                false
            });
            if let Some(error) = error {
                return Err(error);
            }
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                unsafe { std::mem::zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    self.job.0,
                    JobObjectBasicAccountingInformation,
                    std::ptr::from_mut(&mut accounting).cast(),
                    std::mem::size_of_val(&accounting) as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if accounting.ActiveProcesses != 0 || !self.tracked.is_empty() {
                return Ok(None);
            }
            let mut code = 0;
            if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if code == 0 {
                code = self.last_exit.map_or(0, |(_, code)| code);
            }
            if let Some(helper) = &self.helper {
                helper.terminate();
            }
            let status = ExitStatus::from_raw(code);
            self.status = Some(status);
            Ok(Some(status))
        }

        pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
            self.stdin.take();
            loop {
                if let Some(status) = self.try_wait()? {
                    self.helper.take();
                    for relay in self.relays.drain(..) {
                        let _ = relay.join();
                    }
                    return Ok(status);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    impl Drop for WindowsChild {
        fn drop(&mut self) {
            self.stdin.take();
            // The command's lease cannot be released before all its processes die.
            if self.status.is_none() {
                let _ = self.kill();
            }
            let _ = self.wait();
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

    fn reusable_identity(
        launch: &AppContainerLaunch,
    ) -> io::Result<super::windows_registry::PolicyIdentity> {
        let managed_profile = launch.env.as_ref().and_then(|env| {
            env.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("LOCALAPPDATA"))
                .map(|(_, value)| PathBuf::from(value).join("Packages"))
        });
        super::windows_registry::PolicyIdentity::new(
            launch.read_grants.clone(),
            launch.read_node_grants.clone(),
            launch.write_grants.clone(),
            managed_profile,
            launch.allow_internet,
            launch.egress_funnel.is_some(),
        )?
        .with_network(launch.egress_funnel.as_ref())
        .map(|identity| {
            identity
                .with_private_tmp(launch.private_tmp)
                .with_native_compat(
                    launch
                        .native_compat
                        .then(crate::backend::windows_native_compat::version),
                )
        })
    }

    /// Derive the stable SID for an already-created policy-named profile.  This is
    /// the documented AppContainer reopen path (and Chromium uses the same split);
    /// profile existence itself remains backed by the durable ownership journal.
    pub(super) fn derive_appcontainer(name: &str) -> io::Result<PSID> {
        let name = to_wide(name);
        let mut sid: PSID = std::ptr::null_mut();
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr != 0 {
            return Err(io::Error::other(format!(
                "DeriveAppContainerSidFromAppContainerName failed hr=0x{hr:08x}"
            )));
        }
        Ok(sid)
    }

    /// AppContainer process creation requires LOCALAPPDATA even with a scrubbed
    /// environment. Supply only the native known-folder path, before fingerprinting
    /// the resulting profile storage grants; never copy the ambient environment.
    fn ensure_appcontainer_environment(env: &mut BTreeMap<String, String>) -> io::Result<()> {
        use windows_sys::Win32::System::Com::CoTaskMemFree;
        use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};
        if env
            .keys()
            .any(|key| key.eq_ignore_ascii_case("LOCALAPPDATA"))
        {
            return Ok(());
        }
        let mut path = std::ptr::null_mut();
        let hr = unsafe {
            SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut path)
        };
        if hr < 0 {
            return Err(io::Error::other(format!(
                "SHGetKnownFolderPath(LocalAppData) failed hr=0x{hr:08x}"
            )));
        }
        let mut len = 0;
        unsafe {
            while *path.add(len) != 0 {
                len += 1;
            }
        }
        let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(path, len) });
        unsafe { CoTaskMemFree(path.cast()) };
        env.insert("LOCALAPPDATA".into(), value);
        Ok(())
    }

    fn appcontainer_folder(sid: PSID) -> io::Result<PathBuf> {
        use windows_sys::Win32::Security::Isolation::GetAppContainerFolderPath;
        use windows_sys::Win32::System::Com::CoTaskMemFree;
        let sid = unsafe { crate::backend::windows_ace::sid_to_string(sid) }?;
        let wide = to_wide(&sid);
        let mut path = std::ptr::null_mut();
        let hr = unsafe { GetAppContainerFolderPath(wide.as_ptr(), &mut path) };
        if hr != 0 {
            return Err(io::Error::other(format!(
                "GetAppContainerFolderPath failed hr=0x{hr:08x}"
            )));
        }
        let mut len = 0;
        unsafe {
            while *path.add(len) != 0 {
                len += 1;
            }
        }
        let folder = PathBuf::from(String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(path, len)
        }));
        unsafe { CoTaskMemFree(path.cast()) };
        Ok(folder)
    }

    /// A profile SID returned by create/derive is separately allocated from the
    /// persistent profile registration.  Closing this guard therefore cannot remove
    /// an identity another nub process is actively using.
    pub(super) struct SidGuard(pub(super) PSID);
    // SAFETY: the SID allocation is immutable until its sole owner's Drop.
    unsafe impl Send for SidGuard {}
    unsafe impl Sync for SidGuard {}
    impl Drop for SidGuard {
        fn drop(&mut self) {
            unsafe { FreeSid(self.0) };
        }
    }

    /// Registry transitions intentionally happen before new work.  The concrete
    /// profile/ACL cleanup is kept here, where a SID can be derived and only the
    /// journaled Nub ACEs are removed; no whole-DACL snapshot is ever restored.
    pub(crate) fn cleanup_resources() -> io::Result<()> {
        let _operation = super::windows_registry::OperationLock::acquire("resources")?;
        recover_idle_resources(true, false)
    }

    #[cfg(test)]
    pub(crate) fn test_crash_transition(stage: &str, profile: &str, private_root: &Path) {
        if !matches!(
            std::env::var("__NUB_WINDOWS_CLEANUP_FIXTURE").as_deref(),
            Ok("fault-acquire" | "fault-cleanup" | "window-witness-replacement-fault")
        ) || std::env::var("__NUB_WINDOWS_CLEANUP_FAULT").as_deref() != Ok(stage)
        {
            return;
        }
        let root = PathBuf::from(std::env::var_os("__NUB_WINDOWS_CLEANUP_ROOT").unwrap());
        let record = serde_json::to_vec(&(stage, profile, private_root)).unwrap();
        std::fs::write(root.join("crash-transition.json"), record).unwrap();
        // Process exit deliberately bypasses Rust destructors and releases native
        // locks/leases as an abruptly lost owner would.
        std::process::exit(91);
    }

    #[cfg(test)]
    pub(super) fn test_profile_has_ace(profile: &str, path: &Path) -> io::Result<bool> {
        let sid = SidGuard(derive_appcontainer(profile)?);
        path_has_sid(path, sid.0)
    }

    #[cfg(test)]
    pub(super) fn test_profile_has_window_grant(
        profile: &str,
        object: &super::windows_registry::WindowObject,
    ) -> io::Result<bool> {
        let sid = SidGuard(derive_appcontainer(profile)?);
        crate::backend::windows_ace::test_has_persistent_grant(object, sid.0)
    }

    #[cfg(test)]
    pub(super) fn test_set_profile_ace(profile: &str, path: &Path, grant: bool) -> io::Result<()> {
        let sid = SidGuard(derive_appcontainer(profile)?);
        if grant {
            set_ace(path, sid.0, 0x0012_0089, GRANT_ACCESS, false)
        } else {
            set_ace(path, sid.0, 0, REVOKE_ACCESS, false)
        }
    }

    #[cfg(test)]
    pub(super) fn test_set_profile_ace_on_handle(
        profile: &str,
        file: &std::fs::File,
        grant: bool,
    ) -> io::Result<()> {
        let sid = SidGuard(derive_appcontainer(profile)?);
        set_ace_on_handle(
            file.as_raw_handle(),
            sid.0,
            0x0012_0089,
            if grant { GRANT_ACCESS } else { REVOKE_ACCESS },
            false,
            true,
        )
    }

    fn recover_idle_resources(all: bool, reserve_slot: bool) -> io::Result<()> {
        let mut first_error = None;
        for entry in super::windows_registry::begin_recovery(all, reserve_slot)? {
            let result = (|| {
                let sid = derive_appcontainer(&entry.profile_name)?;
                let _sid = SidGuard(sid);
                for mutation in &entry.mutations {
                    revoke_recorded_ace(&entry, mutation, sid).map_err(|error| {
                        acl_error(
                            format!(
                                "revoke {:?} ACL {} (recorded ID {:?})",
                                mutation.kind,
                                mutation.path,
                                entry.object_ids.get(&mutation.path)
                            ),
                            error,
                        )
                    })?;
                }
                for path in &entry.private_paths {
                    if Path::new(path).exists() {
                        super::windows_registry::validate_private_path(&entry, Path::new(path))
                            .map_err(|error| {
                                acl_error(format!("validate private directory {path}"), error)
                            })?;
                        std::fs::remove_dir_all(path).map_err(|error| {
                            acl_error(format!("remove private directory {path}"), error)
                        })?;
                        #[cfg(test)]
                        test_crash_transition(
                            "cleanup-private-removed",
                            &entry.profile_name,
                            Path::new(path),
                        );
                    }
                }
                // Keep the name-only window-object grants until every recoverable private-path
                // step has completed. An interrupted private deletion then retries with its
                // ownership witness still present instead of confusing a prior partial cleanup
                // with a same-name replacement object.
                for object in &entry.window_objects {
                    let witnessed = crate::backend::windows_ace::has_persistent_grant(object, sid)?;
                    #[cfg(test)]
                    test_crash_transition(
                        "cleanup-window-object-witness-checked",
                        &entry.profile_name,
                        Path::new("."),
                    );
                    if !witnessed {
                        if entry.window_object_revoke.as_ref() == Some(object) {
                            // A durable intent predates this attempt. A missing witness now means
                            // the previous process completed the revoke before it died, or the
                            // object was replaced without Nub's grant; neither permits a DACL
                            // edit, so retire only this already-in-progress journal operation.
                            super::windows_registry::finish_window_object_revoke(&entry, object)?;
                            continue;
                        }
                        return Err(io::Error::other(format!(
                            "sandbox window-object cleanup ownership witness is absent for {object:?}"
                        )));
                    }
                    // Persist progress only after establishing the ownership witness. If the
                    // process dies before this point, retry must treat a now-missing or
                    // same-name replacement object as fresh ambiguity, never as a completed
                    // revoke.
                    super::windows_registry::begin_window_object_revoke(&entry, object)?;
                    crate::backend::windows_ace::revoke_persistent(object, sid).map_err(
                        |error| acl_error(format!("revoke window object {object:?}"), error),
                    )?;
                    #[cfg(test)]
                    test_crash_transition(
                        "cleanup-window-object-revoked",
                        &entry.profile_name,
                        Path::new("."),
                    );
                    super::windows_registry::finish_window_object_revoke(&entry, object)?;
                    #[cfg(test)]
                    test_crash_transition(
                        "cleanup-window-object-removed",
                        &entry.profile_name,
                        Path::new("."),
                    );
                }
                let name = to_wide(&entry.profile_name);
                let hr = unsafe { DeleteAppContainerProfile(name.as_ptr()) };
                if hr != 0 && !matches!(hr as u32, 0x8007_0002 | 0x8007_0003 | 0x8007_0490) {
                    return Err(io::Error::other(format!(
                        "DeleteAppContainerProfile failed hr=0x{hr:08x}"
                    )));
                }
                Ok(())
            })()
            .map_err(|error| acl_error(format!("recover profile {}", entry.profile_name), error));
            if let Err(error) = &result {
                first_error.get_or_insert_with(|| io::Error::other(error.to_string()));
            }
            super::windows_registry::finish_recovery(&entry, result)?;
        }
        if (all || !reserve_slot)
            && let Some(error) = first_error
        {
            return Err(error);
        }
        Ok(())
    }

    /// Reopen the journaled object even when the caller moved or replaced its name.
    /// A missing ID is conclusive only when the filesystem supports this lookup.
    /// Access denied and unsupported lookups remain errors; even delete-pending
    /// objects can reopen successfully and must have their identities checked.
    pub(super) fn open_recorded_acl_file(
        path: &Path,
        expected: &str,
    ) -> io::Result<Option<std::fs::File>> {
        use super::windows_registry::{object_handle_id, object_id};
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType,
            GetVolumeInformationByHandleW, OpenFileById,
        };

        if object_id(path)
            .map_err(|error| acl_error("read current path identity", error))?
            .as_deref()
            == Some(expected)
        {
            let file =
                open_acl_file(path).map_err(|error| acl_error("open original named ACL", error))?;
            if object_handle_id(file.as_raw_handle())? == expected {
                return Ok(Some(file));
            }
        }
        let parts = expected
            .split(':')
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;
        let [volume, high, low] = parts.as_slice() else {
            return Err(io::Error::other("unsupported sandbox ACL object identity"));
        };
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                FileId: ((u64::from(*high) << 32) | u64::from(*low)) as i64,
            },
        };
        // Any accessible file on the volume is a sufficient hint. Opening a
        // volume device (which could require elevation) is neither needed nor used.
        for ancestor in path.ancestors().skip(1) {
            let hint = match std::fs::OpenOptions::new()
                .access_mode(0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(ancestor)
            {
                Ok(file) => file,
                Err(_) => continue,
            };
            let hint_id = object_handle_id(hint.as_raw_handle())?;
            let hint_parts = hint_id
                .split(':')
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
                .map_err(io::Error::other)?;
            let [hint_volume, hint_high, hint_low] = hint_parts.as_slice() else {
                return Err(io::Error::other("unsupported sandbox ACL volume identity"));
            };
            if hint_volume != volume {
                continue;
            }
            let mut filesystem = [0u16; 32];
            let mut flags = 0;
            if unsafe {
                GetVolumeInformationByHandleW(
                    hint.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut flags,
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                return Err(acl_error(
                    format!("GetVolumeInformationByHandleW hint {}", ancestor.display()),
                    error,
                ));
            }
            let len = filesystem
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(filesystem.len());
            // Existing journals carry 64-bit IDs, which are not unique on ReFS.
            // Do not treat an unsupported or ambiguous lookup as a deleted object.
            const SUPPORTS_OPEN_BY_FILE_ID: u32 = 0x0100_0000;
            if flags & SUPPORTS_OPEN_BY_FILE_ID == 0
                || !String::from_utf16_lossy(&filesystem[..len]).eq_ignore_ascii_case("NTFS")
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "sandbox recorded-object recovery requires NTFS file IDs",
                ));
            }
            let raw = unsafe {
                OpenFileById(
                    hint.as_raw_handle(),
                    &descriptor,
                    0x0002_0000 | 0x0004_0000,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    FILE_FLAG_BACKUP_SEMANTICS,
                )
            };
            if raw == INVALID_HANDLE_VALUE {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(87) {
                    // An ACL-access-specific failure is not evidence of deletion.
                    // Zero desired access is the documented existence-only open.
                    let present = unsafe {
                        OpenFileById(
                            hint.as_raw_handle(),
                            &descriptor,
                            0,
                            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                            std::ptr::null(),
                            FILE_FLAG_BACKUP_SEMANTICS,
                        )
                    };
                    if present != INVALID_HANDLE_VALUE {
                        let file = unsafe { std::fs::File::from_raw_handle(present) };
                        let found = object_handle_id(file.as_raw_handle())?;
                        return Err(acl_error(
                            format!("OpenFileById ACL access failed for existing ID {found}"),
                            error,
                        ));
                    }
                    let existence_error = io::Error::last_os_error();
                    if !matches!(existence_error.raw_os_error(), Some(2 | 3 | 87)) {
                        return Err(acl_error("OpenFileById existence check", existence_error));
                    }
                    if hint_id == expected {
                        return Err(acl_error(
                            "recorded ACL object is still the live volume hint",
                            error,
                        ));
                    }
                    // NTFS also reports INVALID_PARAMETER for a retired file ID.
                    // Do not confuse that with an unsupported call: keep every
                    // parameter identical except the ID, reopen the live volume
                    // hint, and verify the returned object before retiring a record.
                    let control = FILE_ID_DESCRIPTOR {
                        Anonymous: FILE_ID_DESCRIPTOR_0 {
                            FileId: ((u64::from(*hint_high) << 32) | u64::from(*hint_low)) as i64,
                        },
                        ..descriptor
                    };
                    #[cfg(test)]
                    let control = if std::env::var("__NUB_WINDOWS_REPLACEMENT_FIXTURE").as_deref()
                        == Ok("lookup-control-failure")
                    {
                        FILE_ID_DESCRIPTOR {
                            dwSize: 0,
                            ..control
                        }
                    } else {
                        control
                    };
                    let control_raw = unsafe {
                        OpenFileById(
                            hint.as_raw_handle(),
                            &control,
                            0x0002_0000 | 0x0004_0000,
                            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                            std::ptr::null(),
                            FILE_FLAG_BACKUP_SEMANTICS,
                        )
                    };
                    if control_raw == INVALID_HANDLE_VALUE {
                        let control_error = io::Error::last_os_error();
                        return Err(acl_error(
                            format!(
                                "OpenFileById live control ID {hint_id}, hint {} after retired ID {expected}: {error}",
                                ancestor.display()
                            ),
                            control_error,
                        ));
                    }
                    let control_file = unsafe { std::fs::File::from_raw_handle(control_raw) };
                    if object_handle_id(control_file.as_raw_handle())? != hint_id {
                        return Err(io::Error::other(
                            "sandbox ACL live-control lookup returned a different identity",
                        ));
                    }
                    return Ok(None);
                }
                return if matches!(error.raw_os_error(), Some(2 | 3)) {
                    Ok(None)
                } else {
                    Err(acl_error(
                        format!(
                            "OpenFileById ID {expected}, path {}, hint {}",
                            path.display(),
                            ancestor.display()
                        ),
                        error,
                    ))
                };
            }
            // SAFETY: OpenFileById returned an owned file handle.
            let file = unsafe { std::fs::File::from_raw_handle(raw) };
            if object_handle_id(file.as_raw_handle())? != expected {
                return Err(io::Error::other(
                    "sandbox ACL object lookup returned a different identity",
                ));
            }
            return Ok(Some(file));
        }
        Err(io::Error::other(
            "sandbox ACL object volume is unavailable for recovery",
        ))
    }

    fn revoke_recorded_ace(
        entry: &super::windows_registry::Entry,
        mutation: &super::windows_registry::AclMutation,
        sid: PSID,
    ) -> io::Result<()> {
        use super::windows_registry::{AclKind, object_handle_id};
        if !entry.leases.is_empty() {
            return Err(io::Error::other(
                "refusing to revoke a live sandbox resource",
            ));
        }
        let path = Path::new(&mutation.path);
        let expected = entry.object_ids.get(&mutation.path);
        if mutation.kind == AclKind::PrivateProfile {
            let file = match open_acl_file(path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(acl_error("open private ACL", error)),
            };
            if let Some(expected) = expected
                && &object_handle_id(file.as_raw_handle())? != expected
            {
                return Err(io::Error::other("sandbox private ACL object was replaced"));
            }
            return set_ace_on_handle(file.as_raw_handle(), sid, 0, REVOKE_ACCESS, false, true);
        }
        let expected = expected.ok_or_else(|| {
            io::Error::other("sandbox ACL mutation has no recorded object identity")
        })?;
        let original = open_recorded_acl_file(path, expected)?;
        if let Some(file) = &original {
            set_ace_on_handle(
                file.as_raw_handle(),
                sid,
                0,
                REVOKE_ACCESS,
                false,
                mutation.kind == AclKind::Subtree,
            )?;
        }
        // Atomic replacement can copy a DACL. Removal names only the retired SID,
        // never the new resource's SID, and confers no authority to delete the file.
        match open_acl_file(path) {
            Ok(file) => {
                if object_handle_id(file.as_raw_handle())? != *expected {
                    set_ace_on_handle(
                        file.as_raw_handle(),
                        sid,
                        0,
                        REVOKE_ACCESS,
                        false,
                        mutation.kind == AclKind::Subtree,
                    )?;
                }
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(acl_error("open replacement named ACL", error)),
        }
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

    /// Every command gets whole-tree ownership; only confinement imposes a cap.
    fn create_process_job(confined: bool) -> io::Result<HANDLE> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // ACTIVE_PROCESS is transitive to grandchildren and refuses CREATE_BREAKAWAY_FROM_JOB,
        // so confined code cannot escape it; it needs no privilege, which is why it is the
        // containment lever the zero-privilege jail can actually use.
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if confined {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = super::active_process_cap();
        }
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

    /// Owns the running co-package egress-funnel helper. Dropping it closes the helper's
    /// KILL_ON_JOB_CLOSE job handle, which reaps the helper — so the helper lives exactly as long
    /// as the `AppContainerLaunch::run` frame that holds it (i.e. the confined child's lifetime),
    /// and dies with nub even on a crash. The explicit `TerminateProcess` is belt-and-suspenders
    /// for an immediate teardown; the job close is the guarantee.
    struct HelperGuard {
        job: HandleGuard,
        process: HandleGuard,
    }
    impl HelperGuard {
        fn terminate(&self) {
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.0, 0);
            }
        }
    }
    impl Drop for HelperGuard {
        fn drop(&mut self) {
            self.terminate();
            unsafe {
                WaitForSingleObject(self.process.0, u32::MAX);
            }
        }
    }

    /// Launch the CO-PACKAGE egress-proxy helper for the zero-privilege per-host funnel, and read
    /// back the loopback port + bearer token it binds.
    ///
    /// The helper is nub itself, re-invoked through the embedder-registered command
    /// ([`windows_egress_helper_command`](crate::backend::windows_egress_helper_command)) plus a
    /// base64(JSON) [`NetPolicy`] argument, launched as an AppContainer LowBox with `ac_sid` (the
    /// SAME package SID as the confined child) and `internetClient` (+ the client/server loopback
    /// caps, matching the proven harness so the bind/accept is never the variable). It prints
    /// `PROXY_READY port=<p> token=<t>` on the inherited stdout pipe read here.
    ///
    /// Grants NO file ACEs: the medium-IL parent opens the image section, and nub's own
    /// dependencies load from `System32` (ALL APPLICATION PACKAGES readable) — the proven funnel
    /// harness ran the same-shape helper this way with no per-file grant. The window-station ACE
    /// the child already holds (step 1b) covers the helper too, since it shares `ac_sid`.
    fn launch_egress_helper(
        ac_sid: PSID,
        policy: &crate::policy::NetPolicy,
        command_env: Option<&std::collections::BTreeMap<String, String>>,
    ) -> io::Result<(u16, String, HelperGuard)> {
        use base64::Engine as _;
        use std::os::windows::io::AsRawHandle as _;

        // 1. Command line: the registered [image, hidden-flag] + the per-run serialized policy.
        let base = crate::backend::windows_egress_helper_command()
            .ok_or_else(|| io::Error::other("no Windows egress-helper command is registered"))?;
        let (program, flag_args) = base
            .split_first()
            .ok_or_else(|| io::Error::other("the Windows egress-helper command is empty"))?;
        let json = serde_json::to_vec(policy).map_err(io::Error::other)?;
        let blob = base64::engine::general_purpose::STANDARD.encode(&json);
        let mut argv: Vec<std::ffi::OsString> = flag_args.to_vec();
        argv.push(std::ffi::OsString::from(blob));
        let mut cmdline = build_command_line(program, &crate::backend::CommandArgs::Argv(argv));

        // 2. A pipe carrying the helper's stdout back to nub (PROXY_READY). Only the WRITE end is
        //    marked inheritable and scoped into the child via the handle list; the read end stays
        //    private to nub.
        let (reader, writer) = std::io::pipe()?;
        let w: HANDLE = writer.as_raw_handle().cast();
        if unsafe { SetHandleInformation(w, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(io::Error::last_os_error());
        }

        // 3. SECURITY_CAPABILITIES: the child's package SID + internetClient (+ loopback
        //    client/server caps, per the proven harness). The HELPER is trusted nub code, not the
        //    sandboxed principal — the confined child holds ZERO capabilities.
        let cap_owned: Vec<CapSid> = [
            INTERNET_CLIENT_SID,
            INTERNET_CLIENT_SERVER_SID,
            PRIVATE_NETWORK_CLIENT_SERVER_SID,
        ]
        .iter()
        .map(|s| CapSid::new(s))
        .collect::<io::Result<_>>()?;
        let mut caps: Vec<SID_AND_ATTRIBUTES> = cap_owned
            .iter()
            .map(|c| SID_AND_ATTRIBUTES {
                Sid: c.0,
                Attributes: SE_GROUP_ENABLED,
            })
            .collect();
        let mut sec_caps = SECURITY_CAPABILITIES {
            AppContainerSid: ac_sid,
            Capabilities: caps.as_mut_ptr(),
            CapabilityCount: caps.len() as u32,
            Reserved: 0,
        };

        // 4. Proc-thread attribute list: SECURITY_CAPABILITIES + a HANDLE_LIST scoping inheritance
        //    to exactly the stdout write end.
        let inherit = [w];
        let job_guard = HandleGuard(create_process_job(true)?);
        let jobs = [job_guard.0];
        let mut attr = ProcThreadAttrList::new(3)?;
        attr.update(
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            jobs.as_ptr().cast_mut().cast(),
            std::mem::size_of_val(&jobs),
        )?;
        attr.update(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            std::ptr::from_mut(&mut sec_caps).cast(),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
        )?;
        attr.update(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherit.as_ptr().cast_mut().cast(),
            std::mem::size_of::<HANDLE>() * inherit.len(),
        )?;

        // 5. STARTUPINFOEX: stdout+stderr → the pipe write end; stdin none. cwd = System32
        //    (app-package-readable). Inherit the parent env (NULL lpEnvironment) — the helper is
        //    nub itself and only needs enough env to start the proxy.
        let cwd_wide = to_wide(APP_PACKAGE_READABLE_CWD);
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attr.as_ptr();
        si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = std::ptr::null_mut();
        si.StartupInfo.hStdOutput = w;
        si.StartupInfo.hStdError = w;

        // Only OS startup roots, never ambient or injected credential values.
        let mut helper_env: std::collections::BTreeMap<String, String> = [
            "SystemRoot",
            "WINDIR",
            "TEMP",
            "TMP",
            "LOCALAPPDATA",
            "USERPROFILE",
        ]
        .into_iter()
        .filter_map(|key| {
            command_env
                .and_then(|env| env.iter().find(|(name, _)| name.eq_ignore_ascii_case(key)))
                .map(|(_, value)| (key.to_string(), value.clone()))
        })
        .collect();
        ensure_appcontainer_environment(&mut helper_env)?;
        let env_block = build_env_block(&helper_env);
        let flags = EXTENDED_STARTUPINFO_PRESENT
            | CREATE_SUSPENDED
            | CREATE_NO_WINDOW
            | CREATE_UNICODE_ENVIRONMENT;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // This helper is an AppContainer process too: keep its loader attachment out of a
        // concurrent recorded-station borrow.
        // SAFETY: cmdline/cwd_wide/attr/sec_caps/caps all outlive this call; lpCommandLine is a
        // writable UTF-16 buffer; bInheritHandles TRUE so the scoped handle list takes effect.
        let ok = {
            let _station = crate::backend::windows_ace::station_guard();
            unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    cmdline.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    flags,
                    env_block.as_ptr().cast(),
                    cwd_wide.as_ptr(),
                    std::ptr::from_mut(&mut si).cast(),
                    &mut pi,
                )
            }
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("CreateProcessW (AppContainer egress helper) failed: {error}"),
            ));
        }
        let _ = &cap_owned; // backs `sec_caps` — held alive until here

        let process = HandleGuard(pi.hProcess);
        let thread = HandleGuard(pi.hThread);
        let guard = HelperGuard {
            job: job_guard,
            process,
        };
        if unsafe { ResumeThread(thread.0) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        // 7. nub drops its own copy of the write end (else the reader never sees EOF), then reads
        //    PROXY_READY off the pipe on a worker thread, bounded by a deadline. The worker RETURNS
        //    as soon as it has the line (closing nub's read end), so it does not linger; on the
        //    helper's death the read end sees EOF and the worker exits too.
        drop(writer);
        let (tx, rx) = std::sync::mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            use std::io::BufRead as _;
            let mut buf = std::io::BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match buf.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(None);
                        return;
                    }
                    Ok(_) => {
                        if let Some(rest) = line.trim().strip_prefix("PROXY_READY") {
                            let mut port = 0u16;
                            let mut token = String::new();
                            for field in rest.split_whitespace() {
                                if let Some(v) = field.strip_prefix("port=") {
                                    port = v.parse().unwrap_or(0);
                                } else if let Some(v) = field.strip_prefix("token=") {
                                    token = v.to_string();
                                }
                            }
                            let _ = tx.send(Some((port, token)));
                            return;
                        }
                        if line.contains("PROXY_START_FAIL") {
                            let _ = tx.send(None);
                            return;
                        }
                        // Diagnostic principal dump (gated in the helper) — surface it so a
                        // verification run can compare the helper's SID against the child's.
                        if line.contains("TOKEN[") {
                            eprint!("{line}");
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(None);
                        return;
                    }
                }
            }
        });

        let ready = rx.recv_timeout(std::time::Duration::from_secs(20));
        if !matches!(&ready, Ok(Some((port, token))) if *port != 0 && !token.is_empty()) {
            drop(guard);
            let _ = reader_thread.join();
            return Err(io::Error::other(
                "the co-package egress-funnel helper did not report a ready proxy",
            ));
        }
        let _ = reader_thread.join();
        match ready {
            Ok(Some((port, token))) if port != 0 && !token.is_empty() => Ok((port, token, guard)),
            _ => {
                // guard drops here → helper reaped.
                Err(io::Error::other(
                    "the co-package egress-funnel helper did not report a ready proxy",
                ))
            }
        }
    }

    #[cfg(test)]
    fn path_has_sid(path: &Path, sid: PSID) -> io::Result<bool> {
        let wide = to_wide_path(path);
        let mut acl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let result = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let _descriptor = LocalFreeGuard(descriptor);
        if acl.is_null() {
            return Ok(false);
        }
        let mut found = false;
        for_each_ace_of_sid(acl, sid, path, |_, _, _| found = true)?;
        Ok(found)
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
    pub(crate) fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
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
        let file = open_acl_file(path)?;
        set_ace_on_handle(file.as_raw_handle(), sid, access, mode, inherit, true)
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
            ..Default::default()
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

        // ⛔ THE `/**` TWINS ARE LOAD-BEARING, NOT NOISE. This test is about ORIGIN and
        // ABSENCE, and it asserts on `read` — the SUBTREE plan. A bare literal with no twin is
        // a node-only grant now (see `derive_grants`) and lands in `read_nodes` instead, so
        // dropping the twins would make both assertions vacuous while still compiling. The
        // node/subtree split itself is pinned by `a_bare_literal_grants_the_node_not_the_subtree`.
        let subtree = |p: &Path, origin: FsOrigin| {
            let mk = |m: String| FsRule {
                matcher: CanonGlob(m),
                effect: Effect::Allow,
                access: FsAccess::Read,
                origin,
            };
            vec![mk(canon(p)), mk(format!("{}/**", canon(p)))]
        };
        let with_origin = |origin: FsOrigin| subtree(&missing, origin);

        let __g = derive_grants(&fs(
            Effect::Deny,
            [
                subtree(&present, FsOrigin::Authored),
                with_origin(FsOrigin::Speculative),
            ]
            .concat(),
        ));
        let read = __g.read;
        assert_eq!(
            read,
            vec![present.clone()],
            "an absent speculative grant must not reach the ACE plan"
        );

        let __g = derive_grants(&fs(
            Effect::Deny,
            [
                subtree(&present, FsOrigin::Authored),
                with_origin(FsOrigin::Authored),
            ]
            .concat(),
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

    /// ⛔ HERMETIC ON PURPOSE — THIS TEST NAMED `C:/tools` AND ITS OUTCOME DEPENDED ON THE HOST.
    /// `derive_grants` consults the real filesystem, so a bare read rule diverts to the node
    /// list only when the path IS a directory. `C:/tools` does not exist on a dev box and DOES
    /// exist on `windows-latest`, so the same commit passed locally and failed in CI — a green
    /// local run that proved nothing. A tempdir removes the dependence at the source; the `/**`
    /// twin states the subtree intent this assertion is about, since `read` is the subtree plan.
    #[test]
    fn read_only_allow_yields_no_write_grant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canon = dir.path().to_string_lossy().replace('\\', "/");
        let p = fs(
            Effect::Deny,
            vec![
                rule(&canon, Effect::Allow, FsAccess::Read),
                rule(&format!("{canon}/**"), Effect::Allow, FsAccess::Read),
            ],
        );
        let __g = derive_grants(&p);
        assert_eq!(__g.read, vec![dir.path().to_path_buf()]);
        assert!(
            __g.write.is_empty(),
            "a read-only allow must not open a write grant"
        );
    }

    /// A bare literal grants the directory NODE; only the `[P, P/**]` pair grants the subtree.
    ///
    /// ⛔ WHAT GOES WRONG WITHOUT THIS. `preset::project_cwd_node` emits a bare rule on the
    /// consumer's project root precisely so a confined lifecycle script can `getcwd` and still
    /// not read `src/`, `.git/` or a root `.env` — its own doc calls the distinction "the entire
    /// safety argument". Linux honours it (`MountAccess::ListOnly`) and macOS honours it (a
    /// Seatbelt `literal`), while this backend granted an INHERITABLE read over the whole
    /// project, which a pure allowlist has no deny to subtract back. Nothing pinned the split
    /// here, so the divergence was invisible to every gate.
    ///
    /// Both directions are asserted. The subtree arm is the one that catches over-correction: a
    /// node-only rule that swallowed real subtree grants would silently under-grant every
    /// lifecycle script, which surfaces as a laundered EPERM rather than as a failure here.
    #[test]
    fn a_bare_literal_grants_the_node_not_the_subtree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("proj");
        std::fs::create_dir(&target).expect("create proj");
        let canon = target.to_string_lossy().replace('\\', "/");

        let node = derive_grants(&fs(
            Effect::Deny,
            vec![rule(&canon, Effect::Allow, FsAccess::Read)],
        ));
        assert_eq!(
            node.read_nodes,
            vec![target.clone()],
            "a bare literal must grant the directory node"
        );
        assert!(
            node.read.is_empty(),
            "a bare literal must NOT reach the inheritable subtree plan: {:?}",
            node.read
        );

        let whole = derive_grants(&fs(
            Effect::Deny,
            vec![
                rule(&canon, Effect::Allow, FsAccess::Read),
                rule(&format!("{canon}/**"), Effect::Allow, FsAccess::Read),
            ],
        ));
        assert_eq!(
            whole.read,
            vec![target],
            "the `[P, P/**]` pair must still grant the whole subtree"
        );
        assert!(
            whole.read_nodes.is_empty(),
            "a real subtree grant must not be downgraded to its node: {:?}",
            whole.read_nodes
        );
    }

    /// A read grant nested inside a wider read grant is dropped — the inheritable ace on the
    /// outer already reaches it, so the inner one buys nothing and pays a second propagation
    /// walk. Writes are folded only into WRITE outers, never a read one, whose ace carries no
    /// `GENERIC_WRITE`; that direction is what an over-eager fold would turn into a silent
    /// under-grant, so it is asserted rather than assumed.
    #[test]
    fn a_nested_grant_folds_into_its_ancestor_but_a_write_never_folds_into_a_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outer = dir.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).expect("create tree");
        let c = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let sub = |p: &Path, a: FsAccess| {
            vec![
                rule(&c(p), Effect::Allow, a),
                rule(&format!("{}/**", c(p)), Effect::Allow, a),
            ]
        };

        let folded = derive_grants(&fs(
            Effect::Deny,
            [sub(&outer, FsAccess::Read), sub(&inner, FsAccess::Read)].concat(),
        ));
        assert_eq!(
            folded.read,
            vec![outer.clone()],
            "a read nested in a wider read must fold away"
        );

        let kept = derive_grants(&fs(
            Effect::Deny,
            [
                sub(&outer, FsAccess::Read),
                sub(&inner, FsAccess::ReadWrite),
            ]
            .concat(),
        ));
        assert!(
            kept.write.contains(&inner),
            "a WRITE must never fold into a read ancestor: {:?}",
            kept.write
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
        for helper in [false, true] {
            assert_eq!(
                plan_net(&NetPolicy::default(), helper),
                WinNetPlan::Unconfined
            );
            let deny = NetPolicy {
                enforce: true,
                ..Default::default()
            };
            assert_eq!(plan_net(&deny, helper), WinNetPlan::CoarseDeny);
        }
        let mut net = NetPolicy {
            enforce: true,
            rules: vec![NetRule {
                target: NetTarget::Host("example.com".to_string()),
                effect: Effect::Allow,
            }],
            ..Default::default()
        };
        assert_eq!(plan_net(&net, false), WinNetPlan::Unsupported);
        assert_eq!(plan_net(&net, true), WinNetPlan::Funnel);
        net.inspection = Inspection::TlsInspect;
        assert_eq!(plan_net(&net, false), WinNetPlan::Unsupported);
        assert_eq!(plan_net(&net, true), WinNetPlan::Unsupported);
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

        // A helper is required even when the caller happens to be an administrator.
        let per_host = mk(NetPolicy {
            enforce: true,
            rules: vec![NetRule {
                target: NetTarget::Host("example.com".to_string()),
                effect: Effect::Allow,
            }],
            default_effect: Effect::Deny,
            ..Default::default()
        });

        // Do not register the process-global helper in a parallel unit test.
        let port = 59080;
        let res = apply(
            &per_host,
            crate::CommandSpec::new("cmd.exe"),
            Some(port),
            None,
            None,
            None,
        );
        let Err(err) = res else {
            panic!("per-host rules without a helper must fail closed");
        };
        assert!(err.lost.iter().any(|s| s == "net-per-host"));
        assert!(
            err.reason
                .as_deref()
                .unwrap_or_default()
                .contains("unprivileged egress helper")
        );
    }
}

#[cfg(all(test, windows))]
#[path = "windows_native_child_tests.rs"]
mod native_child_tests;

#[cfg(all(test, windows))]
#[path = "windows_cleanup_tests.rs"]
mod windows_cleanup_tests;

#[cfg(all(test, windows))]
#[path = "windows_replacement_tests.rs"]
mod replacement_tests;
