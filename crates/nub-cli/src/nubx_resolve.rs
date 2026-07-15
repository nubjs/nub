//! nubx subject/tier classifier — the argv router behind the unified
//! `nubx <token> [args]` runner.
//!
//! nubx is a PURE PASS-THROUGH runner: it locates the SUBJECT token (the file /
//! script / bin / package the user named), decides which TIER that subject
//! belongs to by precedence (file > script > bin > registry), then hands the RAW
//! remaining argv to that tier's existing runner unchanged. A flag's meaning is
//! determined entirely by what the subject resolves to — we locate the subject,
//! we do not re-interpret the rest.
//!
//! ## The per-target asymmetry (why two mechanisms, not one)
//! Subject location is asymmetric because the grammars are:
//! - The nub-owned tiers (script/`run`, bin, package/registry) are CLOSED grammars
//!   we define, so they resolve through nub's own clap parse (the existing `Nubx`
//!   dispatch, or a re-dispatch to `nub run`) — clap signals both resolution and
//!   non-resolution cleanly.
//! - The Node/FILE tier's flag surface is OPEN, evolving, and not ours, so clap
//!   would fail-closed on a future Node flag and break pass-through. This tier
//!   alone scans argv against a BAKED arity table generated from Node's own
//!   `getCLIOptionsInfo()`. A flag's only relevant property for subject location is
//!   whether it consumes the next argv token as its value; the first bare non-flag
//!   token (after skipping value-flags + their values) is the file subject.
//!
//! ## Forward-compat
//! The baked value set is the UNION across Node 18-26, so it is exhaustive for
//! every currently-supported Node. The only gap is a value-flag added in a Node
//! NEWER than the baked baseline: a stale table would treat it as zero-arity and
//! could mislocate the subject. That gap is closed by a version-gated introspect
//! fallback ([`introspect_node_value_flags`]): when the target Node exceeds the
//! baseline AND an unrecognized `-`-flag is present, derive the value set from the
//! real Node binary and re-scan. Proven unable to silently mis-locate in
//! `wiki/research/node-flag-arity.md`.

use std::collections::HashSet;
use std::path::Path;

use nub_core::node::discovery::discover_node_cached;
use nub_core::node::version::NodeVersion;

mod table {
    //! Generated from `getCLIOptionsInfo()` across Node 18.20 / 20.19 / 22.15 /
    //! 24.14 / 26.2 — NOT hand-maintained. Regenerate with
    //! `.fray/node-flag-arity-table.findings/gen-rust-table.mjs`. Provenance +
    //! the OptionType arity model: `wiki/research/node-flag-arity.md`.

    /// The Node options/aliases that consume the NEXT argv token as their value
    /// (`Integer`/`UInteger`/`String`/`HostPort`/`StringList`). Everything else —
    /// booleans, V8 passthrough, NoOp, and unknowns — is zero-arity for subject
    /// scanning. The short `-r`/`-p` are DELIBERATELY ABSENT: nubx overrides them
    /// to its own `--recursive`/`--package` (decided), so they never read as
    /// Node's `--require`/`--print` here. Sorted for `binary_search`.
    #[rustfmt::skip]
    pub(super) const NODE_VALUE_FLAGS: &[&str] = &[
        "--allow-fs-read", "--allow-fs-write", "--build-sea", "--build-snapshot-config",
        "--conditions", "--cpu-prof-dir", "--cpu-prof-interval", "--cpu-prof-name", "--debug-port",
        "--diagnostic-dir", "--disable-proto", "--disable-warning", "--dns-result-order",
        "--env-file", "--env-file-if-exists", "--es-module-specifier-resolution", "--eval",
        "--experimental-config-file", "--experimental-default-config-file",
        "--experimental-default-type", "--experimental-loader", "--experimental-policy",
        "--experimental-sea-config", "--experimental-specifier-resolution",
        "--experimental-test-isolation", "--experimental-test-tag-filter", "--heap-prof-dir",
        "--heap-prof-interval", "--heap-prof-name", "--heapsnapshot-near-heap-limit",
        "--heapsnapshot-signal", "--icu-data-dir", "--import", "--input-type", "--inspect-port",
        "--inspect-publish-uid", "--loader", "--localstorage-file", "--max-http-header-size",
        "--max-old-space-size-percentage", "--network-family-autoselection-attempt-timeout",
        "--openssl-config", "--policy-integrity", "--redirect-warnings", "--report-dir",
        "--report-directory", "--report-filename", "--report-signal", "--require", "--run",
        "--secure-heap", "--secure-heap-min", "--security-revert", "--security-reverts",
        "--snapshot-blob", "--stack-trace-limit", "--test-concurrency", "--test-coverage-branches",
        "--test-coverage-exclude", "--test-coverage-functions", "--test-coverage-include",
        "--test-coverage-lines", "--test-global-setup", "--test-isolation", "--test-name-pattern",
        "--test-random-seed", "--test-reporter", "--test-reporter-destination",
        "--test-rerun-failures", "--test-shard", "--test-skip-pattern", "--test-timeout",
        "--title", "--tls-cipher-list", "--tls-keylog", "--trace-event-categories",
        "--trace-event-file-pattern", "--trace-require-module", "--unhandled-rejections",
        "--use-largepages", "--v8-pool-size", "--watch-kill-signal", "--watch-path", "-C", "-e",
    ];

