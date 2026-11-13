//! Nub's package-manager hypermanager: the unified PM model shared by every
//! consumer (resolution, provisioning, the `nub pm` CLI surface).
//!
//! There is exactly ONE [`Pm`] enum, ONE pin reader ([`resolve`]), ONE yarn
//! classifier, and ONE `.npmrc` reader (`workspace::scripts::npmrc_value`) —
//! except in [`registry`], which reads the project and user files separately
//! so each gets its own `${VAR}` trust.
//!
//! [`registry`] resolves a spec (exact / dist-tag / range) to a tarball + dist
//! integrity; [`extract`] unpacks the `.tgz`; [`provision`] ties them together
//! with nub's download/verify/cache machinery into a runnable, version-addressed
//! install — reusing the same provisioning skeleton as Node.

pub(crate) mod extract;
pub mod lockfile_version;
pub mod provision;
pub mod registry;
pub mod resolve;
pub mod shim;

use std::fmt;

/// The package managers Nub manages. `bun` is deliberately absent — it is out of
/// scope for v0.x (see `detect_package_manager`'s preserved bun inference).
///
/// `YarnBerry` is a distinct variant from `Yarn` because the two ship as different
/// npm packages with different pin-hash targets (see [`Pm::registry_package`]).
/// Both print `yarn` — the distinction is internal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pm {
    Npm,
    Pnpm,
    Yarn,
    YarnBerry,
}

impl Pm {
    /// The npm package provisioning downloads. The `yarn` package is classic only;
    /// Berry publishes its single-file CLI as `@yarnpkg/cli-dist` (2.4.1 onward),
    /// whose `bin/yarn.js` is byte-identical to the release on repo.yarnpkg.com.
    /// Corepack downloads the same package when pointed at a custom registry, and
    /// a Corepack Berry pin hash is over that `yarn.js`, not over the tarball.
    pub fn registry_package(self) -> &'static str {
        match self {
            Pm::Npm => "npm",
            Pm::Pnpm => "pnpm",
            Pm::Yarn => "yarn",
            Pm::YarnBerry => "@yarnpkg/cli-dist",
        }
    }

    /// The store directory under `<cache>/pm/`. Berry gets its own so classic
    /// yarn never reads Berry's recorded `latest`, and a classic range (`>=1`)
    /// can't fall back to a cached Berry release offline.
    pub fn store_dir(self) -> &'static str {
        match self {
            Pm::YarnBerry => "yarn-berry",
            pm => pm.registry_package(),
        }
    }
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

/// Lowercase-hex render of a digest (`[0xab, 0xcd]` → `"abcd"`). The one home
/// for this across the PM surface — provisioning, the registry's sha1 check,
/// and the CLI's tarball-hash all share it instead of re-deriving it inline.
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}
