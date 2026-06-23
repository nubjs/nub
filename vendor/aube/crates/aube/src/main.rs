// Thin binary wrapper: the whole CLI lives in the `aube` library crate
// (src/lib.rs) so the command layer can also be embedded as a library.
// What belongs here is binary-level / aube-specific policy the library
// must not impose on embedders: the global-allocator choice, the `main`
// that forwards to `aube::cli_main`, and the aube-specific `usage`
// (usage.jdx.dev KDL) command, which is aube's own tooling rather than
// part of the embeddable command layer. The `aubr` / `aubx` multicall
// shims `include!` this file, so all three bins stay byte-identical in
// behavior.

// mimalloc as global allocator on release builds. Cuts linker-phase
// wall time and peak RSS on large installs. Per-thread heaps suit
// rayon work-stealing and tokio's blocking pool. Gated on
// `not(debug_assertions)` so `cargo run` and `cargo test` keep the
// system allocator, which keeps Valgrind, ASAN, and Miri happy.
// `secure` feature skipped. aube's hot path is tarball extraction
// with bounded input, not a sandbox boundary.
#[cfg(all(feature = "mimalloc", not(debug_assertions)))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // The standalone `aube` binary runs with aube's own embedder profile.
    // Embedders call `aube::cli_main` with their own `&'static Embedder`
    // instead (and `cli_main_with_defaults` to also seed setting defaults).
    let embedder = &aube_util::identity::AUBE;

    // `aube usage` prints a usage.jdx.dev KDL spec for the CLI (consumed by
    // `mise render` and the CLI docs build). It's aube-specific tooling, not
    // part of the embeddable command layer — a downstream embedder ships its
    // own top-level usage/completions — so it's intercepted here in the binary
    // before `cli_main` rather than carried as a subcommand in the lib. Only
    // the standalone `aube` invocation reaches it: the `aubr`/`aubx` multicall
    // shims rewrite their argv to `run`/`dlx`, so a `usage` token there is
    // never a top-level command.
    if is_usage_invocation() {
        let mut cmd = aube::command();
        clap_usage::generate(&mut cmd, embedder.name, &mut std::io::stdout());
        return;
    }

    // The binary owns the single `std::process::exit`: `cli_main` returns the
    // code so the library stays embed-safe (a host driving it in-process is
    // never hard-killed), and the standalone binary terminates with it here.
    std::process::exit(aube::cli_main(embedder));
}

/// True for a standalone `aube usage` invocation: argv[0] resolves to the
/// `aube` binary (not the `aubr`/`aubx` multicall shims) and the first
/// argument is `usage`.
fn is_usage_invocation() -> bool {
    let mut args = std::env::args_os();
    let invoked_as_aube = args
        .next()
        .as_deref()
        .map(std::path::Path::new)
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .map(|stem| !matches!(stem.to_ascii_lowercase().as_str(), "aubr" | "aubx"))
        .unwrap_or(true);
    invoked_as_aube && args.next().as_deref() == Some(std::ffi::OsStr::new("usage"))
}
