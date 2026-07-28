//! The host-capability gate the Linux enforcement suites skip on.
//!
//! It exists because the question "can this host enforce?" is PRODUCTION's question,
//! not "does a file named `bwrap` sit in one of two hardcoded places". Each suite used
//! to answer it with its own copy of a `["/usr/bin/bwrap", "/bin/bwrap"]` smoke launch,
//! which is blind to [`DEDICATED_HELPER_PATH`] — the helper `nub setup-sandbox` installs
//! and the resolver prefers. On a correctly set-up Ubuntu 24.04 host that is precisely
//! backwards: AppArmor denies the distro bwrap at its unprofiled path and ONLY the
//! helper can create the namespace, so every suite reported a green all-skip on exactly
//! the configuration it exists to exercise. Candidates are therefore enumerated from
//! [`single_level_bwrap_candidate_paths`] — the same list `open_bwrap_candidate_inventory`
//! resolves — so the gate cannot drift out of step with the resolver again.
//!
//! A skip decision announces itself on the REAL stderr (libtest captures the `eprintln!`
//! family, so a captured message would be invisible on a green run). Without that, "38
//! passed in 0.01s" reads as success.
#![cfg(target_os = "linux")]

use super::linux::{DEDICATED_HELPER_PATH, single_level_bwrap_candidate_paths};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// What the probe found, in the resolver's candidate order.
#[derive(Debug)]
pub struct BwrapProbe {
    /// The first candidate that created the requested namespaces.
    pub working: Option<PathBuf>,
    /// `<path>: <why>` for every candidate that did not, including the ones tried
    /// before a later success.
    pub rejected: Vec<String>,
}

impl BwrapProbe {
    pub fn enforcing(&self) -> bool {
        self.working.is_some()
    }

    /// The candidate ledger, for a skip banner or a `NUB_SANDBOX_REQUIRE_BWRAP` panic.
    pub fn ledger(&self) -> String {
        let mut out = String::new();
        for line in &self.rejected {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
        if let Some(working) = &self.working {
            out.push_str(&format!("    {}: OK\n", working.display()));
        }
        out
    }
}

/// Run the resolver's candidates until one creates the requested namespaces.
///
/// The launch is `--die-with-parent --unshare-user {unshare} --ro-bind / / {view} --
/// /bin/true`. Bubblewrap applies operations in argv order, hence the split: namespace
/// flags must precede the binds, and any view op layered over the root (`--dev`,
/// `--proc`) must follow it. Each suite asks about the primitives it actually depends
/// on rather than a lowest common denominator.
///
/// This deliberately does NOT re-run production's admission checks (root ownership,
/// digest pinning). A candidate production would reject but that launches here makes
/// the suite RUN and fail loudly rather than skip — the safe direction for a gate whose
/// whole failure mode is a silent green.
pub fn probe(unshare: &[&str], view: &[&str]) -> BwrapProbe {
    let (dedicated, system, bundled) = single_level_bwrap_candidate_paths();
    let mut rejected = Vec::new();
    for path in dedicated.into_iter().chain(system).chain(bundled) {
        let mut args: Vec<&str> = vec!["--die-with-parent", "--unshare-user"];
        args.extend_from_slice(unshare);
        args.extend_from_slice(&["--ro-bind", "/", "/"]);
        args.extend_from_slice(view);
        args.extend_from_slice(&["--", "/bin/true"]);
        match Command::new(&path).args(&args).output() {
            Ok(output) if output.status.success() => {
                return BwrapProbe {
                    working: Some(path),
                    rejected,
                };
            }
            Ok(output) => {
                let detail = String::from_utf8_lossy(&output.stderr);
                let detail = detail.trim();
                let detail = if detail.is_empty() {
                    format!("exited {}", output.status)
                } else {
                    detail.to_string()
                };
                rejected.push(format!("{}: {detail}", path.display()));
            }
            Err(error) => rejected.push(format!("{}: {error}", path.display())),
        }
    }
    BwrapProbe {
        working: None,
        rejected,
    }
}

/// The probe for this test binary, run once per distinct launch shape. Leaked rather
/// than held in a `OnceLock<BwrapProbe>` so a binary that asks about two shapes gets
/// two honest answers instead of the first one it happened to ask for.
pub fn cached_probe(unshare: &[&str], view: &[&str]) -> &'static BwrapProbe {
    static PROBES: OnceLock<Mutex<BTreeMap<Vec<String>, &'static BwrapProbe>>> = OnceLock::new();
    let key: Vec<String> = unshare
        .iter()
        .chain(view)
        .map(|arg| (*arg).to_string())
        .collect();
    let mut probes = PROBES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    probes
        .entry(key)
        .or_insert_with(|| Box::leak(Box::new(probe(unshare, view))))
}

/// The working candidate, or `None` when the caller should skip. Suites that hand the
/// resolved path to a helper process call this; the rest call [`skip_without_bwrap`].
pub fn usable_bwrap(suite: &str) -> Option<&'static Path> {
    if skip_without_bwrap(suite) {
        return None;
    }
    cached_probe(&[], &[]).working.as_deref()
}

/// Returns true when the caller should SKIP: no candidate could create a user namespace.
pub fn skip_without_bwrap(suite: &str) -> bool {
    skip_without_bwrap_with(suite, &[], &[])
}

/// [`skip_without_bwrap`] for a suite that needs more than the user namespace — see
/// [`probe`] for where each slice lands on the command line.
///
/// `NUB_SANDBOX_REQUIRE_BWRAP` turns the skip into a hard failure. Independent of that
/// escape hatch, the FIRST skip decision writes the candidate ledger to the real stderr,
/// so a hollow run is legible to anyone skimming CI output.
pub fn skip_without_bwrap_with(suite: &str, unshare: &[&str], view: &[&str]) -> bool {
    let probe = cached_probe(unshare, view);
    if probe.enforcing() {
        return false;
    }
    let required = matches!(
        std::env::var("NUB_SANDBOX_REQUIRE_BWRAP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    assert!(
        !required,
        "NUB_SANDBOX_REQUIRE_BWRAP set but no Bubblewrap candidate could create the \
         namespaces {suite} needs:\n{}",
        probe.ledger()
    );
    announce_once(suite, probe);
    true
}

fn announce_once(suite: &str, probe: &BwrapProbe) {
    static ANNOUNCED: OnceLock<Mutex<BTreeMap<String, ()>>> = OnceLock::new();
    let mut announced = ANNOUNCED
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if announced.insert(suite.to_string(), ()).is_some() {
        return;
    }
    let banner = format!(
        "\n\
         ============================================================================\n\
         NOT ENFORCING: every {suite} test SKIPS — a pass here proves nothing.\n\
         No Bubblewrap candidate could create the required namespaces. Probed in the\n\
         resolver's order (dedicated helper, system, bundled):\n\
         {}\
         On an AppArmor-restricted host the helper at {DEDICATED_HELPER_PATH} is the only\n\
         candidate that can; `sudo nub setup-sandbox` installs it. Set\n\
         NUB_SANDBOX_REQUIRE_BWRAP=1 to turn this skip into a hard failure.\n\
         ============================================================================\n",
        probe.ledger()
    );
    // Straight to fd 2: libtest's capture intercepts the `eprintln!` family, which
    // would hide this on the green run it exists to make legible.
    let _ = std::io::stderr().write_all(banner.as_bytes());
    let _ = std::io::stderr().flush();
}
