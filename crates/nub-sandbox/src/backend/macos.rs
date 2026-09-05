//! macOS Seatbelt backend: resolved [`SandboxPolicy`] IR → an SBPL profile,
//! enforced by wrapping the child in `sandbox-exec -p <profile> -- <cmd>`.
//!
//! POSTURE: `(deny default)`. The [`MACOS_SEATBELT_BASE`] block (ported from Codex
//! / Chromium — see the .sbpl header) is the bootstrap that lets an arbitrary
//! binary dyld-load under a deny-default profile; nub then appends the IR-derived
//! read / write / net rules. SBPL is LAST-MATCH-WINS (verified on macOS 26), so a
//! later nub deny overrides an earlier allow — the IR's last-match-wins evaluation
//! order maps onto SBPL emission order 1:1, per axis.
//!
//! Axis mapping:
//!   - reads:  base essential reads always; `default_effect == Allow` adds a
//!     `(allow file-read* (subpath "/"))` generous base; each IR entry emits a
//!     read allow/deny in order. `file-map-executable` shadows every read-allow so
//!     dylibs in an allowed region load.
//!   - writes: deny-default (the base denies all writes); ONLY a ReadWrite allow emits
//!     `(allow file-write*)` and ONLY a Deny emits `(deny file-write*)`. An Allow is
//!     purely additive on this axis — see the Read arm in [`emit_fs`] for why a
//!     synthesized deny could never do anything but cancel another grant.
//!   - net:    not-enforced → `(allow network*)`; enforced WITH a proxy → egress
//!     permitted ONLY to the proxy's loopback port (per-host enforced through it);
//!     enforced WITHOUT a proxy → the base deny stands (coarse deny, loopback closed).
//!   - env:    the child env IS the policy's constructed map (construction, not an
//!     SBPL primitive — a withheld var is simply absent). BUT a scrubbed secret is
//!     only genuinely withheld if the child cannot RECOVER it from a co-resident
//!     same-uid process's environment via `sysctl KERN_PROCARGS2` — so when the
//!     policy withholds a secret we MUST emit an SBPL profile carrying the env-read
//!     closure (below), even if fs/net are relaxed. The closure is the macOS twin of
//!     the Linux `/proc`-close + `ptrace`-deny.
//!
//! CANONICALIZATION: the IR matchers are already firmlink-resolved on their literal
//! prefix by the compiler (`canonicalize_glob_prefix`), and Seatbelt checks the
//! CANONICAL path — so a `/tmp/…` (firmlink) allow that was NOT canonicalized would
//! be inert (silently denied). The confstr scratch dirs this backend adds ARE
//! canonicalized here (incl. not-yet-existing) for the same reason. The same rule
//! binds the child's `PATH`, which is a path list the child hands back to the kernel
//! — see [`canonicalize_path_var`].

