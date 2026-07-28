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

fn write_global(config_home: &Path, body: &str) {
    let dir = config_home.join("nub");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("nub.jsonc"), body).unwrap();
}

fn write_project(cwd: &Path, body: &str) {
    std::fs::write(cwd.join("nub.jsonc"), body).unwrap();
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
        write_global(&config_home, body);
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

    write_global(&config_home, r#"{ "dlx": { "consent": "prompt" } }"#);
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

#[cfg(unix)]
#[test]
fn project_dlx_env_controls_nubx_without_enabling_dlx_sandbox() {
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
    // `dlx.envFile` is the explicit way to pass them to every kind of tool.
    let absent = run_nubx(&alias, &cwd, &config_home, &["show-dlx-env"]);
    assert_eq!(String::from_utf8_lossy(&absent.stdout).trim(), "missing");

    write_project(
        &cwd,
        r#"{ "dlx": { "envFile": false, "sandbox": { "net": false } } }"#,
    );
    let disabled = run_nubx(&alias, &cwd, &config_home, &["show-dlx-env"]);
    assert_eq!(String::from_utf8_lossy(&disabled.stdout).trim(), "missing");

    write_project(&cwd, r#"{ "dlx": { "envFile": [] } }"#);
    let empty = run_nubx(&alias, &cwd, &config_home, &["show-dlx-env"]);
    assert_eq!(String::from_utf8_lossy(&empty.stdout).trim(), "missing");

    let project_env = cwd.join("env");
    std::fs::create_dir_all(&project_env).unwrap();
    std::fs::write(
        project_env.join("dlx.env"),
        "DLX_CONFIG_VALUE=source-root\n",
    )
    .unwrap();
    // A fully-restrictive dlx sandbox must not affect env sourcing (P6 inertness).
    write_project(
        &cwd,
        r#"{ "dlx": { "envFile": "./env/dlx.env", "sandbox": { "fs": false, "net": false, "env": false } } }"#,
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

    std::fs::create_dir_all(cwd.join("env")).unwrap();
    std::fs::write(cwd.join("env/dlx.env"), "DLX_CONFIG_VALUE=configured\n").unwrap();
    write_project(
        &cwd,
        r#"{ "dlx": {
          "envFile": "./env/dlx.env",
          "sandbox": { "fs": false, "net": false, "env": false }
        } }"#,
    );

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

    std::fs::create_dir_all(cwd.join("env")).unwrap();
    std::fs::write(
        cwd.join("env/dlx.env"),
        "DLX_CONFIG_VALUE=shared-alias-value\n",
    )
    .unwrap();
    write_project(
        &cwd,
        r#"{ "dlx": { "envFile": "./env/dlx.env", "sandbox": true } }"#,
    );

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

    std::fs::write(cwd.join(".env"), "DLX_CONFIG_VALUE=automatic-dlx\n").unwrap();
    write_project(&cwd, r#"{ "dlx": { "envFile": true } }"#);
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

    std::fs::write(cwd.join("explicit.env"), "DLX_CONFIG_VALUE=cli-wins\n").unwrap();
    write_project(&cwd, r#"{ "dlx": { "envFile": "./env/dlx.env" } }"#);
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
