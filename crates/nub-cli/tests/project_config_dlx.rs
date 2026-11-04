use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn nubx_alias(root: &Path) -> PathBuf {
    let alias = root.join(if cfg!(windows) { "nubx.exe" } else { "nubx" });
    #[cfg(unix)]
    std::os::unix::fs::symlink(nub_binary(), &alias).unwrap();
    #[cfg(windows)]
    std::fs::copy(nub_binary(), &alias).unwrap();
    alias
}

fn run_nubx(alias: &Path, cwd: &Path, config_home: &Path, args: &[&str]) -> Output {
    Command::new(alias)
        .args(args)
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_CACHE_HOME", config_home.join("cache"))
        .env("XDG_DATA_HOME", config_home.join("data"))
        .env_remove("CI")
        .output()
        .unwrap()
}

/// The `dlx` section is global-only, so every fixture here writes
/// `$XDG_CONFIG_HOME/nub/nub.jsonc`. Returns that file's directory, so a test
/// can name the path the scope error must point the author at.
fn write_global(config_home: &Path, body: &str) -> PathBuf {
    let dir = config_home.join("nub");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("nub.jsonc"), body).unwrap();
    dir
}

#[test]
fn global_typed_and_legacy_consent_feed_the_implicit_nubx_gate() {
    let temp = tempfile::tempdir().unwrap();
    let alias = nubx_alias(temp.path());
    let cwd = temp.path().join("project");
    let config_home = temp.path().join("config");
    std::fs::create_dir_all(&cwd).unwrap();

    for body in [
        r#"{ "dlx": { "consent": "never" } }"#,
        r#"{ "exec": { "implicitDlx": "never" } }"#,
    ] {
        let _ = write_global(&config_home, body);
        let output = run_nubx(
            &alias,
            &cwd,
            &config_home,
            &["definitely-not-installed-project-config-dlx"],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains("To run a package from the remote registry, try `nub dlx`"),
            "typed/legacy never must use the locked refusal before network: {stderr}"
        );
    }

    let _ = write_global(&config_home, r#"{ "dlx": { "consent": "prompt" } }"#);
    let output = run_nubx(
        &alias,
        &cwd,
        &config_home,
        &["definitely-not-installed-project-config-dlx"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("without a terminal to confirm at"),
        "{stderr}"
    );
}

#[test]
fn a_project_dlx_block_aborts_the_command_and_names_the_global_file() {
    let temp = tempfile::tempdir().unwrap();
    let alias = nubx_alias(temp.path());
    let cwd = temp.path().join("project");
    let config_home = temp.path().join("config");
    std::fs::create_dir_all(&cwd).unwrap();
    let global_root = write_global(&config_home, r#"{ "dlx": { "consent": "never" } }"#);
    // The widening a project file must not be able to perform: `prompt` against
    // a global `never`.
    let project_file = cwd.join("nub.jsonc");
    std::fs::write(&project_file, r#"{ "dlx": { "consent": "prompt" } }"#).unwrap();

    let output = run_nubx(&alias, &cwd, &config_home, &["any-tool"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the run must abort: {stderr}");
    let message = stderr
        .trim()
        .strip_prefix("Error: `dlx` in ")
        .unwrap_or_else(|| panic!("scope error prefix: {stderr}"));
    let (reported_source, message) = message
        .split_once(" is configured globally: move it to ")
        .unwrap_or_else(|| panic!("scope error source/destination separator: {stderr}"));
    let (reported_destination, _) = message
        .split_once(". Settings that configure")
        .unwrap_or_else(|| panic!("scope error destination suffix: {stderr}"));
    assert_eq!(
        PathBuf::from(reported_source).canonicalize().unwrap(),
        project_file.canonicalize().unwrap(),
        "the abort must name the misplaced project file: {stderr}"
    );
    assert_eq!(
        PathBuf::from(reported_destination).canonicalize().unwrap(),
        global_root.join("nub.jsonc").canonicalize().unwrap(),
        "the abort must name the global destination: {stderr}"
    );
}
