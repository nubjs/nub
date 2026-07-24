//! Tracing bridge for the embedded engine's warning channel.
//!
//! The engine emits its non-fatal user-facing notices via `tracing::warn!`
//! (ignored build scripts, missing-integrity imports, pnpmfile hook
//! rejections, …) — under aube's own CLI a fmt subscriber at `warn` level
//! prints them. Nub's old subscriber (`EnvFilter::from_default_env()`)
//! enabled *nothing* without `RUST_LOG`, so every engine warning was
//! silently swallowed — a user whose dep builds were skipped saw no
//! notice at all — and *with* `RUST_LOG=warn` the default fmt layer
//! leaked raw engine branding (`aube::commands::…` targets,
//! `WARN_AUBE_*` codes, `` `aube approve-builds` `` hints).
//!
//! This layer fixes both: engine targets are enabled at `warn` by
//! default (mirroring the engine's own `init_logging` directive list),
//! and every rendered line flows through [`present::rewrite`] so the
//! brand boundary holds on the warning channel too. `RUST_LOG` still
//! takes over the filter when set — but the rendering stays ours, so a
//! debugging user doesn't punch a hole in the boundary.
//!
//! Known caveat: lines are written straight to stderr, so a warning
//! fired while the engine's TTY progress bar is live can interleave
//! with the bar's repaint (the engine's own subscriber pauses the bar
//! via its private `PausingWriter`, which the lib surface doesn't
//! expose). A momentarily garbled bar beats an invisible warning;
//! revisit if the fork ever exports the pausing writer.

use super::present;
use std::borrow::Cow;
use std::sync::OnceLock;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry};

/// Default engine log level: `warn`, so routine install output doesn't
/// collide with the progress display while warnings still surface.
const DEFAULT_ENGINE_LEVEL: &str = "warn";