use crate::backend::{CommandSpec, Degradation, Prepared};
use crate::matcher::path::canonicalize_including_nonexistent;
use crate::matcher::path::normalize_slashes;
use crate::policy::{Effect, FsAccess, SandboxPolicy};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The bootstrap essential block (`(deny default)` + process/mach/sysctl/iokit +
/// framework map + system read surface). See the .sbpl header for provenance.
const MACOS_SEATBELT_BASE: &str = include_str!("macos_seatbelt_base.sbpl");

/// Stamp `label` onto EVERY deny rule in an assembled profile, so a refusal the kernel records
/// says which launch provoked it.
///
/// ⛔ EVERY DENY, NOT JUST `(deny default)`, AND THE DIFFERENCE IS NOT THEORETICAL. The build
/// jail's POLICY is a pure allowlist, which is what makes "everything falls through to the default
/// deny" sound true — but the BACKEND synthesizes denies the policy never asked for, and
/// [`emit_tmp`] is the live one: under `TmpMode::Private` it denies the whole shared tmp, so a
/// script writing `/tmp/build.log` is refused by THAT rule and never reaches the default. Tagging
/// the default alone would have left the most common refusal in the jail invisible.
///
/// ⛔ A TEXT PASS RATHER THAN A PARAMETER THREADED THROUGH THE EMITTERS, deliberately: this cannot
/// be forgotten. A new deny site added later is annotated because it is a deny, not because its
/// author remembered an argument — and there are already nine such sites across four functions.
/// The cost is that the pass must recognize a rule, which is why it requires a COMPLETE
/// s-expression on one line and leaves anything else untouched rather than corrupting it. A test
/// asserts no deny escapes, so a future multi-line rule fails the suite instead of the diagnostic.
///
/// Enforcement is unchanged: `(with message …)` annotates the record the kernel was already going
/// to write. Verified behaviorally against the real kernel in
/// `tagged_default_deny_does_not_change_enforcement`, and for every deny SHAPE the backend emits
/// (`default`, bare `process-info*`, `subpath`, `literal`, `regex`) by profile acceptance.
fn annotate_denies(profile: &str, label: &str) -> String {
    let modifier = format!(" (with message \"{}\")", sbpl_escape(label));
    let mut out = String::with_capacity(profile.len() + 64);
    for line in profile.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        match sole_sexp_close(body).filter(|_| body.trim_start().starts_with("(deny ")) {
            Some(close) => {
                out.push_str(&body[..close]);
                out.push_str(&modifier);
                out.push_str(&body[close..]);
                out.push_str(&line[body.len()..]);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Where `line`'s single balanced s-expression closes, or `None` if it is not exactly one.
///
/// ⛔ THE INDEX IS THE POINT, not the boolean. Inserting before the LAST `)` on the line looks
/// equivalent and is not: a rule with a trailing comment (`(deny default) ; why`) would take the
/// annotation inside the comment, where the kernel never sees it and the rule silently loses its
/// tag. Nothing emits such a line today, which is exactly why the mistake would survive review.
///
/// Parens inside a `"…"` string are data, not structure — `(regex #"^(.*/)?\.env$")` is one
/// balanced rule — so the scan tracks quoting and backslash escapes.
fn sole_sexp_close(line: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut closed_at = None;
    for (i, c) in line.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            ';' => break,
            '(' if closed_at.is_some() => return None,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                if depth == 0 {
                    closed_at = Some(i);
                }
            }
            _ => {}
        }
    }
    if in_string || depth != 0 {
        return None;
    }
    // Nothing but whitespace or a comment may follow the close.
    closed_at.filter(|i| {
        let tail = line[i + 1..].trim_start();
        tail.is_empty() || tail.starts_with(';')
    })
}

/// Mach/socket services real networking needs beyond raw `connect` — DNS resolution
/// (mDNSResponder / SystemConfiguration), TLS trust (trustd / ocspd / SecurityServer),
/// route lookup. Emitted only when net is fully allowed (not-enforced); loopback-only
/// egress needs none of it. Ported from Codex's `seatbelt_network_policy.sbpl`.
const NETWORK_SERVICES: &str = "\
(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))
(allow mach-lookup
  (global-name \"com.apple.bsd.dirhelper\")
  (global-name \"com.apple.system.opendirectoryd.membership\")
  (global-name \"com.apple.SecurityServer\")
  (global-name \"com.apple.networkd\")
  (global-name \"com.apple.ocspd\")
  (global-name \"com.apple.trustd\")
  (global-name \"com.apple.trustd.agent\")
  (global-name \"com.apple.SystemConfiguration.DNSConfiguration\")
  (global-name \"com.apple.SystemConfiguration.configd\")
  (global-name \"com.apple.dnssd.service\")
  (global-name \"com.apple.mDNSResponder.dnsproxy\")
  (global-name \"com.apple.mDNSResponder.uds\"))
(allow sysctl-read (sysctl-name-regex #\"^net.routetable\"))
";

/// The stock, unprivileged entry point to Seatbelt. Every confined launch goes through exactly
/// this path, which is what makes its presence the only readiness question macOS has. (The
/// `macos_setup` no-op readiness shim was dropped with the privileged tier, epic 0.3.)
pub const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// Apply a resolved policy to a command on macOS. When the policy confines neither
/// fs nor net, no SBPL wrap is emitted (env-scrub alone is construction, needs no
/// kernel primitive); otherwise the child is re-homed under `/usr/bin/sandbox-exec`.
pub fn apply(
    policy: &SandboxPolicy,
    spec: CommandSpec,
    proxy_port: Option<u16>,
    proxy_token: Option<&str>,
    ca_bundle: Option<&std::path::Path>,
    tmp_dir: Option<&std::path::Path>,
) -> Result<Prepared, Degradation> {
    if !needs_wrap(policy) {
        // Nothing to confine and no withheld secret to protect: the env-scrub is pure
        // construction (the child gets exactly the constructed map), so no SBPL profile
        // is needed. env is HONESTLY full here — no secret is being denied the child,
        // hence nothing to recover cross-process. (When a secret IS withheld,
        // `needs_wrap` is true and we fall through to emit the env-read closure below.)
        return Ok(Prepared {
            command: base_command(&spec, policy),
            degradation: Degradation::full(),
            proxy: None,
            signal_process_group: false,
            _private_tmp: None,
            redact_stdout: false,
            redact_stderr: false,
        });
    }

    let profile = build_profile(policy, &spec, proxy_port, ca_bundle, tmp_dir);
    let mut wrapped = Command::new(SANDBOX_EXEC_PATH);
    wrapped.arg("-p").arg(&profile).arg("--");
    wrapped.arg(&spec.program);
    spec.args.apply_to(&mut wrapped);
    if let Some(cwd) = &spec.cwd {
        wrapped.current_dir(cwd);
    }
    // Env-scrub is CONSTRUCTION: the wrapped `sandbox-exec` Command would otherwise
    // inherit this process's full parent env at spawn — re-leaking every secret the
    // scrub removed. Clear it and set exactly the constructed map. (Ported hard-won
    // fix: a fresh Command inherits the parent environ, so env_clear is mandatory.)
    wrapped.env_clear();
    for (k, v) in &policy.env.constructed {
        if k == "PATH" {
            wrapped.env(k, canonicalize_path_var(v));
        } else {
            wrapped.env(k, v);
        }
    }
    // Descriptors already open at spawn are outside the profile's reach — SBPL governs
    // what the child may OPEN, never what it INHERITS. The Linux backend sweeps them;
    // this is the Seatbelt twin.
    install_fd_sweep(&mut wrapped);
    // Descendant reaping, when the caller asked for it. See `install_process_group`.
    if spec.reap_descendants {
        install_process_group(&mut wrapped);
    }
    // Point the child at the loopback proxy (cooperative hint; the Seatbelt carve is
    // the real boundary). Set AFTER env_clear so it survives an enforced env scrub.
    if let Some(port) = proxy_port {
        super::set_proxy_env(&mut wrapped, port, proxy_token);
    }
    // CA trust for the child (the leaf-verifying bundle). The read grant lives in the
    // SBPL profile (see build_profile); this is the env half so tools find the bundle.
    if let Some(bundle) = ca_bundle {
        super::set_ca_env(&mut wrapped, bundle);
    }
    // Private tmp: point the child's TMPDIR/TMP/TEMP at the fresh per-run dir (the SBPL
    // profile grants it rw + denies the shared system tmp). Set after env_clear so it
    // survives the scrub. `Deny` sets nothing — the child inherits no usable tmp.
    if let Some(dir) = tmp_dir {
        super::set_tmp_env(&mut wrapped, dir);
    }

    Ok(Prepared {
        command: wrapped,
        degradation: degradation(policy, proxy_port, tmp_dir),
        proxy: None,
        signal_process_group: spec.reap_descendants,
        _private_tmp: None,
        redact_stdout: false,
        redact_stderr: false,
    })
}

/// Rewrite a `PATH` value so every absolute entry is the kernel's CANONICAL path.
///
/// Seatbelt matches its rules against the resolved path, so a lookup that traverses an
/// UNGRANTED symlink is denied — and Seatbelt denies with `EPERM`, which `posix_spawnp`
/// treats as FATAL rather than skipping it like the `ENOENT`/`EACCES` of an ordinary miss.
/// That is libuv's spawner, so it is the one every Node build script reaches through
/// `child_process`; libc `execvp` skips the same entry, which is why an identical PATH
/// works from `/usr/bin/env` and fails from Node.
///
/// One symlinked, ungranted entry therefore MASKS EVERY LATER ENTRY, `/usr/bin` included:
/// a build script spawning a bare `make` / `sh` / `cc` dies with `spawn EPERM`. (Measured:
/// `/opt/homebrew/opt/openjdk/bin` — a Homebrew `opt/<pkg>` link — killed node-gyp's
/// `make` at entry 10 of 56. The same directory reached by its real `Cellar/…` path is
/// skipped harmlessly.) Homebrew `opt` links and version-manager shims make this the
/// common case on a developer machine, not a corner.
///
/// Canonicalizing removes the symlink hop, demoting an unusable entry from fatal to
/// skippable (PATH order is precedence, so order survives). This is NOT invisible to the
/// child: it now observes resolved paths, and a Homebrew `opt/<pkg>` alias resolves to a
/// VERSION-PINNED `Cellar/<pkg>/<version>` one, so a script that bakes `$PATH` into a
/// generated artifact pins a version the next `brew upgrade` invalidates. That is the
/// accepted cost of native builds working at all. A RELATIVE entry is passed through
/// untouched: it resolves against the CHILD's cwd, which is not ours to resolve against,
/// and it carries no symlinked absolute prefix for the kernel to reject.
fn canonicalize_path_var(path: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in path.split(':') {
        let mut resolved = if Path::new(entry).is_absolute() {
            canonicalize_including_nonexistent(Path::new(entry))
                .to_string_lossy()
                .into_owned()
        } else {
            entry.to_string()
        };
        // A colon is legal in a macOS filename but is THE PATH separator, so a canonical
        // form containing one would split into two bogus entries — the tail of which is
        // relative, and so would be searched against the child's cwd (the package dir
        // under the build jail). Keep the original, which at worst stays fatal-on-miss.
        if resolved.contains(':') {
            resolved = entry.to_string();
        }
        if seen.insert(resolved.clone()) {
            out.push(resolved);
        }
    }
    out.join(":")
}

/// Close every inherited descriptor at exec.
///
/// The profile confines what the child may OPEN; it says nothing about a descriptor that
/// is ALREADY OPEN when it starts. A leaked fd is a live handle to a file, socket, or pipe
/// the policy would otherwise deny, and it bypasses the sandbox entirely — measured on the
/// Linux side, where an inherited socket egressed straight through both Landlock and
/// seccomp. Everything nub opens is CLOEXEC by construction today, which makes this a
/// backstop against a single future `dup()` or `socket2::Socket::new_raw` that isn't —
/// one mistake wide, so the backstop is not optional.
///
/// macOS has no `close_range`, and nub raises `RLIMIT_NOFILE` to the hard limit (~1M), so
/// the fd-table walk the Linux backend gets from the kernel must be done by hand — and a
/// blind `3..rlimit` loop would cost ~1M syscalls per spawn. `PROC_PIDLISTFDS` enumerates
/// only the OPEN descriptors instead. It runs in the CHILD, after fork, where the process
/// is single-threaded and the list cannot race; the buffer is sized in the PARENT because
/// the post-fork closure must not allocate. The parent-side probe is load-bearing beyond
/// sizing — it resolves `proc_pidinfo`'s lazy dyld binding, so the post-fork call cannot
/// enter the binder, whose locks another thread may have held at fork. Do not replace it
/// with a hardcoded size.
///
/// CLOEXEC rather than `close()` is deliberate, matching Linux: it leaves std's own
/// post-fork plumbing (the exec-error report pipe) intact and takes effect exactly at
/// exec. Descriptors below 3 are skipped — std redirects stdio onto them before this
/// closure runs, and the child needs them.
///
/// Not installed on the unwrapped path ([`base_command`]): with no policy to confine, an
/// inherited fd conveys no access the child lacked, and `pre_exec` would force std off
/// `posix_spawn` onto fork+exec for nothing.
fn install_fd_sweep(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // Sized in the parent from a live count, doubled plus slack: more descriptors may open
    // before fork, and a too-small buffer silently TRUNCATES the list (once a buffer is
    // supplied the kernel reports bytes written, not bytes needed).
    let probed = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDLISTFDS,
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    let entries = if probed > 0 {
        (probed as usize / size_of::<libc::proc_fdinfo>()) * 2 + 256
    } else {
        1024
    };
    let mut buf = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        entries
    ];
    let bytes = (entries * size_of::<libc::proc_fdinfo>()) as i32;

    // SAFETY: the closure runs between fork and exec, so it may touch only async-signal-safe
    // primitives. `proc_pidinfo` and `fcntl` are bare syscall wrappers and `buf` is already
    // allocated — no allocation, no locks, no libc state.
    unsafe {
        command.pre_exec(move || {
            let got = libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                buf.as_mut_ptr().cast(),
                bytes,
            );
            // FAIL CLOSED, as the Linux twin does (`mark_inherited_fds_cloexec()?`): a
            // confinement control that silently degrades is worse than a refused spawn.
            // Enumerating your own pid cannot realistically fail, and an exactly-full
            // buffer means the list may have been TRUNCATED — dropping precisely the
            // highest descriptors, which is what a late `dup()` produces and so the very
            // case this exists to catch.
            if got <= 0 || got >= bytes {
                return Err(std::io::Error::last_os_error());
            }
            let count = got as usize / size_of::<libc::proc_fdinfo>();
            for info in &buf[..count] {
                let fd = info.proc_fd;
                if fd < 3 {
                    continue;
                }
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
}

/// Put the child in its own process GROUP, so its whole descendant tree is reachable as
/// `-pgid`. Why this backend needs one at all: `CommandSpec::reap_descendants`.
///
/// A process-TREE walk (`libproc`) is not an alternative — a grandchild forked between the
/// walk and the kill is reparented to init and becomes invisible, which is the very leak
/// being closed.
///
/// `setpgid`, NOT the Linux twin's `setsid`: the group is all the reaping needs, and
/// staying in nub's session leaves the controlling terminal available to a script that
/// writes to it. The cost is leaving the FOREGROUND group, where a terminal read would
/// raise `SIGTTIN` and STOP the whole build tree — so both job-control stops are ignored.
/// Ignoring `SIGTTIN` turns that stop into an `EIO` the reading tool reports and moves
/// past; a hung install is the worse of the two. Ignoring `SIGTTOU` is behavior-preserving.
fn install_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `setpgid` and `signal` are async-signal-safe, allocate nothing, and touch no
    // parent state — the only things permitted between fork and exec in a threaded parent.
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            libc::signal(libc::SIGTTIN, libc::SIG_IGN);
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            Ok(())
        });
    }
}

/// The unwrapped command (program + args + cwd + env-scrub) for the no-confinement
/// path. The env axis is enforced by construction here exactly as in the skeleton.
fn base_command(spec: &CommandSpec, policy: &SandboxPolicy) -> Command {
    let mut command = Command::new(&spec.program);
    spec.args.apply_to(&mut command);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command.env_clear();
    for (k, v) in &policy.env.constructed {
        command.env(k, v);
    }
    command
}

/// Whether an SBPL profile must be emitted. Beyond fs/net confinement, a policy that
/// WITHHOLDS an env secret also requires a profile: the env-read closure that stops
/// the child recovering that secret from a co-resident process's environment lives in
/// the SBPL, so an env-only scrub that hides a secret is not genuinely enforced
/// without a wrap. (Mirrors the Linux backend, where `env.enforce` likewise engages
/// the sandbox.) This is what keeps `is_full()` honest: every path that withholds a
/// secret wraps, so none can report full env enforcement while leaving procargs2 open.
fn needs_wrap(policy: &SandboxPolicy) -> bool {
    needs_sandbox(policy) || env_needs_closure(policy)
}

/// A profile is emitted for an fs or net axis to enforce. A fully relaxed fs +
/// non-enforcing net needs no kernel confinement (on its own) — UNLESS the tmp mode
/// confines (`Private`/`Deny`), which is enforced by an SBPL fs deny on the shared tmp
/// and so requires a wrap even with an otherwise-relaxed fs.
fn needs_sandbox(policy: &SandboxPolicy) -> bool {
    fs_confines(policy) || tmp_confines(policy) || policy.net.enforce
}

/// Whether the tmp mode confines (anything other than the default `Shared`).
fn tmp_confines(policy: &SandboxPolicy) -> bool {
    policy.fs.tmp != crate::policy::TmpMode::Shared
}

/// Whether the env axis has a secret to protect cross-process. A passthrough
/// `{env:true}` (enforce set but nothing withheld) denies the child nothing, so there
/// is no secret to recover from a sibling — the env-read closure is unnecessary and we
/// need not wrap for it. Only a scrub that actually WITHHOLDS a var creates the
/// recovery surface the closure shuts.
fn env_needs_closure(policy: &SandboxPolicy) -> bool {
    policy.env.enforce && !policy.env.withheld.is_empty()
}

/// Whether the fs axis confines anything. A relaxed axis is `default_effect ==
/// Allow` with no entries (allow everything); anything else confines.
fn fs_confines(policy: &SandboxPolicy) -> bool {
    policy.fs.rules.default_effect != Effect::Allow || !policy.fs.rules.entries.is_empty()
}

/// The macOS half of the aube-scripts embedder seam: the SBPL profile enforcing `policy`, for a
/// caller that wraps its OWN command as `sandbox-exec -p <profile> -- <cmd>` (the analog of the
/// Linux [`confine_build_jail_command`](crate::confine_build_jail_command) `pre_exec` seam). Returns
/// `None` when the policy confines nothing and needs no kernel wrap. `tmp_dir` is the per-run
/// scratch dir when the policy uses a private tmp mode (the caller also points `TMPDIR` at it).
///
/// A minimal spec is synthesized: the stdio-path grants it would derive are covered by the build
/// jail's read-generous base, and no audit label or redaction applies.
pub fn build_jail_seatbelt_profile(
    policy: &SandboxPolicy,
    tmp_dir: Option<&std::path::Path>,
) -> Option<String> {
    if !needs_wrap(policy) {
        return None;
    }
    let spec = CommandSpec::new("/bin/sh");
    Some(build_profile(policy, &spec, None, None, tmp_dir))
}

/// Build the full SBPL profile text for `policy`.
///
/// NOT a pure function of its arguments: it reads the calling process's own fd table to
/// find the stdio paths the child will inherit (see [`emit_inherited_stdio`]), so the same
/// policy yields a slightly different profile depending on where the caller's stdio points.
fn build_profile(
    policy: &SandboxPolicy,
    spec: &CommandSpec,
    proxy_port: Option<u16>,
    ca_bundle: Option<&std::path::Path>,
    tmp_dir: Option<&std::path::Path>,
) -> String {
    build_profile_with_stdio(
        policy,
        spec,
        proxy_port,
        ca_bundle,
        tmp_dir,
        &inherited_stdio_paths(spec),
    )
}

/// [`build_profile`] with the inherited-stdio paths supplied rather than read from this
/// process — the seam that lets a test pin where the stdio grants land relative to the
/// backend's own denies.
fn build_profile_with_stdio(
    policy: &SandboxPolicy,
    spec: &CommandSpec,
    proxy_port: Option<u16>,
    ca_bundle: Option<&std::path::Path>,
    tmp_dir: Option<&std::path::Path>,
    stdio_paths: &[String],
) -> String {
    let mut out = String::with_capacity(MACOS_SEATBELT_BASE.len() + 2048);
    out.push_str(MACOS_SEATBELT_BASE);
    out.push('\n');

    emit_env_read_closure(&mut out);
    emit_net(policy, proxy_port, &mut out);
    emit_fs(policy, spec, &mut out);
    // Tmp-mode enforcement — emitted AFTER emit_fs so the shared-tmp deny and the
    // private-dir grant win last-match-wins over any generous read/write. (No-op for
    // `Shared`.)
    emit_tmp(policy, tmp_dir, &mut out);
    // The child must READ the CA bundle to trust the minted leaves — grant it explicitly,
    // AFTER emit_fs so it survives even a deny-all fs floor (nub infra, not user config).
    if let Some(bundle) = ca_bundle {
        out.push_str(&format!(
            "(allow file-read* (literal \"{}\"))\n",
            sbpl_escape(&bundle.to_string_lossy())
        ));
    }
    // Last by hygiene, not by necessity — see `emit_stdio_grants` for why position is not what
    // makes this survive the denies above it, and why a policy-denied path is withheld here
    // rather than allowed to punch through.
    emit_stdio_grants(policy, stdio_paths, &mut out);

    // AFTER assembly so it reaches the base's rules as well as every emitter's. Absent a label
    // the profile is byte-identical to what it has always been, which is what keeps a passing
    // install free of any cost from this.
    match &spec.audit_label {
        Some(label) => annotate_denies(&out, label),
        None => out,
    }
}

/// The paths behind the stdio descriptors the child will INHERIT from this process.
///
/// Redaction replaces fd 1 / fd 2 with a pipe at spawn, so those are excluded; stdin is never
/// re-pointed on the [`Prepared::spawn`](crate::backend::Prepared::spawn) path.
///
/// KNOWN OVER-GRANT: [`Prepared::output`](crate::backend::Prepared::output) re-points all three
/// to null/pipes AFTER the profile is frozen, so on that path the grants name paths the child
/// never receives. It has no production caller (the build jail uses `status`, `--sandbox` uses
/// `spawn_with_signal_target`) and the residue is a stat capability on nub's own stdio, but it
/// is why the claim below is "does not exceed the parent's own descriptors", not "the child
/// already holds every path named here".
fn inherited_stdio_paths(spec: &CommandSpec) -> Vec<String> {
    [
        (0, true),
        (1, !spec.redact_stdout),
        (2, !spec.redact_stderr),
    ]
    .into_iter()
    .filter_map(|(fd, inherited)| inherited.then(|| stdio_fd_path(fd)).flatten())
    .collect()
}

/// `file-read-metadata` on each inherited stdio path.
///
/// WHY THIS EXISTS — without it every Node under a confining profile dies with SIGABRT and no
/// diagnostic. Seatbelt evaluates `file-read-metadata` against an fd's vnode on `fstat`, even
/// for a descriptor the process never opened by path. Node's `PlatformInit` stats fds 0/1/2
/// before its own error machinery is up and reads `if (errno != EBADF) ABORT()` — so an
/// ungranted stdio path turns EPERM into a message-less abort inside
/// `InitializeOncePerProcessInternal`. Denial line that named it:
/// `Sandbox: node(3101) deny(1) file-read-metadata /private/tmp/.../out.log`. Node is only the
/// loudest victim; any program that stats its own stdio hits the same wall.
///
/// Measured: only a WRITE-ONLY fd is affected. An `O_RDWR` stdio fd stats fine ungranted, which
/// is why an interactive shell survives and a `>` redirect — the shape a log-capturing harness
/// and every CI job produce — does not.
///
/// SCOPE. Metadata only (never read-data: verified that a bare metadata grant yields `statSync`
/// and `access` but EPERM on read/open/readlink/readdir), on the exact resolved path, never a
/// parent directory. A pipe or socket has no vnode, `F_GETPATH` fails, and nothing is granted.
///
/// WHY THE POLICY-DENY CHECK IS LOAD-BEARING, and position is not. SBPL is last-match-wins only
/// WITHIN one operation node: a `file-read-metadata` allow beats a `file-read*` deny on the same
/// path whether it is emitted before or after it — measured, both orders boot Node — because the
/// leaf operation outranks the group. Every deny in a compiled profile is `file-read*` (both the
/// policy's own, via `emit_fs`, and `emit_tmp`'s shared-tmp deny), so this grant would silently
/// punch a stat-shaped hole through the `.env`/`~/.ssh` floor that `compiler::defaults` promises
/// no later allow can reopen. Withholding the path is the only thing that actually closes it;
/// only a same-leaf `file-read-metadata` deny would be shadowed by ordering, and none is emitted.
fn emit_stdio_grants(policy: &SandboxPolicy, paths: &[String], out: &mut String) {
    let mut seen: Vec<&str> = Vec::new();
    for path in paths {
        if seen.contains(&path.as_str()) || policy_denies(policy, path) {
            continue;
        }
        out.push_str(&format!(
            "(allow file-read-metadata (literal \"{}\"))\n",
            sbpl_escape(path)
        ));
        seen.push(path);
    }
}

/// Whether an EXPLICIT policy deny covers `path`. Deliberately ignores `default_effect`: under
/// the build jail's pure allowlist every path outside the grants defaults to Deny, so honoring
/// the default here would withhold every stdio grant and restore the abort.
fn policy_denies(policy: &SandboxPolicy, path: &str) -> bool {
    policy
        .fs
        .rules
        .entries
        .iter()
        .filter(|rule| rule.effect == Effect::Deny)
        .filter_map(|rule| crate::matcher::path::compile_glob(rule.matcher.as_str()).ok())
        .any(|glob| glob.is_match(path))
}

/// The path behind `fd`, or `None` when it has no vnode (pipe, socket, closed).
///
/// `F_GETPATH` returns the kernel's CANONICAL path — measured: an fd opened as `/tmp/x` reports
/// `/private/tmp/x`, and one opened through a symlink reports the target — which is the form the
/// kernel matches a `(literal …)` against, so no further canonicalization is correct here.
/// Notably NOT `normalize_slashes`: a backslash is a legal macOS filename byte and rewriting it
/// to `/` would both miss the real path and name an unrelated one.
///
/// No existence check either. `F_GETPATH` still reports the stale path of an UNLINKED-but-open
/// fd, the kernel still honors a `(literal …)` grant on that stale name (measured, both halves),
/// and an fd on a deleted temp file is an ordinary stdio shape — so requiring existence would
/// drop the grant exactly where it is still needed.
fn stdio_fd_path(fd: std::os::raw::c_int) -> Option<String> {
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: `F_GETPATH` takes no length and writes into `buf` unconditionally, so the buffer
    // MUST be at least MAXPATHLEN (== PATH_MAX == 1024 here). Do not shrink it.
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0)?;
    let path = std::str::from_utf8(&buf[..len]).ok()?;
    // The base profile already grants `file-read-metadata` across all of `/dev` (the regex near
    // the end of `macos_seatbelt_base.sbpl`), which covers every tty, `/dev/null`, and `/dev/fd`
    // stdio — the interactive and `cargo test` cases. Emitting those would be pure noise.
    if !path.starts_with('/') || path.starts_with("/dev/") {
        return None;
    }
    Some(path.to_string())
}

/// The macOS shared-system-tmp roots hidden under `TmpMode::{Private,Deny}`: the per-user
/// DARWIN confstr scratch (`$TMPDIR` = `/private/var/folders/<uid>/T`) and the world-shared
/// `/private/tmp` (the `/tmp` firmlink target). Canonical (confstr dirs already resolved;
/// `/private/tmp` is canonical). These are the only tmp surfaces a child normally reaches.
fn shared_tmp_dirs() -> Vec<String> {
    let mut out = confstr_scratch_dirs();
    out.push("/private/tmp".to_string());
    out
}

/// Emit the tmp-mode SBPL. `Shared` is a no-op (the confstr write grant that emit_fs
/// already emitted stands). `Private`/`Deny` DENY read+write on the shared-tmp roots
/// (last-match-wins over a generous read); `Private` additionally grants the fresh
/// per-run dir rw via [`regrant_over_tmp_deny`], which is per-operation-node because a
/// general `file*` allow cannot override those denies. Emitted after emit_fs so the
/// shared-tmp deny is authoritative even under a `(subpath "/")` generous read.
///
/// COMPILER CARVE-OUT: `$tmp` is a PRIVATE PER-RUN DIR ONLY, plus ONE documented non-private
/// carve-out on macOS — Apple's fixed toolchain lookup cache. `Private` therefore denies the
/// WHOLE shared tmp (confstr scratch included) and grants back exactly
/// [`darwin_compiler_cache_files`].
///
/// The carve-out is ONE FILE, not the enclosing scratch: `$TMPDIR` is a long-lived per-user
/// directory holding every application's state (~7.5k entries on a dev host), so granting the
/// subpath handed a confined child read+write over all of it.
///
/// `xcrun_db` earns its exemption by being non-redirectable — measured, `TMPDIR=<elsewhere>
/// xcrun --find cc` still writes the confstr-resolved `$TMPDIR/xcrun_db` and leaves the
/// override untouched. Anything the toolchain scratches that DOES follow `TMPDIR` needs no
/// grant (the jail points it at the per-run dir) and must not be added here. See
/// LIMITATIONS.md.
fn emit_tmp(policy: &SandboxPolicy, tmp_dir: Option<&std::path::Path>, out: &mut String) {
    use crate::policy::TmpMode;
    if policy.fs.tmp == TmpMode::Shared {
        return;
    }
    // Both modes hide the WHOLE shared tmp; they differ only in what is granted back below
    // (Private: the policy's own grants + the compiler cache + the per-run dir; Deny: only
    // the policy's own grants).
    let roots = shared_tmp_dirs();
    for dir in &roots {
        let term = format!("(subpath \"{}\")", sbpl_escape(dir));
        out.push_str(&format!("(deny file-read* {term})\n"));
        out.push_str(&format!("(deny file-write* {term})\n"));
    }
    // Re-open the policy's OWN explicit grants that happen to live inside the shared tmp.
    // The deny above targets the AMBIENT scratch, not a tree someone deliberately put there
    // — CI checkouts, `npm pack`, and nub's own dlx staging all run under `$TMPDIR`. Because
    // the deny is emitted after `emit_fs` (so it can override a generous base read), without
    // this it would also silently nuke the build jail's package-dir write grant and every
    // read it depends on: the documented `/private/tmp` footgun, generalized to the whole
    // per-user scratch.
    //
    // ORDER IS THE WHOLE PROBLEM HERE. Re-emitting the same rule SET is not order-neutral:
    // these allows land after everything `emit_fs` wrote, so a naive replay would out-rank
    // the policy's own denies and re-open, say, `$TMPDIR/work/.env` on a policy that still
    // carries the secret floor. So each re-grant is followed by a replay of every deny that
    // matches at or under the same root, restoring last-match-wins, and the write arm
    // re-applies `is_dangerous_write_root` — `emit_fs` guards its write grants with it, and
    // skipping it here would hand out `(allow file-write* (subpath "/private/tmp"))`.
    let mut regranted = false;
    for rule in &policy.fs.rules.entries {
        if rule.effect != Effect::Allow || !grant_is_under(rule.matcher.as_str(), &roots) {
            continue;
        }
        let m = to_match_term(rule.matcher.as_str());
        let term = emit_term(&m);
        out.push_str(&format!("(allow file-read* {term})\n"));
        out.push_str(&format!("(allow file-map-executable {term})\n"));
        if rule.access == FsAccess::ReadWrite && !is_dangerous_write_root(&m) {
            out.push_str(&format!("(allow file-write* {term})\n"));
        }
        regranted = true;
    }
    if regranted {
        for rule in &policy.fs.rules.entries {
            if rule.effect != Effect::Deny {
                continue;
            }
            let term = emit_term(&to_match_term(rule.matcher.as_str()));
            out.push_str(&format!("(deny file-read* {term})\n"));
            out.push_str(&format!("(deny file-write* {term})\n"));
        }
    }
    if policy.fs.tmp == TmpMode::Private {
        // xcrun WRITES the db, not merely reads it, so this re-grant must clear BOTH denies.
        //
        // AND IT NEVER WRITES THE NAME IN PLACE — the trailing `*` is what makes this grant
        // do anything at all. The toolchain stages through an `mkstemp`-suffixed sibling
        // (`xcrun_db-pH2r2bhb`) and renames it over the real name, so a bare `(literal
        // ".../xcrun_db")` denies the only write that ever happens. Measured on the macOS
        // corpus break shard: 40 denials in ONE run — `c++: error: couldn't create cache file
        // '/var/folders/<uid>/T/xcrun_db-XXXXXX' (errno=Operation not permitted)`, from `c++`,
        // `make` and `libtool` — and every from-source native build behind them failed.
        //
        // Still FILE-level, which is the property this carve-out exists to hold: `*` does not
        // span a path component, so the pattern reaches the cache and its own staging
        // siblings and nothing else in the ~7.5k-entry shared scratch. Routed through
        // `to_match_term` rather than a hand-written regex so it uses the same translator
        // (and the same globset oracle tests) as every other matcher here.
        for file in darwin_compiler_cache_files() {
            regrant_over_tmp_deny(&emit_term(&to_match_term(&format!("{file}*"))), out);
        }
    }
    if policy.fs.tmp == TmpMode::Private
        && let Some(dir) = tmp_dir
    {
        // Canonicalize (the tempdir root can sit under a firmlink) so the grant matches
        // the kernel's canonical view; grant read+write+map of the fresh dir.
        let canon = canonicalize_including_nonexistent(dir);
        let p = normalize_slashes(&canon.to_string_lossy());
        if !p.is_empty() && p != "/" {
            regrant_over_tmp_deny(&format!("(subpath \"{}\")", sbpl_escape(&p)), out);
        }
    }
}

/// Re-open `term` after the shared-tmp denies above it.
///
/// MEASURED SBPL PRECEDENCE (2026-07-28, `sandbox-exec` on darwin 25.5): last-match-wins holds
/// only WITHIN one operation node. Across nodes the MORE SPECIFIC node wins regardless of
/// position, so `(allow file* X)` does NOT override `(deny file-write* X/..)` — placing it after
/// the deny changes nothing. Each denied op must be re-granted in ITS OWN node, positioned after
/// the deny. `file*` is retained for the ops nothing here denies (ioctl, clone, …).
///
/// This is why the per-run tmp dir was unwritable: `make_private_tmp` lands it inside the confstr
/// scratch this function denies, and a lone `file*` allow could never re-open it.
fn regrant_over_tmp_deny(term: &str, out: &mut String) {
    out.push_str(&format!("(allow file* {term})\n"));
    out.push_str(&format!("(allow file-read* {term})\n"));
    out.push_str(&format!("(allow file-map-executable {term})\n"));
    out.push_str(&format!("(allow file-write* {term})\n"));
}

/// The macOS env-read closure — the load-bearing security default that stops a
/// confined child recovering a scrubbed secret from a co-resident same-uid process's
/// environment. Emitted UNCONDITIONALLY on every wrapped profile, all macOS versions.
///
/// The vector: `sysctl KERN_PROCARGS2` (and its `KERN_PROCARGS` twin) return a target
/// pid's argv+environ. The kernel permits that read iff, for the target, EITHER
/// `sysctl-read` OR `process-info*` is allowed — a DISJUNCTION, so BOTH arms must be
/// denied. Under this backend's `(deny default)` only the process-info arm is open:
///
/// - sysctl arm: already shut — procargs2's (pid-parameterized, unnameable) sysctl is
///   not in the base allowlist, and the base allows kern.* only by SPECIFIC NAME
///   (never a `(sysctl-name-prefix "kern.")`, which WOULD re-admit it). No sysctl rule
///   is needed here.
/// - process-info arm: OPEN — `process-info*` is allowed-by-default even under
///   `(deny default)`, so it must be denied EXPLICITLY. This is that denial.
///
/// The self-restore is `(target self)` and nothing wider: `(target others)` leaks a
/// sibling's env, and `(target same-sandbox)` re-opens the hole (a confined child's
/// own siblings/children ARE same-sandbox); node needs only self-introspection.
/// Empirically verified 20/20 with negative controls on macOS 26 (xnu-12377).
fn emit_env_read_closure(out: &mut String) {
    out.push_str("(deny process-info*)\n");
    out.push_str("(allow process-info* (target self))\n");
}

/// Net axis. Three cases:
///   - not enforced → allow all egress + the DNS/TLS service block.
///   - enforced WITH a proxy → permit egress ONLY to the proxy's loopback port, so
///     the child must route per-host through it. This deliberately does NOT carve all
///     of loopback: arbitrary local services (a sibling listener, a docker daemon on
///     127.0.0.1) and AF_UNIX sockets (`docker.sock`) stay DENIED by the base — the
///     local-exfil holes the old `localhost:*` carve left open are closed here.
///   - enforced WITHOUT a proxy (coarse deny-all) → NO carve at all; the base
///     `(deny default)` denies every egress including loopback (nothing reachable).
///
/// Seatbelt requires `localhost`/`*` as the host in a `remote ip` literal (a numeric
/// `127.0.0.1` literal is a PARSE ERROR that fails the whole profile load); `localhost`
/// covers loopback on both 127.0.0.1 and ::1, and the explicit `:<port>` pins the one
/// proxy port.
fn emit_net(policy: &SandboxPolicy, proxy_port: Option<u16>, out: &mut String) {
    if !policy.net.enforce {
        out.push_str("(allow network*)\n");
        out.push_str(NETWORK_SERVICES);
        return;
    }
    if let Some(port) = proxy_port {
        out.push_str(&format!(
            "(allow network* (remote ip \"localhost:{port}\"))\n"
        ));
    }
    // else: coarse deny-all — emit nothing (the base (deny default) closes all egress,
    // loopback and AF_UNIX included).
}

/// Filesystem axis: reads then writes, each reproducing the IR's last-match-wins
/// over the same ordered entry list.
fn emit_fs(policy: &SandboxPolicy, spec: &CommandSpec, out: &mut String) {
    if !fs_confines(policy) {
        // Fully relaxed fs — grant every file op (we wrapped only to enforce net).
        out.push_str("(allow file*)\n");
        return;
    }

    // ── reads ────────────────────────────────────────────────────────────────
    if policy.fs.rules.default_effect == Effect::Allow {
        // Unmatched reads allowed (generous base); entries below tighten it.
        out.push_str("(allow file-read* (subpath \"/\"))\n");
        out.push_str("(allow file-map-executable (subpath \"/\"))\n");
    }
    // Auto-grant read/map of the target binary FILE so read-confine can exec it
    // (system tools are already covered by the essential base). Only the file — NOT
    // its parent dir: a directory grant would expose the program's SIBLINGS (e.g. a
    // `.env`/key next to a project-local tool), defeating a tight read allowlist. A
    // non-system toolchain's out-of-dir libs need an explicit toolchain allow.
    if let Some(term) =
        program_read_term(spec, policy.env.constructed.get("PATH").map(String::as_str))
    {
        out.push_str(&format!("(allow file-read* {term})\n"));
        out.push_str(&format!("(allow file-map-executable {term})\n"));
    }
    for rule in &policy.fs.rules.entries {
        let term = emit_term(&to_match_term(rule.matcher.as_str()));
        match rule.effect {
            Effect::Allow => {
                out.push_str(&format!("(allow file-read* {term})\n"));
                out.push_str(&format!("(allow file-map-executable {term})\n"));
            }
            Effect::Deny => out.push_str(&format!("(deny file-read* {term})\n")),
        }
    }

    // ── writes (base denies all writes) ───────────────────────────────────────
    for rule in &policy.fs.rules.entries {
        let m = to_match_term(rule.matcher.as_str());
        let term = emit_term(&m);
        match (rule.effect, rule.access) {
            (Effect::Allow, FsAccess::ReadWrite) => {
                // Refuse a write grant that resolves to a dangerous top-level root
                // (a `..` in a surface path can collapse a grant up to `/private`
                // etc. — an accidental filesystem-wide write hole). Fail-safe: drop
                // the over-broad grant rather than emit it.
                if is_dangerous_write_root(&m) {
                    continue;
                }
                out.push_str(&format!("(allow file-write* {term})\n"));
            }
            // AN ALLOW NEVER SUBTRACTS. A read-only Allow grants read and takes nothing
            // away; Deny is the only subtractive verb on this axis. That is [`FsPolicy`]'s
            // own contract (write-set = the ReadWrite allows; a Deny removes read AND
            // write) and the only reading the other backends can render — Landlock unions
            // its rules and has no deny primitive at any ABI, and Windows accumulates a
            // read set and a write set with no ordering at all.
            //
            // The synthesized deny this arm used to emit had NOTHING to cap: the write base
            // is `(deny default)` and a generous `default_effect` widens only READS, so the
            // sole thing it could ever cancel was another nub grant. It did exactly that —
            // `curated::project_reads` appends its read pair AFTER `sibling_dirs`, so
            // `(deny file-write* (subpath <proj>/node_modules))` landed last and revoked the
            // write grant beneath it (measured: `projectReads: ["node_modules"]` added alone
            // took `.prisma/client` from 20 written entries to 0). Same root cause as the
            // `project_cwd` node/subtree widening, which was fixed at its call site and so
            // survived here.
            //
            // The cost, stated: "readable but not writable INSIDE a writable grant" is not
            // expressible — on any backend. Removing access is a Deny, which removes read too.
            (Effect::Allow, FsAccess::Read) => {}
            (Effect::Deny, _) => out.push_str(&format!("(deny file-write* {term})\n")),
        }
    }
    // The Apple toolchain (xcrun/cc/libtool) writes its `xcrun_db` scratch to the
    // per-user DARWIN confstr TEMP dir — NOT redirectable via TMPDIR — so a
    // from-source compile fails without this grant. Emitted LAST so it survives every
    // write-deny above it under last-match-wins; since only a Deny emits one, the only
    // thing it can override is a user write-deny targeting the OS temp, which is rare
    // and acceptable (and `emit_move_block` re-asserts that deny's unlink/create half
    // afterwards). The persistent DARWIN CACHE dir is deliberately NOT
    // granted — it is a cross-build poisoning surface a later unsandboxed tool
    // consumes, and `cc`/`xcrun` need only the temp scratch.
    for dir in confstr_scratch_dirs() {
        out.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            sbpl_escape(&dir)
        ));
    }

    emit_move_block(policy, out);
}

/// Close the move/rename secret-relocation bypass (SRT's `generateMoveBlockingRules`).
/// A secret is protected by a write-DENY on its path, but two macOS holes let a child
/// relocate the bytes past that path-keyed deny: (1) the trailing confstr
/// `(allow file-write* <temp>)` grant above is last-match-wins, so it re-opens
/// unlink/rename on any denied path living under `$TMPDIR`; (2) an anchored deny
/// (`/proj/.env`) blocks the file `mv` but not `mv proj proj2`, which relocates the whole
/// containing dir out from under the anchored deny.
///
/// INVARIANT (load-bearing): these denies MUST be emitted AFTER the confstr grant so they
/// win the last-match-wins race, and ONLY the Deny arm + the ancestor-dir chain are
/// re-denied — never an Allow (which subtracts nothing anywhere in this backend) and never
/// the confstr grant itself, which would re-deny the legit `xcrun_db` scratch write.
fn emit_move_block(policy: &SandboxPolicy, out: &mut String) {
    // Fix 1 — re-assert each Deny's unlink/create block. A `(subpath)` deny covers the
    // denied file/subtree; re-emitting the unlink/create primitives here restores the deny
    // that the trailing confstr write grant would otherwise override for a `$TMPDIR` secret.
    for rule in &policy.fs.rules.entries {
        if rule.effect == Effect::Deny {
            let term = emit_term(&to_match_term(rule.matcher.as_str()));
            out.push_str(&format!("(deny file-write-unlink {term})\n"));
            out.push_str(&format!("(deny file-write-create {term})\n"));
        }
    }

    // Fix 2 — ancestor move-block for DIRECTORY-PINNING denies. For each deny, pin
    // unlink/create on the directory chain from the secret's innermost writable container
    // up to (and including) the enclosing write-grant root, so renaming a container can't
    // relocate the secret past its path-keyed deny. The chain start differs by deny shape,
    // because Fix 1's re-asserted deny covers a different innermost path in each:
    //   • LITERAL `(subpath)` deny (`/proj/.env`, `/proj/secrets` subtree) — Fix 1's subpath
    //     deny already matches its own root path, so renaming the secret / subtree-root
    //     itself is blocked; only the ANCESTORS need pinning. Probe = the secret path; the
    //     walk pins parent(secret) upward.
    //   • REGEX directory-pinning deny (`!secrets/*.key` → `/proj/secrets/*.key`) — Fix 1's
    //     regex deny matches only the glob LEAF files, NOT their literal container dir
    //     `/proj/secrets`, so `mv secrets secretz` relocates the leaves past the deny. Pin
    //     the deny's literal directory PREFIX itself and up. Probe = `<prefix>/*`, so the
    //     walk pins `<prefix>` (not just its parent) upward.
    // A deny with no absolute literal directory prefix (`**/secrets/**` — the matched dir
    // name floats, no fixed anchor), or one whose relocation-sensitive container is itself a
    // PARTIAL non-leaf glob (`sec*/x.key`), yields nothing (or too shallow) to pin — a bounded
    // residual documented in LIMITATIONS.md. The `(literal P)` denies are EXACT-path — they block
    // renaming dir `P` itself, never a create/write INSIDE it, so `echo > proj/other.txt`
    // and writes under `/proj/secrets/` still work.
    let grant_roots = write_grant_roots(policy);
    for rule in &policy.fs.rules.entries {
        if rule.effect != Effect::Deny {
            continue;
        }
        let probe = match to_match_term(rule.matcher.as_str()) {
            // Both anchored shapes probe the same path; a deny arrives as the pair
            // `[P, P/**]`, whose halves now classify differently and must not diverge here.
            MatchTerm::Literal(denied) | MatchTerm::Subpath(denied) => denied,
            MatchTerm::Regex(_) => {
                let Some(prefix) = regex_literal_dir_prefix(rule.matcher.as_str()) else {
                    continue;
                };
                format!("{prefix}/*")
            }
        };
        for anc in move_block_ancestors(&probe, &grant_roots) {
            let lit = format!("(literal \"{}\")", sbpl_escape(&anc));
            out.push_str(&format!("(deny file-write-unlink {lit})\n"));
            out.push_str(&format!("(deny file-write-create {lit})\n"));
        }
    }
}

/// The literal directory PREFIX of a glob deny — the leading run of glob-free path
/// components (`/proj/secrets/*.key` → `/proj/secrets`; `/proj/packages/*/.env` →
/// `/proj/packages`). Pinning it + its ancestors blocks relocating a secret whose
/// container is this literal prefix OR a FULL glob component below it (`packages/*/.env`:
/// renaming the `*`-matched intermediate keeps it matched; renaming `packages` is pinned).
/// `None` when there is no absolute multi-component prefix to anchor (a first-segment or
/// leading-`**` glob). The meta set matches `to_match_term`'s Regex classifier.
///
/// RESIDUAL (see LIMITATIONS.md): a PARTIAL glob in a NON-LEAF component (`sec*/x.key`)
/// leaves its relocation-sensitive container (`/proj/secrets`, matched by `sec*`) BELOW this
/// literal prefix and thus unpinned — renaming it to a name outside the pattern escapes. A
/// literal `}`/`]` in a dir name hits the same residual (regex-classified, truncates here).
fn regex_literal_dir_prefix(glob: &str) -> Option<String> {
    let meta = glob.find(['*', '?', '[', ']', '{', '}'])?;
    let slash = glob[..meta].rfind('/')?;
    let prefix = &glob[..slash];
    (prefix.len() > 1 && prefix.starts_with('/')).then(|| prefix.to_string())
}

/// The write-granted subpath roots: every rw Allow that survives the dangerous-root
/// guard, plus the confstr scratch dirs (also `(allow file-write* (subpath …))` grants).
/// A directory rename can only relocate a secret when the container is writable, so these
/// roots bound how far up the ancestor move-block must reach.
fn write_grant_roots(policy: &SandboxPolicy) -> Vec<String> {
    let mut roots = Vec::new();
    for rule in &policy.fs.rules.entries {
        if let (Effect::Allow, FsAccess::ReadWrite) = (rule.effect, rule.access) {
            let m = to_match_term(rule.matcher.as_str());
            if is_dangerous_write_root(&m) {
                continue;
            }
            // `Subpath` only: these roots bound how far the ancestor move-block walks, and
            // the thing that makes a rename possible is a writable CONTAINER. A `Literal`
            // rw grant is the node alone and contains nothing. The subtree pair still
            // contributes its root via the `/**` half, so no real grant is lost.
            if let MatchTerm::Subpath(p) = m {
                roots.push(p);
            }
        }
    }
    roots.extend(confstr_scratch_dirs());
    roots
}

/// Ancestor directories to move-block for an anchored deny at `denied`: from the secret's
/// PARENT up to and including the outermost (shortest) write-grant root that STRICTLY
/// contains it. Empty when no write grant encloses the deny — no writable container to
/// rename, so nothing to block (the base denies write on every ancestor).
fn move_block_ancestors(denied: &str, grant_roots: &[String]) -> Vec<String> {
    let Some(root) = grant_roots
        .iter()
        .filter(|g| path_strictly_contains(g, denied))
        .min_by_key(|g| g.len())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur = parent_dir(denied);
    while let Some(dir) = cur {
        out.push(dir.to_string());
        if dir == root.as_str() {
            break;
        }
        cur = parent_dir(dir);
    }
    out
}

/// Whether `root` is a strict ancestor directory of `child` (`child` == `root` + `/…`).
/// Strict (not equal) so a deny whose path equals a grant root is left to the file-level
/// deny, never walked as its own ancestor.
fn path_strictly_contains(root: &str, child: &str) -> bool {
    child
        .strip_prefix(root)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The parent directory of a path as a `&str`, or `None` at the filesystem root. Filters
/// the empty parent so a top-level entry doesn't yield `""`.
fn parent_dir(p: &str) -> Option<&str> {
    Path::new(p)
        .parent()
        .and_then(Path::to_str)
        .filter(|s| !s.is_empty())
}

/// Top-level roots a write grant must never cover — a `..`-collapsed surface path
/// (`/tmp/..` → `/private`) would otherwise open filesystem-wide write. Reads are
/// exempt (a generous `(subpath "/")` read is the legitimate default posture).
///
/// The matcher reaching here is already firmlink-CANONICALIZED, so the entries must
/// be the canonical forms the guard actually sees: `/var`/`/etc`/`/tmp` resolve to
/// `/private/var`/`/private/etc`/`/private/tmp`. The firmlink spellings are kept
/// too (harmless, self-documenting); `/private/tmp` is deliberately absent — it is
/// the legitimate temp firmlink target, not a broad system root.
fn is_dangerous_write_root(term: &MatchTerm) -> bool {
    // `Literal` is checked alongside `Subpath` because a subtree arrives as the PAIR
    // `[P, P/**]` and only the second half classifies as `Subpath` — guarding one half
    // would hand out `(allow file-write* (literal "/private"))`, i.e. permission to
    // rename or unlink the root itself.
    let (MatchTerm::Literal(p) | MatchTerm::Subpath(p)) = term else {
        return false;
    };
    matches!(
        p.as_str(),
        "/" | "/private"
            | "/private/var"
            | "/private/etc"
            | "/System"
            | "/Users"
            | "/usr"
            | "/bin"
            | "/sbin"
            | "/etc"
            | "/var"
            | "/opt"
            | "/Library"
            | "/Applications"
            | "/Volumes"
            | "/Network"
            | "/cores"
    )
}

/// Best-effort read/map grant for the target program FILE so read-confine can exec
/// it. `None` when the program can't be resolved (a bare name with no PATH hit) —
/// the essential base still covers system tools.
fn program_read_term(spec: &CommandSpec, child_path: Option<&str>) -> Option<String> {
    let resolved = resolve_program(&spec.program, spec.cwd.as_deref(), child_path)?;
    let file = normalize_slashes(&resolved.to_string_lossy());
    Some(format!("(subpath \"{}\")", sbpl_escape(&file)))
}

/// Resolve a program to an absolute, canonical path. A cwd-relative program is
/// resolved against the CHILD's cwd (`spec.cwd`, where the kernel will resolve it),
/// falling back to the process cwd; a bare name is searched on the constructed
/// child `PATH`, matching the lookup performed after `sandbox-exec` enters the child.
fn resolve_program(
    program: &std::ffi::OsStr,
    child_cwd: Option<&Path>,
    child_path: Option<&str>,
) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() {
        return Some(canonicalize_including_nonexistent(p));
    }
    if p.components().count() > 1 {
        // cwd-relative (`./x`, `dir/x`) — anchor at the child's cwd, not ours.
        let base = match child_cwd {
            Some(c) => c.to_path_buf(),
            None => std::env::current_dir().ok()?,
        };
        return Some(canonicalize_including_nonexistent(&base.join(p)));
    }
    // bare name → PATH search
    let path_var = child_path?;
    for dir in std::env::split_paths(std::ffi::OsStr::new(path_var)) {
        // POSIX PATH resolves relative components (including an empty component) from
        // the process cwd. The process will run in `spec.cwd`, so profile construction
        // must use that same base rather than nub's ambient cwd.
        let dir = if dir.is_absolute() {
            dir
        } else {
            let base = match child_cwd {
                Some(cwd) => cwd.to_path_buf(),
                None => std::env::current_dir().ok()?,
            };
            base.join(dir)
        };
        let cand = dir.join(p);
        if cand.is_file() {
            return Some(canonicalize_including_nonexistent(&cand));
        }
    }
    None
}

/// The per-user DARWIN confstr TEMP scratch dir (`/private/var/folders/<uid>/T`),
/// canonicalized (a `/var/folders/…` firmlink resolving under `/private`). Only the
/// TEMP dir — NOT the persistent CACHE dir (`…/C`), which is a cross-build poisoning
/// surface. Empty off macOS or when confstr yields nothing.
/// Whether an fs rule's literal path lies STRICTLY INSIDE one of the shared-tmp roots, so
/// the tmp deny would otherwise swallow a grant the policy made deliberately.
///
/// Strictly inside: a grant of a tmp root ITSELF is the very thing the `$tmp` posture exists
/// to hide, and re-opening it would let `fs: {"/private/tmp": "rw"}` defeat `$tmp: "rw"` —
/// the posture is authoritative over the shared roots, which is why `emit_tmp` runs after
/// `emit_fs` at all. Only a tree someone placed under a root is re-opened.
///
/// Compares the glob's literal prefix (the `/**` subtree twin stripped) on a path-COMPONENT
/// boundary, so `/private/var/folders/x/T` never matches a sibling `/private/var/folders/x/Tools`.
/// A matcher carrying embedded globs has no literal prefix to compare and is simply not
/// re-granted — fail-closed, and the same shape `emit_fs` already declines to widen.
fn grant_is_under(matcher: &str, roots: &[String]) -> bool {
    let literal = matcher.strip_suffix("/**").unwrap_or(matcher);
    roots.iter().any(|root| {
        let root = root.trim_end_matches('/');
        literal
            .strip_prefix(root)
            .is_some_and(|rest| rest.len() > 1 && rest.starts_with('/'))
    })
}

/// The ONE documented non-private carve-out inside the shared tmp: Apple's fixed toolchain
/// lookup cache, granted back after `$tmp`'s deny so native builds keep working.
///
/// `xcrun` resolves this path through `confstr(_CS_DARWIN_USER_TEMP_DIR)` rather than
/// `$TMPDIR`, so redirecting the child's `TMPDIR` at the private per-run dir does NOT move it
/// — measured directly: `TMPDIR=<fresh dir> xcrun --find cc` updated the real
/// `$TMPDIR/xcrun_db` and left the fresh dir empty. That non-redirectability is the entire
/// reason this is a carve-out; anything the toolchain writes that DOES follow `TMPDIR` needs
/// no grant and must not be added here.
///
/// A FILE, never its parent directory — the parent is the shared scratch this narrowing
/// exists to withhold.
///
/// The returned path is the cache's own name. The grant emitted from it covers that name
/// AND its `mkstemp` staging siblings — see the emission site, where the widening and the
/// measurement that forced it are argued.
fn darwin_compiler_cache_files() -> Vec<String> {
    confstr_scratch_dirs()
        .into_iter()
        .map(|dir| format!("{}/xcrun_db", dir.trim_end_matches('/')))
        .collect()
}

fn confstr_scratch_dirs() -> Vec<String> {
    let mut out = Vec::new();
    if let Some(dir) = confstr_dir(libc::_CS_DARWIN_USER_TEMP_DIR) {
        let canon = canonicalize_including_nonexistent(&dir);
        let s = normalize_slashes(&canon.to_string_lossy());
        // Refuse a root/empty grant (would be a filesystem-wide write hole).
        if !s.is_empty() && s != "/" {
            out.push(s);
        }
    }
    out
}

/// Query one `confstr(3)` path. Two-call idiom: size probe, then fill.
fn confstr_dir(name: libc::c_int) -> Option<PathBuf> {
    // SAFETY: standard confstr two-call sequence — first a NULL/0 size probe, then
    // a fill into an exactly-sized buffer; the returned string is NUL-terminated.
    unsafe {
        let len = libc::confstr(name, std::ptr::null_mut(), 0);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        let got = libc::confstr(name, buf.as_mut_ptr() as *mut libc::c_char, len);
        if got == 0 || got > len {
            return None;
        }
        // Trim at the NUL and any trailing slash the OS appends.
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[..end]).into_owned();
        let s = s.trim_end_matches('/');
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }
}

/// A translated SBPL match term: one exact path, an absolute-literal subtree, or a
/// glob rendered as an anchored Seatbelt regex.
enum MatchTerm {
    Literal(String),
    Subpath(String),
    Regex(String),
}

/// Translate one canonical IR glob into an SBPL match term. An absolute literal
/// becomes `(literal …)` — the ONE path, see below; its `/**` twin becomes
/// `(subpath …)`; a whole-fs `**` becomes `(subpath "/")`; anything with embedded
/// globs becomes an anchored regex (Seatbelt has no glob syntax).
///
/// THE IR SPELLS A SUBTREE AS THE PAIR `[P, P/**]` (`compiler::defaults::subtree_globs`), so a
/// bare `P` NAMES THE DIRECTORY NODE AND NOTHING UNDER IT. Rendering it `(subpath P)`
/// — which this did — silently widened `curated::project_cwd`'s node grant into a read of
/// the consumer's WHOLE project. It also, at the time, widened the write loop's
/// then-synthesized `(deny file-write* (subpath P))` over every grant `P` enclosed, which
/// under Seatbelt's within-node last-match-wins revoked `package_dir` and `siblingDirs`
/// (measured: `EPERM mkdir <cell>/node_modules/.prisma` on a path the policy granted rw;
/// flipping `projectCwd` alone made it succeed). That second face is gone at its root —
/// `emit_fs`'s Allow arms no longer emit any deny — but the widening is still wrong on the
/// read axis. `(literal P)` matches only operations whose target IS `P`, so the node stays
/// listable and renameable-blocked while paths BELOW it are governed by the rules that
/// actually name them.
///
/// Emitting the pair as `(literal P)` + `(subpath P)` is the same coverage it always
/// had — `(subpath P)` already includes `P` — so nothing that spells a subtree the IR's
/// way changes shape.
fn to_match_term(glob: &str) -> MatchTerm {
    if glob == "**" || glob == "/**" || glob == "/" {
        return MatchTerm::Subpath("/".to_string());
    }
    let has_meta = glob.contains(['*', '?', '[', ']', '{', '}']);
    if !has_meta && glob.starts_with('/') {
        return MatchTerm::Literal(glob.to_string());
    }
    // Literal prefix + trailing `/**` (the common subtree twin) → subpath of prefix.
    if let Some(prefix) = glob.strip_suffix("/**")
        && !prefix.contains(['*', '?', '[', ']', '{', '}'])
        && prefix.starts_with('/')
    {
        return MatchTerm::Subpath(prefix.to_string());
    }
    MatchTerm::Regex(glob_to_seatbelt_regex(glob))
}

/// Render a [`MatchTerm`] as its SBPL fragment.
fn emit_term(term: &MatchTerm) -> String {
    match term {
        MatchTerm::Literal(p) => format!("(literal \"{}\")", sbpl_escape(p)),
        MatchTerm::Subpath(p) => format!("(subpath \"{}\")", sbpl_escape(p)),
        MatchTerm::Regex(r) => format!("(regex #\"{}\")", r.replace('"', "\\\"")),
    }
}

/// Translate a git-style glob into an anchored Seatbelt regex. `**/` spans zero or
/// more components, `**` spans anything, `*`/`?` stay within one component, `[…]`
/// stays a character class, `{a,b}` is brace alternation. A metachar-free pattern
/// gets a subtree `(/.*)?` suffix. Ported from Codex's
/// `seatbelt_regex_for_unreadable_glob` (Apache-2.0); brace support added by nub.
fn glob_to_seatbelt_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut regex = String::from("^");
    let mut saw_glob = false;
    let mut run = RecurRun::None;
    let mut i = 0;
    while i < chars.len() {
        // `{` opens a brace group; `}`/`,` are literals at the top level (only a `,`
        // *inside* a group separates branches). Everything else is one glob unit.
        if chars[i] == '{' {
            i += 1;
            saw_glob = true;
            run = RecurRun::None;
            regex.push_str(&brace_to_regex(&chars, &mut i, &mut saw_glob));
        } else {
            translate_unit(&chars, &mut i, &mut regex, &mut saw_glob, false, &mut run);
        }
    }
    // A WHOLE leading-slash-free pattern that is nothing but recursive prefixes ending in
    // `/` (`**/`, `**/**/`, …) matches EVERYTHING in globset (its lone-`RecursivePrefix`
    // whole-pattern special case), NOT just the trailing-slash-or-empty set `(.*/)?`
    // describes — anchoring it to `.*` keeps a deny of `**/` from under-enforcing. Two
    // guards make this precise: the body must be a pure `(.*/)?` chain (a single `*`
    // component, a literal, or a suffix `**`→`.*` all leave residue → NOT this case, and
    // a leading `/` emits a literal first so the body wouldn't start with `(.*/)?`), AND
    // the source is only `*`/`/` — an empty brace (`**/{}`) also emits body `(.*/)?` but
    // its `{` breaks globset's lone-`RecursivePrefix`, so it must stay `(.*/)?`. (In a
    // brace BRANCH the same tokens stay `(.*/)?`; the special case is top-level-only.)
    let body = &regex[1..];
    if !body.is_empty()
        && body.replace("(.*/)?", "").is_empty()
        && chars.iter().all(|&c| c == '*' || c == '/')
    {
        regex.truncate(1);
        regex.push_str(".*");
    }
    if !saw_glob {
        regex.push_str("(/.*)?");
    }
    regex.push('$');
    regex
}

/// Expand a brace group `{a,b}` (the `{` already consumed, `*i` at its first inner
/// char) into a regex alternation `(a|b)`, advancing `*i` past the matching `}`.
///
/// WHY (security): braces are STANDARD glob syntax and nub's userspace/Linux matcher
/// (`globset`) expands them, but Seatbelt has no glob syntax — before this, the
/// translator escaped `{`/`}` as literals, so an fs deny `!secrets/{a,b}.key` matched
/// only a file literally named `{a,b}.key` and silently under-enforced (the
/// sandbox-glob-deny-fidelity leak). Alternation makes macOS consistent with globset.
///
/// globset-FIDELITY (the shape correctness that keeps it leak-free):
///   • nested `{a,{b,c}}` → `(a|(b|c))` and cartesian `{a,b}/{c,d}` → `(a|b)/(c|d)`
///     fall out for free — each `{` recurses, so two groups in sequence multiply.
///   • an EMPTY branch is DROPPED, matching globset's default `empty_alternates=false`
///     (`{a,}` matches `a` only, NOT `a`-or-empty; `{}`/`{,}` emit nothing at all).
///   • an unbalanced `{` (globset hard-errors on it) is auto-closed at input end so the
///     emitted regex stays valid AND a deny keeps biting (fail-safe, not fail-open).
///   • a `**` inside a branch is recursive (crosses `/`) ONLY where globset makes it so —
///     when it forms a whole path component (see `translate_unit`); a non-component `**`
///     like `{**,x}`/`pre{**,x}post` degrades to a single-component `[^/]*`, NOT the
///     dir-crossing `.*` (the brace-`**` over-grant closed after the #411 review).
/// A class-internal `,`/`}` (`{a,[,]}`) never splits: `translate_unit` consumes the
/// whole `[…]` before this loop sees the next char.
fn brace_to_regex(chars: &[char], i: &mut usize, saw_glob: &mut bool) -> String {
    let mut branches: Vec<String> = Vec::new();
    let mut cur = String::new();
    // Adjacent-recursive-`**` collapse is per-branch (globset dedupes recursive tokens
    // within one alternate, not across a branch boundary) — reset on `,` and `{`.
    let mut run = RecurRun::None;
    while *i < chars.len() {
        match chars[*i] {
            '}' => {
                *i += 1;
                break;
            }
            ',' => {
                *i += 1;
                run = RecurRun::None;
                branches.push(std::mem::take(&mut cur));
            }
            '{' => {
                *i += 1;
                run = RecurRun::None;
                cur.push_str(&brace_to_regex(chars, i, saw_glob));
            }
            _ => translate_unit(chars, i, &mut cur, saw_glob, true, &mut run),
        }
    }
    branches.push(cur);
    // Drop empty branches (globset default) — an all-empty group (`{}`/`{,}`) emits
    // nothing, exactly as globset erases empty alternates.
    let non_empty: Vec<String> = branches.into_iter().filter(|b| !b.is_empty()).collect();
    if non_empty.is_empty() {
        String::new()
    } else {
        format!("({})", non_empty.join("|"))
    }
}

/// State of an in-progress run of adjacent recursive `**` components. globset collapses
/// such a run into ONE recursive token, and the KIND is sticky in a globset-specific way
/// (`parse_star`): a run that starts at a pattern/branch boundary is a `RecursivePrefix`
/// and STAYS one no matter what follows; a run that starts after a literal `/` takes the
/// kind of its LAST `**` (a trailing suffix `**` makes the whole run `.*`). `translate_unit`
/// mirrors that so `**/**` never emits the `(.*/)?.*`-matches-everything over-grant.
#[derive(Clone, Copy, PartialEq)]
enum RecurRun {
    None,
    Prefix,
    Slash,
}

/// Translate ONE glob unit at `chars[*i]` — `*`/`**`/`?`/`[…]`/`]`/literal — into
/// `out`, advancing `*i`. `{`/`}`/`,` are handled by the callers (top level +
/// `brace_to_regex`), so this never sees an unescaped brace; a top-level `}`/`,`
/// reaches the literal arm and is escaped like any other char. `in_brace` tells the
/// `**` recursion test whether a `{`/`,` before it is a branch boundary and whether a
/// `,`/`}` after it is a branch end (both are literals at the top level). `run` carries
/// the adjacent-recursive-`**` collapse state (see [`RecurRun`]).
fn translate_unit(
    chars: &[char],
    i: &mut usize,
    out: &mut String,
    saw_glob: &mut bool,
    in_brace: bool,
    run: &mut RecurRun,
) {
    let ch = chars[*i];
    *i += 1;
    // Any unit other than a recursive `**` breaks the run; the `**` arm restarts it.
    let prev_run = std::mem::replace(run, RecurRun::None);
    match ch {
        '*' => {
            *saw_glob = true;
            if chars.get(*i) == Some(&'*') {
                // `**`. It is RECURSIVE (crosses `/`) only where globset recognizes a
                // whole path component (globset `parse_star`); a non-component `**`
                // degrades to two `*` = one `[^/]*` there, so emitting the dir-crossing
                // `.*`/`(.*/)?` outside a component OVER-grants (the brace-`**` leak).
                // Recursive iff:
                //   • Case A — `**` at pattern-start or brace-branch-start: peek is `/`
                //     or end (a branch-end `,`/`}` does NOT count → `{**,x}` is literal);
                //   • Case B — `**` right after a literal `/`: peek is `/`, end, or (in a
                //     brace) a branch-end `,`/`}` → `{a/**,b}` stays recursive.
                // The recursive SHAPES: `**/`→`(.*/)?` (consuming the `/`), a suffix
                // `**`→`.*`; both are already globset-equivalent.
                let first_star = *i - 1;
                *i += 1; // consume the second `*`
                let prev = first_star.checked_sub(1).map(|p| chars[p]);
                let case_a = first_star == 0 || (in_brace && matches!(prev, Some('{') | Some(',')));
                let peek = chars.get(*i).copied();
                let peek_slash = peek == Some('/');
                let peek_boundary =
                    peek.is_none() || (in_brace && matches!(peek, Some(',') | Some('}')));
                let recursive = if case_a {
                    peek_slash || peek.is_none()
                } else if prev == Some('/') {
                    peek_slash || peek_boundary
                } else {
                    false
                };
                if !recursive {
                    out.push_str("[^/]*");
                    return;
                }
                // Consume a trailing `/` so `**/` spans zero+ components.
                if peek_slash {
                    *i += 1;
                }
                match prev_run {
                    // A fresh run: emit this `**`'s shape and record which kind of run it
                    // opens (boundary-start → sticky prefix; slash-preceded → slash run).
                    RecurRun::None => {
                        out.push_str(if peek_slash { "(.*/)?" } else { ".*" });
                        *run = if case_a {
                            RecurRun::Prefix
                        } else {
                            RecurRun::Slash
                        };
                    }
                    // A `RecursivePrefix` run stays a prefix no matter what follows —
                    // absorb this `**` entirely (globset's prefix stickiness).
                    RecurRun::Prefix => *run = RecurRun::Prefix,
                    // A slash-started run takes its LAST `**`'s kind: a trailing suffix
                    // `**` (peek is end/branch-end) turns the whole run into `.*`, so
                    // rewrite the `(.*/)?` the run's head emitted; a `/`-followed `**`
                    // leaves it a zero-or-more `(.*/)?` (absorb, no change).
                    RecurRun::Slash => {
                        if !peek_slash && out.ends_with("(.*/)?") {
                            out.truncate(out.len() - "(.*/)?".len());
                            out.push_str(".*");
                        }
                        *run = RecurRun::Slash;
                    }
                }
            } else {
                out.push_str("[^/]*");
            }
        }
        '?' => {
            *saw_glob = true;
            out.push_str("[^/]");
        }
        '[' => {
            *saw_glob = true;
            let class_start = *i;
            let mut class = String::new();
            let mut closed = false;
            while *i < chars.len() {
                let c = chars[*i];
                *i += 1;
                if c == ']' {
                    closed = true;
                    break;
                }
                class.push(c);
            }
            if !closed {
                // Unterminated `[` → literal, reprocess the rest normally.
                out.push_str("\\[");
                *i = class_start;
                return;
            }
            out.push('[');
            let mut it = class.chars();
            if let Some(first) = it.next() {
                match first {
                    '!' => out.push('^'),
                    '^' => out.push_str("\\^"),
                    _ => out.push(first),
                }
            }
            for c in it {
                if c == '\\' {
                    out.push_str("\\\\");
                } else {
                    out.push(c);
                }
            }
            out.push(']');
        }
        ']' => {
            *saw_glob = true;
            out.push_str("\\]");
        }
        _ => out.push_str(&regex_escape_char(ch)),
    }
}

/// Escape a literal char for embedding in a regex. `/` and ordinary chars pass
/// through; the glob metachars (`*?[]`) never reach here as literals.
fn regex_escape_char(c: char) -> String {
    match c {
        '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '\\' => format!("\\{c}"),
        _ => c.to_string(),
    }
}

/// Escape a path for an SBPL double-quoted string literal.
fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The degradation for a WRAPPED profile. env is genuinely enforced on this path (the
/// profile carries the unconditional env-read closure), so it is never reported lost —
/// and the `!needs_wrap` early-return only fires when no secret is withheld, so no path
/// reports full env enforcement while leaving procargs2 open. The one degradable axis
/// is net-per-host: if net enforces per-host allows but the proxy could NOT be started
/// (`proxy_port == None`) the profile coarse-denies and we report `net-per-host`
/// degraded (fail-safe, not silent). With a proxy the per-host allows ARE enforced (via
/// SNI/target gating), so enforcement is full.
fn degradation(
    policy: &SandboxPolicy,
    proxy_port: Option<u16>,
    tmp_dir: Option<&std::path::Path>,
) -> Degradation {
    let mut deg = Degradation::full();
    if policy.net.enforce
        && proxy_port.is_none()
        && policy.net.rules.iter().any(|r| r.effect == Effect::Allow)
    {
        deg.lost.push("net-per-host".to_string());
        deg.reason = Some(
            "egress proxy unavailable — per-host allows denied (coarse network deny)".to_string(),
        );
    }
    if policy.fs.tmp == crate::policy::TmpMode::Private && tmp_dir.is_none() {
        deg.lost.push("tmp-private".to_string());
        deg.reason.get_or_insert_with(|| {
            "private temporary directory allocation failed; shared temp remains hidden".to_string()
        });
    }
    deg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        CanonGlob, FsOrigin, FsPolicy, FsRule, FsRuleSet, NetPolicy, NetRule, NetTarget, TmpMode,
    };
    use tempfile::TempDir;

    fn spec() -> CommandSpec {
        CommandSpec::new("/bin/cat")
    }

    fn fs_policy(default_effect: Effect, entries: Vec<FsRule>) -> SandboxPolicy {
        SandboxPolicy {
            fs: FsPolicy {
                rules: FsRuleSet {
                    entries,
                    default_effect,
                },
                tmp: TmpMode::Shared,
            },
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

    fn term_str(glob: &str) -> String {
        emit_term(&to_match_term(glob))
    }

    // ── inherited stdio (the message-less SIGABRT) ────────────────────────────

    /// A `TmpMode::Private` policy granting only Node's own bin dir, so a log file in
    /// `/private/tmp` lands inside the shared-tmp deny — the shape that took the build jail down.
    fn node_on_path() -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("node"))
                .find(|p| p.is_file())
        })
    }

    fn stdio_fixture() -> Option<(PathBuf, TempDir, String, SandboxPolicy)> {
        let node = node_on_path()?;
        let tmp = tempfile::Builder::new()
            .prefix("nub-stdio-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let log = std::fs::canonicalize(tmp.path()).unwrap().join("out.log");
        let log_str = log.to_string_lossy().into_owned();
        let mut policy = fs_policy(
            Effect::Deny,
            vec![rule(
                &node.parent().unwrap().to_string_lossy(),
                Effect::Allow,
                FsAccess::Read,
            )],
        );
        policy.fs.tmp = TmpMode::Private;
        Some((node, tmp, log_str, policy))
    }

    /// Does THIS host make Node abort on a stdio fd whose path no grant covers? That abort is
    /// the PRECONDITION the two differential tests below are built on, and it is a property of
    /// Node-on-this-Darwin rather than of anything nub does — so a host where it does not hold
    /// cannot verify those tests, and must say so instead of reporting a pass or a failure.
    ///
    /// ⛔ MEASURED, and the obvious explanations are all dead. GitHub's `macos-14` (Darwin 23.6.0)
    /// and `macos-15` (Darwin 24.6.0) runners are IDENTICAL here — Node exits 0 — while Darwin
    /// 25.5.0 aborts as the tests expect. So it is not a 23→24 boundary, and **bumping the runner
    /// image does not fix it**. Nor is it stdio SHAPE (all four pass locally from `/dev/null`, to
    /// a file, through a pipe, and backgrounded off any tty) and nor is it Node MAJOR (v20.19.0 /
    /// v22.23.1 / v24.17.0 / v26.5.0 all abort on Darwin 25). What remains — a Darwin-25+ boundary
    /// or an unidentified runner factor — is not distinguishable with available hardware, and the
    /// handling is the same either way.
    ///
    /// ⛔⛔ THE ALTERNATIVE WAS NOT "SAFELY LEAVE IT RED". The failing step SKIPS every macOS
    /// conformance step behind it, so the platform had NO effective gate coverage and nothing in
    /// the output said so. A loud skip states the gap; a red job hid it. ⇒ This weakens no
    /// assertion: both survive unchanged and still fire wherever the precondition holds.
    fn stdio_abort_is_observable() -> bool {
        static OBSERVABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OBSERVABLE.get_or_init(|| {
            use std::os::unix::process::ExitStatusExt;
            let Some((node, _tmp, log_str, policy)) = stdio_fixture() else {
                return false;
            };
            let base = build_profile_with_stdio(&policy, &spec(), None, None, None, &[]);
            let dir = tempfile::Builder::new()
                .prefix("nub-stdio-probe-")
                .tempdir_in("/private/tmp")
                .unwrap();
            let path = dir.path().join("p.sb");
            std::fs::write(&path, &base).unwrap();
            let out = std::fs::File::create(&log_str).unwrap();
            let status = Command::new("/usr/bin/sandbox-exec")
                .arg("-f")
                .arg(&path)
                .arg(&node)
                .args(["-e", "0"])
                .stdin(std::process::Stdio::null())
                .stdout(out.try_clone().unwrap())
                .stderr(out)
                .status()
                .unwrap();
            status.signal() == Some(libc::SIGABRT)
        })
    }

    /// True when the caller may proceed. Otherwise it has already announced the skip on the real
    /// stderr, so a hollow run is legible to anyone skimming CI output — the same contract
    /// `skip_without_bwrap_with` provides on Linux, including the env-var escape hatch that turns
    /// the skip into a hard failure for a host that is supposed to be able to verify this.
    fn stdio_abort_precondition(test: &str) -> bool {
        if stdio_abort_is_observable() {
            return true;
        }
        assert!(
            std::env::var_os("NUB_SANDBOX_REQUIRE_STDIO_ABORT").is_none(),
            "NUB_SANDBOX_REQUIRE_STDIO_ABORT is set, but ungranted stdio does not abort Node on \
             this host, so {test} cannot verify anything here"
        );
        eprintln!(
            "NOT VERIFIABLE: {test} SKIPS — ungranted stdio does not abort Node on this host, so \
             its differential has no control and a pass here would prove nothing. Set \
             NUB_SANDBOX_REQUIRE_STDIO_ABORT=1 to make this a hard failure."
        );
        false
    }

    /// Seatbelt gates `fstat` on an already-open fd by its vnode, so a stdio descriptor whose
    /// path no grant covers makes `fstat(1)` return EPERM. Node's `PlatformInit` turns that into
    /// a bare `ABORT()` — exit 134 with a native stack trace and no message.
    ///
    /// The kernel-level differential: same profile builder, same child, the inherited-stdio path
    /// list the ONLY variable. `File::create` is `O_WRONLY`, which is what makes it bite — an
    /// `O_RDWR` stdio fd stats fine ungranted. The log deliberately sits inside `TmpMode::Private`'s
    /// shared-tmp deny, so a pass also proves the grant survives that deny in a real profile.
    #[test]
    fn an_inherited_stdio_grant_is_what_keeps_node_from_aborting() {
        use std::os::unix::process::ExitStatusExt;

        let Some((node, _tmp, log_str, policy)) = stdio_fixture() else {
            eprintln!("skipping: node not on PATH");
            return;
        };
        if !stdio_abort_precondition("an_inherited_stdio_grant_is_what_keeps_node_from_aborting") {
            return;
        }
        let base = build_profile_with_stdio(&policy, &spec(), None, None, None, &[]);
        let granted = build_profile_with_stdio(
            &policy,
            &spec(),
            None,
            None,
            None,
            std::slice::from_ref(&log_str),
        );

        let run = |profile: &str| {
            let dir = tempfile::Builder::new()
                .prefix("nub-stdio-prof-")
                .tempdir_in("/private/tmp")
                .unwrap();
            let path = dir.path().join("p.sb");
            std::fs::write(&path, profile).unwrap();
            let out = std::fs::File::create(&log_str).unwrap();
            Command::new("/usr/bin/sandbox-exec")
                .arg("-f")
                .arg(&path)
                .arg(&node)
                .args(["-e", "0"])
                .stdin(std::process::Stdio::null())
                .stdout(out.try_clone().unwrap())
                .stderr(out)
                .status()
                .unwrap()
        };

        // `sandbox-exec` execs Node in place, so the abort surfaces as a SIGABRT-terminated
        // status here; the 134 a user sees is aube mapping 128+signal.
        let control = run(&base);
        assert_eq!(
            control.signal(),
            Some(libc::SIGABRT),
            "control: with no stdio grant Node must still die on SIGABRT, else this test proves \
             nothing — got {control:?}"
        );
        let treated = run(&granted);
        assert!(
            treated.success(),
            "the inherited-stdio grant is what lets Node boot — got {treated:?}"
        );
    }

    /// A stdio path the POLICY denies is WITHHELD, never re-opened. A `file-read-metadata` allow
    /// beats a `file-read*` deny on the same path at ANY position (leaf outranks group — measured
    /// in both orders), and every emitted deny is `file-read*`, so this check — not the emit
    /// position — is what keeps the grant from punching a stat hole through the secret floor.
    #[test]
    fn a_policy_denied_stdio_path_is_withheld() {
        let Some((_node, _tmp, log_str, mut policy)) = stdio_fixture() else {
            eprintln!("skipping: node not on PATH");
            return;
        };
        policy
            .fs
            .rules
            .entries
            .push(rule("**/out.log", Effect::Deny, FsAccess::Read));
        let prof = build_profile_with_stdio(
            &policy,
            &spec(),
            None,
            None,
            None,
            std::slice::from_ref(&log_str),
        );
        assert!(
            !prof.contains(&format!("(literal \"{log_str}\")")),
            "a policy-denied stdio path must not be granted back"
        );
    }

    /// THE REACHABILITY VERDICT for the withhold branch above: it cannot fire under the BUILD
    /// JAIL, which is why the residual abort it leaves is a `nub sandbox` shape and not a
    /// lifecycle-script one. `preset::enforce_pure_allowlist` strips every deny from a
    /// build-jail policy and [`policy_denies`] reads only explicit `Effect::Deny` entries, so
    /// the grant is emitted whatever the child's stdio points at.
    ///
    /// The `nub sandbox` arm is the control, and it is what stops this passing hollow: the same
    /// paths through the same call ARE withheld there, so a regression that simply stopped
    /// withholding would fail here rather than sail through the build-jail half. The paths are
    /// chosen to sit on the generous-read secret floor for the same reason.
    #[test]
    fn the_build_jail_never_withholds_an_inherited_stdio_grant() {
        use crate::compiler::{CompileCtx, ScopeCapabilities, compile, compile_build_jail};
        use crate::matcher::Homes;
        use serde_json::json;
        use std::collections::BTreeMap;

        let homes = || Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        };
        let jail = compile_build_jail(
            homes(),
            Path::new("/proj/node_modules/somepkg"),
            None,
            None,
            vec![PathBuf::from("/testhome/.cache/nub/node/v26/bin/node")],
            Vec::new(),
            BTreeMap::new(),
        )
        .expect("build-jail compiles");
        let sandbox = compile(
            &json!(true),
            &CompileCtx::new(
                homes(),
                PathBuf::from("/proj"),
                ScopeCapabilities::approved(),
                BTreeMap::new(),
            ),
        )
        .expect("generous-read wrapper compiles");

        for target in ["/proj/.env.log", "/testhome/.ssh/out.log"] {
            assert!(
                !policy_denies(&jail, target),
                "the build jail must never withhold the stdio grant for {target} — it emits no \
                 denies, so a redirect there cannot abort the child"
            );
            assert!(
                policy_denies(&sandbox, target),
                "control: the generous-read wrapper must deny {target}, else the build-jail \
                 assertion above proves nothing"
            );
        }
    }

    /// The contract for what an inherited stdio descriptor earns, across the four shapes a child
    /// actually gets — one policy, one profile builder, the fd the child inherits the only input
    /// that moves.
    ///
    /// `/dev/null` and a pipe earn NOTHING and still boot: the base profile already covers all of
    /// `/dev` and a pipe has no vnode to name, so the grant set stays as small as the mechanism
    /// allows instead of blanket-covering stdio. `/dev/null` stands in for the terminal case and
    /// is the stronger probe of the two — opened `O_WRONLY` it is in the failing class, where a
    /// tty is `O_RDWR` and stats fine ungranted regardless.
    ///
    /// A redirect into a granted dir earns its grant and boots. A redirect into a policy-DENIED
    /// dir is withheld and Node still aborts — ACCEPTED, not fixed. Withholding is correct
    /// (granting would punch a stat-shaped hole through the secret floor); the resulting EPERM is
    /// handled fine by most programs (`/bin/echo`, `bash`, `python3` all succeed; `cat` fails
    /// cleanly with `stdout: Operation not permitted`); Node alone turns it into a bare `ABORT()`
    /// in `PlatformInit`, which nub cannot fix from outside. And it is unreachable under the build
    /// jail — see `the_build_jail_never_withholds_an_inherited_stdio_grant`.
    #[test]
    fn node_boots_under_every_stdio_shape_except_a_policy_denied_redirect() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::process::ExitStatusExt;

        let Some(node) = node_on_path() else {
            eprintln!("skipping: node not on PATH");
            return;
        };
        let dir = tempfile::Builder::new()
            .prefix("nub-stdio-shape-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join("denied")).unwrap();
        let policy = fs_policy(
            Effect::Deny,
            vec![
                rule(
                    &node.parent().unwrap().to_string_lossy(),
                    Effect::Allow,
                    FsAccess::Read,
                ),
                rule(
                    &format!("{}/denied/**", root.display()),
                    Effect::Deny,
                    FsAccess::Read,
                ),
            ],
        );

        // Derives the grant list from the fd the way a real spawn does, so "which shape" is the
        // only thing that varies between arms. Returns the emitted grant TEXT alongside the
        // status — "booted" and "booted for the stated reason" are different claims — recovered
        // by differencing against the same profile built with no stdio paths, which also pins
        // that `emit_stdio_grants` really is the last thing appended.
        let bare = build_profile_with_stdio(&policy, &spec(), None, None, None, &[]);
        let run = |fd: &OwnedFd| -> (std::process::ExitStatus, String) {
            let paths: Vec<String> = stdio_fd_path(fd.as_raw_fd()).into_iter().collect();
            let profile = build_profile_with_stdio(&policy, &spec(), None, None, None, &paths);
            let granted = profile
                .strip_prefix(bare.as_str())
                .expect("stdio grants are appended after everything else")
                .to_string();
            let prof_dir = tempfile::Builder::new()
                .prefix("nub-shape-prof-")
                .tempdir_in("/private/tmp")
                .unwrap();
            let path = prof_dir.path().join("p.sb");
            std::fs::write(&path, profile).unwrap();
            let status = Command::new("/usr/bin/sandbox-exec")
                .arg("-f")
                .arg(&path)
                .arg(&node)
                .args(["-e", "0"])
                .stdin(std::process::Stdio::null())
                .stdout(fd.try_clone().unwrap())
                .stderr(fd.try_clone().unwrap())
                .status()
                .unwrap();
            (status, granted)
        };

        // Both ends held: an abort trace (~1 KiB) fits the pipe buffer, so no drain is needed,
        // but dropping the read end would turn a failure into SIGPIPE and mask it.
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both descriptors come from a successful `pipe(2)` and are owned here.
        let (_pipe_r, pipe_w) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

        let ok_log = root.join("ok.log");
        for (shape, fd, expected) in [
            (
                "/dev/null",
                OwnedFd::from(std::fs::File::create("/dev/null").unwrap()),
                String::new(),
            ),
            ("pipe", pipe_w, String::new()),
            (
                "granted-dir redirect",
                OwnedFd::from(std::fs::File::create(&ok_log).unwrap()),
                format!(
                    "(allow file-read-metadata (literal \"{}\"))\n",
                    ok_log.display()
                ),
            ),
        ] {
            let (status, granted) = run(&fd);
            assert_eq!(
                granted, expected,
                "{shape}: a pass means nothing if the grant set is not the one claimed"
            );
            assert!(status.success(), "{shape}: Node must boot — got {status:?}");
        }

        let denied = OwnedFd::from(std::fs::File::create(root.join("denied/out.log")).unwrap());
        let (status, granted) = run(&denied);
        assert!(
            granted.is_empty(),
            "a policy-denied redirect target must earn no grant, got {granted:?}"
        );
        // ⛔ THE SECURITY PROPERTY IS THE ASSERT ABOVE, AND IT IS DELIBERATELY NOT GATED. What a
        // policy-denied path earns is decided by nub's own withhold branch, not by the OS, so it
        // is verifiable on every host and keeps gating here unconditionally — as does the
        // `granted == expected` check on each shape, which is this test's real content. Only the
        // SIGABRT below is a downstream CONSEQUENCE of the host's stat floor, and on a host that
        // does not abort it says nothing about nub. Gating the consequence rather than skipping
        // the test keeps the security half of this case live on macOS CI.
        if stdio_abort_precondition(
            "node_boots_under_every_stdio_shape_except_a_policy_denied_redirect",
        ) {
            assert_eq!(
                status.signal(),
                Some(libc::SIGABRT),
                "documented residual: with the grant withheld Node still aborts. If this now \
                 passes, check WHY before relaxing it — the likely cause is the withhold branch \
                 regressing into granting a policy-denied path, which reopens the stat floor"
            );
        }
    }

    // ── matcher translation ──────────────────────────────────────────────────

    #[test]
    fn whole_fs_globs_become_root_subpath() {
        // The generous-read `**` entry and its `/**`/`/` spellings all mean "all".
        assert_eq!(term_str("**"), "(subpath \"/\")");
        assert_eq!(term_str("/**"), "(subpath \"/\")");
        assert_eq!(term_str("/"), "(subpath \"/\")");
    }

    /// The node and its subtree twin are DIFFERENT terms, because the IR spells a subtree
    /// as the pair and therefore spells a node as the bare path alone. Emitting `(subpath)`
    /// for both is what let `projectCwd` grant a whole-project read and revoke every write
    /// under it; the pair together still covers the subtree, so nothing spelled the IR's
    /// way changed.
    #[test]
    fn a_bare_path_is_the_node_and_only_its_twin_is_the_subtree() {
        assert_eq!(term_str("/proj/data"), "(literal \"/proj/data\")");
        assert_eq!(term_str("/proj/data/**"), "(subpath \"/proj/data\")");
    }

    // ── brace alternation (the sandbox-glob-deny-fidelity fix) ────────────────

    #[test]
    fn brace_shapes_translate_to_alternation() {
        // Simple, nested, cartesian, single-element, brace+star, dir-level — the
        // exact shapes the fidelity audit flagged as silent leaks.
        assert_eq!(term_str("/p/{a,b}.key"), "(regex #\"^/p/(a|b)\\.key$\")");
        assert_eq!(
            term_str("/p/{a,{b,c}}.key"),
            "(regex #\"^/p/(a|(b|c))\\.key$\")"
        );
        assert_eq!(term_str("/p/{a,b}/{c,d}"), "(regex #\"^/p/(a|b)/(c|d)$\")");
        assert_eq!(term_str("/p/{a}.key"), "(regex #\"^/p/(a)\\.key$\")");
        assert_eq!(
            term_str("/p/{a,b}/*.key"),
            "(regex #\"^/p/(a|b)/[^/]*\\.key$\")"
        );
        assert_eq!(
            term_str("/p/{a,b}/x.key"),
            "(regex #\"^/p/(a|b)/x\\.key$\")"
        );
    }

    #[test]
    fn brace_empty_branches_are_dropped_like_globset() {
        // globset compiles with `empty_alternates=false`, so an empty branch VANISHES
        // (`{a,}` matches `a` only, never `a`-or-empty) and an all-empty group emits
        // nothing. A `(a|)`/`()` translation would over-match — the `{a,}` adversarial
        // case that would re-open the leak.
        assert_eq!(term_str("/p/{a,}.key"), "(regex #\"^/p/(a)\\.key$\")");
        assert_eq!(term_str("/p/{,a}.key"), "(regex #\"^/p/(a)\\.key$\")");
        assert_eq!(term_str("/p/{a,,b}.key"), "(regex #\"^/p/(a|b)\\.key$\")");
        // `{}` / `{,}` collapse to nothing — the group emits no regex, and (seeing a
        // brace at all set `saw_glob`) no subtree suffix is appended, so the pattern is
        // its exact literal remainder — matching globset's `^/p/x$` (not a subtree).
        assert_eq!(term_str("/p/{}x"), "(regex #\"^/p/x$\")");
        assert_eq!(term_str("/p/{,}x"), "(regex #\"^/p/x$\")");
    }

    #[test]
    fn brace_unbalanced_open_is_auto_closed_failsafe() {
        // globset hard-errors on `{a,b` (unclosed); the translator auto-closes so the
        // emitted regex stays valid and a deny keeps biting `a`/`b` rather than
        // producing a broken profile. A stray `}` is a literal.
        assert_eq!(term_str("/p/{a,b"), "(regex #\"^/p/(a|b)$\")");
        assert_eq!(term_str("/p/a}b*"), "(regex #\"^/p/a\\}b[^/]*$\")");
    }

    /// The globset ORACLE: nub's userspace/Linux fs matcher IS globset, so the macOS
    /// Seatbelt regex must accept EXACTLY the paths globset accepts for the same glob.
    /// A translation bug re-creates the silent leak, so this cross-checks the emitted
    /// regex against globset over a shared candidate pool. Case-sensitive on both sides
    /// isolates brace/glob STRUCTURE (case-folding is a separate, already-refuted axis).
    #[test]
    fn brace_regex_matches_globset_oracle() {
        use globset::GlobBuilder;
        use regex::Regex;

        let globs = [
            "/p/{a,b}.key",
            "/p/{a,{b,c}}.key",
            "/p/{a,{b,{c,d}}}.k",
            "/p/{a,b}/{c,d}",
            "/p/{a}.key",
            "/p/{a,}.key",
            "/p/{,a}.key",
            "/p/{a,,b}.key",
            "/p/{a,b}/*.key",
            "/p/{a,[bc]}.k",
            "/p/pre{a,b}post",
            "/p/{a,b}/**",
            // Empty-brace edges cross-checked against real globset (not just the
            // reasoning-asserts): the group emits nothing, so the pattern is its
            // literal remainder.
            "/p/{}x",
            "/p/{,}x",
            "/p/pre{}post",
            // `**`-in-brace shapes — the over-grant closed after the #411 review. A
            // non-component `**` (`{**,x}`, `pre{**,x}post`, `a**b`) must NOT cross `/`;
            // a component `**` (`{**/x,y}`, `{a/**,b}`) stays recursive. globset is the
            // oracle for every one.
            "/p/{**/*.k,x}",
            "/p/{**/a.k,x}",
            "/p/{a,**/b}",
            "/p/pre{**,x}post",
            "/p/{**,*}",
            "/p/{a,{**,b}}",
            "/p/{**}",
            "/p/{a/**,b}",
            "/p/a**b",
            "/p/{a**b,c}",
            "/p/foo**/bar",
            "/p/a**/b",
            "/p/**bar",
            "/p/bar**",
            // Consecutive-`**` collapse chains (longer than the generative 3-token space):
            // globset folds adjacent recursive components into one, so these must match
            // globset exactly — the over-grant closed here was `(.*/)?.*`-matches-all.
            "**/**",
            "**/**/x.k",
            "/p/**/**/a.k",
            "/p/a/**/**/b",
            "{**/**/x,y}",
            "{a/**/**,b}",
        ];
        // A pool that exercises match + non-match for every glob above, including the
        // literal-brace spelling (must NOT match — the leak was matching only that).
        let candidates = [
            "/p/a.key",
            "/p/b.key",
            "/p/c.key",
            "/p/d.key",
            "/p/.key",
            "/p/a/c",
            "/p/a/d",
            "/p/b/c",
            "/p/b/e",
            "/p/a/x.key",
            "/p/b/y.key",
            "/p/c/x.key",
            "/p/a/x.pem",
            "/p/a.k",
            "/p/b.k",
            "/p/c.k",
            "/p/preapost",
            "/p/prebpost",
            "/p/{a,b}.key",
            "/p/a/deep/nested/file",
            "/p/b/deep",
            "/p/x",
            "/p/prepost",
            "/p/x/sub",
            // dir-crossing candidates — these separate a recursive `**` (matches) from a
            // degraded single-component `**` (must NOT match across `/`).
            "/p/deep/a.k",
            "/p/deep/nested/a.k",
            "/p/deep/x.k",
            "/p/deep/b",
            "/p/pre/deep/post",
            "/p/predeeppost",
            "/p/a/deep/thing",
            "/p/deep/thing",
            "/p/a/x.k",
            "/p/anything",
            "/p/a**b",
            "/p/aXXb",
            "/p/aX/Yb",
            "/p/ab",
            "/p/c",
            "/p/bar",
            "/p/barXX",
            "/p/bar/deep",
            "/p/XXbar",
            "/p/X/Ybar",
        ];
        for g in globs {
            let emitted = super::glob_to_seatbelt_regex(g);
            let re = Regex::new(&emitted)
                .unwrap_or_else(|e| panic!("emitted regex for `{g}` is invalid: {e}\n{emitted}"));
            let gs = GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .unwrap_or_else(|e| panic!("globset rejected `{g}`: {e}"))
                .compile_matcher();
            for c in candidates {
                assert_eq!(
                    re.is_match(c),
                    gs.is_match(c),
                    "DIVERGENCE glob=`{g}` candidate=`{c}` emitted=`{emitted}` \
                     (seatbelt={}, globset={})",
                    re.is_match(c),
                    gs.is_match(c),
                );
            }
        }
    }

    /// EXHAUSTIVE `**`-fidelity oracle: enumerate every 2-and-3-token glob over an
    /// alphabet that mixes `**` with the boundaries that flip its meaning (`/`, a
    /// literal, a brace open/branch/close), wrap each in a top-level and a braced
    /// frame, and cross-check the emitted Seatbelt regex against globset over a
    /// dir-depth-varied candidate pool. The invariant PROVEN: for EVERY compilable
    /// shape the Seatbelt match set EQUALS the globset set — never a superset (the
    /// over-grant) and never a subset (an under-enforcement). globset-rejected shapes
    /// (unbalanced braces) are skipped; the auto-close fail-safe is covered above.
    #[test]
    fn starstar_fidelity_exhaustive_oracle() {
        use globset::GlobBuilder;
        use regex::Regex;

        // Tokens whose adjacency to `**` decides recursive-vs-single-component (`?` and a
        // literal are in here so a `**` neighbored by a single-char glob or text is
        // covered too).
        let toks = ["**", "*", "?", "a", "/", "x.k", "{", "}", ",", "b"];
        let candidates = [
            "a",
            "b",
            "x.k",
            "a.k",
            "ab",
            "aXb",
            "a/b",
            "a/x.k",
            "deep/a.k",
            "a/deep/b",
            "x/y/z",
            "a/b/c/d",
            "pre/mid/post",
            "",
            "a/",
            "/a",
            "a.k/b",
            "deep/nested/x.k",
            "abc",
            "a/x/y.k",
            // exercise the `p…q` / `p/…/q` literal frames and `?`
            "pq",
            "paq",
            "pXq",
            "pabq",
            "pa/bq",
            "p/a/q",
            "p/x.k/q",
            "p/deep/nested/q",
            "p//q",
            "p/q",
        ];
        // Frames: raw (top-level), braced (forces the in_brace path), and pre/post
        // literals (a `**` glued to surrounding text, where the boundary rule differs).
        let frames: [&dyn Fn(&str) -> String; 4] = [
            &|s: &str| s.to_string(),
            &|s: &str| format!("{{{s},zz}}"),
            &|s: &str| format!("p{s}q"),
            &|s: &str| format!("p/{s}/q"),
        ];

        let mut checked = 0usize;
        // 2-, 3-, and 4-token bodies; every body must contain at least one `**`.
        let mut bodies: Vec<String> = Vec::new();
        for a in toks {
            for b in toks {
                bodies.push(format!("{a}{b}"));
                for c in toks {
                    bodies.push(format!("{a}{b}{c}"));
                    for d in toks {
                        bodies.push(format!("{a}{b}{c}{d}"));
                    }
                }
            }
        }
        for body in &bodies {
            if !body.contains("**") {
                continue;
            }
            for frame in frames {
                let g = frame(body);
                let Ok(glob) = GlobBuilder::new(&g).literal_separator(true).build() else {
                    continue; // globset rejected (e.g. unbalanced brace) — skip.
                };
                let gs = glob.compile_matcher();
                let emitted = super::glob_to_seatbelt_regex(&g);
                let Ok(re) = Regex::new(&emitted) else {
                    panic!("emitted regex invalid for `{g}`: {emitted}");
                };
                for c in candidates {
                    assert_eq!(
                        re.is_match(c),
                        gs.is_match(c),
                        "DIVERGENCE glob=`{g}` candidate=`{c}` emitted=`{emitted}` \
                         gsregex=`{}` (seatbelt={}, globset={})",
                        glob.regex(),
                        re.is_match(c),
                        gs.is_match(c),
                    );
                    checked += 1;
                }
            }
        }
        assert!(
            checked > 5_000,
            "oracle coverage too thin: {checked} checks"
        );
    }

    #[test]
    fn embedded_globs_become_anchored_regex() {
        // The depth-independent `.env` denies (the security-critical case).
        assert_eq!(term_str("**/.env"), "(regex #\"^(.*/)?\\.env$\")");
        assert_eq!(term_str("**/.env.*"), "(regex #\"^(.*/)?\\.env\\.[^/]*$\")");
        // A single-component glob stays within one path segment.
        assert_eq!(term_str("/proj/*.pem"), "(regex #\"^/proj/[^/]*\\.pem$\")");
        // A mid-path single `*` does not cross a separator.
        assert_eq!(
            term_str("/proj/packages/*/.env"),
            "(regex #\"^/proj/packages/[^/]*/\\.env$\")"
        );
    }

    // ── profile shape ────────────────────────────────────────────────────────

    #[test]
    fn read_generous_emits_root_allow_then_secret_deny() {
        // `sandbox: true`-shaped: a `**` allow (generous) then a `.env` deny.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                rule("**/.env", Effect::Deny, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(prof.contains("(allow file-read* (subpath \"/\"))"));
        // The `.env` deny is emitted AFTER the generous allow (last-match-wins).
        let allow_at = prof.find("(allow file-read* (subpath \"/\"))").unwrap();
        let deny_at = prof
            .find("(deny file-read* (regex #\"^(.*/)?\\.env$\"))")
            .unwrap();
        assert!(
            deny_at > allow_at,
            "the .env deny must follow the generous allow"
        );
    }

    #[test]
    fn read_confine_has_no_global_read_allow() {
        // default_effect Deny + explicit project allow = read-confine; unmatched
        // paths fall through to the base `(deny default)`, not a global read allow.
        //
        // The grant is spelled as the compiler's subtree PAIR. Hand-writing only `/proj`
        // names the directory NODE, which is a different rule and emits `(literal …)`.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("/proj", Effect::Allow, FsAccess::ReadWrite),
                rule("/proj/**", Effect::Allow, FsAccess::ReadWrite),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(!prof.contains("(allow file-read* (subpath \"/\"))\n"));
        assert!(prof.contains("(allow file-read* (subpath \"/proj\"))"));
    }

    /// The write axis in one table: rw Allow → allow, Deny → deny, read-only Allow →
    /// NOTHING. The base denies writes, so only an rw Allow ever opens one.
    ///
    /// Both spellings of a read-only allow are asserted — the subtree PAIR and the bare
    /// NODE — because the two once rendered differently and each broke the same way. A
    /// synthesized deny has nothing to cap (the write base is `(deny default)`) and can
    /// only cancel another grant, which is what it did to `siblingDirs` and `package_dir`.
    #[test]
    fn write_axis_allows_only_readwrite_and_denies_only_on_deny() {
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("/proj", Effect::Allow, FsAccess::ReadWrite),
                rule("/proj/**", Effect::Allow, FsAccess::ReadWrite),
                rule("/proj/ro", Effect::Allow, FsAccess::Read),
                rule("/proj/ro/**", Effect::Allow, FsAccess::Read),
                rule("/proj/cwd", Effect::Allow, FsAccess::Read),
                rule("/proj/secret", Effect::Deny, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(prof.contains("(allow file-write* (subpath \"/proj\"))"));
        assert!(prof.contains("(deny file-write* (literal \"/proj/secret\"))"));
        for term in [
            "(subpath \"/proj/ro\")",
            "(literal \"/proj/ro\")",
            "(subpath \"/proj/cwd\")",
            "(literal \"/proj/cwd\")",
        ] {
            assert!(
                !prof.contains(&format!("(deny file-write* {term}")),
                "a read-only allow must emit no write deny, got one for {term}:\n{prof}"
            );
        }
    }

    /// The BUG-H regression, at the profile level: an rw grant and, AFTER it, a read-only
    /// grant that ENCLOSES it — the exact order `curated::grant_from_table` appends
    /// (`sibling_dirs` rw, then `project_reads` r). The enclosing read must leave the write
    /// allow as the last word on the nested path.
    ///
    /// Both directions are pinned. Dropping the write-deny is not allowed to turn the read
    /// grant into a write grant: `/proj/node_modules` itself must gain no `file-write*`
    /// allow, so the only writable thing under it is the path an rw rule actually named.
    #[test]
    fn a_read_grant_does_not_revoke_a_write_grant_it_encloses() {
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule(
                    "/proj/node_modules/.prisma",
                    Effect::Allow,
                    FsAccess::ReadWrite,
                ),
                rule(
                    "/proj/node_modules/.prisma/**",
                    Effect::Allow,
                    FsAccess::ReadWrite,
                ),
                rule("/proj/node_modules", Effect::Allow, FsAccess::Read),
                rule("/proj/node_modules/**", Effect::Allow, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(
            prof.contains("(allow file-write* (subpath \"/proj/node_modules/.prisma\"))"),
            "the enclosed write grant must be emitted:\n{prof}"
        );
        assert!(
            !prof.contains("(deny file-write* (subpath \"/proj/node_modules\"))"),
            "the enclosing read grant must not emit a write deny over it:\n{prof}"
        );
        assert!(
            prof.contains("(allow file-read* (subpath \"/proj/node_modules\"))"),
            "the read grant itself must survive:\n{prof}"
        );
        assert!(
            !prof.contains("(allow file-write* (subpath \"/proj/node_modules\"))"),
            "and must not become a write grant:\n{prof}"
        );
    }

    #[test]
    fn confstr_scratch_write_follows_a_policy_write_deny() {
        // The C1 regression: the Apple toolchain's xcrun_db write is silently denied unless
        // the confstr grant is the LAST word on the temp dir. A policy deny that covers the
        // DARWIN scratch root is the only thing that can now precede it on that path (a
        // read-only allow emits no write deny at all), so that is what this drives.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                rule("/private/var/folders/**", Effect::Deny, FsAccess::Read),
                rule("/proj", Effect::Allow, FsAccess::ReadWrite),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        let deny = prof
            .find("(deny file-write* (subpath \"/private/var/folders\"))")
            .unwrap();
        let confstr = prof
            .find("(allow file-write* (subpath \"/private/var/folders/")
            .unwrap();
        assert!(confstr > deny, "confstr grant must follow the policy deny");
    }

    #[test]
    fn move_block_reasserts_deny_after_confstr_grant() {
        // Hole #1: a `.env` deny under a generous-read policy is capped by
        // `(deny file-write* <.env>)`, but the trailing confstr grant re-opens write for a
        // `$TMPDIR`-resident secret (last-match-wins). The move-block re-emits the
        // unlink/create denies AFTER the confstr grant so the deny wins the race.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                rule("**/.env", Effect::Deny, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        let confstr = prof
            .find("(allow file-write* (subpath \"/private/var/folders/")
            .expect("confstr temp grant present");
        let unlink = prof
            .find("(deny file-write-unlink (regex #\"^(.*/)?\\.env$\"))")
            .expect("re-asserted unlink deny present");
        let create = prof
            .find("(deny file-write-create (regex #\"^(.*/)?\\.env$\"))")
            .expect("re-asserted create deny present");
        assert!(
            unlink > confstr && create > confstr,
            "move-block denies must follow the confstr grant to win last-match-wins"
        );
    }

    #[test]
    fn move_block_reasserts_only_deny_entries() {
        // The move block runs AFTER the confstr grant, so anything it re-asserts outranks
        // the xcrun_db scratch write. Only the Deny arm may be re-emitted: a read-only
        // Allow contributes nothing to the write axis, and it must contribute nothing here
        // either — re-asserting a generous `**` read as unlink/create denies would
        // blanket-block the temp dir. This is the move-block half of "an Allow never
        // subtracts", asserted for both a whole-fs read and an enclosing subtree read.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                rule("/proj", Effect::Allow, FsAccess::ReadWrite),
                rule("/proj/**", Effect::Allow, FsAccess::ReadWrite),
                rule("/proj/ro", Effect::Allow, FsAccess::Read),
                rule("/proj/ro/**", Effect::Allow, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        for op in ["file-write-unlink", "file-write-create"] {
            for term in ["(subpath \"/\")", "(subpath \"/proj/ro\")"] {
                assert!(
                    !prof.contains(&format!("(deny {op} {term})")),
                    "the move block must re-assert no Allow, got {op} {term}:\n{prof}"
                );
            }
        }
        // And the confstr grant is still the last word on the temp dir.
        assert!(prof.contains("(allow file-write* (subpath \"/private/var/folders/"));
    }

    #[test]
    fn move_block_denies_ancestor_dirs_for_anchored_deny() {
        // Hole #2: a literal deny `/root/proj/.env` blocks the file mv but not
        // `mv proj proj2`. The ancestor move-block denies unlink/create on `/root/proj`
        // and `/root` (up to the rw-grant root), so no container rename relocates it.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                // The rw grant root is the compiler's subtree PAIR; the bare node alone is a
                // different rule and bounds no writable container for the ancestor walk.
                rule("/root", Effect::Allow, FsAccess::ReadWrite),
                rule("/root/**", Effect::Allow, FsAccess::ReadWrite),
                rule("/root/proj/.env", Effect::Deny, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(prof.contains("(deny file-write-unlink (literal \"/root/proj\"))"));
        assert!(prof.contains("(deny file-write-create (literal \"/root/proj\"))"));
        assert!(prof.contains("(deny file-write-unlink (literal \"/root\"))"));
        // The grant root is the stopping point — nothing above it (writable region ends).
        assert!(!prof.contains("(deny file-write-unlink (literal \"/\"))"));
    }

    #[test]
    fn move_block_skips_basename_glob_deny_ancestors() {
        // A basename-glob deny (`**/.env`) has no literal ancestor and is already immune to
        // ancestor rename (the basename survives), so Fix 2 emits no `(literal …)` ancestor
        // denies for it — only the Fix 1 regex re-assertion.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                // The rw grant root is the compiler's subtree PAIR; the bare node alone is a
                // different rule and bounds no writable container for the ancestor walk.
                rule("/root", Effect::Allow, FsAccess::ReadWrite),
                rule("/root/**", Effect::Allow, FsAccess::ReadWrite),
                rule("**/.env", Effect::Deny, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(!prof.contains("(deny file-write-unlink (literal \"/root\"))"));
    }

    #[test]
    fn move_block_pins_regex_dir_prefix_ancestors() {
        // A user directory-pinning glob deny (`!secrets/*.key` → `/root/secrets/*.key`) is a
        // regex, so Fix 1 blocks the leaf `*.key` files but NOT their container `/root/secrets`
        // — `mv secrets secretz` would relocate them past the deny. Fix 2 pins the literal
        // prefix dir `/root/secrets` AND its ancestors up to the rw-grant root.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                // The rw grant root is the compiler's subtree PAIR; the bare node alone is a
                // different rule and bounds no writable container for the ancestor walk.
                rule("/root", Effect::Allow, FsAccess::ReadWrite),
                rule("/root/**", Effect::Allow, FsAccess::ReadWrite),
                rule("/root/secrets/*.key", Effect::Deny, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(prof.contains("(deny file-write-unlink (literal \"/root/secrets\"))"));
        assert!(prof.contains("(deny file-write-create (literal \"/root/secrets\"))"));
        assert!(prof.contains("(deny file-write-unlink (literal \"/root\"))"));
        // EXACT-path, never a subpath — a legit write UNDER secrets/ stays permitted.
        assert!(!prof.contains("(literal \"/root/secrets/"));
        // The grant root is the stopping point — nothing above it.
        assert!(!prof.contains("(deny file-write-unlink (literal \"/\"))"));
    }

    #[test]
    fn regex_literal_dir_prefix_extracts_leading_literal_run() {
        // The leading glob-free component run, dropping the glob leaf/segment.
        assert_eq!(
            regex_literal_dir_prefix("/root/secrets/*.key").as_deref(),
            Some("/root/secrets")
        );
        assert_eq!(
            regex_literal_dir_prefix("/root/packages/*/.env").as_deref(),
            Some("/root/packages")
        );
        // No fixed anchor: a leading `**` (basename/floating glob) or a first-segment glob.
        assert_eq!(regex_literal_dir_prefix("**/.env"), None);
        assert_eq!(regex_literal_dir_prefix("/*.key"), None);
    }

    #[test]
    fn move_block_no_regex_pin_without_literal_prefix() {
        // A floating-name deny (`**/secrets/**`) has no absolute literal prefix to anchor, so
        // Fix 2 emits no `(literal …)` ancestor denies for it — the documented residual.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                // The rw grant root is the compiler's subtree PAIR; the bare node alone is a
                // different rule and bounds no writable container for the ancestor walk.
                rule("/root", Effect::Allow, FsAccess::ReadWrite),
                rule("/root/**", Effect::Allow, FsAccess::ReadWrite),
                rule("**/secrets/**", Effect::Deny, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(!prof.contains("(deny file-write-unlink (literal \"/root\"))"));
    }

    #[test]
    fn move_block_no_ancestors_without_enclosing_write_grant() {
        // An anchored deny with NO write grant enclosing it (read-only project) has no
        // writable container to rename — emit no ancestor denies.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("**", Effect::Allow, FsAccess::Read),
                rule("/root/proj/.env", Effect::Deny, FsAccess::Read),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(!prof.contains("(deny file-write-unlink (literal \"/root/proj\"))"));
        assert!(!prof.contains("(deny file-write-unlink (literal \"/root\"))"));
    }

    #[test]
    fn confstr_grants_temp_not_cache() {
        // Only the DARWIN TEMP dir is granted; the persistent CACHE dir (…/C) is a
        // cross-build poisoning surface and must NOT be write-granted.
        let p = fs_policy(
            Effect::Deny,
            vec![rule("/proj", Effect::Allow, FsAccess::ReadWrite)],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        if let Some(cache) = confstr_dir(libc::_CS_DARWIN_USER_CACHE_DIR) {
            let cache =
                normalize_slashes(&canonicalize_including_nonexistent(&cache).to_string_lossy());
            assert!(
                !prof.contains(&format!("(allow file-write* (subpath \"{cache}\"))")),
                "the DARWIN cache dir must not be write-granted"
            );
        }
    }

    #[test]
    fn private_tmp_carves_the_confstr_compiler_scratch_but_hides_private_tmp() {
        // $tmp:rw (Private) hides the world-shared /private/tmp but KEEPS the confstr TEMP
        // scratch (the Apple toolchain's fixed xcrun_db cache) granted so native builds work
        // — the doc's "granting $tmp also grants Apple's fixed compiler-cache directory".
        let mut p = fs_policy(
            Effect::Deny,
            vec![rule("/proj", Effect::Allow, FsAccess::ReadWrite)],
        );
        p.fs.tmp = TmpMode::Private;
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(
            prof.contains("(deny file-read* (subpath \"/private/tmp\"))"),
            "Private must hide the world-shared /private/tmp"
        );
        // The confstr scratch is HIDDEN like the rest of the shared tmp. `$TMPDIR` is a
        // long-lived per-user directory holding every application's scratch state, so
        // granting the whole subpath handed a lifecycle script read+write over all of it —
        // far broader than "a private per-run dir plus Apple's fixed compiler cache".
        for dir in confstr_scratch_dirs() {
            assert!(
                prof.contains(&format!("(deny file-read* (subpath \"{dir}\"))")),
                "Private must hide the confstr scratch, not grant it wholesale"
            );
            assert!(
                prof.contains(&format!("(deny file-write* (subpath \"{dir}\"))")),
                "...for writes as well as reads"
            );
        }
        // Exactly ONE thing is granted back, and as a FILE: `xcrun_db`, which xcrun resolves
        // via confstr rather than $TMPDIR and so cannot be redirected into the per-run dir.
        // Each grant must sit in the SAME operation node as the deny it re-opens and AFTER it
        // — a general `(allow file* …)` loses to a specific `file-write*` deny at any position.
        for file in darwin_compiler_cache_files() {
            let dir = file.trim_end_matches("/xcrun_db");
            // The grant is the name PLUS its mkstemp staging siblings — the toolchain only
            // ever writes `xcrun_db-XXXXXX` and renames, so asserting the bare literal here
            // is what let a completely inert carve-out look correct for as long as it did.
            let term = emit_term(&to_match_term(&format!("{file}*")));
            for op in ["file-read*", "file-write*"] {
                let grant = format!("(allow {op} {term})");
                assert!(
                    prof.contains(&grant),
                    "the compiler-cache carve-out must be granted back per-op: {grant}"
                );
                assert!(
                    prof.find(&grant) > prof.find(&format!("(deny {op} (subpath \"{dir}\"))")),
                    "the {op} carve-out grant must follow the {op} deny it re-opens"
                );
            }
        }
    }

    /// The per-run private tmp dir must be granted back PER OPERATION NODE. Measured
    /// 2026-07-28: `(allow file* X)` does not override `(deny file-write* <parent>)` at any
    /// position, and `make_private_tmp` puts the per-run dir under the confstr scratch this
    /// backend denies — so a lone `file*` grant left `os.tmpdir()` unwritable (EPERM).
    #[test]
    fn private_tmp_dir_is_granted_read_and_write_in_their_own_nodes() {
        let mut p = fs_policy(Effect::Allow, vec![]);
        p.fs.tmp = crate::policy::TmpMode::Private;
        let dir = std::path::Path::new("/private/tmp/nub-tmp-unit-fixture");
        let prof = build_profile(&p, &spec(), None, None, Some(dir));
        let term = format!("(subpath \"{}\")", dir.display());
        for op in ["file-read*", "file-write*"] {
            let grant = format!("(allow {op} {term})");
            assert!(
                prof.contains(&grant),
                "per-run tmp dir must carry its own {op} grant, else the shared-tmp \
                 {op} deny wins and the child's own tmp is unusable; profile:\n{prof}"
            );
        }
        for dir in confstr_scratch_dirs() {
            assert!(
                prof.find(&format!("(allow file-write* {term})"))
                    > prof.find(&format!("(deny file-write* (subpath \"{dir}\"))")),
                "the tmp-dir write grant must follow the shared-tmp write deny"
            );
        }
    }

    /// The tmp re-grant must not out-rank the policy's own denies. It is emitted after
    /// `emit_fs` (so it can override a generous base read), which means replaying the same
    /// rule SET is NOT order-neutral — without a deny replay behind it, a tmp-resident grant
    /// re-opens `$TMPDIR/<grant>/.env` on any policy still carrying the secret floor.
    #[test]
    fn the_tmp_regrant_does_not_reopen_a_policy_deny() {
        let Some(scratch) = confstr_scratch_dirs().into_iter().next() else {
            return;
        };
        let work = format!("{scratch}/work");
        let mut p = fs_policy(
            Effect::Deny,
            vec![
                rule(&work, Effect::Allow, FsAccess::ReadWrite),
                rule(&format!("{work}/**"), Effect::Allow, FsAccess::ReadWrite),
                rule("**/.env*", Effect::Deny, FsAccess::Read),
            ],
        );
        p.fs.tmp = TmpMode::Private;
        let prof = build_profile(&p, &spec(), None, None, None);
        let regrant = prof
            .rfind(&format!("(allow file-read* (subpath \"{work}\"))"))
            .expect("the tmp-resident grant must be re-opened");
        let deny = prof
            .rfind("(deny file-read* (regex")
            .expect("the .env floor must still be emitted");
        assert!(
            deny > regrant,
            "the deny replay must follow the tmp re-grant, or $TMPDIR/work/.env reopens"
        );
    }

    /// The re-grant's write arm must apply the same dangerous-root guard `emit_fs` does,
    /// or a `/private/tmp` rw grant becomes a filesystem-wide write hole under Private.
    #[test]
    fn the_tmp_regrant_still_refuses_a_dangerous_write_root() {
        let mut p = fs_policy(
            Effect::Deny,
            vec![
                rule("/private/tmp", Effect::Allow, FsAccess::ReadWrite),
                rule("/private/tmp/**", Effect::Allow, FsAccess::ReadWrite),
            ],
        );
        p.fs.tmp = TmpMode::Private;
        let private = build_profile(&p, &spec(), None, None, None);
        // The DIFFERENTIAL is the point: `Shared` skips `emit_tmp` entirely, so whatever it
        // emits is `emit_fs`'s pre-existing behavior. The re-grant must not add a write that
        // `emit_fs` did not already allow — this pins the re-grant specifically, not the
        // dangerous-root policy in general (`/private/tmp` is deliberately NOT on that list).
        p.fs.tmp = TmpMode::Shared;
        let shared = build_profile(&p, &spec(), None, None, None);
        let w = "(allow file-write* (subpath \"/private/tmp\"))";
        assert_eq!(
            private.matches(w).count(),
            shared.matches(w).count(),
            "the tmp re-grant must not add a write grant emit_fs did not already emit"
        );
    }

    #[test]
    fn deny_tmp_hides_the_confstr_scratch_too() {
        // $tmp:false (Deny) = no tmp at all: BOTH /private/tmp AND the confstr scratch hidden.
        let mut p = fs_policy(
            Effect::Deny,
            vec![rule("/proj", Effect::Allow, FsAccess::ReadWrite)],
        );
        p.fs.tmp = TmpMode::Deny;
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(prof.contains("(deny file-read* (subpath \"/private/tmp\"))"));
        for dir in confstr_scratch_dirs() {
            assert!(
                prof.contains(&format!("(deny file-read* (subpath \"{dir}\"))")),
                "Deny must hide the confstr scratch too (no carve-out)"
            );
        }
    }

    #[test]
    fn dangerous_write_roots_are_dropped() {
        // A `..`-collapsed grant that resolves to a top-level root must not emit a
        // write allow (filesystem-wide write hole). Read of `/` stays legal.
        let p = fs_policy(
            Effect::Deny,
            vec![
                rule("/private", Effect::Allow, FsAccess::ReadWrite),
                rule("/private/**", Effect::Allow, FsAccess::ReadWrite),
            ],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(!prof.contains("(allow file-write* (subpath \"/private\"))"));
        // The pair's node half too: a `(literal)` write grant on a top-level root permits
        // renaming or unlinking the root itself, so both halves of the pair must be dropped.
        assert!(!prof.contains("(allow file-write* (literal \"/private\"))"));
        assert!(is_dangerous_write_root(&MatchTerm::Subpath(
            "/private".to_string()
        )));
        assert!(is_dangerous_write_root(&MatchTerm::Literal(
            "/private".to_string()
        )));
        // The canonical forms of firmlink roots (`/var`→`/private/var`) — what the
        // guard actually sees after the matcher's canonicalization — must be caught.
        assert!(is_dangerous_write_root(&MatchTerm::Subpath(
            "/private/var".to_string()
        )));
        assert!(is_dangerous_write_root(&MatchTerm::Subpath(
            "/private/etc".to_string()
        )));
        assert!(is_dangerous_write_root(&MatchTerm::Subpath(
            "/Volumes".to_string()
        )));
        // A real project dir under a guarded root is NOT over-blocked (exact match).
        assert!(!is_dangerous_write_root(&MatchTerm::Subpath(
            "/proj".to_string()
        )));
        assert!(!is_dangerous_write_root(&MatchTerm::Subpath(
            "/Users/me/proj".to_string()
        )));
        assert!(!is_dangerous_write_root(&MatchTerm::Subpath(
            "/private/tmp/scratch".to_string()
        )));
    }

    #[test]
    fn relaxed_fs_grants_all_file_ops() {
        // default Allow + no entries = relaxed; wrapped only because net enforces.
        let mut p = fs_policy(Effect::Allow, vec![]);
        p.net = NetPolicy {
            enforce: true,
            rules: vec![],
            default_effect: Effect::Deny,
            ..Default::default()
        };
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(prof.contains("(allow file*)"));
    }

    #[test]
    fn net_enforced_with_proxy_carves_only_the_proxy_port() {
        // A proxy on port 54321: egress permitted to EXACTLY localhost:54321, nothing
        // else — no blanket allow, and critically NOT all-loopback (`localhost:*`), so
        // a sibling listener / docker-on-loopback stays denied (local-exfil closed).
        let mut p = fs_policy(Effect::Allow, vec![]);
        p.net = NetPolicy {
            enforce: true,
            rules: vec![],
            default_effect: Effect::Deny,
            ..Default::default()
        };
        let prof = build_profile(&p, &spec(), Some(54321), None, None);
        assert!(prof.contains("(allow network* (remote ip \"localhost:54321\"))"));
        assert!(
            !prof.contains("localhost:*"),
            "must not carve all of loopback"
        );
        assert!(!prof.contains("(allow network*)\n"), "no blanket egress");
    }

    #[test]
    fn net_enforced_coarse_deny_carves_nothing() {
        // Coarse deny-all (net enforce, no proxy): NO network allow at all — the base
        // (deny default) closes every egress incl. loopback + AF_UNIX.
        let mut p = fs_policy(Effect::Allow, vec![]);
        p.net = NetPolicy {
            enforce: true,
            rules: vec![],
            default_effect: Effect::Deny,
            ..Default::default()
        };
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(
            !prof.contains("(allow network*"),
            "coarse deny emits no egress carve"
        );
    }

    #[test]
    fn net_not_enforced_allows_all_plus_services() {
        // fs confines (so we wrap) but net is relaxed → full egress + DNS/TLS block.
        let p = fs_policy(
            Effect::Deny,
            vec![rule("/proj", Effect::Allow, FsAccess::ReadWrite)],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(prof.contains("(allow network*)\n"));
        assert!(prof.contains("com.apple.trustd"));
    }

    #[test]
    fn degradation_reports_lost_per_host_only_without_proxy() {
        let mut p = fs_policy(Effect::Allow, vec![]);
        p.net = NetPolicy {
            enforce: true,
            rules: vec![NetRule {
                target: NetTarget::Host("example.com".to_string()),
                effect: Effect::Allow,
            }],
            default_effect: Effect::Deny,
            ..Default::default()
        };
        // No proxy available → per-host can't be enforced → degraded.
        let deg = degradation(&p, None, None);
        assert_eq!(deg.lost, vec!["net-per-host".to_string()]);
        // WITH a proxy the per-host allows ARE enforced (SNI/target gating) → full.
        assert!(degradation(&p, Some(9999), None).is_full());
        // A pure deny-all net (no allow rules) is fully enforced, not degraded.
        p.net.rules.clear();
        assert!(degradation(&p, None, None).is_full());
    }

    #[test]
    fn bare_program_grant_uses_the_constructed_child_path() {
        let root = tempfile::tempdir().unwrap();
        let child_bin = root.path().join("child-bin");
        std::fs::create_dir(&child_bin).unwrap();
        let program = child_bin.join("tool");
        std::fs::write(&program, b"tool").unwrap();

        let child_path = child_bin.to_string_lossy().into_owned();
        let term = program_read_term(&CommandSpec::new("tool"), Some(&child_path))
            .expect("constructed PATH resolves the entry program");
        assert!(
            term.contains(&program.to_string_lossy().replace('\\', "\\\\")),
            "program grant must name the child-PATH executable: {term}"
        );
    }

    #[test]
    fn path_canonicalization_resolves_absolutes_and_leaves_the_rest_alone() {
        let root = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(root.path()).unwrap().join("real");
        let link = real.parent().unwrap().join("link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let colon_target = real.parent().unwrap().join("we:ird");
        let colon_link = real.parent().unwrap().join("clink");
        std::fs::create_dir_all(&colon_target).unwrap();
        std::os::unix::fs::symlink(&colon_target, &colon_link).unwrap();
        let p = |s: &str| canonicalize_path_var(s);
        let d = |x: &std::path::Path| x.to_string_lossy().into_owned();

        assert_eq!(
            p(&format!("{}:/usr/bin", d(&link))),
            format!("{}:/usr/bin", d(&real))
        );
        // A relative entry resolves against the CHILD's cwd, so it must survive verbatim —
        // as must the empty entry, which is POSIX for "the current directory".
        assert_eq!(p("bin:/usr/bin:"), "bin:/usr/bin:");
        // Aliases that resolve onto each other collapse; the FIRST wins, so precedence holds.
        assert_eq!(p(&format!("{}:{}", d(&link), d(&real))), d(&real));
        // A colon in the resolved form would split one entry into two bogus ones, the tail
        // of which is relative — keep the original rather than corrupt the list.
        assert_eq!(p(&d(&colon_link)), d(&colon_link));
        // A dangling entry is a harmless skippable miss; it must not panic or vanish.
        assert_eq!(p("/nonexistent-xyz/bin"), "/nonexistent-xyz/bin");
    }

    #[test]
    fn bare_program_grant_anchors_relative_and_empty_path_at_child_cwd() {
        let root = tempfile::tempdir().unwrap();
        let child_cwd = root.path().join("child");
        let child_bin = child_cwd.join("bin");
        std::fs::create_dir_all(&child_bin).unwrap();
        let relative_program = child_bin.join("relative-tool");
        let empty_program = child_cwd.join("empty-tool");
        std::fs::write(&relative_program, b"tool").unwrap();
        std::fs::write(&empty_program, b"tool").unwrap();

        let relative = program_read_term(
            &CommandSpec::new("relative-tool").cwd(&child_cwd),
            Some("bin"),
        )
        .expect("relative PATH resolves from child cwd");
        assert!(relative.contains(&relative_program.to_string_lossy().replace('\\', "\\\\")));

        let empty = program_read_term(&CommandSpec::new("empty-tool").cwd(&child_cwd), Some(":"))
            .expect("empty PATH component resolves from child cwd");
        assert!(empty.contains(&empty_program.to_string_lossy().replace('\\', "\\\\")));
    }

    #[test]
    fn missing_private_tmp_is_reported_as_fail_safe_over_confinement() {
        let mut p = fs_policy(Effect::Allow, vec![]);
        p.fs.tmp = TmpMode::Private;
        let deg = degradation(&p, None, None);
        assert_eq!(deg.lost, vec!["tmp-private"]);
        assert!(
            deg.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("shared temp remains hidden"))
        );
    }

    #[test]
    fn no_sandbox_wrap_when_nothing_confines() {
        // Relaxed fs + non-enforcing net + no env secret = env-scrub only, no SBPL.
        let p = fs_policy(Effect::Allow, vec![]);
        assert!(!needs_sandbox(&p));
        assert!(!needs_wrap(&p));
    }

    #[test]
    fn env_withholding_a_secret_forces_a_wrap() {
        // A scrub that WITHHOLDS a var (relaxed fs/net) must still wrap, so the
        // env-read closure is emitted and the secret can't be recovered cross-process
        // via KERN_PROCARGS2. A passthrough `{env:true}` (nothing withheld) need not.
        let mut p = fs_policy(Effect::Allow, vec![]);
        p.env.enforce = true;
        assert!(
            !needs_wrap(&p),
            "passthrough env withholds nothing → no wrap"
        );
        p.env.withheld = vec!["AWS_SECRET_ACCESS_KEY".to_string()];
        assert!(needs_wrap(&p), "a withheld secret must force the SBPL wrap");
        assert!(
            !needs_sandbox(&p),
            "and it is env — not fs/net — driving it"
        );
    }

    #[test]
    fn every_wrapped_profile_carries_the_env_read_closure() {
        // The closure is unconditional: any wrapped profile (here: an fs-confining one)
        // denies process-info* for all-but-self, and NEVER re-grants the same-sandbox
        // form the base once carried (the env-leak footgun).
        let p = fs_policy(
            Effect::Deny,
            vec![rule("/proj", Effect::Allow, FsAccess::ReadWrite)],
        );
        let prof = build_profile(&p, &spec(), None, None, None);
        assert!(prof.contains("(deny process-info*)\n"));
        assert!(prof.contains("(allow process-info* (target self))"));
        assert!(
            !prof.contains("(allow process-info* (target same-sandbox))"),
            "the same-sandbox process-info grant re-opens the env-read hole"
        );
        assert!(
            !prof.contains("(allow process-info* (target others))"),
            "target-others leaks a sibling's env"
        );
        // The sysctl arm stays shut by deny-default: no broad kern. prefix and no
        // procargs sysctl is ever allowed (either would re-admit the procargs2 read).
        assert!(!prof.contains("(sysctl-name-prefix \"kern.\")"));
        assert!(!prof.contains("kern.procargs"));
    }

    // ── denial attribution (the audit label) ──────────────────────────────────

    fn jail_policy() -> SandboxPolicy {
        fs_policy(
            Effect::Deny,
            vec![rule("/proj", Effect::Allow, FsAccess::ReadWrite)],
        )
    }

    /// A policy shaped like the real build jail: private tmp, so the backend synthesizes the
    /// shared-tmp denies that a default-only tag would have missed.
    fn tmp_confined_policy(tmp: &Path) -> SandboxPolicy {
        let mut policy = fs_policy(
            Effect::Deny,
            vec![rule(
                &tmp.to_string_lossy(),
                Effect::Allow,
                FsAccess::ReadWrite,
            )],
        );
        policy.fs.tmp = crate::policy::TmpMode::Private;
        policy
    }

    #[test]
    fn an_unlabelled_launch_emits_the_profile_it_always_did() {
        let profile = build_profile_with_stdio(&jail_policy(), &spec(), None, None, None, &[]);
        assert!(profile.contains("\n(deny default)\n"));
        assert!(
            !profile.contains("with message"),
            "no label means no annotation: a passing install must pay nothing"
        );
    }

    /// ⛔ EVERY deny, including the ones the BACKEND synthesizes rather than the policy. Tagging
    /// only `(deny default)` reads as sufficient — the jail's policy is a pure allowlist — and is
    /// not: the private-tmp denies below out-rank the default, so a script refused a `/tmp` write
    /// would have produced an untagged record and an empty diagnostic.
    #[test]
    fn a_label_reaches_every_deny_the_profile_carries() {
        let dir = tempfile::tempdir().unwrap();
        let policy = tmp_confined_policy(dir.path());
        let label = "NUBPKG:pkg@1.0.0:7-1";
        let profile = build_profile_with_stdio(
            &policy,
            &spec().audit_label(label),
            None,
            None,
            Some(dir.path()),
            &[],
        );

        let denies: Vec<&str> = profile
            .lines()
            .filter(|l| l.trim_start().starts_with("(deny "))
            .collect();
        assert!(
            denies.len() > 3,
            "fixture must exercise the synthesized denies, not just the default: {denies:?}"
        );
        let untagged: Vec<&&str> = denies
            .iter()
            .filter(|l| !l.contains("(with message"))
            .collect();
        assert!(
            untagged.is_empty(),
            "these denies would produce records nub cannot attribute: {untagged:?}"
        );
        assert!(
            profile.contains(&format!("(deny default (with message \"{label}\"))")),
            "the default deny is annotated in place, not appended"
        );
    }

    /// Removing the annotation must reproduce the unlabelled profile byte-for-byte — so the pass
    /// adds a modifier and changes nothing else. A rule it corrupted would show up here.
    #[test]
    fn annotating_perturbs_nothing_but_the_modifier() {
        let dir = tempfile::tempdir().unwrap();
        let policy = tmp_confined_policy(dir.path());
        let label = "NUBPKG:pkg@1.0.0:7-1";
        let tagged = build_profile_with_stdio(
            &policy,
            &spec().audit_label(label),
            None,
            None,
            Some(dir.path()),
            &[],
        );
        let bare = build_profile_with_stdio(&policy, &spec(), None, None, Some(dir.path()), &[]);
        assert_eq!(
            tagged.replace(&format!(" (with message \"{label}\")"), ""),
            bare
        );
    }

    /// The pass must decline anything it cannot recognize as one complete rule, rather than
    /// splicing a modifier into the middle of it. Nothing emits a multi-line deny today; the point
    /// is that adding one degrades the diagnostic instead of corrupting the profile.
    #[test]
    fn annotation_declines_a_rule_it_cannot_recognize() {
        let cases = [
            // Complete single-line rules — annotated.
            (
                "(deny default)",
                Some("(deny default (with message \"L\"))"),
            ),
            (
                "(deny process-info*)",
                Some("(deny process-info* (with message \"L\"))"),
            ),
            (
                "(deny file-read* (subpath \"/a/b\"))",
                Some("(deny file-read* (subpath \"/a/b\") (with message \"L\"))"),
            ),
            // A paren inside a quoted string is data, not structure.
            (
                "(deny file-read* (regex #\"^(.*/)?\\.env$\"))",
                Some("(deny file-read* (regex #\"^(.*/)?\\.env$\") (with message \"L\"))"),
            ),
            (
                "(deny file-read* (literal \"/a(b\"))",
                Some("(deny file-read* (literal \"/a(b\") (with message \"L\"))"),
            ),
            // The modifier lands before the rule's close, NOT before the last `)` on the line —
            // which for a commented rule is inside the comment.
            (
                "(deny default) ; see note(2)",
                Some("(deny default (with message \"L\")) ; see note(2)"),
            ),
            // Incomplete, or more than one expression — declined outright.
            ("(deny file-read*", None),
            ("(deny file-read* (subpath \"/a\")) (allow default)", None),
            ("(deny file-read* (literal \"/a\")", None),
            // Not a deny at all.
            ("(allow file-read* (subpath \"/a\"))", None),
            ("; (deny default) in a comment", None),
        ];
        for (line, want) in cases {
            let got = annotate_denies(&format!("{line}\n"), "L");
            assert_eq!(
                got,
                format!("{}\n", want.unwrap_or(line)),
                "annotate_denies({line:?})"
            );
        }
    }

    /// ⛔ THE LOAD-BEARING CLAIM OF THE WHOLE DIAGNOSTIC: annotating the default deny does not
    /// change WHAT is denied. A diagnostic that widened or narrowed the jail to describe itself
    /// would be a security regression dressed as an error message.
    ///
    /// Differential against the real kernel rather than the profile text, because the question is
    /// what Seatbelt DOES with the modifier, not what nub wrote. One variable: the same policy,
    /// the same command, the same denied path, labelled and unlabelled.
    #[test]
    fn tagged_default_deny_does_not_change_enforcement() {
        let dir = tempfile::Builder::new()
            .prefix("nub-denylabel-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let denied = root.join("denied.txt");
        std::fs::write(&denied, "secret").unwrap();
        let granted = root.join("granted.txt");
        std::fs::write(&granted, "public").unwrap();

        // Read-granted on ONE leaf so the pair exercises both verdicts: the granted read must
        // still succeed (the label did not narrow) and the sibling must still fail (it did not
        // widen). A deny-only probe could not tell a working label from a broken profile.
        let policy = fs_policy(
            Effect::Deny,
            vec![rule(
                &granted.to_string_lossy(),
                Effect::Allow,
                FsAccess::Read,
            )],
        );

        let run = |label: Option<&str>, target: &Path| {
            let mut s = CommandSpec::new("/bin/cat");
            if let Some(label) = label {
                s = s.audit_label(label);
            }
            let profile = build_profile(&policy, &s, None, None, None);
            let path = root.join("p.sb");
            std::fs::write(&path, &profile).unwrap();
            Command::new(SANDBOX_EXEC_PATH)
                .arg("-f")
                .arg(&path)
                .arg("/bin/cat")
                .arg(target)
                .output()
                .unwrap()
        };

        for (name, target, want_success) in [
            ("denied", denied.as_path(), false),
            ("granted", granted.as_path(), true),
        ] {
            let bare = run(None, target);
            let tagged = run(Some("NUBPKG:pkg@1.0.0:7-1"), target);
            assert_eq!(
                bare.status.success(),
                want_success,
                "precondition: the UNLABELLED profile must {} the {name} read — got {:?} / {}",
                if want_success { "allow" } else { "refuse" },
                bare.status,
                String::from_utf8_lossy(&bare.stderr)
            );
            assert_eq!(
                tagged.status.code(),
                bare.status.code(),
                "the {name} read changed status under the label: {:?} vs {:?}",
                tagged.status,
                bare.status
            );
            assert_eq!(
                tagged.stdout, bare.stdout,
                "the {name} read returned different bytes under the label"
            );
            assert_eq!(
                String::from_utf8_lossy(&tagged.stderr),
                String::from_utf8_lossy(&bare.stderr),
                "the {name} read reported a different error under the label"
            );
        }
    }

    /// The label reaches the kernel's records, from a GRANDCHILD as well as the direct child —
    /// which is the case that matters, since a lifecycle script is a shell whose `node-gyp` →
    /// `make` → `cc` descendants do the work that gets refused.
    #[test]
    fn a_labelled_launch_is_recoverable_from_the_unified_log() {
        if !Path::new("/usr/bin/log").exists() {
            eprintln!("SKIP: no /usr/bin/log on this host");
            return;
        }
        let dir = tempfile::Builder::new()
            .prefix("nub-denylog-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let denied = root.join("denied.txt");
        std::fs::write(&denied, "secret").unwrap();

        // Unique per run: the retrieval predicate is the label, so a fixed one would match a
        // previous run of this very test still inside the lookback window.
        let label = format!(
            "NUBPKG:nub-selftest@0.0.0:{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let policy = fs_policy(Effect::Deny, vec![]);
        let profile = build_profile(
            &policy,
            &CommandSpec::new("/bin/sh").audit_label(&label),
            None,
            None,
            None,
        );
        let path = root.join("p.sb");
        std::fs::write(&path, &profile).unwrap();
        let out = Command::new(SANDBOX_EXEC_PATH)
            .arg("-f")
            .arg(&path)
            .arg("/bin/sh")
            .arg("-c")
            .arg(format!("/bin/sh -c '/bin/cat {}'", denied.display()))
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "precondition: the grandchild read must be refused"
        );

        // ⛔ THE KERNEL’S DELIVERY IS NOT NUB’S BEHAVIOUR, AND ASSERTING IT MADE THIS TEST A FLAKE.
        // `log show` reads the unified log, which drops and delays records under load — the same
        // best-effort channel measured at 18 of 20 real installs, both misses being the record absent
        // from the kernel afterwards too. This test went red on a host running four concurrent build
        // lanes and green on the same commit minutes earlier, and it sits on a prerelease branch that
        // CI never arbitrates, so nothing else would have caught it.
        //
        // ⛔ THE SPLIT IS NOT A LOOSENED ASSERTION — it is the assertion finally aimed at the right
        // thing. One query, read twice: no record at all is the ENVIRONMENT and unknowable here; a
        // record that does not parse to the refused path is a PARSING REGRESSION, which is the only
        // reason this test exists. Re-querying instead of reusing `raw` would reintroduce the race.
        let raw = crate::macos_denials::raw_for_launch(&label, std::time::Duration::from_secs(60))
            .filter(|r| !r.trim().is_empty());
        let Some(raw) = raw else {
            eprintln!(
                "SKIP: the unified log returned no kernel record for {label}; the refusal itself was \
                 asserted above, and delivery is best-effort"
            );
            return;
        };
        let denials = crate::macos_denials::parse(&raw);

        // ⛔ THE SPLIT ABOVE WAS ONE STEP TOO COARSE, AND CI PROVED IT. It assumed a record coming
        // back for the label means EVERY record for that label came back, so a set missing the
        // refused path had to be a parsing regression. The unified log does not work that way — it
        // drops records INDIVIDUALLY. Measured on the macos leg of run 33730167920 (288 passed, 1
        // failed): the query returned three denials that ALL parsed correctly
        // (`dyld_shared_cache_arm64e`, plus two reads of the checkout under `/Users/runner/work`)
        // and simply did not include the tempdir path. Parsing was fine; delivery was partial.
        //
        // So the discriminator is whether the record parsed to ANYTHING. Nothing parsed => the
        // parser no longer understands the kernel's format, which is the regression this test
        // exists to catch and stays a hard failure. Something parsed but not the refused path =>
        // the same best-effort delivery the branch above already tolerates.
        //
        // ⛔ THIS IS NOT THE ASSERTION LOOSENED TO GET GREEN: `macos_denials` carries seven
        // hermetic tests that pin the parse against fixed input, so the format regression keeps a
        // deterministic guard no kernel scheduling can skip.
        assert!(
            !denials.is_empty(),
            "the kernel record came back for {label} but parsed to NO denials at all — the parser \
             no longer understands the record format.\nraw: {raw}"
        );
        if !denials
            .iter()
            .any(|d| d.path == denied.to_string_lossy() && d.operation.starts_with("file-"))
        {
            eprintln!(
                "SKIP: the unified log delivered {} denial(s) for {label} but not the refused \
                 path; delivery is per-record and best-effort, and the refusal itself was asserted \
                 above.\nparsed: {denials:?}",
                denials.len()
            );
        }
    }
}
