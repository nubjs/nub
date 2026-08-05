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
        .env_remove("DLX_CONFIG_VALUE")
        .env_remove("__NUB_DLX_ENV_CONTEXT")
        .output()
        .unwrap()
}

/// Writes `$XDG_CONFIG_HOME/nub/nub.jsonc`. Returns that file's directory, so a
/// test can name the path the scope error must point the author at.
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

/// `dlx` is global-only, so the whole block — `env` and `sandbox` alike — is
/// authored in `$XDG_CONFIG_HOME/nub/nub.jsonc`, and its relative env sources
/// resolve against THAT file's directory rather than the project's.
#[cfg(unix)]
#[test]
fn dlx_env_controls_nubx_without_enabling_dlx_sandbox() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let alias = nubx_alias(temp.path());
    let cwd = temp.path().join("project");
    let config_home = temp.path().join("config");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("package.json"), r#"{ "name": "fixture" }"#).unwrap();
    let bin_dir = cwd.join("node_modules/.bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join("show-dlx-env");
    std::fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"${DLX_CONFIG_VALUE:-missing}\"\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&bin).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&bin, permissions).unwrap();
    std::fs::write(cwd.join(".env"), "DLX_CONFIG_VALUE=automatic\n").unwrap();

    // A non-Node bin does not receive the runtime's automatic `.env` values.
    // `dlx.env` is the explicit way to pass them to every kind of tool.
    let absent = run_nubx(&alias, &cwd, &config_home, &["show-dlx-env"]);
    assert_eq!(String::from_utf8_lossy(&absent.stdout).trim(), "missing");

    let global_root = write_global(
        &config_home,
        r#"{ "dlx": { "env": false, "sandbox": true } }"#,
    );
    let disabled = run_nubx(&alias, &cwd, &config_home, &["show-dlx-env"]);
    assert_eq!(String::from_utf8_lossy(&disabled.stdout).trim(), "missing");

    write_global(&config_home, r#"{ "dlx": { "env": [] } }"#);
    let empty = run_nubx(&alias, &cwd, &config_home, &["show-dlx-env"]);
    assert_eq!(String::from_utf8_lossy(&empty.stdout).trim(), "missing");

    // Beside the GLOBAL file, not the project: `./env/dlx.env` must anchor to the
    // directory of the file that named it.
    let config_env = global_root.join("env");
    std::fs::create_dir_all(&config_env).unwrap();
    std::fs::write(config_env.join("dlx.env"), "DLX_CONFIG_VALUE=source-root\n").unwrap();
    // A fully-restrictive dlx sandbox must not affect env sourcing (P6 inertness).
    write_global(
        &config_home,
        r#"{ "dlx": { "env": "./env/dlx.env", "sandbox": true } }"#,
    );
    let sourced = run_nubx(&alias, &cwd, &config_home, &["show-dlx-env"]);
    assert_eq!(
        String::from_utf8_lossy(&sourced.stdout).trim(),
        "source-root"
    );
}

#[cfg(unix)]
#[test]
fn nubx_node_suppresses_config_env_for_local_and_forced_fetch() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let alias = nubx_alias(temp.path());
    let cwd = temp.path().join("project");
    let config_home = temp.path().join("config");
    let bin_dir = cwd.join("node_modules/.bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(cwd.join("package.json"), r#"{ "name": "fixture" }"#).unwrap();

    let local_bin = bin_dir.join("show-dlx-env");
    std::fs::write(
        &local_bin,
        "#!/bin/sh\nprintf '%s\\n' \"${DLX_CONFIG_VALUE:-missing}\"\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&local_bin).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&local_bin, permissions).unwrap();

    let package = cwd.join("forced-tool");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{
            "name": "forced-tool",
            "version": "1.0.0",
            "bin": { "show-dlx-env": "bin.js" }
        }"#,
    )
    .unwrap();
    let package_bin = package.join("bin.js");
    std::fs::write(
        &package_bin,
        "#!/usr/bin/env node\nconsole.log(process.env.DLX_CONFIG_VALUE ?? 'missing')\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&package_bin).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&package_bin, permissions).unwrap();

    let global_root = write_global(
        &config_home,
        r#"{ "dlx": {
          "env": "./env/dlx.env",
          "sandbox": true
        } }"#,
    );
    std::fs::create_dir_all(global_root.join("env")).unwrap();
    std::fs::write(
        global_root.join("env/dlx.env"),
        "DLX_CONFIG_VALUE=configured\n",
    )
    .unwrap();

    let local_augmented = run_nubx(&alias, &cwd, &config_home, &["show-dlx-env"]);
    assert!(
        local_augmented.status.success(),
        "local augmented run failed: {}",
        String::from_utf8_lossy(&local_augmented.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&local_augmented.stdout).trim(),
        "configured"
    );

    let package_spec = format!("file:{}", package.display());
    let fetched_augmented = run_nubx(
        &alias,
        &cwd,
        &config_home,
        &["-y", "-p", package_spec.as_str(), "show-dlx-env"],
    );
    assert!(
        fetched_augmented.status.success(),
        "forced-fetch augmented run failed: {}",
        String::from_utf8_lossy(&fetched_augmented.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&fetched_augmented.stdout).trim(),
        "configured"
    );

    let local = run_nubx(&alias, &cwd, &config_home, &["--node", "show-dlx-env"]);
    assert!(
        local.status.success(),
        "local --node failed: {}",
        String::from_utf8_lossy(&local.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&local.stdout).trim(), "missing");

    let fetched = run_nubx(
        &alias,
        &cwd,
        &config_home,
        &["--node", "-y", "-p", package_spec.as_str(), "show-dlx-env"],
    );
    assert!(
        fetched.status.success(),
        "forced-fetch --node failed: {}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&fetched.stdout).trim(), "missing");
}

#[cfg(unix)]
#[test]
fn nub_dlx_and_x_apply_the_same_configured_environment_to_local_bins() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    let config_home = temp.path().join("config");
    let bin_dir = cwd.join("node_modules/.bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(cwd.join("package.json"), r#"{ "name": "fixture" }"#).unwrap();
    let bin = bin_dir.join("show-dlx-env");
    std::fs::write(&bin, "#!/bin/sh\nprintf '%s\\n' \"$DLX_CONFIG_VALUE\"\n").unwrap();
    let mut permissions = std::fs::metadata(&bin).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&bin, permissions).unwrap();

    let global_root = write_global(
        &config_home,
        r#"{ "dlx": { "env": "./env/dlx.env", "sandbox": true } }"#,
    );
    std::fs::create_dir_all(global_root.join("env")).unwrap();
    std::fs::write(
        global_root.join("env/dlx.env"),
        "DLX_CONFIG_VALUE=shared-alias-value\n",
    )
    .unwrap();

    for verb in ["dlx", "x"] {
        let output = Command::new(nub_binary())
            .args([verb, "show-dlx-env"])
            .current_dir(&cwd)
            .env("XDG_CONFIG_HOME", &config_home)
            .env_remove("DLX_CONFIG_VALUE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "nub {verb} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "shared-alias-value"
        );
    }

    // `env: true` is default discovery, which anchors at the PROJECT root — the
    // one arm that does not follow the config file.
    std::fs::write(cwd.join(".env"), "DLX_CONFIG_VALUE=automatic-dlx\n").unwrap();
    write_global(&config_home, r#"{ "dlx": { "env": true } }"#);
    let automatic = Command::new(nub_binary())
        .args(["x", "show-dlx-env"])
        .current_dir(&cwd)
        .env("XDG_CONFIG_HOME", &config_home)
        .env_remove("DLX_CONFIG_VALUE")
        .output()
        .unwrap();
    assert!(
        automatic.status.success(),
        "{}",
        String::from_utf8_lossy(&automatic.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&automatic.stdout).trim(),
        "automatic-dlx"
    );

    // `--env-file` is a CLI path, resolved from the invocation cwd.
    std::fs::write(cwd.join("explicit.env"), "DLX_CONFIG_VALUE=cli-wins\n").unwrap();
    write_global(&config_home, r#"{ "dlx": { "env": "./env/dlx.env" } }"#);
    let cli = Command::new(nub_binary())
        .args(["--env-file", "explicit.env", "dlx", "show-dlx-env"])
        .current_dir(&cwd)
        .env("XDG_CONFIG_HOME", &config_home)
        .env_remove("DLX_CONFIG_VALUE")
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&cli.stdout).trim(), "cli-wins");

    let disabled = Command::new(nub_binary())
        .args(["--no-env-file", "x", "show-dlx-env"])
        .current_dir(&cwd)
        .env("XDG_CONFIG_HOME", &config_home)
        .env_remove("DLX_CONFIG_VALUE")
        .output()
        .unwrap();
    assert!(
        disabled.status.success(),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&disabled.stdout).trim(), "");
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
