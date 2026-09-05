//! Phase-4 probe (epic 4.1): the injected sandbox SEAM confines a REAL dependency lifecycle script,
//! driven through the actual `aube_scripts::run_script` path — the same entry `nub install` uses.
//!
//! This proves the CONSUMER half of 4.1 by running: aube-scripts consults the injected
//! `set_script_sandbox` hook, threads the opaque confinement guard across its async spawn, and the
//! child postinstall is enforced by the shared nub-sandbox engine (Landlock). The `confine` hook
//! below MIRRORS nub-cli's `pm_engine::sandbox_closure` mapping exactly (read-anywhere,
//! write-{package_dir, jail home, /dev, write_paths}, coarse net) — the posture that reproduces
//! aube's embedded jail, so a working install cannot regress.
//!
//! Linux-only (Landlock). Each attack arm has a failing control so a block is attributable:
//!   1. compat  — confined, jail grants package_dir: the postinstall writes INSIDE package_dir -> file appears
//!   2. attack  — SAME run: the postinstall also writes to a dir OUTSIDE the allow-set -> file ABSENT
//!   3. control-unconfined — jail = None (root-style, aube runs it unjailed): same outside write -> file appears
//!   4. control-granted   — confined, jail ALSO grants the outside dir: same outside write -> file appears
//! Arms 3 and 4 discriminate: arm 2's block is the allow-set deny, not ENOENT / a broken launch.

use std::path::{Path, PathBuf};

/// Mirror of `nub-cli` `pm_engine::sandbox_closure::{confine, build_jail_policy}`.
fn confine(
    command: &mut tokio::process::Command,
    jail: &aube_scripts::ScriptJail,
    home: &Path,
) -> std::io::Result<Box<dyn Send>> {
    use serde_json::json;
    let mut fs = serde_json::Map::new();
    fs.insert("/".to_string(), json!("r"));
    fs.insert(home.to_string_lossy().into_owned(), json!("rw"));
    fs.insert(jail.package_dir.to_string_lossy().into_owned(), json!("rw"));
    for path in &jail.write_paths {
        fs.insert(path.to_string_lossy().into_owned(), json!("rw"));
    }
    let surface = json!({ "fs": fs, "net": jail.network });
    let homes = nub_sandbox::Homes {
        home: home.to_path_buf(),
        tmp: home.join("tmp"),
        cache: home.join(".cache"),
        project: jail.package_dir.clone(),
    };
    let ctx = nub_sandbox::CompileCtx::new(
        homes,
        jail.package_dir.clone(),
        nub_sandbox::ScopeCapabilities::approved(),
        std::env::vars().collect(),
    );
    let policy = nub_sandbox::compile(&surface, &ctx)
        .map_err(|e| std::io::Error::other(format!("compile: {e}")))?;
    // `as_std_mut`: tokio's Command re-exposes `pre_exec` as an inherent method and does not impl
    // the std `CommandExt` trait `confine_build_jail_command` is generic over; its inner std command
    // does, and tokio's spawn honors a `pre_exec` set there.
    let guard = nub_sandbox::confine_build_jail_command(command.as_std_mut(), &policy, None, None)
        .map_err(|d| {
            std::io::Error::other(format!(
                "confine lost {:?}: {}",
                d.lost,
                d.reason.as_deref().unwrap_or("no reason")
            ))
        })?;
    Ok(Box::new(guard))
}

