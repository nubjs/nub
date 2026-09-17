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
    //
    // THREE THINGS THE DOCUMENT IS USED FOR, AND ONLY ONE OF THEM IS THE POLICY ITSELF.
    // `with_policy_files` is what denies the confined command its own policy file, read and
    // write, so it can neither learn the rules confining it nor edit them for the next run;
    // `with_document` is the base a `...:#/pointer` resolves against, and without it every
    // reuse pointer in a real policy file is a dangling one. Both are easy to leave out and
    // silent when you do: the policy still compiles, just weaker and with reuse broken.
    let ctx = CompileCtx::new(
        homes(cwd),
        cwd.to_path_buf(),
        ScopeCapabilities::approved(),
        ambient,
    )
    .with_policy_files(vec![
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    ])
    .with_document(document.clone());

    // A policy file holds the axes object directly, or under a `sandbox` key. The pointer base
    // above stays the WHOLE file either way, which is what lets a `sandbox` block reuse a
    // `#/shared/…` sibling that is not itself an axis.
    //
    // ⛔ WRITING BOTH SHAPES AT ONCE IS AN ERROR, not a precedence rule. Taking the `sandbox`
    // block and ignoring a top-level `fs` would hand the user a policy they did not write, and
    // it would do it silently — the command still runs, just under different rules. That is the
    // one failure this frontend refuses everywhere else, so it refuses it here too.
    let surface = match document.get("sandbox") {
        Some(block) => {
            if let Some(stray) = ["fs", "net", "vars", "secrets"]
                .into_iter()
                .find(|axis| document.get(axis).is_some())
            {
                bail!(
                    "`{}` has a `sandbox` block AND a top-level `{stray}` axis — the axes go in \
                     one place or the other, or one of them is silently ignored",
                    path.display()
                );
            }
            block
        }
        None => &document,
    };
    let (policy, warnings) = compile_with_warnings(surface, &ctx)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("compiling `{}`", path.display()))?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    Ok(policy)
}

/// The marker nub sets in every confined child's environment so a nested `nub sandbox` can see the
/// enclosing one. Internal plumbing, never a documented user knob — the brand boundary permits a
/// `__NUB_*` sentinel for exactly this.
const NESTED_SENTINEL: &str = "__NUB_SANDBOX_ACTIVE";

/// Run `argv` confined by the policy document at `policy_path`, returning its exit code.
pub(crate) fn run_confined(policy_path: &Path, argv: &[String]) -> Result<i32> {
    // ⛔ A SANDBOX INSIDE A SANDBOX IS REFUSED BEFORE ANY WORK, and being first is the point.
    // Linux allows ONE seccomp user-notification listener per process — a second
    // `SECCOMP_FILTER_FLAG_NEW_LISTENER` returns EBUSY (measured, kernel 6.17) — so the inner
    // launch can never get its own syscall supervisor, and running half-confined is what this
    // frontend refuses everywhere else. The engine does fail closed on its own, but only deep in
    // the launch, after the proxy, the ruleset and the fork, where the cause is no longer legible:
    // it surfaces as whatever broke FIRST under the OUTER policy's confinement — a "filesystem
    // grant disappeared", or a stall with nothing to read. Refusing here, before anything is
    // acquired, is what makes the message name the real cause and the exit immediate.
    //
    // The kernel limit is not permanent, so this refusal is not either: an upstream series adds an
    // opt-in `SECCOMP_FILTER_FLAG_ALLOW_NESTED_LISTENERS`, under which every listener in the chain
    // that sets the flag may be stacked. It is still under review rather than in a released kernel.
    // When it lands, the flag goes on the supervisor's own filter and this check narrows to "the
    // enclosing sandbox did not opt in" — which still covers a foreign sandbox that holds the slot.
    if std::env::var_os(NESTED_SENTINEL).is_some() {
        bail!(
            "nub sandbox: another sandbox is already active on this process, and Linux allows \
             only one — run this command outside the enclosing sandbox"
        );
    }
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("nub sandbox: provide a command to run"))?;
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    let mut policy = policy_from(policy_path, &cwd)?;
    // Mark the child so a nested `nub sandbox` refuses immediately instead of discovering the
    // one-listener limit deep in its own launch. Set AFTER compiling so it rides whatever `vars`
    // does: the child's environment IS `constructed`, and this is nub's own plumbing rather than a
    // variable the policy is describing.
    policy
        .env
        .constructed
        .insert(NESTED_SENTINEL.to_string(), "1".to_string());

    let sandbox = Sandbox::new(&policy).map_err(|d| refused("cannot acquire the sandbox", &d))?;

    // Scrub declared secret VALUES out of the child's output. `sensitive_keys` names the
    // secret-classified keys; their values live in `constructed`. A brokered secret is named in
    // `sensitive_keys` but withheld from `constructed`, so it is correctly absent — the child never
    // receives its value, so there is nothing in the child's output to scrub for it. An empty value
    // would match everywhere, so it is dropped.
    let secret_values: Vec<Vec<u8>> = policy
        .env
        .sensitive_keys
        .iter()
        .filter_map(|key| policy.env.constructed.get(key))
        .filter(|value| !value.is_empty())
        .map(|value| value.clone().into_bytes())
        .collect();
    let redact = !secret_values.is_empty();

    // With no secret to scrub, stdio stays INHERITED: the child keeps its TTY, colors, and live
    // streaming, and the path is byte-for-byte what it was. With secrets, both fds are piped so the
    // host drainer can see the bytes. Piping REQUIRES that drainer: `redact_stdout(true)` sets
    // `Stdio::piped()`, and a piped fd nothing reads makes the child block forever once it fills the
    // pipe buffer. `sandbox_redact::drain_confined` reads both fds on their own threads as it
    // scrubs, concurrently with `wait()`, so that deadlock cannot form.
    let mut spec = CommandSpec::new(program.as_str())
        .args(args.iter().map(String::as_str))
        .cwd(&cwd);
    if redact {
        spec = spec.redact_stdout(true).redact_stderr(true);
    }
    let prepared = sandbox
        .prepare(spec)
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

    let status = if redact {
        let child = prepared.spawn().context("spawning the confined command")?;
        crate::sandbox_redact::drain_confined(child, secret_values)
            .context("running the confined command")?
    } else {
        prepared.status().context("running the confined command")?
    };
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
