//! Windows **agent-sandbox** backend: a dedicated local account + WFP.
//!
//! WHY A SECOND WINDOWS BACKEND (the 2026-07-24 pivot). The AppContainer backend
//! ([`super::windows`]) is a pure ALLOWLIST: reachable only where an object's ACL names the
//! per-run AppContainer SID. That maps build-jail exactly — and cannot express agent-sandbox
//! at all. Two things defeat it:
//!   1. **Generous-read-minus-secrets is inexpressible.** An allowlist cannot say "read
//!      everything except these", so the policy degrades to the explicit allow-set.
//!   2. **Deny-inside-allow does not hold.** A secret under a dir carrying an inherited
//!      `ALL APPLICATION PACKAGES` grant is readable regardless of the allow-set — the AAP
//!      grant satisfies the LowBox check before default-deny is reached.
//!
//! Both dissolve when the child runs as a **separate local principal**: the invoking user's
//! own profile (`~/.ssh`, `~/.aws`, a home-dir `.env`) is denied by DEFAULT with no ACE
//! authored at all, and no AAP grant ever covers a user SID, so an explicit deny ACE on a
//! secret *inside* a granted tree wins on canonical DACL order.
//!
//! And per-host egress was never admin-free on Windows regardless: there is no unprivileged
//! per-host mechanism (AppContainer's loopback exemption is all-or-nothing), so the full
//! grammar always implied WFP, which always implied elevation. Given elevation is required
//! anyway, the dedicated account buys the *whole* grammar rather than a subset — which is
//! why the deny-strip (`SE_DACL_PROTECTED` + a DACL restore journal) was dropped rather than
//! finished: its only advantage had been avoiding an elevation that turned out to be
//! mandatory. See `.fray/sandbox-decisions-current.md` §2.
//!
//! **THE PRIVILEGE SPLIT IS THE WHOLE PRODUCT DECISION.** One elevated
//! `nub setup-sandbox` per machine creates the account and installs four persistent
//! WFP filters over a pre-authorized loopback port window. Every run after that is
//! **fully unelevated**: read the credential, ACL the policy's paths, and
//! `CreateProcessWithLogonW` the child through the Secondary Logon service — which needs no
//! privilege because the unelevated broker never holds a foreign primary token.
//! build-jail keeps using [`super::windows`] and stays admin-free end to end.
//!
//! SPIKE BOUNDS (deliberate, not hidden — see LIMITATIONS.md): the child is launched in ONE
//! hop straight to the target. SRT and Codex add a second hop that re-launches through a
//! runner holding a *restricted* token; nub's confinement comes from the account's ACL reach
//! plus SID-keyed WFP, neither of which needs that token, so hop 2 is a hardening follow-up.
//! Consequences: `lpDesktop` stays NULL, at the cost of desktop isolation (and NOT because it
//! avoids window-station work — seclogon's auto-grant covers only `WinSta0`, so the launch
//! aces the caller's station and desktop itself; see [`launch`]), and Job-Object whole-tree
//! kill is best-effort because seclogon's own job refuses cross-session nesting.

#![cfg(any(target_os = "windows", test))]

use crate::policy::{Effect, FsAccess, FsPolicy};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(target_os = "windows")]
pub(crate) mod account;
#[cfg(target_os = "windows")]
pub(crate) mod acl;
// `pub(crate)` for `WindowAceGuard` alone: the AppContainer backend needs the same
// window-station/desktop ace, and duplicating the masks and the restore-on-drop logic there
// would be two implementations of one security-relevant behaviour.
#[cfg(target_os = "windows")]
pub(crate) mod launch;
pub(crate) mod state;
#[cfg(target_os = "windows")]
pub(crate) mod wfp;

/// The local account nub provisions. 20 chars is the SAM limit for a local account; this is
/// 11. Stable — the WFP filters and every granted ACE key on the SID it resolves to.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const SANDBOX_ACCOUNT: &str = "nub-sandbox";

/// The command a user runs to provision this machine. Every fail-closed message that demands
/// setup names this constant, so the verb people are told to run and the verb the CLI actually
/// accepts cannot drift — they diverged once already, when these messages pointed at the
/// internal `nub run --sandbox-admin setup` flag that no documentation mentions.
pub const SETUP_COMMAND: &str = "nub setup-sandbox";

