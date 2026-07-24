//! `nub init` through the real binary — scaffold contents, refuse-don't-
//! clobber, the JS variant, and the non-TTY prompt fallback. Everything here
//! is offline by construction (`--no-install`); the install-by-default path
//! rides the engine's own install coverage plus manual e2e. Design record:
//! wiki/commands/init.md.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// Unique temp project dir under the system temp root (never under $HOME, so
/// manifest walk-ups can't escape into stray ancestors).
fn tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-init-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Piped stdio — every run here exercises the non-TTY path (prompts must
/// fall back to defaults, never hang).
fn run_init(dir: &Path, args: &[&str]) -> Out {
    let out = Command::new(nub_binary())
        .arg("init")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .output()
        .expect("failed to spawn nub");
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        code: out.status.code().unwrap_or(-1),
    }
}

#[test]
fn scaffolds_five_files_with_identity_pin_and_type_devdeps() {
    let dir = tmpdir("basic");
    let out = run_init(&dir, &["-y", "--no-install"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    for f in [
        "package.json",
        "tsconfig.json",
        "index.ts",
        ".gitignore",
        "README.md",
    ] {
        assert!(dir.join(f).exists(), "{f} must be written");
    }
    assert!(dir.join(".git").exists(), "git init runs by default");

    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    let ver = env!("CARGO_PKG_VERSION");
    assert_eq!(pkg["packageManager"], format!("nub@{ver}"));
    assert_eq!(pkg["devEngines"]["packageManager"]["name"], "nub");
    assert_eq!(pkg["devDependencies"]["@nubjs/types"], format!("^{ver}"));
    assert!(pkg["devDependencies"]["@types/node"].is_string());
    assert!(pkg["devDependencies"]["typescript"].is_string());
    assert_eq!(pkg["type"], "module");

    let tsconfig = std::fs::read_to_string(dir.join("tsconfig.json")).unwrap();
    assert!(
        tsconfig.contains(r#""types": ["node", "@nubjs/types"]"#),
        "tsconfig must wire the type packages: {tsconfig}"
    );
    assert!(
        out.stdout.contains("next: nub index.ts"),
        "summary names the next command: {}",
        out.stdout
    );
}

#[test]
fn js_variant_skips_tsconfig_and_type_devdeps() {
    let dir = tmpdir("js");
    let out = run_init(
        &dir,
        &["-y", "--js", "--no-install", "--no-git", "--name", "My App"],
    );
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(dir.join("index.js").exists());
    assert!(!dir.join("tsconfig.json").exists(), "no tsconfig for JS");
    assert!(!dir.join(".git").exists(), "--no-git must skip git init");

    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(pkg["name"], "my-app", "--name is sanitized");
    assert!(pkg.get("devDependencies").is_none());
    assert_eq!(pkg["scripts"]["start"], "nub index.js");
}

/// `git check-ignore` is the only honest test of a `.gitignore`: the scaffold's
/// pattern list has a negation in it, so matching on file contents would pin
/// the spelling without proving the effect.
fn git_ignores(dir: &Path, path: &str) -> bool {
    Command::new("git")
        .args(["check-ignore", "-q", path])
        .current_dir(dir)
        .status()
        .expect("failed to spawn git check-ignore")
        .success()
}

#[test]
fn scoped_name_reaches_the_manifest_and_the_env_example_stays_tracked() {
    let dir = tmpdir("scoped");
    let out = run_init(&dir, &["-y", "--no-install", "--name", "@My Org/My App"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);

    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(
        pkg["name"], "@my-org/my-app",
        "the scope must survive sanitization"
    );

    for secret in [".env", ".env.local", ".env.production", ".env.development"] {
        assert!(git_ignores(&dir, secret), "{secret} must stay out of git");
    }
    assert!(
        !git_ignores(&dir, ".env.example"),
        ".env.example is the committed template — it must stay tracked"
    );
}

#[test]
fn refuses_existing_files_and_force_overwrites() {
    let dir = tmpdir("conflict");
    std::fs::write(dir.join("package.json"), "{\"name\":\"keep\"}").unwrap();
    let out = run_init(&dir, &["-y", "--no-install"]);
    assert_ne!(out.code, 0, "must refuse on conflict");
    assert!(
        out.stderr.contains("package.json") && out.stderr.contains("--force"),
        "refusal names the conflict and the override: {}",
        out.stderr
    );
    // Refusal is all-or-nothing: no sibling file may have been written.
    assert!(!dir.join("index.ts").exists());
    let kept = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(kept.contains("keep"), "refusal must not touch the file");

    let out = run_init(&dir, &["-y", "--no-install", "--force", "--no-git"]);
    assert_eq!(out.code, 0, "--force overwrites: {}", out.stderr);
    let pkg = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(!pkg.contains("keep"), "--force must replace the manifest");
}

#[test]
fn rejects_positionals_with_a_create_hint() {
    let dir = tmpdir("arg");
    let out = run_init(&dir, &["react-app"]);
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.contains("nubx create-react-app"),
        "the error hints the create flow: {}",
        out.stderr
    );
    assert!(!dir.join("package.json").exists(), "nothing scaffolded");
}

#[test]
fn empty_sanitizing_directory_name_falls_back_to_app() {
    // A non-Latin basename sanitizes to nothing; the manifest must never
    // carry an invalid `"name": ""`.
    let dir = tmpdir("unicode").join("中文项目");
    std::fs::create_dir_all(&dir).unwrap();
    let out = run_init(&dir, &["-y", "--no-install", "--no-git"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap()).unwrap();
    assert_eq!(pkg["name"], "app");
}

#[test]
fn non_tty_without_yes_takes_defaults_and_never_hangs() {
    // No `-y`: with piped stdin the prompts must self-skip to defaults —
    // the CI/piped contract.
    let dir = tmpdir("notty");
    let out = run_init(&dir, &["--no-install", "--no-git"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(dir.join("index.ts").exists(), "defaults to TypeScript");
}
