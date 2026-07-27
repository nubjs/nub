//! One-time privileged host setup for the Linux agent-sandbox — `nub sandbox {setup,status,teardown}`.
//!
//! WHY THIS IS PRIVILEGED, AND ONLY ONCE. On Ubuntu 23.10+/24.04 the kernel strips every
//! capability from an unprivileged user namespace (the AppArmor `unprivileged_userns`
//! transition), so bubblewrap cannot remap the filesystem view the agent-sandbox needs. The
//! sole escape is a path-keyed AppArmor profile that grants `userns` to one exact executable.
//! Because nub installs to relocatable npm/curl paths, the profile cannot key on nub's own
//! moving binary — so setup installs a fixed, root-owned copy of the packaged bubblewrap at
//! `/usr/libexec/nub/nub-bwrap` (epic B2) and keys the profile to that stable path. Root is
//! paid once per machine; every sandbox run afterward is fully unprivileged. The global
//! `apparmor_restrict_unprivileged_userns` control is never touched — the grant is one path,
//! one group. Design record: `.fray/sandbox-escalation-ux.md`; mechanism proof:
//! `.fray/sandbox-linux-privilege-reexam.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use super::linux::DEDICATED_HELPER_PATH;

const HELPER_DIR: &str = "/usr/libexec/nub";
const HELPER_GROUP: &str = "nub-sandbox";
const PROFILE_NAME: &str = "nub-bwrap-userns";
const PROFILE_PATH: &str = "/etc/apparmor.d/nub-bwrap-userns";
/// The path-bound profile bytes shipped in-tree; setup writes these verbatim so the loaded
/// profile is exactly what nub was built with.
const PROFILE_CONTENT: &str = include_str!("../../setup/linux-nesting/nub-bwrap-userns.apparmor");

/// The `--version` string the packaged bubblewrap must report, derived from the version nub was
/// built against (`NUB_BWRAP_VERSION`), so a release and its bundled bwrap can never drift. Falls
/// back to the 0.11.2 release default for a dev build that set no version.
fn expected_bwrap_version() -> String {
    format!(
        "bubblewrap {}",
        option_env!("NUB_BWRAP_VERSION").unwrap_or("0.11.2")
    )
}

/// The elevated command a normal user must run. Emitted by the "not set up" runtime error and
/// by `status`, so the exact string lives in one place.
pub const SETUP_COMMAND: &str = "sudo nub sandbox setup";
/// The variant that needs no fresh login afterwards — the one-line form for CI and any other
/// single-tenant machine.
pub const SETUP_COMMAND_ALL_USERS: &str = "sudo nub sandbox setup --all-users";

/// Who may execute the installed helper. The AppArmor grant is path-keyed either way, so this
/// decides only WHO can reach the one profiled binary — never what a namespace it creates can
/// touch, and never the global `apparmor_restrict_unprivileged_userns` control.
///
/// `Group` (default) keeps unprivileged-userns creation reachable only by `nub-sandbox`
/// members, which is what a shared multi-user host wants. Its cost is that a group added by
/// this run cannot reach the session that ran it, so the caller must re-login or use `sg`.
/// `AllUsers` trades that restriction away for a setup whose effect is immediate — correct for
/// an ephemeral CI runner or a single-user workstation, where "other local users" is an empty
/// set and the group buys nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HelperAccess {
    Group,
    AllUsers,
}

impl HelperAccess {
    fn mode(self) -> &'static str {
        match self {
            Self::Group => "0750",
            Self::AllUsers => "0755",
        }
    }
}

/// What `setup` did, and — the distinction the exit status turns on — whether the session that
/// ran it can actually use the result. A group added mid-session never reaches an
/// already-running process, so "installed and verified" and "usable by you, now" are different
/// facts and are never reported as one.
pub struct SetupReport {
    pub text: String,
    pub usable_now: bool,
}