async fn run_postinstall(
    label: &str,
    package_dir: &Path,
    project_root: &Path,
    manifest: &aube_manifest::PackageJson,
    script: &str,
    jail: Option<&aube_scripts::ScriptJail>,
) -> bool {
    // Returns whether the script exited 0 (the script always catches its own write errors, so a
    // non-zero exit means a launch/confinement failure — the FILE oracle is the real signal).
    match aube_scripts::run_script(
        package_dir,
        project_root,
        "node_modules",
        manifest,
        "postinstall",
        script,
        &[],
        jail,
    )
    .await
    {
        Ok(()) => true,
        Err(e) => {
            eprintln!("  [{label}] run_script error: {e}");
            false
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let base = PathBuf::from(format!("/tmp/nub-seam-{}", std::process::id()));
    let package_dir = base.join("node_modules").join("leftpad");
    let escape = base.join("escape"); // OUTSIDE {package_dir, jail home, /dev}
    std::fs::create_dir_all(&package_dir).expect("mkdir package");
    std::fs::create_dir_all(&escape).expect("mkdir escape");

    let manifest: aube_manifest::PackageJson =
        serde_json::from_value(serde_json::json!({ "name": "leftpad", "version": "1.0.0" }))
            .expect("manifest");

    let inpkg_file = package_dir.join("built.txt");
    let escape_file = escape.join("pwned.txt");
    let node_err = package_dir.join("node.err"); // package_dir is write-granted in confined arms
                                                 // Attempt BOTH writes; catch each so the process exits 0 and the FILES are the oracle. Redirect
                                                 // node's own stderr to a granted path so a run-but-fail (vs never-exec) is visible.
    let script = format!(
        "node -e \"const fs=require('fs');try{{fs.writeFileSync(process.argv[1],'x')}}catch(e){{}}try{{fs.writeFileSync(process.argv[2],'x')}}catch(e){{}}\" '{}' '{}' 2>'{}'",
        inpkg_file.display(),
        escape_file.display(),
        node_err.display()
    );

    let clean = |inpkg: &Path, esc: &Path| {
        let _ = std::fs::remove_file(inpkg);
        let _ = std::fs::remove_file(esc);
    };

    // Inject the seam ONCE (set-once OnceLock, like nub-cli's register()).
    aube_scripts::set_script_sandbox(std::sync::Arc::new(confine));

    // ---- arms 1 + 2: confined, jail grants only package_dir ----
    clean(&inpkg_file, &escape_file);
    let jail_pkg = aube_scripts::ScriptJail::new(&package_dir).with_network(false);
    let ok1 = run_postinstall(
        "compat",
        &package_dir,
        &base,
        &manifest,
        &script,
        Some(&jail_pkg),
    )
    .await;
    let compat = inpkg_file.exists();
    let attack_blocked = !escape_file.exists();
    if let Ok(err) = std::fs::read_to_string(&node_err) {
        if !err.trim().is_empty() {
            eprintln!("  [compat] node stderr:\n{err}");
        }
    }

    // ---- arm 3: unconfined (jail = None → aube runs it unjailed, hook not consulted) ----
    clean(&inpkg_file, &escape_file);
    let ok3 = run_postinstall("unconfined", &package_dir, &base, &manifest, &script, None).await;
    let unconfined_escape = escape_file.exists();

    // ---- arm 4: confined, jail ALSO grants the escape dir ----
    clean(&inpkg_file, &escape_file);
    let jail_granted = aube_scripts::ScriptJail::new(&package_dir)
        .with_network(false)
        .with_write_paths(vec![escape.clone()]);
    let ok4 = run_postinstall(
        "granted",
        &package_dir,
        &base,
        &manifest,
        &script,
        Some(&jail_granted),
    )
    .await;
    let granted_escape = escape_file.exists();

    let _ = std::fs::remove_dir_all(&base);

    println!();
    println!("1 compat            (confined, write package_dir)  -> exists={compat} ran_ok={ok1}   [want true]");
    println!("2 attack            (confined, write outside)      -> blocked={attack_blocked}   [want true]");
    println!("3 control-unconfined(jail=None, write outside)     -> exists={unconfined_escape} ran_ok={ok3}   [want true]");
    println!("4 control-granted   (confined+granted, write out)  -> exists={granted_escape} ran_ok={ok4}   [want true]");

    let pass = compat && attack_blocked && unconfined_escape && granted_escape;
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