    /// Every flag name Node recognizes at all (any arity), across the same majors.
    /// Used ONLY to detect a genuinely-unrecognized `-`-flag for the introspect
    /// gate — so a known zero-arity boolean (`--inspect`, `--enable-source-maps`)
    /// never spuriously triggers the fallback. Sorted for `binary_search`.
    #[rustfmt::skip]
    pub(super) const NODE_KNOWN_FLAGS: &[&str] = &[
        "--abort-on-uncaught-exception", "--addons", "--allow-addons", "--allow-child-process",
        "--allow-ffi", "--allow-fs-read", "--allow-fs-write", "--allow-inspector", "--allow-net",
        "--allow-wasi", "--allow-worker", "--async-context-frame", "--build-sea",
        "--build-snapshot", "--build-snapshot-config", "--check", "--completion-bash",
        "--conditions", "--cpu-prof", "--cpu-prof-dir", "--cpu-prof-interval", "--cpu-prof-name",
        "--debug", "--debug-arraybuffer-allocations", "--debug-brk", "--debug-port",
        "--deprecation", "--diagnostic-dir", "--disable-proto", "--disable-sigusr1",
        "--disable-warning", "--disable-wasm-trap-handler",
        "--disallow-code-generation-from-strings", "--dns-result-order",
        "--enable-etw-stack-walking", "--enable-fips", "--enable-network-family-autoselection",
        "--enable-source-maps", "--entry-url", "--env-file", "--env-file-if-exists",
        "--es-module-specifier-resolution", "--eval", "--experimental-abortcontroller",
        "--experimental-addon-modules", "--experimental-async-context-frame",
        "--experimental-config-file", "--experimental-default-config-file",
        "--experimental-default-type", "--experimental-detect-module",
        "--experimental-eventsource", "--experimental-fetch", "--experimental-ffi",
        "--experimental-global-customevent", "--experimental-global-navigator",
        "--experimental-global-webcrypto", "--experimental-import-meta-resolve",
        "--experimental-inspector-network-resource", "--experimental-json-modules",
        "--experimental-loader", "--experimental-modules", "--experimental-network-imports",
        "--experimental-network-inspection", "--experimental-permission", "--experimental-policy",
        "--experimental-print-required-tla", "--experimental-quic", "--experimental-repl-await",
        "--experimental-report", "--experimental-require-module", "--experimental-sea-config",
        "--experimental-shadow-realm", "--experimental-specifier-resolution",
        "--experimental-sqlite", "--experimental-storage-inspection", "--experimental-stream-iter",
        "--experimental-strip-types", "--experimental-test-coverage",
        "--experimental-test-isolation", "--experimental-test-module-mocks",
        "--experimental-test-snapshots", "--experimental-test-tag-filter",
        "--experimental-top-level-await", "--experimental-transform-types",
        "--experimental-vm-modules", "--experimental-wasi-unstable-preview1",
        "--experimental-wasm-modules", "--experimental-websocket", "--experimental-webstorage",
        "--experimental-worker", "--experimental-worker-inspection", "--expose-gc",
        "--expose-internals", "--extra-info-on-fatal-exception", "--force-async-hooks-checks",
        "--force-context-aware", "--force-fips", "--force-node-api-uncaught-exceptions-policy",
        "--frozen-intrinsics", "--global-search-paths", "--harmony-shadow-realm", "--heap-prof",
        "--heap-prof-dir", "--heap-prof-interval", "--heap-prof-name",
        "--heapsnapshot-near-heap-limit", "--heapsnapshot-signal", "--help", "--http-parser",
        "--huge-max-old-generation-size", "--icu-data-dir", "--import", "--input-type",
        "--insecure-http-parser", "--inspect", "--inspect-brk", "--inspect-brk-node",
        "--inspect-port", "--inspect-publish-uid", "--inspect-wait", "--interactive",
        "--interpreted-frames-native-stack", "--jitless", "--loader", "--localstorage-file",
        "--max-heap-size", "--max-http-header-size", "--max-old-space-size",
        "--max-old-space-size-percentage", "--max-semi-space-size", "--napi-modules",
        "--network-family-autoselection", "--network-family-autoselection-attempt-timeout",
        "--node-memory-debug", "--node-snapshot", "--openssl-config", "--openssl-legacy-provider",
        "--openssl-shared-config", "--pending-deprecation", "--perf-basic-prof",
        "--perf-basic-prof-only-functions", "--perf-prof", "--perf-prof-unwinding-info",
        "--permission", "--permission-audit", "--policy-integrity", "--preserve-symlinks",
        "--preserve-symlinks-main", "--print", "--prof", "--prof-process", "--redirect-warnings",
        "--report-compact", "--report-dir", "--report-directory", "--report-exclude-env",
        "--report-exclude-network", "--report-filename", "--report-on-fatalerror",
        "--report-on-signal", "--report-signal", "--report-uncaught-exception", "--require",
        "--require-module", "--run", "--secure-heap", "--secure-heap-min", "--security-revert",
        "--security-reverts", "--snapshot-blob", "--stack-trace-limit", "--strip-types", "--test",
        "--test-concurrency", "--test-coverage-branches", "--test-coverage-exclude",
        "--test-coverage-functions", "--test-coverage-include", "--test-coverage-lines",
        "--test-force-exit", "--test-global-setup", "--test-isolation", "--test-name-pattern",
        "--test-only", "--test-random-seed", "--test-randomize", "--test-reporter",
        "--test-reporter-destination", "--test-rerun-failures", "--test-shard",
        "--test-skip-pattern", "--test-timeout", "--test-udp-no-try-send",
        "--test-update-snapshots", "--throw-deprecation", "--title", "--tls-cipher-list",
        "--tls-keylog", "--tls-max-v1.2", "--tls-max-v1.3", "--tls-min-v1.0", "--tls-min-v1.1",
        "--tls-min-v1.2", "--tls-min-v1.3", "--trace-atomics-wait", "--trace-deprecation",
        "--trace-env", "--trace-env-js-stack", "--trace-env-native-stack",
        "--trace-event-categories", "--trace-event-file-pattern", "--trace-events-enabled",
        "--trace-exit", "--trace-promises", "--trace-require-module", "--trace-sigint",
        "--trace-sync-io", "--trace-tls", "--trace-uncaught", "--trace-warnings",
        "--track-heap-objects", "--unhandled-rejections", "--use-bundled-ca", "--use-env-proxy",
        "--use-largepages", "--use-openssl-ca", "--use-system-ca", "--v8-options",
        "--v8-pool-size", "--verify-base-objects", "--version", "--warnings", "--watch",
        "--watch-kill-signal", "--watch-path", "--watch-preserve-output", "--webstorage",
        "--zero-fill-buffers", "-C", "-c", "-e", "-h", "-i", "-p", "-pe", "-r", "-v",
    ];

