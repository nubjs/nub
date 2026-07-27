//! End-to-end enforcement of nub's build-jail (the policy `compile_build_jail`
//! produces, applied to a real child) — the load-bearing security assertions: the
//! package dir is writable, and home SECRETS stay both unreadable AND unwritable even
//! when the confined child runs inside a writable scratch.
//!
//! macOS only. Seatbelt (`sandbox-exec`) enforces in a plain process, so this runs in
//! `cargo test` on the dev host. Linux enforcement (Bubblewrap + the empirically
//! derived read set — project/`$tooldirs`/interpreter grants, own-pkg write, egress
//! deny) rides CI / a real-kernel VM; the read-set spike validated it via Docker+bwrap.
//!
//! macOS caveat (deliberate build-jail behavior): the DARWIN confstr scratch
//! (`/var/folders/<uid>/T`, where `tempfile` puts this fixture) stays WRITABLE — the
//! Apple toolchain's `xcrun_db` lives there and is not `TMPDIR`-redirectable. So a
//! generic "write outside the package dir" cannot be asserted for a fixture under the
//! scratch; this asserts the SECURITY-critical denials (secrets read+write) instead,
//! which hold even inside the scratch via the secret floor + the move-block re-deny.
#![cfg(target_os = "macos")]

use std::collections::BTreeMap;

fn homes(home: &std::path::Path, project: &std::path::Path) -> nub_sandbox::Homes {
    nub_sandbox::Homes {
        home: home.to_path_buf(),
        tmp: std::env::temp_dir(),
        cache: home.join("Library/Caches"),
        project: project.to_path_buf(),
    }
}

/// A native build/postinstall confined by the build-jail: it may read its own source
/// and write its own package dir, but the home secret is neither readable nor writable.
#[test]
fn build_jail_confines_writes_and_hides_secrets() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/.aube/native@1.0.0/node_modules/native");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_rsa"), b"PRIVATE-KEY-DO-NOT-LEAK").unwrap();
    // A source file in the package dir the build legitimately reads.
    std::fs::write(package_dir.join("binding.gyp"), b"{}").unwrap();
    // Project-local `.npmrc` with a hardcoded token — at the root AND nested. Both sit
    // inside the project `./` READ grant, so only the secret-file floor keeps them hidden.
    std::fs::write(
        project.join(".npmrc"),
        b"//registry.npmjs.org/:_authToken=NPMRC-TOKEN-DO-NOT-LEAK",
    )
    .unwrap();
    let nested_npmrc = project.join("packages/api");
    std::fs::create_dir_all(&nested_npmrc).unwrap();
    std::fs::write(
        nested_npmrc.join(".npmrc"),
        b"//registry.npmjs.org/:_authToken=NESTED-NPMRC-TOKEN",
    )
    .unwrap();

    // The interpreter path is only granted-read; the script doesn't run it here, so a
    // placeholder under the (real) home cache keeps the compile honest without needing
    // a provisioned Node in the test.
    let interpreter = home.join("Library/Caches/nub/node/bin/node");
    let ambient: BTreeMap<String, String> =
        [("PATH", "/usr/bin:/bin"), ("npm_package_name", "native")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        vec![interpreter],
        Vec::new(),
        ambient,
    )
    .expect("compile build-jail");

    // The confined script: (1) read its own source (ok), (2) write its own build output
    // (ok), (3) try to READ the home secret, (4) try to WRITE into the home secret dir.
    // It prints a deterministic marker per outcome so the parent asserts on captured
    // stdout rather than on file side effects alone.
    let secret = home.join(".ssh/id_rsa");
    let secret_write = home.join(".ssh/planted.txt");
    let npmrc = project.join(".npmrc");
    let npmrc_nested = nested_npmrc.join(".npmrc");
    let script = format!(
        r#"
        cat binding.gyp >/dev/null 2>&1 && echo READ_OK || echo READ_FAIL
        echo built > build_out.txt 2>/dev/null && echo WRITE_PKG_OK || echo WRITE_PKG_FAIL
        cat '{secret}' >/dev/null 2>&1 && echo SECRET_LEAK || echo SECRET_HIDDEN
        echo evil > '{secret_write}' 2>/dev/null && echo SECRET_WRITE_WROTE || echo SECRET_WRITE_BLOCKED
        cat '{npmrc}' >/dev/null 2>&1 && echo NPMRC_LEAK || echo NPMRC_HIDDEN
        cat '{npmrc_nested}' >/dev/null 2>&1 && echo NPMRC_NESTED_LEAK || echo NPMRC_NESTED_HIDDEN
        "#,
        secret = secret.display(),
        secret_write = secret_write.display(),
        npmrc = npmrc.display(),
        npmrc_nested = npmrc_nested.display(),
    );

    let spec = nub_sandbox::CommandSpec::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .cwd(&package_dir)
        .deny_search_roots([project.clone(), package_dir.clone()]);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let prepared = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail (fail-closed on error)");
    let out = prepared.output().expect("run confined script");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("READ_OK"),
        "package source must be readable:\n{stdout}"
    );
    assert!(
        stdout.contains("WRITE_PKG_OK"),
        "the package dir must be writable:\n{stdout}"
    );
    assert!(
        stdout.contains("SECRET_HIDDEN") && !stdout.contains("SECRET_LEAK"),
        "the home secret must be unreadable:\n{stdout}"
    );
    assert!(
        stdout.contains("SECRET_WRITE_BLOCKED") && !stdout.contains("SECRET_WRITE_WROTE"),
        "a write into the home secret dir must be blocked:\n{stdout}"
    );
    assert!(
        stdout.contains("NPMRC_HIDDEN") && !stdout.contains("NPMRC_LEAK"),
        "a project-local .npmrc must be unreadable:\n{stdout}"
    );
    assert!(
        stdout.contains("NPMRC_NESTED_HIDDEN") && !stdout.contains("NPMRC_NESTED_LEAK"),
        "a nested project .npmrc must be unreadable:\n{stdout}"
    );
    // Belt-and-suspenders: the planted file must not exist; the secret is untouched.
    assert!(
        !secret_write.exists(),
        "the secret-dir write must not have landed"
    );
    assert!(package_dir.join("build_out.txt").exists());
    assert_eq!(
        std::fs::read(&secret).unwrap(),
        b"PRIVATE-KEY-DO-NOT-LEAK",
        "the secret file is untouched"
    );
}