/// A local group holding the sandbox account. NOTHING KEYS ON IT TODAY: the credential store
/// is locked by a PROTECTED DACL naming SYSTEM, `BUILTIN\Administrators` and the provisioning
/// user, which needs no deny trustee at all. It is created and deleted anyway because it is the
/// stable trustee a future per-session-account design would add members to rather than
/// rewriting DACLs (SRT's rationale), and provisioning it is free.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const SANDBOX_GROUP: &str = "nub-sandbox-users";

/// What the account model could not express for a policy, so the caller is told rather than
/// silently over- or under-confined.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct AccountFsDegrade {
    /// A read-all base. The account reads what any standard local user reads (system dirs,
    /// Program Files) PLUS the explicit grants — but never the invoking user's profile. That
    /// is narrower than "read everything", so it is over-confinement and is reported.
    pub(crate) generous_read_narrowed: bool,
    /// An embedded-glob allow can't be one inheritable ACE; skipped rather than widened to
    /// its literal prefix, which could sweep in a sibling secret.
    pub(crate) glob_read_unenforced: bool,
    /// An embedded-glob DENY can't be one ACE either — and a missed deny is a HOLE, not
    /// over-confinement, so it is reported distinctly.
    pub(crate) glob_deny_unenforced: bool,
}

/// The net posture the account backend can achieve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountNet {
    /// Policy allows specific hosts: the child reaches nub's loopback proxy (permitted by the
    /// installed port window) and nothing else.
    ProxyOnly,
    /// Pure deny-all: the persistent block filters alone, no proxy.
    DenyAll,
    /// Policy leaves net unconfined, but the account is PERMANENTLY fenced by the persistent
    /// filters, so egress is denied anyway. Over-confinement — reported, never silent.
    UnconfinedButFenced,
}

/// A resolved account-backend launch plan. Plain data so the derivation is unit-tested on the
/// dev host; the FFI in [`launch`] is `#[cfg(windows)]`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct AccountLaunch {
    pub(crate) program: OsString,
    pub(crate) args: crate::backend::CommandArgs,
    pub(crate) cwd: Option<PathBuf>,
    /// Subtrees granted read+execute to the sandbox SID, inheritable.
    pub(crate) read_grants: Vec<PathBuf>,
    /// Subtrees granted modify (read+write+delete, NEVER `FILE_DELETE_CHILD`), inheritable.
    pub(crate) write_grants: Vec<PathBuf>,
    /// Paths carrying an explicit DENY ACE for the sandbox SID. Only meaningful INSIDE a
    /// grant — outside one the account already has no reach — but applying them
    /// unconditionally is free and survives a later widening of the grant set.
    pub(crate) denies: Vec<PathBuf>,
    /// `Some` ⇒ the child env IS this map. `None` ⇒ seclogon builds the account's own
    /// profile environment.
    pub(crate) env: Option<BTreeMap<String, String>>,
}

// The net posture deliberately does NOT ride the launch plan: it is fully decided in `apply`
// (the degradation it reports and the port-window gate it enforces) and then carried by the
// PERSISTENT WFP filters the elevated setup installed. There is no per-run network work to
// do, which is exactly the property that keeps every run unelevated.

/// Whether this machine has completed the one-time elevated setup.
///
/// Cheap and read-only: the marker's mere presence is the signal. [`apply`] re-reads it and
/// re-validates the recorded SID against the live account, so a stale or tampered marker still
/// fails closed there — this is a ROUTING question ("is the account path available at all?"),
/// deliberately not the trust decision.
#[cfg(target_os = "windows")]
pub(crate) fn is_provisioned() -> bool {
    matches!(state::read_marker(), Ok(Some(_)))
}

/// Whether a policy needs the ACCOUNT backend rather than the AppContainer allowlist.
///
/// The account backend costs a one-time elevated setup, so it is chosen only where the
/// allowlist genuinely cannot carry the policy — which is exactly the agent-sandbox shape:
/// a generous-read base, a deny that has to be carved inside a grant, or per-host egress.
/// A pure default-deny allowlist with coarse/absent net is build-jail and stays on the
/// admin-free AppContainer path.
pub(crate) fn needs_account_backend(policy: &crate::policy::SandboxPolicy) -> bool {
    let fs = &policy.fs;
    let generous_read =
        fs.rules.default_effect == Effect::Allow
            || fs.rules.entries.iter().any(|r| {
                r.effect == Effect::Allow && super::windows::is_whole_fs(r.matcher.as_str())
            });
    // A deny only needs carving when it can land inside something the policy also grants;
    // `deny_shadows_grant` is exactly that test, and it is the AppContainer's known hole.
    let __g = super::windows::derive_grants(fs);
    let read_grants = __g.read;
    let deny_needs_carve = super::windows::deny_shadows_grant(&fs.rules.entries, &read_grants);
    let per_host_net =
        policy.net.enforce && policy.net.rules.iter().any(|r| r.effect == Effect::Allow);
    generous_read || deny_needs_carve || per_host_net
}

