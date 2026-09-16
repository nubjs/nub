//! `nub sandbox <command>` — the user-invoked confinement frontend.
//!
//! THE SUBCOMMAND IS VISIBLE ON EVERY PLATFORM AND ONLY WORKS ON ONE. Hiding it off Linux would
//! answer `nub sandbox curl example.com` with clap's "unrecognized subcommand", which tells the
//! reader nothing about why. Reaching the engine and surfacing its own typed refusal names the
//! platform and the axes it could not enforce instead. `crates/nub-sandbox` compiles everywhere
//! precisely so this path exists rather than being `#[cfg]`-ed away.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::Path;

use nub_sandbox::{
    CommandSpec, CompileCtx, Degradation, Homes, Sandbox, ScopeCapabilities, compile_with_warnings,
};

/// Render a [`Degradation`] as the user-facing reason a launch was refused.
///
/// The engine reports an unsupported host as a degradation rather than a distinct error type, so
/// the Linux-only refusal arrives here as "every axis lost, because the OS is wrong". That is the
/// same shape as a Linux host missing a kernel facility, and both should read the same way: what
/// could not be enforced, and why.
fn refused(what: &str, d: &Degradation) -> anyhow::Error {
    match d.reason.as_deref() {
        Some(reason) => anyhow!("{what}: {reason} (not enforced: {})", d.lost.join(", ")),
        None => anyhow!("{what}: cannot enforce {}", d.lost.join(", ")),
    }
}

/// The symbolic roots a policy's `~`, `$cache` and `./` patterns expand against.
fn homes(cwd: &Path) -> Homes {
    Homes {
        home: dirs_next::home_dir().unwrap_or_else(|| cwd.to_path_buf()),
        cache: dirs_next::cache_dir().unwrap_or_else(std::env::temp_dir),
        tmp: std::env::temp_dir(),
        project: cwd.to_path_buf(),
    }
}

/// Load and compile a policy document into an enforceable policy.
fn policy_from(path: &Path, cwd: &Path) -> Result<nub_sandbox::SandboxPolicy> {
    let text = crate::jsonc::read_guarded(path)
        .with_context(|| format!("reading the policy file `{}`", path.display()))?;
    let document = crate::jsonc::parse_to_value(&text)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("parsing the policy file `{}`", path.display()))?
        .ok_or_else(|| {
            anyhow!(
                "`{}` holds no policy document — an empty file would confine nothing",
                path.display()
            )
        })?;

    let ambient: BTreeMap<String, String> = std::env::vars().collect();
    // The policy file is the user's own, so it carries the full scope capabilities: env
    // substitution and credential brokering are both things they are entitled to ask for in a
    // document they wrote. A dependency-authored policy would not be compiled through here.
    let ctx = CompileCtx::new(
        homes(cwd),
        cwd.to_path_buf(),
        ScopeCapabilities::approved(),
        ambient,
    );
    let (policy, warnings) = compile_with_warnings(&document, &ctx)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("compiling `{}`", path.display()))?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    Ok(policy)
}

/// Run `argv` confined by the policy document at `policy_path`, returning its exit code.
pub(crate) fn run_confined(policy_path: &Path, argv: &[String]) -> Result<i32> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("nub sandbox: provide a command to run"))?;
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    let policy = policy_from(policy_path, &cwd)?;

    let sandbox = Sandbox::new(&policy).map_err(|d| refused("cannot acquire the sandbox", &d))?;
    let prepared = sandbox
        .prepare(
            // ⛔ REDACTION IS DELIBERATELY OFF, and turning it on here is a trap I already fell
            // into. `redact_stdout(true)` sets `Stdio::piped()` so a HOST can drain the child
            // through a redactor — but `Prepared::status()` only spawns and waits. Nothing reads
            // those pipes, so the user sees NO output at all, and a command writing more than the
            // pipe buffer blocks forever against a parent sitting in `wait()`. Inheriting is also
            // what makes an interactive command work: a TTY, colors, and live streaming. Scrubbing
            // granted secret VALUES out of the output is worth having, and it needs a drainer that
            // forwards as it scrubs — its own change, not a flag flipped here.
            CommandSpec::new(program.as_str())
                .args(args.iter().map(String::as_str))
                .cwd(&cwd),
        )
        .map_err(|d| refused("cannot confine this command", &d))?;

    // ⛔ A PARTIAL ENFORCEMENT IS A REFUSAL, NOT A WARNING. Running the command with an axis
    // unenforced would hand the user the confinement they asked for in name only, and the failure
    // is silent — the command succeeds, and whatever the missing axis was meant to stop is not
    // stopped. The engine already fails closed at acquisition for what it can detect there; this
    // covers what is only knowable once the command is prepared.
    if !prepared.degradation.is_full() {
        bail!(
            "{}",
            prepared
                .degradation
                .warning()
                .unwrap_or_else(|| "the sandbox could not fully enforce this policy".into())
        );
    }

    let status = prepared.status().context("running the confined command")?;
    // A signal-terminated child has no code. 128+signo is the shell convention, and inventing a
    // plain 1 here would make "killed" indistinguishable from "exited 1".
    Ok(status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map_or(1, |s| 128 + s)
        }
        #[cfg(not(unix))]
        {
            1
        }
    }))
}