    /// The highest Node major the baked tables were generated from. Beyond this,
    /// an unrecognized `-`-flag may be a new value-flag, so the introspect
    /// fallback is consulted.
    pub(super) const NODE_ARITY_BASELINE: super::NodeVersion = super::NodeVersion::new(26, 2, 0);
}

/// `-p`/`--package` (incl. inline `--package=spec`): nubx owns this short and it
/// FORCES the registry tier (decided: `-p` = `--package`, beating Node's `--print`).
fn is_package_flag(tok: &str) -> bool {
    tok == "-p" || tok == "--package" || tok.starts_with("--package=")
}

/// dlx fetch-path flags. Their presence means a registry fetch is on the table, so
/// the subject must resolve through the `Nubx` dispatch — never re-dispatched to
/// `nub run` (which lacks these flags).
fn is_dlx_flag(tok: &str) -> bool {
    matches!(
        tok,
        "--no-install" | "--no" | "-q" | "--quiet" | "-y" | "--yes" | "--ignore-existing"
    )
}

/// `nub run`/workspace routing flags. Their presence means the subject is NOT a
/// local file, but the scan keeps walking for the subject — a workspace/script run
/// (`nubx --filter foo build`, `nubx --reporter silent build`) still resolves the
/// script and re-dispatches to `nub run`. The bool is whether the flag consumes a
/// following value token (so the scan skips it). The value-consuming arm must stay
/// in sync with `value_consuming_flags("run")` in cli.rs so a run value-flag's
/// argument is never mistaken for the subject.
fn nub_routing_flag(tok: &str) -> Option<bool> {
    match tok {
        "-r"
        | "--recursive"
        | "--workspaces"
        | "-w"
        | "--workspace-root"
        | "--include-workspace-root"
        | "--parallel"
        | "--fail-if-no-match" => Some(false),
        "-F"
        | "--filter"
        | "--workspace"
        | "--workspace-concurrency"
        | "--cwd"
        | "--resume-from"
        | "--script-shell"
        | "--reporter" => Some(true),
        _ => None,
    }
}