/// Derive the account's grant/deny sets from the fs IR.
///
/// Unlike the AppContainer derivation, DENIES ARE REAL HERE: an explicit deny ACE for the
/// sandbox SID on a path inside a granted subtree outranks the grant's *inherited* allow,
/// because Windows orders every explicit ACE ahead of every inherited one. That ordering is
/// the mechanism the whole agent-sandbox fs axis rests on.
pub(crate) fn derive_plan(
    fs: &FsPolicy,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>, AccountFsDegrade) {
    let mut read = Vec::new();
    let mut write = Vec::new();
    let mut deny = Vec::new();
    let mut degrade = AccountFsDegrade {
        generous_read_narrowed: fs.rules.default_effect == Effect::Allow,
        ..Default::default()
    };

    for rule in &fs.rules.entries {
        let g = rule.matcher.as_str();
        match rule.effect {
            Effect::Allow => match super::windows::literal_subtree(g) {
                Some(dir) => {
                    if !read.contains(&dir) {
                        read.push(dir.clone());
                    }
                    if rule.access == FsAccess::ReadWrite
                        && !super::windows::is_dangerous_write_root(&dir)
                        && !write.contains(&dir)
                    {
                        write.push(dir);
                    }
                }
                None if super::windows::is_whole_fs(g) => degrade.generous_read_narrowed = true,
                None if super::windows::has_glob_meta(g) => degrade.glob_read_unenforced = true,
                None => {}
            },
            Effect::Deny => match super::windows::literal_subtree(g) {
                Some(p) => {
                    if !deny.contains(&p) {
                        deny.push(p);
                    }
                }
                // A whole-fs deny is the default-deny base, carried by the account having no
                // reach rather than by an ACE — not a lost deny.
                None if super::windows::is_whole_fs(g) => {}
                // A glob deny (`**/.env`, `C:/proj/*.pem`) is NOT expressible as one ACE.
                // This is a HOLE, not over-confinement, so it is reported separately.
                None => degrade.glob_deny_unenforced = true,
            },
        }
    }
    (read, write, deny, degrade)
}

/// Decide the net posture. Mirrors `backend::start_proxy_if_needed`, which is what actually
/// decides whether a proxy is running.
pub(crate) fn plan_net(net: &crate::policy::NetPolicy) -> AccountNet {
    if !net.enforce {
        return AccountNet::UnconfinedButFenced;
    }
    if net.rules.iter().any(|r| r.effect == Effect::Allow) {
        AccountNet::ProxyOnly
    } else {
        AccountNet::DenyAll
    }
}

// ── one-time setup / teardown / status (the elevated half) ──────────────────────

/// ELEVATED, once per machine. Lock nub's state directory, create the sandbox account, install
/// the WFP egress fence over `port_range`, and record the marker every later unelevated run
/// reads. Idempotent — safe to re-run to repair a partial install.
///
/// ORDER IS DELIBERATE AT BOTH ENDS. The state directory is locked FIRST, before `provision`
/// writes a credential into it: the DACL is the DPAPI blob's only boundary, so writing the
/// credential first would leave it world-readable for a live account if the lock then failed.
/// The marker is written LAST, so a failure anywhere leaves the machine looking un-provisioned
/// and every run fails closed with "run the setup", rather than half-provisioned and failing in
/// some less legible way later.
#[cfg(target_os = "windows")]
pub fn setup(port_range: Option<(u16, u16)>) -> std::io::Result<String> {
    if !account::is_elevated() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            elevated_setup_instruction(),
        ));
    }
    let range = port_range.unwrap_or(wfp::DEFAULT_PROXY_PORT_RANGE);
    // `store_credential` uses `create_dir_all`, a no-op on the directory created here, and the
    // elevated writer keeps access through the Administrators ace this stamps.
    let cred_dir = account::credential_dir()?;
    std::fs::create_dir_all(&cred_dir)?;
    acl::lock_to_admins(&cred_dir)?;
    let sid = account::provision()?;
    wfp::install(&sid, range)?;
    state::write_marker(&state::Marker {
        version: state::MARKER_VERSION,
        sid: sid.clone(),
        port_low: range.0,
        port_high: range.1,
    })?;
    Ok(sid)
}

