//! `microbe <name[@spec]> <dir> [--registry <url>]` — install into `<dir>/node_modules` and
//! print what landed. The library is the product; this binary exists to measure it and to
//! try it from a shell.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let mut registry = None;
    while let Some(a) = args.next() {
        if a == "--registry" {
            registry = args.next();
        } else {
            positional.push(a);
        }
    }
    let [spec, dir] = positional.as_slice() else {
        eprintln!("usage: microbe <name[@spec]> <dir> [--registry <url>]");
        return ExitCode::from(2);
    };
    let run = || -> Result<microbe::Installed, microbe::Error> {
        let mut m = microbe::Microbe::new()?;
        if let Some(r) = &registry {
            m = m.registry(r);
        }
        m.install(spec, Path::new(dir))
    };
    match run() {
        Ok(installed) => {
            println!(
                "{}@{} ({} packages) -> {}",
                installed.name,
                installed.version,
                installed.packages,
                installed.dir.display()
            );
            for (cmd, path) in &installed.bins {
                println!("  bin {cmd} -> {}", path.display());
            }
            if !installed.skipped_install_scripts.is_empty() {
                println!(
                    "  install scripts not run: {}",
                    installed.skipped_install_scripts.join(", ")
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("microbe: {e}");
            ExitCode::FAILURE
        }
    }
}