/// Whether the session that invoked nub can execute the installed helper RIGHT NOW.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SessionAccess {
    /// The helper is world-executable, or the caller is root (its owner).
    Open,
    /// The caller's LIVE group set already carries the helper's group.
    Member,
    /// The helper is group-gated and the caller's live group set lacks it. The account may
    /// well be a member on paper — `/etc/group` is not what the kernel checks.
    NeedsFreshSession,
    /// The caller's group set could not be read.
    Unknown,
}

fn is_root() -> bool {
    // Safe FFI: geteuid never fails and touches no memory.
    unsafe { libc::geteuid() == 0 }
}

/// Whether nub could re-run itself under `sudo` with no prompt — true on a GitHub Actions
/// runner and on most single-user workstations. REPORTED, NEVER ACTED ON: whether nub may
/// elevate on its own initiative is a posture call this code does not make.
pub fn passwordless_sudo_available() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn proc_status_field(pid: u32, field: &str) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(field).map(|rest| rest.trim().to_string()))
}

fn first_id(field: &str) -> Option<u32> {
    field.split_whitespace().next()?.parse().ok()
}

/// The group set the KERNEL will check for the session that invoked this command.
///
/// Under `sudo` this process's own group set is root's, and the account's `/etc/group` rows are
/// not it either — the credentials that matter were fixed when the caller's shell was created.
/// So walk the parent chain to the first ancestor still running as `SUDO_UID` and read the
/// group set that process actually holds. That is the only reading that answers "will the next
/// command in this shell be able to open the helper?".
fn invoking_session_gids() -> Option<Vec<u32>> {
    let Some(sudo_uid) = std::env::var("SUDO_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return Some(current_process_gids());
    };
    let mut pid = std::process::id();
    for _ in 0..16 {
        let ppid = proc_status_field(pid, "PPid:").and_then(|f| first_id(&f))?;
        if ppid <= 1 {
            return None;
        }
        if proc_status_field(ppid, "Uid:").and_then(|f| first_id(&f)) == Some(sudo_uid) {
            let mut gids: Vec<u32> = proc_status_field(ppid, "Groups:")
                .map(|list| {
                    list.split_whitespace()
                        .filter_map(|g| g.parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            if let Some(primary) = proc_status_field(ppid, "Gid:").and_then(|f| first_id(&f)) {
                gids.push(primary);
            }
            return Some(gids);
        }
        pid = ppid;
    }
    None
}

fn current_process_gids() -> Vec<u32> {
    // Safe FFI: the standard two-call getgroups — size query, then fill a buffer of exactly
    // that size. `getgid` cannot fail.
    unsafe {
        let count = libc::getgroups(0, std::ptr::null_mut());
        if count < 0 {
            return vec![libc::getgid() as u32];
        }
        let mut buffer = vec![0 as libc::gid_t; count as usize];
        let filled = libc::getgroups(count, buffer.as_mut_ptr());
        if filled < 0 {
            return vec![libc::getgid() as u32];
        }
        buffer.truncate(filled as usize);
        let mut gids: Vec<u32> = buffer.into_iter().map(|gid| gid as u32).collect();
        gids.push(libc::getgid() as u32);
        gids
    }
}

fn group_gid(group: &str) -> Option<u32> {
    let out = Command::new("getent")
        .args(["group", group])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .split(':')
        .nth(2)?
        .parse()
        .ok()
}

fn helper_mode() -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(DEDICATED_HELPER_PATH)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o7777)
}

/// Resolve [`SessionAccess`] from the helper's real mode bits and the caller's live credentials
/// — never from `/etc/group`, which is exactly the source that produced the false success this
/// function replaces.
fn session_access() -> SessionAccess {
    let Some(mode) = helper_mode() else {
        return SessionAccess::Unknown;
    };
    if mode & 0o001 != 0 {
        return SessionAccess::Open;
    }
    // Root owns the helper and 0750 carries the owner-execute bit, so a root caller with no
    // unprivileged session behind it is genuinely unblocked.
    if is_root() && std::env::var_os("SUDO_UID").is_none() {
        return SessionAccess::Open;
    }
    let Some(gid) = group_gid(HELPER_GROUP) else {
        return SessionAccess::Unknown;
    };
    match invoking_session_gids() {
        Some(gids) if gids.contains(&gid) => SessionAccess::Member,
        Some(_) => SessionAccess::NeedsFreshSession,
        None => SessionAccess::Unknown,
    }
}

/// The exact command a caller whose session lacks the group can run to work around it without
/// logging out. `sg` starts a new process with the group added, which is the only way an
/// already-running shell reaches a group it did not have at login.
fn sg_workaround() -> String {
    format!("    sg {HELPER_GROUP} -c '<your command>'")
}

/// Locate nub's packaged bubblewrap next to the running binary, matching the runtime resolver's
/// search (`<nub-dir>/nub-resources/bwrap` and the `../` sibling for the npm layout).
fn packaged_bwrap() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locating the nub executable: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "the nub executable has no parent directory".to_string())?;
    for candidate in [
        dir.join("nub-resources/bwrap"),
        dir.join("../nub-resources/bwrap"),
    ] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "nub's packaged bubblewrap was not found next to {} (looked for nub-resources/bwrap)",
        exe.display()
    ))
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// Run a system tool, mapping a spawn failure or non-zero exit into a message that names the
/// tool and its stderr. `ok_codes` lets an idempotent tool's "already exists" exit be accepted.
fn run_tool(cmd: &mut Command, ok_codes: &[i32]) -> Result<(), String> {
    let program = format!("{:?}", cmd.get_program());
    let out = cmd
        .output()
        .map_err(|e| format!("running {program}: {e}"))?;
    let code = out.status.code().unwrap_or(-1);
    if out.status.success() || ok_codes.contains(&code) {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(format!("{program} failed (exit {code}): {}", stderr.trim()))
}

fn tool_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn user_in_group(user: &str, group: &str) -> bool {
    Command::new("id")
        .args(["-nG", user])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .any(|g| g == group)
        })
        .unwrap_or(false)
}