/// ELEVATED. Undo [`setup`] completely: sweep every ace the ledger records, remove the WFP
/// objects, delete the account/group/profile, and drop the marker. Every step is idempotent
/// and a later failure never skips the cleanup already done.
#[cfg(target_os = "windows")]
pub fn teardown() -> std::io::Result<()> {
    if !account::is_elevated() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "removing the nub sandbox account and its network filters needs administrator. Open a \
             terminal as administrator — right-click Windows Terminal or PowerShell and choose \
             \"Run as administrator\" — then run `nub setup-sandbox --undo`.",
        ));
    }
    // Sweep BEFORE the account is deleted: once the SID stops resolving, the leftover aces
    // render as raw SID strings and are far harder to attribute.
    let _ = clean();
    let wfp_result = wfp::uninstall();
    let account_result = account::deprovision();
    let _ = state::remove_marker();
    let _ = state::clear_ledger();
    // The state directory carries the protected DACL and Administrators owner `setup` wrote,
    // so leaving it behind is residue an unelevated user cannot clear themselves.
    let _ = state::state_dir().map(std::fs::remove_dir_all);
    wfp_result.and(account_result)
}

/// UNELEVATED. Strip every ace the ledger recorded, then clear it. This is the crash-residue
/// path: a run killed between grant and strip leaves aces behind, and this collects them.
/// Returns how many paths were swept. A path that no longer exists is pruned rather than
/// retried forever.
#[cfg(target_os = "windows")]
pub fn clean() -> std::io::Result<usize> {
    let Some(marker) = state::read_marker()? else {
        return Ok(0);
    };
    // The SAME live-SID check the launch makes, for the same reason. This sweep strips explicit
    // aces for whatever trustee the marker names, from whatever paths the ledger lists — BOTH
    // read off disk. Trusting them unverified would make `clean` an ace-removal primitive
    // aimed by anyone who can write nub's state directory.
    if account::lookup_sid()?.as_deref() != Some(marker.sid.as_str()) {
        return Err(std::io::Error::other(
            "the recorded sandbox SID does not match the live account, so nub will not sweep \
             aces for it — re-run `nub setup-sandbox` from an elevated prompt",
        ));
    }
    let paths = state::ledger_paths()?;
    let mut swept = 0;
    for p in &paths {
        match acl::strip(p, &marker.sid) {
            Ok(()) => swept += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => swept += 1,
            Err(e) => {
                tracing::debug!(path = %p.display(), error = %e, "sandbox: ace sweep failed");
            }
        }
    }
    if swept == paths.len() {
        state::clear_ledger()?;
    }
    Ok(swept)
}

