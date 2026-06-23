//! Compile-time smoke test for the library seam: an external consumer can
//! construct [`InstallOptions`] and reach [`aube::commands::install::run`]
//! without going through the CLI. `cargo build --example embed_smoke` is
//! the check; running it performs no install and touches no project.

use aube::commands::install::{FrozenMode, InstallOptions};

fn main() {
    let opts = InstallOptions::with_mode(FrozenMode::Prefer);
    // Name the entry point so the seam (not just the options struct) is
    // proven to resolve and link.
    let _entry = aube::commands::install::run;
    println!("aube embeds: install mode = {:?}", opts.mode);
}
