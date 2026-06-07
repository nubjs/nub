//! Nub's package-manager hypermanager: the unified PM model shared by every
//! consumer (resolution, provisioning, the CLI passthrough layer).
//!
//! There is exactly ONE [`Pm`] enum, ONE pin reader ([`resolve`]), ONE yarn
//! classifier, and ONE `.npmrc` reader (`workspace::scripts::npmrc_value`).
//! Later phases (P1) fill in [`registry`], [`extract`], and [`provision`].

pub mod extract;
pub mod provision;
pub mod registry;
pub mod resolve;

use std::fmt;

/// The package managers Nub manages. `bun` is deliberately absent — it is out of
/// scope for v0.x (see `detect_package_manager`'s preserved bun inference).
///
/// `YarnBerry` is a distinct variant from `Yarn` because the provisioning engine
/// branches hard on it (Berry ships as a committed release / Plug'n'Play, classic
/// Yarn downloads a tarball). Both print `yarn` — the distinction is internal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pm {
    Npm,
    Pnpm,
    Yarn,
    YarnBerry,
}

impl fmt::Display for Pm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Pm::Npm => "npm",
            Pm::Pnpm => "pnpm",
            Pm::Yarn | Pm::YarnBerry => "yarn",
        };
        f.write_str(name)
    }
}