/// A human-readable provisioning report.
///
/// The filter count is only reachable when elevated: WFP gates even ENUMERATION on
/// administrator, so an unelevated caller genuinely cannot see it and the report says so
/// rather than implying the fence is absent. That unreadability deliberately does NOT make the
/// report "not ready": the launch's own fail-closed gate is the marker plus a live SID that
/// matches it, so readiness is reported against the same two facts the launch checks — an
/// unelevated caller would otherwise be told the sandbox is broken every single time.
#[cfg(target_os = "windows")]
pub fn status() -> std::io::Result<crate::backend::StatusReport> {
    let marker = state::read_marker()?;
    let live_sid = account::lookup_sid()?;
    let mut out = String::new();
    let mut ready = false;
    match (&marker, &live_sid) {
        (None, None) => out.push_str("sandbox account: not set up\n"),
        (None, Some(sid)) => out.push_str(&format!(
            "sandbox account: `{SANDBOX_ACCOUNT}` exists ({sid}) but nub has no setup record\n"
        )),
        (Some(m), None) => out.push_str(&format!(
            "sandbox account: recorded as {} but no longer exists\n",
            m.sid
        )),
        (Some(m), Some(sid)) if m.sid != *sid => out.push_str(&format!(
            "sandbox account: SID changed (recorded {}, live {sid})\n",
            m.sid
        )),
        (Some(m), Some(sid)) => {
            ready = true;
            out.push_str(&format!(
                "sandbox account: `{SANDBOX_ACCOUNT}` ({sid})\nproxy port window: {}-{}\n",
                m.port_low, m.port_high
            ));
        }
    }
    // THE ONE PRECONDITION THE PER-RUN PATH CANNOT CHECK. `apply` trusts the marker plus a live
    // SID match, because WFP gates even ENUMERATION on administrator — so an unelevated run
    // genuinely cannot tell whether the egress fence still exists. An administrator who deleted
    // the filters and left the marker leaves every later run believing it is fenced when it is
    // not. This report is the only place that can catch it, and only when elevated, so when it
    // CAN look it treats an empty filter set as not-ready rather than reporting a count nobody
    // reads.
    let mut filters_gone = false;
    out.push_str(
        &match (account::is_elevated(), wfp::installed_filter_count()) {
            (false, _) => {
                "wfp filters: cannot read (enumeration requires administrator)\n".to_string()
            }
            (true, Ok(0)) => {
                filters_gone = true;
                "wfp filters: NONE installed — the egress fence is missing\n".to_string()
            }
            (true, Ok(n)) => format!("wfp filters: {n} installed\n"),
            (true, Err(e)) => format!("wfp filters: could not read ({e})\n"),
        },
    );
    out.push_str(&format!(
        "acl ledger: {} path(s) recorded\n",
        state::ledger_paths().map(|p| p.len()).unwrap_or(0)
    ));
    // `filters_gone` only DESCRIBES a machine that is otherwise provisioned. On a machine with
    // no account it is the ordinary post-teardown state, and reporting "the account exists but
    // its filters are gone" there contradicts the account line printed two lines above it.
    let filters_gone = filters_gone && ready;
    let ready = ready && !filters_gone;
    out.push_str(&if ready {
        "\nThe sandbox can enforce on this host.\n".to_string()
    } else if filters_gone {
        format!(
            "\nThe sandbox cannot enforce on this host: the account exists but its network \
             filters are gone, so egress is unfenced. Re-run the setup to reinstall them.\n\n{}\n",
            elevated_setup_instruction()
        )
    } else {
        format!(
            "\nThe sandbox cannot enforce on this host: the dedicated account is not \
             provisioned.\n\n{}\n",
            elevated_setup_instruction()
        )
    });
    Ok(crate::backend::StatusReport { text: out, ready })
}

/// The remedy every fail-closed Windows message ends on.
///
/// NUB DOES NOT SELF-ELEVATE HERE, and the choice is deliberate rather than a missing feature.
/// A `ShellExecuteExW`/`runas` re-launch would raise one UAC consent dialog, but it starts a
/// SEPARATE process on a new console — so the setup's own report, which is the part that tells
/// a user what was installed and what to do next, lands in a window that closes on exit. It
/// would also give Windows an elevation posture Linux deliberately does not have, which is a
/// decision about nub's relationship to privilege, not a per-platform convenience call.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn elevated_setup_instruction() -> String {
    format!(
        "Creating the account and installing the network filters both need administrator. Open a \
         terminal as administrator — right-click Windows Terminal or PowerShell and choose \"Run \
         as administrator\" — then run:\n\n    {SETUP_COMMAND}"
    )
}

// ── the apply() entry (Windows-only: constructs Prepared.launch) ────────────────

