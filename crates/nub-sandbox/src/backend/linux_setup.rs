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
use std::process::Command;

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

fn is_root() -> bool {
    // Safe FFI: geteuid never fails and touches no memory.
    unsafe { libc::geteuid() == 0 }
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
/// group, and verify. Idempotent: re-running repairs only what drifted. Returns a
/// human-readable report of what it did.
pub fn setup() -> Result<String, String> {
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

    // 2. Membership — the human who ran sudo. A fresh login is required for it to apply.
    let target_user = std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty());
    match &target_user {
        Some(user) if user_in_group(user, HELPER_GROUP) => {
            report.push(format!("{user} already in {HELPER_GROUP}"));
        }
        Some(user) => {
            run_tool(
                Command::new("gpasswd").args(["--add", user, HELPER_GROUP]),
                &[],
            )?;
            report.push(format!(
                "added {user} to {HELPER_GROUP} (log out and back in, or run `newgrp {HELPER_GROUP}`, for it to apply)"
            ));
        }
        None => report.push(format!(
            "no SUDO_USER in the environment; add members with: sudo gpasswd --add <login> {HELPER_GROUP}"
        )),
    }

    // 3. Helper: root-owned, group-executable, NOT setuid. `install(1)` is atomic.
    std::fs::create_dir_all(HELPER_DIR).map_err(|e| format!("creating {HELPER_DIR}: {e}"))?;
    run_tool(
        Command::new("install").args([
            "-D",
            "-o",
            "root",
            "-g",
            HELPER_GROUP,
            "-m",
            "0750",
            &src.to_string_lossy(),
            DEDICATED_HELPER_PATH,
        ]),
        &[],
    )?;
    report.push(format!(
        "installed helper at {DEDICATED_HELPER_PATH} (root:{HELPER_GROUP} 0750)"
    ));

    // 4. Profile: path-bound `userns` grant for the helper only. The global control is left on.
    std::fs::write(PROFILE_PATH, PROFILE_CONTENT)
        .map_err(|e| format!("writing {PROFILE_PATH}: {e}"))?;
    run_tool(
        Command::new("apparmor_parser").args(["--replace", PROFILE_PATH]),
        &[],
    )?;
    report.push(format!("loaded AppArmor profile {PROFILE_NAME}"));

    // 5. Verify the profile actually unlocks userns FOR AN UNPRIVILEGED GROUP MEMBER — a root
    //    probe is a false positive (root can create a userns regardless of the profile), so drop
    //    to the invoking user with the group active. `runuser` run by root sets the group with no
    //    membership check, reproducing exactly the runtime scenario a fresh-login user will hit.
    match &target_user {
        Some(user) if tool_exists("runuser") => match verify_as_member(user) {
            Ok(()) => {
                report.push("verified: a nub-sandbox member can confine a command".to_string())
            }
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
         {DEDICATED_HELPER_PATH} is opted in, for {HELPER_GROUP} members"
    ));
    Ok(report.join("\n"))
}

/// One bounded confined launch through the installed helper, run as `user` with `nub-sandbox`
/// as the active group. Proves the path-keyed profile grants the userns for a real member.
fn verify_as_member(user: &str) -> Result<(), String> {
    let out = Command::new("runuser")
        .args([
            "-u",
            user,
            "-g",
            HELPER_GROUP,
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

    // Group membership of the human running this — SUDO_USER under sudo, else USER, so an
    // elevated `status` reports the invoking user's membership, not root's.
    let user = std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_default();
    if !user.is_empty() {
        if user_in_group(&user, HELPER_GROUP) {
            out.push_str(&format!("group: {user} is in {HELPER_GROUP}\n"));
        } else {
            ready = false;
            out.push_str(&format!(
                "group: {user} is NOT in {HELPER_GROUP} — re-run setup, then start a fresh login\n"
            ));
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
