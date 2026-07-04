//! Phantom-classification primitives shared by the shipped `nub` CLI's
//! pre-run phantom scan (`nub-cli`'s `verify_deps`) and the excluded
//! `nub-phantom` eval tool.
//!
//! The three pieces here are the precision-critical, must-never-diverge core:
//!
//! - [`extract`] — oxc-parsed specifier occurrences, with the guard modeling
//!   (try/catch, conditional branches → `soft`) and type-only erasure that keep
//!   the never-false-flag bar. Runs on the SAME oxc 0.132.0 parser nub
//!   transpiles with, so extraction matches what nub actually loads.
//! - [`specifier`] — bare-vs-relative-vs-nonpackage classification + package-name
//!   extraction (`lodash/fp` → `lodash`).
//! - [`builtins`] — Node builtin recognition (never a phantom).
//!
//! The *verdict* layer (what counts as a phantom against a given declared
//! surface) is deliberately NOT here: the CLI's project-source scan and the
//! eval tool's published-tarball scan ask different questions (undeclared +
//! transitively-present vs. published-consumer-unresolvable), so each owns its
//! own aggregation over these shared primitives.

pub mod builtins;
pub mod extract;
pub mod specifier;
