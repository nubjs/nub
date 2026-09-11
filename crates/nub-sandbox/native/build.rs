use std::path::{Path, PathBuf};

const SOURCES: &[&str] = &[
    "detours.cpp",
    "modules.cpp",
    "disasm.cpp",
    "image.cpp",
    "creatwth.cpp",
    "disolx86.cpp",
    "disolx64.cpp",
    "disolia64.cpp",
    "disolarm.cpp",
    "disolarm64.cpp",
];

fn compiler(target: &str) -> cc::Build {
    let mut build = cc::Build::new();
    build
        .target(target)
        .cpp(true)
        .opt_level(2)
        .include("native/detours")
        .flag("/std:c++17")
        .flag("/EHsc")
        .define("WIN32_LEAN_AND_MEAN", None)
        .warnings(false);
    build
}

pub fn build() {
    println!("cargo:rerun-if-changed=native");
    let target = std::env::var("TARGET").expect("Cargo target");
    if !target.ends_with("-pc-windows-msvc") {
        return;
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    let root =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo crate directory"));
    for (arch, target) in [
        ("x64", "x86_64-pc-windows-msvc"),
        ("arm64", "aarch64-pc-windows-msvc"),
    ] {
        let dir = out.join(arch);
        std::fs::create_dir_all(&dir).expect("native adapter build directory");
        // DLLs carry their CRT; the static parent library follows Rust's CRT mode.
        let mut command = compiler(target)
            .static_crt(true)
            .get_compiler()
            .to_command();
        command.current_dir(&dir).arg("/LD");
        for source in std::iter::once(PathBuf::from("native/compat.cpp")).chain(
            SOURCES
                .iter()
                .map(|source| Path::new("native/detours").join(source)),
        ) {
            command.arg(root.join(source));
        }
        // cc's include path is relative to the crate; the compiler runs in OUT_DIR.
        command.arg(format!("/I{}", root.join("native/detours").display()));
        command
            .args(["/link", "/EXPORT:SandboxCompatMarker,@1", "advapi32.lib"])
            .arg(format!(
                "/OUT:{}",
                out.join(format!("compat-{arch}.dll")).display()
            ));
        let status = command.status().expect("MSVC native adapter compiler");
        assert!(
            status.success(),
            "building {arch} sandbox adapter failed: {status}"
        );
    }
    let mut parent = compiler(&target);
    parent
        .define("SANDBOX_COMPAT_HOST", None)
        .file("native/compat.cpp");
    for source in SOURCES {
        parent.file(Path::new("native/detours").join(source));
    }
    parent.compile("sandbox_compat_host");
}