/// Build the account-backend launch plan, or fail CLOSED with an actionable message.
///
/// Two fail-closed gates, both deliberate: the machine must be provisioned (the account and
/// the WFP filters exist), and — when the policy allows specific hosts — the egress proxy must
/// have bound INSIDE the pre-authorized loopback window. A proxy outside the window is not a
/// degradation to warn about, it is a child that would reach nothing while the policy claimed
/// per-host access, so it is an error.
#[cfg(target_os = "windows")]
pub(crate) fn apply(
    policy: &crate::policy::SandboxPolicy,
    spec: crate::backend::CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<&std::path::Path>,
) -> Result<crate::backend::Prepared, crate::backend::Degradation> {
    use crate::backend::{Degradation, Prepared, windows::WindowsLaunch};

    let marker = match state::read_marker() {
        Ok(Some(m)) => m,
        Ok(None) => return Err(not_set_up("this machine has no nub sandbox account")),
        // A marker that exists but cannot be READ is a different failure from one that was
        // never written: re-running setup does not fix a permission problem, so it must not
        // borrow that instruction.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(crate::backend::Degradation {
                lost: vec!["fs".to_string(), "net".to_string()],
                reason: Some(format!(
                    "nub's sandbox setup record exists but is not readable by this user ({e}). \
                     The state directory is readable only by administrators and the user who \
                     ran the setup — run the sandboxed command as that user, or re-run \
                     `{SETUP_COMMAND}` as the user who will run it."
                )),
            });
        }
        Err(e) => return Err(not_set_up(&e.to_string())),
    };

    let net = plan_net(&policy.net);
    if net == AccountNet::ProxyOnly {
        let Some(port) = proxy_port else {
            return Err(Degradation {
                lost: vec!["net-per-host".to_string()],
                reason: Some(
                    "the egress proxy required for per-host / TLS-inspect enforcement could not \
                     start"
                        .to_string(),
                ),
            });
        };
        if port < marker.port_low || port > marker.port_high {
            return Err(Degradation {
                lost: vec!["net-per-host".to_string()],
                reason: Some(format!(
                    "nub's egress proxy bound port {port}, outside the {}-{} loopback window the \
                     installed WFP filters permit — the sandboxed child could not reach it. Free a \
                     port in that range, or re-run `{SETUP_COMMAND}` from an elevated prompt to \
                     authorize a different one.",
                    marker.port_low, marker.port_high
                )),
            });
        }
    }

    let (mut read_grants, write_grants, denies, fs_degrade) = derive_plan(&policy.fs);

    // The account is a DIFFERENT principal, so nothing under the invoking user's profile is
    // reachable by default — including the program image itself when it is a per-user install
    // (nvm/fnm Node, Scoop, `%LOCALAPPDATA%\Programs`). Granting the program FILE (not its
    // parent dir, which would sweep in a neighbouring secret) is the minimum that lets the
    // child exec at all. A program that loads SIBLING DLLs from its own directory still needs
    // the front-end to put that toolchain dir in the read allow-set — the same launcher
    // contract the macOS backend documents.
    if let Some(prog) = crate::backend::windows::resolve_program(&spec.program, spec.cwd.as_deref())
        && !read_grants.contains(&prog)
    {
        read_grants.push(prog);
    }
    // CreateProcess resolves the child's working directory under the CHILD's token, so an
    // ungranted cwd fails the spawn outright rather than merely hiding files.
    if let Some(cwd) = &spec.cwd
        && !read_grants.contains(cwd)
    {
        read_grants.push(cwd.clone());
    }
    if let Some(bundle) = ca_bundle {
        let b = bundle.to_path_buf();
        if !read_grants.contains(&b) {
            read_grants.push(b);
        }
    }

    let mut deg = Degradation::full();
    let mut reason: Option<String> = None;
    if fs_degrade.generous_read_narrowed {
        deg.lost.push("fs-read".to_string());
        reason.get_or_insert_with(|| {
            "the sandbox runs as a separate local account, so a read-everything policy reaches \
             only what any standard local user reads plus the explicit grants — never the \
             invoking user's profile (over-confined, not widened)"
                .to_string()
        });
    }
    if fs_degrade.glob_read_unenforced {
        deg.lost.push("fs-read-glob".to_string());
        reason.get_or_insert_with(|| {
            "an embedded-glob read allow can't be a single inheritable ACE — that path is not \
             read-granted (over-confined)"
                .to_string()
        });
    }
    // A missed DENY is a hole, not over-confinement — reported separately so it is never read
    // as the benign case above.
    if fs_degrade.glob_deny_unenforced {
        deg.lost.push("fs-deny-glob".to_string());
        reason.get_or_insert_with(|| {
            "an embedded-glob deny can't be a single ACE — that path is NOT denied to the \
             sandbox account"
                .to_string()
        });
    }
    if net == AccountNet::UnconfinedButFenced {
        deg.lost.push("net".to_string());
        reason.get_or_insert_with(|| {
            "the dedicated sandbox account is permanently egress-fenced by the WFP filters \
             installed at setup, so an unconfined net policy is still denied (over-confined)"
                .to_string()
        });
    }
    deg.reason = reason;

    let launch = AccountLaunch {
        program: spec.program,
        args: spec.args,
        cwd: spec.cwd,
        read_grants,
        write_grants,
        denies,
        env: build_child_env(&policy.env, proxy_port, proxy_token, ca_bundle),
    };

    Ok(Prepared {
        command: std::process::Command::new(&launch.program),
        degradation: deg,
        proxy: None,
        launch: Some(WindowsLaunch::Account(launch)),
        // The three fields the AppContainer backend also defaults. This backend does not own a
        // private tmp (the account's own profile provides the isolation) and does not redact,
        // matching `windows::apply`'s account-free path exactly.
        _private_tmp: None,
        redact_stdout: false,
        redact_stderr: false,
    })
}