fn is_node_value_flag(tok: &str, extra: Option<&HashSet<String>>) -> bool {
    table::NODE_VALUE_FLAGS.binary_search(&tok).is_ok() || extra.is_some_and(|s| s.contains(tok))
}

/// A `-`-flag Node does not recognize at all (after stripping an inline `=value`),
/// and which is not one of nubx's own routing/compat flags. Such a token on a
/// newer-than-baseline Node is the only thing that can defeat the baked table.
fn is_unrecognized_node_flag(tok: &str) -> bool {
    if !tok.starts_with('-') || tok == "-" || tok == "--" {
        return false;
    }
    let head = tok.split('=').next().unwrap_or(tok);
    if table::NODE_KNOWN_FLAGS.binary_search(&head).is_ok() {
        return false;
    }
    // nubx-owned / shared flags are "recognized" for this purpose.
    !(head == "--node"
        || is_package_flag(head)
        || is_dlx_flag(head)
        || nub_routing_flag(head).is_some()
        || matches!(head, "--eval" | "--print"))
}

/// Where a nubx invocation routes. The caller maps each arm to an existing runner.
#[derive(Debug, PartialEq, Eq)]
pub enum NubxRoute {
    /// Node/FILE tier — a file subject, an `-e`/`--eval`/`--print` eval, or stdin
    /// `-`. Delegate `argv` (with nub's own LEADING `--node` removed) to the file
    /// runner in `compat` mode. The flags reach Node verbatim; Node binds
    /// flags-vs-entry.
    File {
        compat: bool,
        argv: Vec<String>,
        /// A leading Node env-file flag is forwarded; config env injection must
        /// stand down so Node can apply that explicit file with CLI precedence.
        explicit_env_file: bool,
    },
    /// SCRIPT tier — the bare subject is a package.json script and no registry/dlx
    /// flag is present. Re-dispatch the ORIGINAL argv through `nub run` for its full
    /// flag surface (`--if-present`, `--filter`, `--reporter`, …).
    Script,
    /// BIN / REGISTRY / workspace-bin tier, `-p`-forced fetch, or no subject at all
    /// — the existing `Nubx` clap dispatch resolves it (and owns the missing-name
    /// bail).
    Owned,
}

