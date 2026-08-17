fn main() {
    // Reserve Mach-O header padding so `nub compile`'s libsui section injection can
    // add the `__SUI` load command WITHOUT shifting `__TEXT`. libsui's arm64
    // `build()` copies the post-load-command data from `sizeofcmds + header_size`,
    // which grows when the injected segment command is added; without spare header
    // pad it skips `seg.cmdsize` bytes of code and the result traps (SIGILL). A
    // compact optimized binary has no natural slack, so reserve it explicitly.
    // 0x1000 comfortably exceeds the injected command size (~152 bytes).
    // A build script's own `cfg(target_os)` describes the HOST running this
    // script, not the launcher TARGET. Cargo exposes the latter explicitly.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-headerpad,0x1000");
    }
}