/// A FETCHED git checkout is confined as a whole, and every byte of it is attacker-authored:
/// `prepare_scratch_copy` uses `cp -a`, so a symlink committed in the repo survives into the
/// scratch tree the jail grants. Seatbelt matches the CANONICAL path, so a read through
/// `evil -> $HOME` is checked as the home path, misses the checkout's subpath allow, and
/// falls to default-deny.
///
/// The second escape is the jail's PROJECT anchor. The scratch lives in the system temp dir
/// alongside every other git dep's scratch, and the anchor used to follow the importer
/// directory — which a checkout picks itself via `workspaces: ["../**"]`. Anchoring on the
/// sibling is compiled here as the negative control: the same script, one variable changed,
/// reads the sibling's file. That contrast is what makes the denial evidence rather than a
/// hollow pass, together with the checkout's own read + write succeeding in both runs.
#[test]
fn a_fetched_checkout_reads_nothing_outside_itself_through_a_symlink_or_a_steered_anchor() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let checkout = root.path().join("checkout");
    // Another package's scratch copy, sibling to this one — what `../**` reaches for.
    let sibling = root.path().join("sibling-scratch");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(home.join(".nub-secret"), b"HOME-SECRET-DO-NOT-LEAK").unwrap();
    std::fs::write(sibling.join("scratch.txt"), b"SIBLING-SCRATCH-DO-NOT-LEAK").unwrap();
    std::fs::write(checkout.join("src/own.txt"), b"CHECKOUT-OWN-FILE").unwrap();
    // The committed symlink. `cp -a` reproduces it verbatim into the scratch copy.
    std::os::unix::fs::symlink(&home, checkout.join("evil")).unwrap();

    let ambient: BTreeMap<String, String> = [("PATH", "/usr/bin:/bin")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let script = format!(
        r#"
        cat src/own.txt 2>/dev/null && echo OWN_READ_OK || echo OWN_READ_FAIL
        echo built > out.txt 2>/dev/null && echo OWN_WRITE_OK || echo OWN_WRITE_FAIL
        cat '{symlinked}' 2>&1 | sed 's/^/SYMLINK: /'
        cat '{sibling_file}' 2>&1 | sed 's/^/SIBLING: /'
        "#,
        symlinked = checkout.join("evil/.nub-secret").display(),
        sibling_file = sibling.join("scratch.txt").display(),
    );

    let run = |project: &std::path::Path| -> String {
        let policy = nub_sandbox::compile_build_jail(
            homes(&home, project),
            &checkout,
            Vec::new(),
            Vec::new(),
            ambient.clone(),
        )
        .expect("compile build-jail");
        let spec = nub_sandbox::CommandSpec::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .cwd(&checkout)
            .deny_search_roots([project.to_path_buf(), checkout.clone()]);
        let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
        let prepared = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
            .expect("apply build-jail (fail-closed on error)");
        let out = prepared.output().expect("run confined script");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let anchored = run(&checkout);
    assert!(
        anchored.contains("CHECKOUT-OWN-FILE") && anchored.contains("OWN_READ_OK"),
        "the checkout must be readable — without this the denials below prove nothing:\n{anchored}"
    );
    assert!(
        anchored.contains("OWN_WRITE_OK"),
        "the checkout must be writable:\n{anchored}"
    );
    assert!(
        !anchored.contains("HOME-SECRET-DO-NOT-LEAK"),
        "a symlink committed in the checkout reached the home secret:\n{anchored}"
    );
    assert!(
        anchored.contains("SYMLINK: cat:") && anchored.contains("Operation not permitted"),
        "the read through the symlink must be denied EPERM, not merely empty:\n{anchored}"
    );
    assert!(
        !anchored.contains("SIBLING-SCRATCH-DO-NOT-LEAK"),
        "the checkout read a sibling scratch directory:\n{anchored}"
    );

    // One variable changed: the project anchor moves to the sibling, as a
    // `workspaces: ["../**"]` importer used to make it. The sibling now reads.
    let steered = run(&sibling);
    assert!(
        steered.contains("SIBLING-SCRATCH-DO-NOT-LEAK"),
        "control failed: anchoring the project on the sibling must expose it, otherwise \
         the anchored run's denial is not evidence that the anchor is what withheld it:\n{steered}"
    );
    assert!(
        steered.contains("OWN_READ_OK") && steered.contains("OWN_WRITE_OK"),
        "control sanity: the checkout's own grants are unaffected by the anchor:\n{steered}"
    );
}

/// The header read-grant that makes node-gyp compile offline: a confined script READS the
/// provisioned Node's `include/node` tree (passed as the per-spawn extra read), while an
/// ungranted sibling under the same store stays hidden. Models the real defect — nub's
/// provisioned Node lives outside `$tooldirs` and the interpreter grant, so absent this
/// grant node-gyp finds no headers and falls to a network download the jail denies.
#[test]
fn build_jail_grants_node_headers_and_nothing_else_under_the_store() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/.aube/native@1.0.0/node_modules/native");
    std::fs::create_dir_all(&package_dir).unwrap();

    // A provisioned Node root under nub's version store (NOT `/usr`, NOT a `$tooldir`):
    // `bin/node` + an `include/node` header tree, plus a NON-header sibling that must stay
    // unreadable (only `include/node` is granted, not the whole root).
    let node_root = home.join("Library/Caches/nub/node/22.15.0");
    let include_node = node_root.join("include/node");
    std::fs::create_dir_all(&include_node).unwrap();
    std::fs::write(include_node.join("node_api.h"), b"// napi header").unwrap();
    std::fs::create_dir_all(node_root.join("lib")).unwrap();
    std::fs::write(node_root.join("lib/private.txt"), b"NOT-A-HEADER").unwrap();
    let interpreter = node_root.join("bin/node");

    let ambient: BTreeMap<String, String> = [
        ("PATH", "/usr/bin:/bin"),
        ("npm_config_nodedir", node_root.to_str().unwrap()),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();

    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        vec![interpreter],
        vec![include_node.clone()],
        ambient,
    )
    .expect("compile build-jail");

    let header = include_node.join("node_api.h");
    let sibling = node_root.join("lib/private.txt");
    let script = format!(
        r#"
        cat '{header}' >/dev/null 2>&1 && echo HEADER_OK || echo HEADER_FAIL
        cat '{sibling}' >/dev/null 2>&1 && echo SIBLING_LEAK || echo SIBLING_HIDDEN
        "#,
        header = header.display(),
        sibling = sibling.display(),
    );

    let spec = nub_sandbox::CommandSpec::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .cwd(&package_dir)
        .deny_search_roots([project.clone(), package_dir.clone()]);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let prepared = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail (fail-closed on error)");
    let out = prepared.output().expect("run confined script");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("HEADER_OK"),
        "the provisioned Node's include/node must be readable:\n{stdout}"
    );
    assert!(
        stdout.contains("SIBLING_HIDDEN") && !stdout.contains("SIBLING_LEAK"),
        "only include/node is granted — a sibling under the store stays hidden:\n{stdout}"
    );
}