/// Outcome of the flag scan, before the script-vs-bin filesystem decision.
enum Scan {
    /// File subject / eval / stdin → the Node tier. `compat` is set when nub's own
    /// `--node` opt-out stood in the LEADING flag region; `argv` is the original
    /// argv with exactly those leading `--node` flags removed. A program-arg
    /// `--node` after the subject, and a `--node` consumed as a value-flag's value,
    /// are NOT removed (the scan stops at the subject and skips value tokens, so
    /// they never register as leading flags).
    NodeTier {
        compat: bool,
        argv: Vec<String>,
        explicit_env_file: bool,
    },
    /// A bare subject token that is not a local file. `allow_script` is false when a
    /// registry/dlx flag was seen (those forbid the `nub run` re-dispatch).
    Subject { name: String, allow_script: bool },
    /// `-p`-forced registry, or no subject token at all → the `Nubx` dispatch.
    Owned,
}

/// Build the file-tier argv: the original args minus the LEADING `--node` flags at
/// `node_idx`. Tokens after the subject (a program-arg `--node`) and a `--node`
/// consumed as a value-flag's value never appear in `node_idx`, so they survive.
fn file_argv(args: &[String], node_idx: &[usize]) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(i, _)| !node_idx.contains(i))
        .map(|(_, a)| a.clone())
        .collect()
}

/// Walk argv with the Node arity table to locate the subject and classify the
/// tier, without touching the filesystem beyond the injected `is_file` predicate.
/// `extra_value` augments the baked value set with introspected flags (forward-
/// compat). Pure + deterministic — the unit tests drive it with a fake `is_file`.
fn scan(
    args: &[String],
    is_file: &dyn Fn(&str) -> bool,
    extra_value: Option<&HashSet<String>>,
) -> Scan {
    let mut saw_routing = false; // a workspace/dlx flag → the subject is not a file
    let mut allow_script = true; // a registry/dlx flag forbids the `nub run` route
    let mut compat = false; // nub's `--node` opt-out seen in the leading flag region
    let mut explicit_env_file = false; // leading Node env-file flag reaches the file tier
    let mut node_idx: Vec<usize> = Vec::new(); // positions of those leading `--node` flags
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();

        // `--` ends flags: the NEXT token is the subject verbatim (even if it looks
        // like a flag). Node does the same.
        if tok == "--" {
            return match args.get(i + 1) {
                Some(sub) if !saw_routing && is_file(sub) => Scan::NodeTier {
                    compat,
                    argv: file_argv(args, &node_idx),
                    explicit_env_file,
                },
                Some(sub) => Scan::Subject {
                    name: sub.clone(),
                    allow_script,
                },
                None => Scan::Owned, // `nubx --` with nothing after
            };
        }
        // A bare `-` is stdin — Node's subject, not a flag.
        if tok == "-" {
            return Scan::NodeTier {
                compat,
                argv: file_argv(args, &node_idx),
                explicit_env_file,
            };
        }
        // A bare token (no leading `-`) is the subject candidate.
        if !tok.starts_with('-') {
            if !saw_routing && is_file(tok) {
                return Scan::NodeTier {
                    compat,
                    argv: file_argv(args, &node_idx),
                    explicit_env_file,
                };
            }
            return Scan::Subject {
                name: tok.to_string(),
                allow_script,
            };
        }

        // ── it is a flag ──
        // `-p`/`--package` forces the registry tier outright.
        if is_package_flag(tok) {
            return Scan::Owned;
        }
        // Eval flags put Node in eval/print mode with no file subject. Gated on no
        // prior routing flag so a workspace/dlx flag still wins. (`-p` is excluded
        // above — it is nubx's `--package`, never Node's `--print`.)
        if !saw_routing && matches!(tok, "-e" | "--eval" | "--print") {
            return Scan::NodeTier {
                compat,
                argv: file_argv(args, &node_idx),
                explicit_env_file,
            };
        }
        // `nub run`/workspace routing: not a file; keep scanning for the subject so
        // a workspace/script run can still resolve + re-dispatch to `nub run`.
        if let Some(consumes_value) = nub_routing_flag(tok) {
            saw_routing = true;
            if consumes_value && !tok.contains('=') {
                i += 1; // skip the flag's separate-token value
            }
            i += 1;
            continue;
        }
        // dlx fetch flags: a registry fetch is possible, so forbid the run
        // re-dispatch and keep scanning for the bin/package subject.
        if is_dlx_flag(tok) {
            saw_routing = true;
            allow_script = false;
            i += 1;
            continue;
        }
        // nub's own `--node` (the compat opt-out) standing as a LEADING flag: record
        // its position for removal + flip compat, then keep scanning. Only a
        // standalone leading `--node` reaches here — one consumed as a value-flag's
        // value (`--import --node`) was already skipped by that flag's `i += 2`, and
        // one AFTER the subject is never walked (the scan returns at the subject). So
        // both stay in argv verbatim with compat OFF, matching `nub <file>` / real
        // node. (Fixes the P1: the old whole-argv strip ate a program-arg `--node`
        // and wrongly forced compat.)
        if tok == "--node" {
            compat = true;
            node_idx.push(i);
            i += 1;
            continue;
        }
        if matches!(tok, "--env-file" | "--env-file-if-exists")
            || tok.starts_with("--env-file=")
            || tok.starts_with("--env-file-if-exists=")
        {
            explicit_env_file = true;
        }
        // A Node value-flag consumes its separate-token value (skip two). The inline
        // `--flag=value` form never reaches here — it isn't in the table verbatim, so
        // it falls through below as a zero-arity token, which is exactly right (the
        // value is self-contained, no separate token to skip).
        if is_node_value_flag(tok, extra_value) {
            i += 2;
            continue;
        }
        // Anything else (an inline `--flag=value`, a Node boolean / V8 / NoOp, or an
        // unknown flag) is zero-arity for subject scanning — skip one.
        i += 1;
    }
    // Only flags, no subject / eval / stdin.
    Scan::Owned
}

