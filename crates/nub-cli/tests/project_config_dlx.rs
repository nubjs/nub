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

#[test]
fn global_dlx_env_controls_nubx_without_enabling_dlx_sandbox() {
    let temp = tempfile::tempdir().unwrap();
    let alias = nubx_alias(temp.path());
    let cwd = temp.path().join("project");
    let config_home = temp.path().join("config");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("package.json"), r#"{ "name": "fixture" }"#).unwrap();
    std::fs::write(
        cwd.join("print.mjs"),
        "console.log(process.env.DLX_CONFIG_VALUE ?? 'missing')\n",
    )
    .unwrap();
    std::fs::write(cwd.join(".env"), "DLX_CONFIG_VALUE=automatic\n").unwrap();

    let absent = run_nubx(&alias, &cwd, &config_home, &["print.mjs"]);
    assert_eq!(String::from_utf8_lossy(&absent.stdout).trim(), "automatic");

    write_global(
        &config_home,
        r#"{ "dlx": { "env": false, "sandbox": { "net": false } } }"#,
    );
    let disabled = run_nubx(&alias, &cwd, &config_home, &["print.mjs"]);
    assert_eq!(String::from_utf8_lossy(&disabled.stdout).trim(), "missing");

    write_global(&config_home, r#"{ "dlx": { "env": [] } }"#);
    let empty = run_nubx(&alias, &cwd, &config_home, &["print.mjs"]);
    assert_eq!(String::from_utf8_lossy(&empty.stdout).trim(), "missing");

    let global_root = config_home.join("nub");
    std::fs::create_dir_all(global_root.join("env")).unwrap();
    std::fs::write(
        global_root.join("env/dlx.env"),
        "DLX_CONFIG_VALUE=source-root\n",
    )
    .unwrap();
    write_global(
        &config_home,
        r#"{ "dlx": { "env": "./env/dlx.env", "sandbox": false } }"#,
    );
    let sourced = run_nubx(&alias, &cwd, &config_home, &["print.mjs"]);
    assert_eq!(
        String::from_utf8_lossy(&sourced.stdout).trim(),
        "source-root"
    );

    std::fs::write(cwd.join("explicit.env"), "DLX_CONFIG_VALUE=cli\n").unwrap();
    write_global(&config_home, r#"{ "dlx": { "env": false } }"#);
    let cli = run_nubx(
        &alias,
        &cwd,
        &config_home,
        &["--env-file=explicit.env", "print.mjs"],
    );
    assert_eq!(String::from_utf8_lossy(&cli.stdout).trim(), "cli");
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

    let global_root = config_home.join("nub");
    std::fs::create_dir_all(global_root.join("env")).unwrap();
    std::fs::write(
        global_root.join("env/dlx.env"),
        "DLX_CONFIG_VALUE=shared-alias-value\n",
    )
    .unwrap();
    write_global(
        &config_home,
        r#"{ "dlx": { "env": "./env/dlx.env", "sandbox": true } }"#,
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
