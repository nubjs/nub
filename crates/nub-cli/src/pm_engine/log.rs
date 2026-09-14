//! Tracing subscriber for nub and the embedded engine.
//!
//! The engine reports what a user must see through its own reporter and prints
//! no tracing events unless a developer asks, so nothing is enabled by default.
//! `RUST_LOG` sets the filter when a developer does ask. Each rendered line
//! still passes through [`present::rewrite`], so a debugging session cannot
//! print a credential an event carried.

use super::present;
use tracing::field::{Field, Visit};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Install the process-global subscriber. Call once, before any engine
/// (or nub) code can emit tracing events.
pub fn init() {
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.is_empty() => EnvFilter::new(spec),
        _ => EnvFilter::new("off"),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(RewriteLayer)
        .init();
}

/// Minimal event renderer: `LEVEL message [field=value …]`, no timestamp or
/// module-path target (a Rust module path is engine internals, not user output).
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