/// True if `token` should run as a FILE: an explicit path shape, or a verbatim
/// file by that name in `cwd`. The verbatim-existence check (not extension
/// sniffing) is what lets a bare `index.ts` or relative `sub/app.js` run as a file
/// rather than fall through to the registry — the soundness fix the review flagged.
fn file_tier_match(token: &str, cwd: Option<&Path>) -> bool {
    is_path_shaped(token) || cwd.is_some_and(|c| c.join(token).is_file())
}

/// A token the user clearly means as a filesystem path — an explicit relative or
/// absolute prefix. A bare name is NOT path-shaped; it only counts as the file tier
/// if it verbatim-exists (see [`file_tier_match`]).
pub fn is_path_shaped(token: &str) -> bool {
    token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('/')
        || (cfg!(windows) && (token.starts_with(".\\") || token.starts_with("..\\")))
}

/// Classify a nubx argv into its execution tier. `cwd` anchors the file/script
/// resolution; `is_script` reports whether a bare name is a package.json script
/// (injected so the scan stays filesystem-light + unit-testable).
pub fn classify(
    args: &[String],
    cwd: Option<&Path>,
    is_script: &dyn Fn(&str) -> bool,
) -> NubxRoute {
    let is_file = |t: &str| file_tier_match(t, cwd);

    // Forward-compat: if the target Node is newer than the baked baseline AND an
    // unrecognized `-`-flag is present, the baked value set may be missing a new
    // value-flag. Derive the real set from the target Node and re-scan with it.
    let extra = extra_value_flags(args, cwd);

    match scan(args, &is_file, extra.as_ref()) {
        Scan::NodeTier {
            compat,
            argv,
            explicit_env_file,
        } => NubxRoute::File {
            compat,
            argv,
            explicit_env_file,
        },
        Scan::Subject { name, allow_script } => {
            if allow_script && is_script(&name) {
                NubxRoute::Script
            } else {
                NubxRoute::Owned
            }
        }
        Scan::Owned => NubxRoute::Owned,
    }
}

/// Forward-compat gate: returns introspected value-flags ONLY when the target Node
/// exceeds the baked baseline AND argv carries an unrecognized `-`-flag. Empty/None
/// in the steady state — the baked table is exhaustive for Node ≤ baseline, so this
/// never spawns for a currently-supported Node.
fn extra_value_flags(args: &[String], cwd: Option<&Path>) -> Option<HashSet<String>> {
    if !args.iter().any(|a| is_unrecognized_node_flag(a)) {
        return None;
    }
    let node = discover_node_cached(cwd?)?;
    if node.version <= table::NODE_ARITY_BASELINE {
        return None;
    }
    introspect_node_value_flags(node.path.as_std_path())
}