#[cfg(target_os = "windows")]
fn not_set_up(detail: &str) -> crate::backend::Degradation {
    crate::backend::Degradation {
        lost: vec!["fs".to_string(), "net".to_string()],
        reason: Some(format!(
            "{detail}. This policy needs nub's dedicated Windows sandbox account (it uses a \
             generous-read base, a deny inside a granted directory, or per-host network rules — \
             none of which Windows can express without one). Run `{SETUP_COMMAND}` once from an \
             elevated (Run as administrator) prompt."
        )),
    }
}

/// The child's environment, or `None` to let seclogon build the sandbox account's own profile
/// environment (isolated `USERPROFILE`/`TEMP`/`LOCALAPPDATA`, machine `PATH`).
///
/// The Windows launch block is all-or-nothing — unlike a `Command`'s inherit-plus-override —
/// so "inherit and add the proxy vars" has to be MATERIALIZED from the parent's environment
/// here. That is only reached when the policy did NOT ask for env confinement, in which case
/// inheriting is exactly the contract the mac/linux backends give.
#[cfg(target_os = "windows")]
fn build_child_env(
    env: &crate::policy::EnvPolicy,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<&std::path::Path>,
) -> Option<BTreeMap<String, String>> {
    let injecting = proxy_port.is_some() || ca_bundle.is_some();
    if !env.enforce && !injecting {
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
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        CanonGlob, EnvPolicy, FsOrigin, FsRule, FsRuleSet, NetPolicy, NetRule, NetTarget,
        PidPolicy, SandboxPolicy, TmpMode,
    };

    fn fs_policy(default_effect: Effect, entries: Vec<FsRule>) -> FsPolicy {
        FsPolicy {
            rules: FsRuleSet {
                entries,
                default_effect,
            },
            tmp: TmpMode::Shared,
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

    fn policy(fs: FsPolicy, net: NetPolicy) -> SandboxPolicy {
        SandboxPolicy {
            fs,
            net,
            env: EnvPolicy::default(),
            pid: PidPolicy::default(),
            build_jail: false,
        }
    }

    fn net_off() -> NetPolicy {
        NetPolicy {
            enforce: false,
            ..Default::default()
        }
    }

    /// build-jail's exact shape — default-deny allowlist, no net — must stay on the
    /// admin-free AppContainer path. A regression here would make `nub install` demand
    /// elevation, which is the one thing the pivot promised it would not do.
    #[test]
    fn build_jail_shape_does_not_need_the_account_backend() {
        let p = policy(
            fs_policy(
                Effect::Deny,
                vec![rule("C:/jail", Effect::Allow, FsAccess::ReadWrite)],
            ),
            net_off(),
        );
        assert!(!needs_account_backend(&p));
    }

    /// A coarse net DENY-ALL still needs no account — nothing is reachable and no proxy runs,
    /// which the AppContainer expresses by withholding `internetClient`.
    #[test]
    fn coarse_net_deny_all_does_not_need_the_account_backend() {
        let p = policy(
            fs_policy(
                Effect::Deny,
                vec![rule("C:/jail", Effect::Allow, FsAccess::ReadWrite)],
            ),
            NetPolicy {
                enforce: true,
                default_effect: Effect::Deny,
                ..Default::default()
            },
        );
        assert!(!needs_account_backend(&p));
    }

    /// Per-host egress is the axis that forces WFP, which forces the account.
    #[test]
    fn per_host_net_needs_the_account_backend() {
        let p = policy(
            fs_policy(
                Effect::Deny,
                vec![rule("C:/jail", Effect::Allow, FsAccess::ReadWrite)],
            ),
            NetPolicy {
                enforce: true,
                default_effect: Effect::Deny,
                rules: vec![NetRule {
                    target: NetTarget::Host("example.com".into()),
                    effect: Effect::Allow,
                }],
                ..Default::default()
            },
        );
        assert!(needs_account_backend(&p));
    }

    /// The agent-sandbox fs shape — generous read — is inexpressible as an allowlist, so it
    /// routes to the account backend even with net untouched.
    #[test]
    fn generous_read_needs_the_account_backend() {
        let p = policy(fs_policy(Effect::Allow, vec![]), net_off());
        assert!(needs_account_backend(&p));
    }

    /// The whole-fs `**` Allow entry is what the compiler actually emits for `"..."` /
    /// `sandbox: true` — the primary real-world agent-sandbox policy — so its routing is
    /// pinned separately from the `default_effect == Allow` arm above.
    #[test]
    fn whole_fs_allow_entry_needs_the_account_backend() {
        let p = policy(
            fs_policy(
                Effect::Deny,
                vec![
                    rule("**", Effect::Allow, FsAccess::Read),
                    rule("C:/proj", Effect::Allow, FsAccess::ReadWrite),
                ],
            ),
            net_off(),
        );
        assert!(needs_account_backend(&p));
    }

    /// A deny that lands inside a granted subtree is the AppContainer's known hole (its
    /// inheritable allow defeats the deny), so it must route to the account.
    #[test]
    fn deny_inside_a_grant_needs_the_account_backend() {
        let p = policy(
            fs_policy(
                Effect::Deny,
                vec![
                    rule("C:/proj", Effect::Allow, FsAccess::ReadWrite),
                    rule("C:/proj/.env", Effect::Deny, FsAccess::Read),
                ],
            ),
            net_off(),
        );
        assert!(needs_account_backend(&p));
    }

    /// Denies become REAL ACE targets here — the difference from the allowlist backend, which
    /// drops them entirely.
    #[test]
    fn derive_plan_keeps_literal_denies_as_ace_targets() {
        let (read, write, deny, degrade) = derive_plan(&fs_policy(
            Effect::Deny,
            vec![
                rule("C:/proj", Effect::Allow, FsAccess::ReadWrite),
                rule("C:/proj/.env", Effect::Deny, FsAccess::Read),
            ],
        ));
        assert_eq!(read, vec![PathBuf::from("C:/proj")]);
        assert_eq!(write, vec![PathBuf::from("C:/proj")]);
        assert_eq!(deny, vec![PathBuf::from("C:/proj/.env")]);
        assert_eq!(degrade, AccountFsDegrade::default());
    }

    /// A glob deny cannot be one ACE. It must be reported as a distinct LOST DENY — reporting
    /// it as over-confinement (like a glob allow) would understate a real hole.
    #[test]
    fn glob_deny_is_reported_as_a_hole_not_as_over_confinement() {
        let (_, _, deny, degrade) = derive_plan(&fs_policy(
            Effect::Deny,
            vec![
                rule("C:/proj", Effect::Allow, FsAccess::ReadWrite),
                rule("**/.env", Effect::Deny, FsAccess::Read),
            ],
        ));
        assert!(deny.is_empty(), "a glob deny yields no literal ACE target");
        assert!(degrade.glob_deny_unenforced);
        assert!(!degrade.glob_read_unenforced);
    }

    /// The whole-fs deny is the default-deny BASE, carried by the account simply having no
    /// reach — flagging it as a lost deny would emit a permanent false degradation on every
    /// ordinary confining policy.
    #[test]
    fn whole_fs_deny_base_is_not_a_lost_deny() {
        let (_, _, deny, degrade) = derive_plan(&fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Deny, FsAccess::Read),
                rule("C:/proj", Effect::Allow, FsAccess::ReadWrite),
            ],
        ));
        assert!(deny.is_empty());
        assert!(!degrade.glob_deny_unenforced);
    }

    /// A `..`-collapsed write grant must never land on a system root, same guard as the
    /// AppContainer path — an inheritable modify ACE there would be a filesystem-wide hole.
    #[test]
    fn dangerous_write_roots_get_no_write_grant() {
        let (read, write, _, _) = derive_plan(&fs_policy(
            Effect::Deny,
            vec![rule("C:/Windows", Effect::Allow, FsAccess::ReadWrite)],
        ));
        assert_eq!(read, vec![PathBuf::from("C:/Windows")]);
        assert!(write.is_empty(), "C:/Windows must never be write-granted");
    }

    /// Net-unconfined under this backend is still fenced by the persistent filters, so it is
    /// over-confinement that must be reported rather than a silently honored allow-all.
    #[test]
    fn unconfined_net_under_the_account_is_reported_as_fenced() {
        assert_eq!(plan_net(&net_off()), AccountNet::UnconfinedButFenced);
    }

    /// The elevation remedy is the ONLY thing an unelevated caller can act on, and it once
    /// named the internal `--sandbox-admin` flag that no help text or doc mentions. It must
    /// name the documented verb, and it must not smuggle a self-elevation instruction that the
    /// code does not perform.
    #[test]
    fn the_elevation_remedy_names_the_documented_verb() {
        let remedy = elevated_setup_instruction();
        assert!(
            remedy.contains(SETUP_COMMAND),
            "the remedy must name `{SETUP_COMMAND}`: {remedy}"
        );
        assert!(
            !remedy.contains("--sandbox-admin"),
            "the remedy must not point at the internal flag: {remedy}"
        );
        assert!(
            remedy.contains("administrator"),
            "the remedy must say what privilege is missing: {remedy}"
        );
    }
}