/// ELEVATED. Install the fixed-path helper + AppArmor profile, add the invoking user to the
/// group, and verify. Idempotent: re-running repairs only what drifted.
///
/// The report separates two facts that an earlier version collapsed into one: that the helper
/// and profile WORK, and that the CALLER can use them. `usable_now` carries the second, and the
/// caller maps it to the exit status — a setup whose result the caller cannot reach must not
/// exit 0.
pub fn setup(access: HelperAccess) -> Result<SetupReport, String> {
    if !is_root() {
        return Err(format!(
            "the sandbox setup installs a root-owned helper and an AppArmor profile, so it needs \
             root. Re-run it with:\n\n    {SETUP_COMMAND}\n"
        ));
    }
    if !tool_exists("apparmor_parser") {
        return Err(
            "apparmor_parser is not available, so this host does not run AppArmor. On a host \
             without AppArmor the sandbox needs no setup; if you are on Ubuntu, install the \
             `apparmor` package first."
                .to_string(),
        );
    }

    let src = packaged_bwrap()?;
    // Verify the digest BEFORE executing the binary: a tampered helper must not be run as root
    // even for `--version`. When nub carries no pinned digest (dev build) there is nothing to
    // verify against, so the version string is the only gate.
    let src_digest = sha256_hex(&src)?;
    let digest_note = match option_env!("NUB_BWRAP_SHA256") {
        Some(expected) if expected == src_digest => {
            format!("verified packaged bubblewrap digest {src_digest}")
        }
        Some(expected) => {
            return Err(format!(
                "the packaged bubblewrap digest {src_digest} does not match the digest nub was \
                 built with ({expected}); the install is corrupt"
            ));
        }
        None => format!(
            "this nub build carries no pinned digest (dev build); installed digest {src_digest}"
        ),
    };

    let src_version = Command::new(&src)
        .arg("--version")
        .output()
        .map_err(|e| format!("running the packaged bubblewrap: {e}"))?;
    let version = String::from_utf8_lossy(&src_version.stdout);
    let version = version.trim();
    let expected = expected_bwrap_version();
    if version != expected {
        return Err(format!(
            "the packaged bubblewrap reported `{version}`, expected `{expected}`"
        ));
    }

    let mut report = Vec::new();

    // 1. Group.
    if group_exists(HELPER_GROUP) {
        report.push(format!("group {HELPER_GROUP} already present"));
    } else {
        // groupadd exit 9 = "group already exists" — tolerated for idempotency under races.
        run_tool(
            Command::new("groupadd").args(["--system", HELPER_GROUP]),
            &[9],
        )?;
        report.push(format!("created system group {HELPER_GROUP}"));
    }

    // 2. Membership — the human who ran sudo. Under `AllUsers` the group is not the gate, so
    //    the caller's memberships are left alone rather than changed to no effect.
    let target_user = std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty());
    match (&target_user, access) {
        (_, HelperAccess::AllUsers) => report.push(format!(
            "{HELPER_GROUP} membership not required (--all-users installs a world-executable helper)"
        )),
        (Some(user), _) if user_in_group(user, HELPER_GROUP) => {
            report.push(format!("{user} already in {HELPER_GROUP}"));
        }
        (Some(user), _) => {
            run_tool(
                Command::new("gpasswd").args(["--add", user, HELPER_GROUP]),
                &[],
            )?;
            report.push(format!("added {user} to {HELPER_GROUP}"));
        }
        (None, _) => report.push(format!(
            "no SUDO_USER in the environment; add members with: sudo gpasswd --add <login> {HELPER_GROUP}"
        )),
    }

    // 3. Helper: root-owned, NOT setuid. `install(1)` is atomic.
    std::fs::create_dir_all(HELPER_DIR).map_err(|e| format!("creating {HELPER_DIR}: {e}"))?;
    run_tool(
        Command::new("install").args([
            "-D",
            "-o",
            "root",
            "-g",
            HELPER_GROUP,
            "-m",
            access.mode(),
            &src.to_string_lossy(),
            DEDICATED_HELPER_PATH,
        ]),
        &[],
    )?;
    report.push(format!(
        "installed helper at {DEDICATED_HELPER_PATH} (root:{HELPER_GROUP} {})",
        access.mode()
    ));

    // 4. Profile: path-bound `userns` grant for the helper only. The global control is left on.
    std::fs::write(PROFILE_PATH, PROFILE_CONTENT)
        .map_err(|e| format!("writing {PROFILE_PATH}: {e}"))?;
    run_tool(
        Command::new("apparmor_parser").args(["--replace", PROFILE_PATH]),
        &[],
    )?;
    report.push(format!("loaded AppArmor profile {PROFILE_NAME}"));

    // 5. Verify the profile actually unlocks userns FOR AN UNPRIVILEGED CALLER — a root probe is
    //    a false positive (root can create a userns regardless of the profile), so drop to the
    //    invoking user. Under `Group` the launch takes the group explicitly, which proves the
    //    profile works for a member but says NOTHING about the caller's current session; step 6
    //    is what answers that.
    match &target_user {
        Some(user) if tool_exists("runuser") => match verify_as(user, access) {
            Ok(()) => report.push(match access {
                HelperAccess::Group => {
                    format!("verified: a {HELPER_GROUP} member can confine a command")
                }
                HelperAccess::AllUsers => {
                    format!("verified: {user} can confine a command with no group membership")
                }
            }),
            Err(e) => {
                return Err(format!(
                    "setup installed the helper and profile, but the verification launch failed, \
                     so the sandbox is not usable yet: {e}"
                ));
            }
        },
        _ => report.push(
            "skipped the behavioral verify (no SUDO_USER or no runuser); the profile is loaded"
                .to_string(),
        ),
    }

    report.push(digest_note);
    report.push(format!(
        "the global unprivileged-user-namespace restriction is left enabled; only \
         {DEDICATED_HELPER_PATH} is opted in"
    ));

    // 6. THE FACT THE VERIFY ABOVE CANNOT ESTABLISH: can the session that ran this command use
    //    the helper? A group added moments ago is absent from every already-running process, so
    //    on the `Group` path the honest answer is usually "not yet" — and a setup that reported
    //    success there sent people chasing AppArmor for a credentials problem.
    let access_now = session_access();
    let usable_now = matches!(access_now, SessionAccess::Open | SessionAccess::Member);
    report.push(match access_now {
        SessionAccess::Open => {
            "this session can execute the helper now; no fresh login needed".to_string()
        }
        SessionAccess::Member => {
            format!("this session already carries {HELPER_GROUP}; no fresh login needed")
        }
        SessionAccess::NeedsFreshSession => format!(
            "\nNOT USABLE FROM THIS SESSION YET. The helper is installed and works, but this \
             shell's group set was fixed when it started, so it does not have {HELPER_GROUP} and \
             neither will anything it launches — including the rest of a CI job. Start a fresh \
             login, or run each command through:\n\n{}\n\nOn an ephemeral or single-user machine \
             (CI runners especially) install without the group gate instead, and the very next \
             command works:\n\n    {SETUP_COMMAND_ALL_USERS}",
            sg_workaround()
        ),
        SessionAccess::Unknown => format!(
            "\nCOULD NOT CONFIRM THIS SESSION CAN USE THE HELPER (its credentials were not \
             readable). If the next command fails to reach {DEDICATED_HELPER_PATH}, start a \
             fresh login or re-run with --all-users."
        ),
    });

    Ok(SetupReport {
        text: report.join("\n"),
        usable_now,
    })
}

