//! Ubuntu enforcement-matrix probe (measurement-only; not part of the product).
//!
//! usage: matrix_probe <agent|buildjail> <confined|control>
//!
//! Builds one fixture, compiles the tier's policy, applies it through the SAME
//! `apply_with_runtime` seam both product tiers use, and runs a child that reports
//! a positive control (its own package dir read+write) alongside the denial of a
//! seeded secret outside every grant. The `control` arm re-runs the identical spawn
//! under `{"fs": true}` so a denial can never be confused with a missing file or a
//! failed spawn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nub_sandbox::compiler::{CompileCtx, ScopeCapabilities, ShellRunner};
use nub_sandbox::matcher::Homes;
use nub_sandbox::{CommandSpec, apply_with_runtime, compile, earliest_bootstrap};

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn main() {
    // MUST be the first action: it self-dispatches the monitor/describe/probe sentinels.
    let runtime = match earliest_bootstrap() {
        Ok(runtime) => runtime,
        Err(error) => {
            println!("BOOTSTRAP=failed");
            println!("BOOTSTRAP_ERR={error}");
            std::process::exit(3);
        }
    };
    println!("BOOTSTRAP=ok");

    let mut args = std::env::args().skip(1);
    let tier = args.next().unwrap_or_else(|| "agent".to_string());
    let arm = args.next().unwrap_or_else(|| "confined".to_string());
    println!("TIER={tier}");
    println!("ARM={arm}");

    // `setup` / `status` / `teardown` run the EXACT production functions behind
    // `nub sandbox {setup,status,teardown}` (nub-cli's run_sandbox_admin calls these).
    #[cfg(target_os = "linux")]
    match tier.as_str() {
        "setup" => {
            match nub_sandbox::linux_admin::setup() {
                Ok(report) => println!("SETUP=ok\n{report}"),
                Err(error) => println!("SETUP=failed\n{error}"),
            }
            println!("PROBE_DONE");
            return;
        }
        "status" => {
            match nub_sandbox::linux_admin::status() {
                Ok(report) => println!("STATUS=ok\n{report}"),
                Err(error) => println!("STATUS=failed\n{error}"),
            }
            println!("PROBE_DONE");
            return;
        }
        "teardown" => {
            match nub_sandbox::linux_admin::teardown() {
                Ok(report) => println!("TEARDOWN=ok\n{report}"),
                Err(error) => println!("TEARDOWN=failed\n{error}"),
            }
            println!("PROBE_DONE");
            return;
        }
        _ => {}
    }

    let base = std::env::var("NUBMX_BASE").unwrap_or_else(|_| {
        format!(
            "{}/nubmx-fixture-{}",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
            std::process::id()
        )
    });
    let base = PathBuf::from(base);
    let _ = std::fs::remove_dir_all(&base);
    let proj = base.join("proj");
    let home = base.join("home");
    let outside = base.join("outside");
    let package_dir = proj.join("node_modules/.aube/native@1.0.0/node_modules/native");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(proj.join("pub.txt"), b"PUBLIC").unwrap();
    std::fs::write(package_dir.join("binding.gyp"), b"{}").unwrap();
    std::fs::write(home.join(".ssh/id_rsa"), b"IDRSA-DO-NOT-LEAK").unwrap();
    std::fs::write(outside.join("secret.txt"), b"OUTSIDE-SECRET-DO-NOT-LEAK").unwrap();

    // Canonicalize AFTER creation so the grants and the child's paths agree on the
    // real path (a symlinked /tmp or /home would otherwise split them).
    let base = std::fs::canonicalize(&base).unwrap();
    let proj = base.join("proj");
    let home = base.join("home");
    let outside = base.join("outside");
    let package_dir = proj.join("node_modules/.aube/native@1.0.0/node_modules/native");

    let homes = Homes {
        home: home.clone(),
        tmp: std::env::temp_dir(),
        cache: home.join(".cache"),
        project: proj.clone(),
    };

    let script = format!(
        r#"
echo POSTINSTALL_RAN
cat '{proj_pub}' >/dev/null 2>&1 && echo PROJ_READ_OK || echo PROJ_READ_DENIED
cat '{pkg_src}' >/dev/null 2>&1 && echo PKG_READ_OK || echo PKG_READ_DENIED
echo built > '{pkg_out}' 2>/dev/null && echo PKG_WRITE_OK || echo PKG_WRITE_DENIED
if cat '{secret}' >/dev/null 2>&1; then echo SECRET_LEAK; else echo SECRET_DENIED; fi
echo "SECRET_ERR=$(cat '{secret}' 2>&1 >/dev/null | head -1)"
if cat '{home_secret}' >/dev/null 2>&1; then echo HOMESECRET_LEAK; else echo HOMESECRET_DENIED; fi
echo "HOMESECRET_ERR=$(cat '{home_secret}' 2>&1 >/dev/null | head -1)"
"#,
        proj_pub = s(&proj.join("pub.txt")),
        pkg_src = s(&package_dir.join("binding.gyp")),
        pkg_out = s(&package_dir.join("probe_out.txt")),
        secret = s(&outside.join("secret.txt")),
        home_secret = s(&home.join(".ssh/id_rsa")),
    );

    let ambient: BTreeMap<String, String> = [("PATH", "/usr/bin:/bin"), ("npm_package_name", "native")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let ctx = || CompileCtx {
        homes: homes.clone(),
        cwd: proj.clone(),
        policy_file: None,
        caps: ScopeCapabilities::approved(),
        ambient_env: ambient.clone(),
        document: serde_json::Value::Null,
        interpreter: Vec::new(),
        runner: Box::new(ShellRunner),
    };

    let policy = match (tier.as_str(), arm.as_str()) {
        (_, "control") => compile(&serde_json::json!({ "fs": true }), &ctx()).expect("compile control"),
        ("agent", _) => compile(
            &serde_json::json!({ "fs": { "./": "rw" } }),
            &ctx(),
        )
        .expect("compile agent policy"),
        ("buildjail", _) => nub_sandbox::compile_build_jail(
            homes.clone(),
            &package_dir,
            Vec::new(),
            Vec::new(),
            ambient.clone(),
        )
        .expect("compile build-jail policy"),
        (other, _) => {
            println!("TIER_UNKNOWN={other}");
            std::process::exit(2);
        }
    };

    let cwd = if tier == "buildjail" {
        package_dir.clone()
    } else {
        proj.clone()
    };
    let mut spec = CommandSpec::new("/bin/sh").arg("-c").arg(&script).cwd(&cwd);
    if nub_sandbox::requires_deny_search_roots(&policy) {
        spec = spec.deny_search_roots([proj.clone(), package_dir.clone()]);
    }

    match apply_with_runtime(&policy, spec, &runtime) {
        Ok(prepared) => {
            println!("APPLY=ok");
            println!("DEGRADED_LOST={}", prepared.degradation.lost.join(","));
            if let Some(reason) = prepared.degradation.reason.clone() {
                println!("DEGRADED_REASON={}", reason.replace('\n', " | "));
            }
            match prepared.output() {
                Ok(out) => {
                    println!("EXIT={}", out.status.code().unwrap_or(-1));
                    print!("{}", String::from_utf8_lossy(&out.stdout));
                    let err = String::from_utf8_lossy(&out.stderr);
                    if !err.trim().is_empty() {
                        println!("CHILD_STDERR={}", err.replace('\n', " | "));
                    }
                }
                Err(error) => println!("SPAWN_FAILED={error}"),
            }
        }
        Err(degradation) => {
            println!("APPLY=failed");
            println!("FAILED_LOST={}", degradation.lost.join(","));
            println!(
                "FAILED_REASON={}",
                degradation
                    .reason
                    .unwrap_or_else(|| "<none>".to_string())
                    .replace('\n', " | ")
            );
        }
    }
    println!("PROBE_DONE");
    let _ = std::fs::remove_dir_all(&base);
}