/// Ask a Node binary for its authoritative value-accepting option set via
/// `getCLIOptionsInfo()`. Best-effort: any failure (older Node without the binding,
/// a spawn error) returns `None` and the caller proceeds with the baked table. The
/// `--no-warnings` suppresses the internal-binding notice.
fn introspect_node_value_flags(node: &Path) -> Option<HashSet<String>> {
    const DUMP: &str = "const{internalBinding:b}=require('internal/test/binding');\
        const o=b('options');const i=o.getCLIOptionsInfo?o.getCLIOptionsInfo():o.getCLIOptions();\
        const v=[];for(const[n,m]of i.options)if([3,4,5,6,7].includes(m.type))v.push(n);\
        process.stdout.write(v.join('\\n'))";
    let out = std::process::Command::new(node)
        .args(["--no-warnings", "--expose-internals", "-e", DUMP])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let set: HashSet<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with('-'))
        .map(str::to_string)
        .collect();
    (!set.is_empty()).then_some(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fake project: `build`/`test` are scripts; `app.js`/`./local.js`/`sub/x.js`
    // are files. Drives `classify` with no real filesystem so the routing logic is
    // tested in isolation from disk + the Node version gate.
    fn route(argv: &[&str]) -> NubxRoute {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let is_script = |t: &str| matches!(t, "build" | "test");
        // Inject file-ness through a path-shape-or-known-name predicate by faking a
        // cwd-less classify: route via `scan` directly with a fake `is_file`.
        let is_file =
            |t: &str| is_path_shaped(t) || matches!(t, "app.js" | "sub/x.js" | "index.ts");
        match scan(&args, &is_file, None) {
            Scan::NodeTier {
                compat,
                argv,
                explicit_env_file,
            } => NubxRoute::File {
                compat,
                argv,
                explicit_env_file,
            },
            Scan::Subject { name, allow_script } => {
                if allow_script && is_script(&name) {
                    NubxRoute::Script
                } else {
                    NubxRoute::Owned
                }
            }
            Scan::Owned => NubxRoute::Owned,
        }
    }

    fn is_file_route(argv: &[&str], expect_compat: bool) {
        match route(argv) {
            NubxRoute::File { compat, .. } => assert_eq!(compat, expect_compat, "{argv:?}"),
            other => panic!("{argv:?} → expected File, got {other:?}"),
        }
    }

    #[test]
    fn node_flag_before_file_routes_to_file() {
        // The headline #224 fix: a Node flag preceding the file no longer errors.
        is_file_route(&["--inspect", "app.js"], false);
        is_file_route(&["--max-old-space-size=64", "app.js"], false);
        is_file_route(&["--import", "./l.js", "app.js"], false); // value-flag skips its value
        is_file_route(&["--enable-source-maps", "app.js"], false);
    }

    #[test]
    fn eval_and_stdin_are_node_tier() {
        is_file_route(&["-e", "console.log(1)"], false);
        is_file_route(&["--eval", "1+1"], false);
        is_file_route(&["--print", "1+1"], false);
        is_file_route(&["-"], false);
    }

    #[test]
    fn paths_and_bare_files_route_to_file() {
        is_file_route(&["app.js"], false);
        is_file_route(&["./local.js"], false); // path-shaped, even if missing
        is_file_route(&["sub/x.js"], false); // bare relative, exists → file (the review's P0)
        is_file_route(&["app.js", "--foo"], false); // trailing args stay raw
    }

    #[test]
    fn double_dash_forces_the_next_token_as_subject() {
        is_file_route(&["--", "app.js"], false);
        // `--` then a non-file bare token = a forced subject, owned tiers.
        assert_eq!(route(&["--", "mytool"]), NubxRoute::Owned);
    }

    #[test]
    fn node_flag_wins_node_meaning_only_via_long_form() {
        // `-r`/`-p` are nubx's; `--require`/`--print` reach Node.
        is_file_route(&["--require", "./pre.js", "app.js"], false);
        // `-r` is `--recursive` (workspace) → not a file; `build` is a script.
        assert_eq!(route(&["-r", "build"]), NubxRoute::Script);
    }

    #[test]
    fn node_flag_only_stripped_in_leading_position() {
        // Asserts a File route with the exact compat bit AND argv.
        let file = |argv: &[&str], compat: bool, expect_argv: &[&str]| match route(argv) {
            NubxRoute::File {
                compat: c, argv: a, ..
            } => {
                assert_eq!(c, compat, "compat for {argv:?}");
                assert_eq!(a, expect_argv, "argv for {argv:?}");
            }
            other => panic!("{argv:?} → expected File, got {other:?}"),
        };

        // LEADING `--node` is nub's compat opt-out: stripped + flips compat.
        file(&["--node", "app.js"], true, &["app.js"]);
        // P1 regression: a `--node` AFTER the subject is a PROGRAM ARGUMENT — it must
        // survive verbatim and must NOT flip compat (file tier stays byte-identical
        // to `nub <file>` / real node). The old whole-argv strip ate it.
        file(
            &["app.js", "arg1", "--node", "arg2"],
            false,
            &["app.js", "arg1", "--node", "arg2"],
        );
        // A `--node` consumed as a value-flag's VALUE (`--import --node`) is that
        // flag's argument, not nub's compat flag — preserved, no compat flip.
        file(
            &["--import", "--node", "app.js"],
            false,
            &["--import", "--node", "app.js"],
        );
        // Mixed: the leading `--node` is stripped; a trailing program `--node` stays.
        file(
            &["--node", "app.js", "--node", "x"],
            true,
            &["app.js", "--node", "x"],
        );
    }

    #[test]
    fn leading_env_file_metadata_stops_at_the_file_subject() {
        let NubxRoute::File {
            explicit_env_file, ..
        } = route(&["--require", "./pre.js", "--env-file", "cli.env", "app.js"])
        else {
            panic!("expected file route");
        };
        assert!(explicit_env_file);

        let NubxRoute::File {
            explicit_env_file, ..
        } = route(&["app.js", "--env-file=program-argument.env"])
        else {
            panic!("expected file route");
        };
        assert!(!explicit_env_file);
    }

    #[test]
    fn scripts_route_to_run_bins_and_registry_to_owned() {
        assert_eq!(route(&["build"]), NubxRoute::Script);
        assert_eq!(route(&["--if-present", "build"]), NubxRoute::Script);
        assert_eq!(route(&["mytool", "arg1"]), NubxRoute::Owned); // bin
        assert_eq!(route(&["left-pad"]), NubxRoute::Owned); // registry
        assert_eq!(route(&["--inspect", "build"]), NubxRoute::Script); // run rejects --inspect later
    }

    #[test]
    fn package_and_dlx_flags_force_owned() {
        assert_eq!(route(&["-p", "cowsay", "mytool"]), NubxRoute::Owned);
        assert_eq!(
            route(&["-p", "cowsay", "--no-install", "mytool"]),
            NubxRoute::Owned
        );
        // a dlx flag forbids the run re-dispatch even on a script name.
        assert_eq!(route(&["--no-install", "build"]), NubxRoute::Owned);
    }

    #[test]
    fn workspace_filter_on_a_script_routes_to_run() {
        assert_eq!(route(&["--filter", "foo", "build"]), NubxRoute::Script);
        assert_eq!(route(&["--filter", "foo", "tsc"]), NubxRoute::Owned); // bin
    }

    #[test]
    fn run_value_flags_before_the_subject_consume_their_value() {
        // A `nub run` value-flag must skip its value so the subject is located, not
        // mis-read as the flag's argument (the run value-flag set is shared with
        // value_consuming_flags("run")).
        assert_eq!(route(&["--reporter", "silent", "build"]), NubxRoute::Script);
        assert_eq!(
            route(&["--resume-from", "pkg", "-r", "build"]),
            NubxRoute::Script
        );
        assert_eq!(
            route(&["--script-shell", "/bin/sh", "build"]),
            NubxRoute::Script
        );
    }

    #[test]
    fn no_subject_is_owned() {
        assert_eq!(route(&[]), NubxRoute::Owned);
        assert_eq!(route(&["--node"]), NubxRoute::Owned);
    }

    #[test]
    fn value_tables_are_sorted_for_binary_search() {
        assert!(table::NODE_VALUE_FLAGS.windows(2).all(|w| w[0] < w[1]));
        assert!(table::NODE_KNOWN_FLAGS.windows(2).all(|w| w[0] < w[1]));
    }
}