/// One bounded confined launch through the installed helper, run as `user`. Proves the
/// path-keyed profile grants the userns to a real unprivileged caller. Under `Group` the group
/// is set explicitly (`runuser` run by root does so with no membership check); under
/// `AllUsers` it is deliberately NOT set, so the launch exercises the access a non-member has.
fn verify_as(user: &str, access: HelperAccess) -> Result<(), String> {
    let mut command = Command::new("runuser");
    command.args(["-u", user]);
    if access == HelperAccess::Group {
        command.args(["-g", HELPER_GROUP]);
    }
    let out = command
        .args([
            "--",
            DEDICATED_HELPER_PATH,
            "--unshare-user",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "/bin/true",
        ])
        .output()
        .map_err(|e| format!("spawning the verification launch: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "confined launch exited {}: {}",
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

fn group_exists(group: &str) -> bool {
    Command::new("getent")
        .args(["group", group])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Secondary members of `group` (the comma-separated 4th field of the `getent group` line). A
/// user whose PRIMARY group this is would not appear, but the group is created `--system` and is
/// never a primary group, so this is the complete member set for the teardown emptiness check.
fn group_members(group: &str) -> Vec<String> {
    Command::new("getent")
        .args(["group", group])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .and_then(|line| line.rsplit(':').next())
                .map(|members| {
                    members
                        .split(',')
                        .filter(|m| !m.trim().is_empty())
                        .map(str::to_string)
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// UNPRIVILEGED. Report what is and isn't set up, changing nothing. Ends with the setup command
/// when anything is missing.
pub fn status() -> Result<String, String> {
    let mut out = String::new();
    let mut ready = true;
    // Kept apart from `ready`: an installed helper the CALLER cannot reach is a different
    // problem with a different remedy, and telling that caller to re-run setup would repeat the
    // misdirection this reporting exists to remove.
    let mut session_gap = false;

    // Helper presence + digest.
    match std::fs::metadata(DEDICATED_HELPER_PATH) {
        Ok(_) => match (
            option_env!("NUB_BWRAP_SHA256"),
            sha256_hex(Path::new(DEDICATED_HELPER_PATH)),
        ) {
            (Some(expected), Ok(actual)) if expected == actual => {
                out.push_str(&format!(
                    "helper: installed at {DEDICATED_HELPER_PATH} (digest matches)\n"
                ));
            }
            (Some(_), Ok(actual)) => {
                ready = false;
                out.push_str(&format!(
                    "helper: installed at {DEDICATED_HELPER_PATH} but digest {actual} does not match this nub build — re-run setup\n"
                ));
            }
            (_, _) => out.push_str(&format!("helper: installed at {DEDICATED_HELPER_PATH}\n")),
        },
        Err(_) => {
            ready = false;
            out.push_str(&format!(
                "helper: not installed ({DEDICATED_HELPER_PATH} missing)\n"
            ));
        }
    }
    if let Some(mode) = helper_mode() {
        out.push_str(&format!(
            "helper: mode {mode:04o} ({})\n",
            if mode & 0o001 != 0 {
                "any local user may execute it".to_string()
            } else {
                format!("only {HELPER_GROUP} members may execute it")
            }
        ));
    }

    // Profile file + loaded state (loaded-state read may need privilege; report what we can).
    if Path::new(PROFILE_PATH).exists() {
        out.push_str(&format!("profile: present at {PROFILE_PATH}\n"));
    } else {
        ready = false;
        out.push_str(&format!("profile: not present ({PROFILE_PATH} missing)\n"));
    }
    match std::fs::read_to_string("/sys/kernel/security/apparmor/profiles") {
        Ok(list) if list.lines().any(|l| l.starts_with("nub_bwrap")) => {
            out.push_str("profile: loaded in the kernel\n")
        }
        Ok(_) => out.push_str("profile: NOT loaded in the kernel — re-run setup\n"),
        Err(_) => out.push_str("profile: loaded-state not readable (needs root to enumerate)\n"),
    }

    // Two DIFFERENT questions, reported separately because they disagree exactly when it
    // matters: is the account a member on paper, and does the live session hold the group?
    // Only the second decides whether the next command can open the helper.
    let user = std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default();
    if !user.is_empty() {
        if user_in_group(&user, HELPER_GROUP) {
            out.push_str(&format!("group: {user} is in {HELPER_GROUP}\n"));
        } else {
            out.push_str(&format!("group: {user} is not in {HELPER_GROUP}\n"));
        }
    }
    // Skipped when nothing is installed — the missing-helper line above is the finding there,
    // and a second line about session credentials would read as an unrelated problem.
    if helper_mode().is_some() {
        match session_access() {
            SessionAccess::Open => out.push_str(&format!(
                "session: this session can execute {DEDICATED_HELPER_PATH}\n"
            )),
            SessionAccess::Member => out.push_str(&format!(
                "session: this session carries {HELPER_GROUP} and can execute the helper\n"
            )),
            SessionAccess::NeedsFreshSession => {
                session_gap = true;
                out.push_str(&format!(
                    "session: this session does NOT carry {HELPER_GROUP}, so it cannot execute \
                     the helper — start a fresh login, run commands through `sg {HELPER_GROUP} \
                     -c ...`, or re-run setup with --all-users\n"
                ));
            }
            SessionAccess::Unknown => out.push_str(
                "session: could not read this session's group set to confirm helper access\n",
            ),
        }
    }

    // The global control (informational).
    let sysctl = std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "unavailable (not an AppArmor-restricted host)".to_string());
    out.push_str(&format!(
        "global apparmor_restrict_unprivileged_userns: {sysctl}\n"
    ));

    if !ready {
        out.push_str(&format!(
            "\nnot fully set up. Run:\n\n    {SETUP_COMMAND}\n"
        ));
    } else if session_gap {
        out.push_str(&format!(
            "\nthe host is set up, but THIS SESSION cannot reach the helper. Start a fresh login, \
             prefix commands with `sg {HELPER_GROUP} -c ...`, or drop the group gate with:\n\n    \
             {SETUP_COMMAND_ALL_USERS}\n"
        ));
    } else if out.contains("not readable") {
        out.push_str(
            "\nhelper and profile are installed; run `sudo nub sandbox status` to confirm the profile is loaded in the kernel.\n",
        );
    } else {
        out.push_str("\nsandbox setup is complete on this host.\n");
    }
    Ok(out)
}

/// ELEVATED. Undo [`setup`]: unload + remove the profile, remove the helper (and its now-empty
/// directory), and drop the group if it has no other members. Idempotent — a missing piece is a
/// no-op, not an error.
pub fn teardown() -> Result<String, String> {
    if !is_root() {
        return Err(
            "removing the sandbox helper and its AppArmor profile needs root. Re-run it with:\n\n    sudo nub sandbox teardown\n"
                .to_string(),
        );
    }
    let mut report = Vec::new();

    if Path::new(PROFILE_PATH).exists() {
        // `apparmor_parser --remove` reads the profile file to learn the profile name.
        let _ = run_tool(
            Command::new("apparmor_parser").args(["--remove", PROFILE_PATH]),
            &[],
        );
        std::fs::remove_file(PROFILE_PATH).map_err(|e| format!("removing {PROFILE_PATH}: {e}"))?;
        report.push(format!(
            "removed and unloaded AppArmor profile {PROFILE_NAME}"
        ));
    } else {
        report.push("AppArmor profile already absent".to_string());
    }

    if Path::new(DEDICATED_HELPER_PATH).exists() {
        std::fs::remove_file(DEDICATED_HELPER_PATH)
            .map_err(|e| format!("removing {DEDICATED_HELPER_PATH}: {e}"))?;
        // Only remove the dir if empty — never clobber unrelated content under /usr/libexec.
        let _ = std::fs::remove_dir(HELPER_DIR);
        report.push(format!("removed helper {DEDICATED_HELPER_PATH}"));
    } else {
        report.push("helper already absent".to_string());
    }

    // Drop the invoking user's membership first, so a single-user host's group ends up empty and
    // is removed below, while a shared host keeps the group for its other members.
    if let Some(user) = std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty())
        && user_in_group(&user, HELPER_GROUP)
    {
        let _ = run_tool(
            Command::new("gpasswd").args(["--delete", &user, HELPER_GROUP]),
            &[],
        );
    }

    // Drop the group ONLY when it has no members. `groupdel` succeeds even for a group with
    // secondary members and would silently revoke sandbox access for every other set-up user, so
    // emptiness is checked first rather than relying on a delete failure.
    if group_exists(HELPER_GROUP) {
        if group_members(HELPER_GROUP).is_empty() {
            match run_tool(Command::new("groupdel").arg(HELPER_GROUP), &[]) {
                Ok(()) => report.push(format!("removed group {HELPER_GROUP}")),
                Err(e) => report.push(format!("left group {HELPER_GROUP} in place ({e})")),
            }
        } else {
            report.push(format!(
                "left group {HELPER_GROUP} in place (other members present)"
            ));
        }
    }

    Ok(report.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_session_group_set_is_read_from_the_kernel_not_from_etc_group() {
        // The defect this replaced reported success from an /etc/group membership the caller's
        // live process did not hold. Both readings of the kernel's own answer must agree, so a
        // later refactor cannot quietly go back to consulting the account's rows.
        let pid = std::process::id();
        let from_syscall: HashSet<u32> = current_process_gids().into_iter().collect();
        let mut from_proc: HashSet<u32> = proc_status_field(pid, "Groups:")
            .expect("/proc/self/status must expose Groups:")
            .split_whitespace()
            .filter_map(|gid| gid.parse().ok())
            .collect();
        from_proc.insert(
            proc_status_field(pid, "Gid:")
                .and_then(|field| first_id(&field))
                .expect("/proc/self/status must expose Gid:"),
        );
        assert_eq!(
            from_syscall, from_proc,
            "getgroups() and /proc/{pid}/status disagree about this process's group set"
        );
    }
}