/// Per-engine-crate filter directives at `level`. Mirrors the directive
/// list in the engine's own `init_logging`
/// (`vendor/aube/crates/aube/src/startup.rs`) so warnings (and, when the
/// user raises the level, info/debug) surface from every engine crate, not
/// just the command layer. `level` is a tracing level token (`warn`,
/// `error`, `info`, `debug`) or `off`.
fn engine_directives(level: &str) -> String {
    const CRATES: [&str; 11] = [
        "aube",
        "aube_registry",
        "aube_resolver",
        "aube_lockfile",
        "aube_store",
        "aube_linker",
        "aube_manifest",
        "aube_scripts",
        "aube_workspace",
        "aube_settings",
        "aube_util",
    ];
    CRATES
        .iter()
        .map(|c| format!("{c}={level}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Reload handle for the engine filter, so a per-invocation `--loglevel` /
/// `--reporter` / `--silent` flag can retune verbosity after the global
/// subscriber is installed. `None` until [`init`] runs; absent when
/// `RUST_LOG` owns the filter (see [`set_engine_loglevel`]).
static FILTER_RELOAD: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

/// Whether `RUST_LOG` was set at [`init`] time. When it was, it owns the
/// filter and a per-invocation flag must not override it (mirrors aube's
/// `AUBE_LOG`-wins precedence).
static RUST_LOG_OWNS_FILTER: OnceLock<bool> = OnceLock::new();

/// Install the process-global subscriber. Call once, before any engine
/// (or nub) code can emit tracing events.
pub fn init() {
    let rust_log = std::env::var("RUST_LOG").ok().filter(|s| !s.is_empty());
    let _ = RUST_LOG_OWNS_FILTER.set(rust_log.is_some());
    let filter = match rust_log {
        Some(spec) => EnvFilter::new(spec),
        None => EnvFilter::new(engine_directives(DEFAULT_ENGINE_LEVEL)),
    };
    let (filter, handle) = reload::Layer::new(filter);
    let _ = FILTER_RELOAD.set(handle);
    tracing_subscriber::registry()
        .with(filter)
        .with(RewriteLayer)
        .init();
}

/// Retune the engine log level for the rest of the process (a per-invocation
/// `--loglevel` / `--reporter=silent` / `--silent`). `level` is a tracing
/// level token (`error`, `warn`, `info`, `debug`) or `off`. A no-op when
/// `RUST_LOG` was set at startup — an explicit `RUST_LOG` owns the filter,
/// mirroring aube's `AUBE_LOG`-wins precedence.
pub fn set_engine_loglevel(level: &str) {
    if *RUST_LOG_OWNS_FILTER.get().unwrap_or(&false) {
        return;
    }
    let Some(handle) = FILTER_RELOAD.get() else {
        return;
    };
    if let Ok(filter) = EnvFilter::try_new(engine_directives(level)) {
        let _ = handle.reload(filter);
    }
}

/// Minimal event renderer: `LEVEL message [field=value …]`, no timestamp or
/// module-path target (a Rust module path is engine internals, not user output).
/// Redundant human-facing fields may be collapsed before the whole line is
/// brand-rewritten.
struct RewriteLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RewriteLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = LineVisitor::default();
        event.record(&mut fields);
        let line = format_line(event.metadata().level().as_str(), &fields);
        eprintln!("{}", present::rewrite(&line));
    }
}

/// Render one event as `LEVEL message [field=value …]`.
///
/// The default-trust disclosure collapses to just its message: the engine
/// already names every package the floor let through, so `count`/`packages`
/// restate the list verbatim and `code` buys nothing on an allow-side notice
/// with no action to take (the deny-side warning keeps its code — that one is
/// actionable). Matched on the pre-rewrite `WARN_AUBE_*` spelling because this
/// runs *before* [`present::rewrite`] flips the prefix to `WARN_NUB_*`; swap
/// that order and the match silently stops firing.
fn format_line(level: &str, fields: &LineVisitor) -> String {
    let is_default_trust_disclosure = fields.rest.iter().any(|(name, value)| {
        *name == "code" && value == aube_codes::warnings::WARN_AUBE_DEFAULT_TRUST_BUILDS
    });
    // Upstream renders "…for {n} default-trusted package(s): {list}" — keep the
    // list, drop the count it already implies. If that wording ever moves, the
    // message survives whole rather than truncating to nothing.
    let mut message = Cow::Borrowed(fields.message.as_str());
    if is_default_trust_disclosure
        && let Some((_, packages)) = fields.message.split_once("package(s): ")
    {
        message = format!("defaultTrust: running build scripts for {packages}").into();
    }

    let mut line = format!("{level} {message}");
    for (name, value) in &fields.rest {
        if is_default_trust_disclosure && matches!(*name, "code" | "count" | "packages") {
            continue;
        }
        line.push(' ');
        line.push_str(name);
        line.push('=');
        line.push_str(value);
    }
    line
}

#[derive(Default)]
struct LineVisitor {
    message: String,
    rest: Vec<(&'static str, String)>,
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.rest.push((field.name(), value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.rest.push((field.name(), format!("{value:?}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trust_disclosure_collapses_to_the_package_list() {
        let fields = LineVisitor {
            message: "defaultTrust: running build scripts for 1 default-trusted package(s): msgpackr-extract@3.0.4".into(),
            rest: vec![
                (
                    "code",
                    aube_codes::warnings::WARN_AUBE_DEFAULT_TRUST_BUILDS.into(),
                ),
                ("count", "1".into()),
                ("packages", "[\"msgpackr-extract@3.0.4\"]".into()),
            ],
        };

        assert_eq!(
            format_line("WARN", &fields),
            "WARN defaultTrust: running build scripts for msgpackr-extract@3.0.4"
        );
    }

    /// The collapse is keyed on one code — every other engine warning keeps its
    /// fields, including the deny-side counterpart the user must act on.
    #[test]
    fn other_warnings_keep_their_fields() {
        let fields = LineVisitor {
            message: "Ignored build scripts: core-js@3.40.0".into(),
            rest: vec![
                (
                    "code",
                    aube_codes::warnings::WARN_AUBE_IGNORED_BUILD_SCRIPTS.into(),
                ),
                ("count", "1".into()),
            ],
        };

        assert_eq!(
            format_line("WARN", &fields),
            "WARN Ignored build scripts: core-js@3.40.0 code=WARN_AUBE_IGNORED_BUILD_SCRIPTS count=1"
        );
    }

    /// If upstream ever rewords the message, the packages must still reach the
    /// user — the floor is never a silent allow path.
    #[test]
    fn default_trust_disclosure_survives_an_upstream_reword() {
        let fields = LineVisitor {
            message: "defaultTrust: reworded upstream, esbuild@0.21.5".into(),
            rest: vec![(
                "code",
                aube_codes::warnings::WARN_AUBE_DEFAULT_TRUST_BUILDS.into(),
            )],
        };

        assert_eq!(
            format_line("WARN", &fields),
            "WARN defaultTrust: reworded upstream, esbuild@0.21.5"
        );
    }
}
