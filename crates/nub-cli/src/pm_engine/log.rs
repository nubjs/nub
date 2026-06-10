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
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Default filter: the engine's crate targets at `warn`, nothing else.
/// Mirrors the directive list in the engine's own `init_logging`
/// (`vendor/aube/crates/aube/src/startup.rs`) so warnings surface from
/// every engine crate, not just the command layer.
const ENGINE_WARN_DIRECTIVES: &str = "aube=warn,aube_registry=warn,aube_resolver=warn,\
     aube_lockfile=warn,aube_store=warn,aube_linker=warn,aube_manifest=warn,\
     aube_scripts=warn,aube_workspace=warn,aube_settings=warn,aube_util=warn";

/// Install the process-global subscriber. Call once, before any engine
/// (or nub) code can emit tracing events.
pub fn init() {
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.is_empty() => tracing_subscriber::EnvFilter::new(spec),
        _ => tracing_subscriber::EnvFilter::new(ENGINE_WARN_DIRECTIVES),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(RewriteLayer)
        .init();
}

/// Minimal event renderer: `LEVEL message [field=value …]`, no
/// timestamp, no module-path target (a Rust module path is engine
/// internals, not user output), the whole line brand-rewritten.
struct RewriteLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RewriteLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = LineVisitor::default();
        event.record(&mut fields);
        let mut line = format!("{} {}", event.metadata().level(), fields.message);
        for (name, value) in &fields.rest {
            line.push(' ');
            line.push_str(name);
            line.push('=');
            line.push_str(value);
        }
        eprintln!("{}", present::rewrite(&line));
    }
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

// Untested at the unit level on purpose: the layer's contract (engine
// warnings reach stderr, rewritten, by default) spans the global
// subscriber + fd 2, which unit tests can't observe honestly. It is
// verified at the binary level — `nub install` of a package with
// unapproved build scripts must print the WARN_NUB_IGNORED_BUILD_SCRIPTS
// line — which tests/brand-sweep/run.sh asserts.
