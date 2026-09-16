//! `nub sandbox <command>` — the confinement frontend's contract.
//!
//! The case that matters most here is the NEGATIVE one, and it is not about error text: an
//! invocation that cannot be confined must not run the command ANYWAY. A frontend that printed a
//! warning and executed would hand the user the confinement they asked for in name only, and the
//! failure would be silent — the command succeeds, so nothing looks wrong. Both refusal tests
//! therefore assert on a MARKER FILE the command would have created, not only on the exit code:
//! an exit code can be non-zero for unrelated reasons, where a missing marker proves the program
//! never ran.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Cargo's own path to the built `nub`, not arithmetic on `current_exe()`.
///
/// Two reasons, and the second only shows up off Linux: cargo GUARANTEES the binary is built for
/// an integration test that references this, where popping up from `deps/` just hopes somebody
/// built it; and the variable carries the platform's extension, so this finds `nub.exe` on
/// Windows where `push("nub")` silently would not.
fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn fixture(case: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nub-sandbox-cmd-{}-{}-{case}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A command that creates `marker` when it runs, and the marker's path. Its EXISTENCE is the
/// evidence — see the module doc for why an exit code alone is not.
///
/// Spelled per-platform rather than `#![cfg(unix)]`-ing the whole file away, because WINDOWS is
/// exactly the platform whose refusal had never been executed anywhere: the engine enforces on
/// Linux only, so Windows is one of the two hosts that must refuse, and a test that does not
/// compile there proves nothing about it.
fn marker_command(dir: &Path) -> (String, Vec<String>, PathBuf) {
    let marker = dir.join("marker");
    #[cfg(unix)]
    {
        // ⛔ NO WRITE-THEN-EXEC, and that is not style. Writing a script here and running it
        // races every sibling test in this file: `execve` refuses with ETXTBSY while ANY process
        // holds the file open for writing, and a sibling thread that forks between this write's
        // `open` and its `close` hands the forked child a copy of that writing fd. `O_CLOEXEC`
        // does not save it — the kernel's write-count check happens before the fd table is
        // flushed on exec. It went red on the aarch64 leg exactly once and would have kept doing
        // so. `/bin/sh` already exists and is never written, so there is no race to lose.
        (
            "/bin/sh".to_string(),
            vec![
                "-c".to_string(),
                format!("echo ran > '{}'", marker.display()),
            ],
            marker,
        )
    }
    #[cfg(windows)]
    {
        // ⛔ NO REDIRECTION OPERATOR, and that is the whole point. The obvious spelling —
        // `/c` plus one argument reading `echo ran > "<path>"` — FAILED on CI with "The
        // filename, directory name, or volume label syntax is incorrect": Rust quotes that
        // argument as a unit, so `cmd` receives quotes nested inside quotes and parses neither.
        // `copy NUL <path>` carries no shell metacharacter at all, so each token survives
        // Rust's quoting and `cmd` sees three plain arguments. It writes an empty file, which
        // is all a marker has to be.
        (
            "cmd".to_string(),
            vec![
                "/c".to_string(),
                "copy".to_string(),
                "NUL".to_string(),
                marker.display().to_string(),
            ],
            marker,
        )
    }
}

/// Build the marker command AND PROVE IT WORKS, by running it once directly and checking the
/// marker appears. The file is then removed, so every caller starts from a clean slate.
///
/// ⛔ WITHOUT THIS THE REFUSAL TESTS CAN PASS FOR THE WRONG REASON. They assert the marker is
/// ABSENT to show the command never ran — so a marker command that is simply BROKEN satisfies them
/// perfectly, and the suite goes green having tested nothing. The risk is concentrated on Windows,
/// where the command is `cmd /c echo ... > path`: Rust's argument quoting and `cmd /c`'s own
/// parsing interact badly, and this repo's `win-check.sh` cannot even type-check `nub-cli` for
/// Windows (mimalloc's build script needs a real Windows C toolchain), so CI is the first place
/// that arm runs at all. A positive control is the only thing standing between "refused" and
/// "never worked".
fn verified_marker_command(dir: &Path) -> (String, Vec<String>, PathBuf) {
    let (program, args, marker) = marker_command(dir);
    let status = Command::new(&program)
        .args(&args)
        .current_dir(dir)
        .status()
        .expect("the marker command runs outside the sandbox");
    assert!(status.success(), "the marker command failed: {status:?}");
    assert!(
        marker.exists(),
        "the marker command ran but created no marker — every absence assertion below would then \
         pass without proving anything",
    );
    std::fs::remove_file(&marker).expect("clearing the control's marker");
    (program, args, marker)
}

#[test]
fn a_command_with_no_policy_is_refused_and_never_runs() {
    let dir = fixture("nopolicy");
    let (program, program_args, marker) = verified_marker_command(&dir);

    let mut cmd = Command::new(nub_binary());
    cmd.arg("sandbox").arg(&program).args(&program_args);
    let out = cmd.current_dir(&dir).output().expect("nub runs");

    assert!(!out.status.success(), "a policy-less run must not succeed");
    assert!(
        !marker.exists(),
        "the command RAN despite having no policy — an unconfined run is the one outcome this \
         surface must never produce",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--policy"),
        "the refusal must name the flag that fixes it; got:\n{stderr}",
    );
}

#[test]
fn help_is_available_and_states_the_platform_limit() {
    let out = Command::new(nub_binary())
        .args(["sandbox", "--help"])
        .output()
        .expect("nub runs");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--policy"), "{stdout}");
    assert!(
        stdout.contains("Linux"),
        "help must say where the sandbox enforces; got:\n{stdout}",
    );
}

/// Off Linux the engine enforces nothing, so the frontend must refuse — and, again, not run.
///
/// This is the whole point of keeping the subcommand VISIBLE on macOS and Windows: the user gets a
/// sentence naming the platform instead of clap's "unrecognized subcommand".
#[cfg(not(target_os = "linux"))]
#[test]
fn an_unsupported_host_refuses_by_name_and_never_runs_the_command() {
    let dir = fixture("unsupported");
    let (program, program_args, marker) = verified_marker_command(&dir);
    let policy = dir.join("policy.json");
    std::fs::write(&policy, r#"{"fs": {".": "rw"}, "net": false}"#).unwrap();

    let mut cmd = Command::new(nub_binary());
    cmd.arg("sandbox")
        .arg("--policy")
        .arg(&policy)
        .arg(&program)
        .args(&program_args);
    let out = cmd.current_dir(&dir).output().expect("nub runs");

    assert!(!out.status.success());
    assert!(
        !marker.exists(),
        "the command ran on a host that cannot confine it",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Linux"),
        "the refusal must name the platform that works; got:\n{stderr}",
    );
}

/// On Linux the command actually runs, and a flag AFTER it belongs to it rather than to nub.
#[cfg(target_os = "linux")]
#[test]
fn a_confined_command_runs_and_owns_the_flags_that_follow_it() {
    let dir = fixture("confined");
    let script = dir.join("argv.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let policy = dir.join("policy.json");
    std::fs::write(
        &policy,
        format!(r#"{{"fs": {{"{}": "rw"}}, "net": false}}"#, dir.display()),
    )
    .unwrap();

    let out = Command::new(nub_binary())
        .args([
            "sandbox",
            "--policy",
            policy.to_str().unwrap(),
            script.to_str().unwrap(),
            "--help",
        ])
        .current_dir(&dir)
        .output()
        .expect("nub runs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a confined command must run:\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    // `--help` reached the SCRIPT. Had the frontend claimed it, nub's own help would have printed
    // and the script would never have run.
    assert_eq!(stdout.trim(), "--help", "stderr:\n{stderr}");
}

/// A real policy FILE is more than an axes object, and the loader has to hand the compiler the
/// right piece of it.
///
/// Three things were wrong here and each failed silently in its own way. The frontend passed the
/// whole file as the policy surface, so a document holding its axes under a `sandbox` key — one of
/// the two shapes the docs describe — was rejected as an unknown key. It set no pointer base, so
/// every `...:#/pointer` in a real policy was a dangling one. And it never told the compiler which
/// file the policy came from, so the self-exclusion that keeps a confined command from reading the
/// rules confining it was simply absent.
///
/// The assertion is on the ERROR TEXT rather than on success, because this test also runs where no
/// sandbox exists: off Linux the command is refused by platform, and what distinguishes a loader
/// bug from that refusal is WHICH failure arrives. A compile error names the key or the pointer.
#[test]
fn a_wrapped_policy_resolves_its_own_reuse_pointers() {
    let dir = fixture("wrapped-policy");
    let (program, args, marker) = verified_marker_command(&dir);
    let policy = dir.join("policy.json");
    std::fs::write(
        &policy,
        format!(
            r#"{{"shared": {{"fs": {{"{}": "rw"}}}},
                 "sandbox": {{"fs": {{"...:#/shared/fs": true}}, "net": false}}}}"#,
            dir.display().to_string().replace('\\', "\\\\"),
        ),
    )
    .unwrap();

    let out = Command::new(nub_binary())
        .args(["sandbox", "--policy", policy.to_str().unwrap(), &program])
        .args(&args)
        .current_dir(&dir)
        .output()
        .expect("nub runs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    for bug in ["unknown key", "pointer", "compiling"] {
        assert!(
            !stderr.contains(bug),
            "the policy file failed to COMPILE (`{bug}`), which is a loader bug rather than a \
             platform refusal:\n{stderr}",
        );
    }

    #[cfg(target_os = "linux")]
    {
        assert!(out.status.success(), "stderr:\n{stderr}");
        assert!(
            marker.exists(),
            "the spliced grant must reach the command:\n{stderr}",
        );
        // The policy file names itself, so the confined command must not be able to read it.
        let read = Command::new(nub_binary())
            .args([
                "sandbox",
                "--policy",
                policy.to_str().unwrap(),
                "/bin/cat",
                policy.to_str().unwrap(),
            ])
            .current_dir(&dir)
            .output()
            .expect("nub runs");
        assert!(
            !read.status.success(),
            "a confined command read the policy confining it:\n{}",
            String::from_utf8_lossy(&read.stdout),
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert!(!out.status.success(), "the sandbox is Linux-only");
        assert!(!marker.exists(), "the command ran on an unsupported host");
    }

    // The two shapes are alternatives, not a precedence rule: a document carrying both would
    // otherwise run under the `sandbox` block with the top-level axis silently discarded.
    let mixed = dir.join("mixed.json");
    std::fs::write(
        &mixed,
        r#"{"fs": {"/": "r"}, "sandbox": {"fs": true, "net": false}}"#,
    )
    .unwrap();
    let out = Command::new(nub_binary())
        .args(["sandbox", "--policy", mixed.to_str().unwrap(), "/bin/true"])
        .current_dir(&dir)
        .output()
        .expect("nub runs");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("one place or the other"),
        "a document carrying both shapes must be refused by name:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}
