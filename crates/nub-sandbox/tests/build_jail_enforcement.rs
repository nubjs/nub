//! End-to-end enforcement of nub's build-jail (the policy `compile_build_jail`
//! produces, applied to a real child) — the load-bearing security assertions: the
//! package dir is writable, and home SECRETS stay both unreadable AND unwritable even
//! when the confined child runs inside a writable scratch.
//!
//! macOS only. Seatbelt (`sandbox-exec`) enforces in a plain process, so this runs in
//! `cargo test` on the dev host. Linux enforcement (Bubblewrap + the empirically
//! derived read set — project/`$tooldirs`/interpreter grants, own-pkg write, egress
//! curated to `$downloads`) rides CI / a real-kernel VM; the read-set spike validated it
//! via Docker+bwrap.
//!
//! macOS: the DARWIN confstr scratch (`/var/folders/<uid>/T`, where `tempfile` puts this
//! fixture) is HIDDEN like the rest of the shared tmp. `$tmp` grants a private per-run dir
//! plus exactly one documented carve-out — the Apple toolchain's `xcrun_db`, which is not
//! `TMPDIR`-redirectable — and the policy's own explicit grants are re-opened inside it, so
//! a fixture placed there still builds while the ambient scratch stays withheld. That is
//! what makes the write assertions below meaningful: the fake home sits inside the scratch
//! and is unwritable because nothing grants it, not because a deny carves it back out.
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
    // `.npmrc` with a hardcoded token, at the PACKAGE DIR's root AND nested inside it. Both
    // are READABLE under the grant-only model — the package dir is granted rw — and the
    // probes below pin that as the contract. They stay in the fixture because they mark the
    // exact boundary of what the jail does and does not protect: a dependency's own files,
    // yes; the consumer's, no.
    std::fs::write(
        package_dir.join(".npmrc"),
        b"//registry.npmjs.org/:_authToken=NPMRC-TOKEN-DO-NOT-LEAK",
    )
    .unwrap();
    let nested_npmrc = package_dir.join("src");
    std::fs::create_dir_all(&nested_npmrc).unwrap();
    std::fs::write(
        nested_npmrc.join(".npmrc"),
        b"//registry.npmjs.org/:_authToken=NESTED-NPMRC-TOKEN",
    )
    .unwrap();
    // The consumer's top-level manifest is granted as ONE FILE; the rest of its tree is
    // not. Both halves are probed below — the narrowing is only real if the second holds.
    std::fs::write(project.join("package.json"), b"CONSUMER-MANIFEST-VISIBLE").unwrap();
    std::fs::write(project.join("secrets.ts"), b"CONSUMER-SOURCE-DO-NOT-LEAK").unwrap();

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
        None,
        None,
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
    let npmrc = package_dir.join(".npmrc");
    let npmrc_nested = nested_npmrc.join(".npmrc");
    let script = format!(
        r#"
        cat binding.gyp >/dev/null 2>&1 && echo READ_OK || echo READ_FAIL
        echo built > build_out.txt 2>/dev/null && echo WRITE_PKG_OK || echo WRITE_PKG_FAIL
        cat '{secret}' >/dev/null 2>&1 && echo SECRET_LEAK || echo SECRET_HIDDEN
        echo evil > '{secret_write}' 2>/dev/null && echo SECRET_WRITE_WROTE || echo SECRET_WRITE_BLOCKED
        cat '{npmrc}' >/dev/null 2>&1 && echo NPMRC_LEAK || echo NPMRC_HIDDEN
        cat '{npmrc_nested}' >/dev/null 2>&1 && echo NPMRC_NESTED_LEAK || echo NPMRC_NESTED_HIDDEN
        cat '{manifest}' 2>/dev/null && echo MANIFEST_READ_OK || echo MANIFEST_DENIED
        cat '{consumer_src}' 2>/dev/null && echo SOURCE_READ_OK || echo SOURCE_DENIED
        "#,
        secret = secret.display(),
        secret_write = secret_write.display(),
        npmrc = npmrc.display(),
        npmrc_nested = npmrc_nested.display(),
        manifest = project.join("package.json").display(),
        consumer_src = project.join("secrets.ts").display(),
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
    // An `.npmrc` INSIDE the package dir is readable, and that is the model rather than a
    // gap. The build jail compiles to a pure allowlist, and the package dir is granted
    // read-WRITE outright — the script can overwrite or delete this file whenever it likes,
    // so denying the read protected nothing. It is also the dependency's OWN shipped file,
    // not a consumer credential. Expressing the old deny meant a deny nested inside a grant,
    // which Landlock (no deny primitive) and Windows AppContainer (a deny-ACE naming the
    // container's own SID is inert against its child) cannot enforce at all — it rejected
    // every read-granting build-jail policy on Windows. The consumer's real secrets are
    // withheld by being outside every grant, which is what the assertions above and below
    // pin (`SECRET_HIDDEN`, `SOURCE_DENIED`).
    assert!(
        stdout.contains("NPMRC_LEAK"),
        "the package dir is granted rw, so its own .npmrc is readable by design:\n{stdout}"
    );
    assert!(
        stdout.contains("NPMRC_NESTED_LEAK"),
        "...and likewise one nested inside the package dir:\n{stdout}"
    );
    // The narrowed project read: the top-level manifest is readable, everything else in
    // the consumer's tree is not.
    assert!(
        stdout.contains("MANIFEST_READ_OK") && stdout.contains("CONSUMER-MANIFEST-VISIBLE"),
        "the consumer's top-level package.json must be readable:\n{stdout}"
    );
    assert!(
        stdout.contains("SOURCE_DENIED") && !stdout.contains("CONSUMER-SOURCE-DO-NOT-LEAK"),
        "the consumer's source must be outside the jail's read set:\n{stdout}"
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

/// A CATALOGUED package gets COARSE egress on macOS — it dials any host directly, and no proxy
/// is interposed. This is the behaviour change away from per-host: the jail used to pin the
/// child's egress to a loopback proxy that gated every tunnel against `$downloads`, and macOS was
/// the only platform that could enforce it. Per-host was withdrawn precisely because it was
/// macOS-only (Linux has no netns to route a child through, Windows' loopback exemption is
/// admin-only), so being stricter here threw errors no Linux or Windows user would ever see.
///
/// The listener is the positive control in the direction that now matters: the SAME probe reaches
/// it unconfined a moment earlier, and must still reach it from INSIDE the jail. A regression
/// that re-denied a catalogued package's egress, or restored the proxy wall, fails here rather
/// than passing quietly.
///
/// `PROXY_ENV_MISSING` is the second half, and it is what pins that no proxy is started for a
/// build-jail policy any more: coarse `net: true` derives `ProxyMode::Disabled`, so `proxy_needed`
/// is false and nothing binds a loopback port. That matters beyond tidiness — a bind failure is a
/// HARD apply error, so a listener the jail could never route through was able to refuse an
/// install it does not participate in.
///
/// The differential against `an_uncatalogued_package_gets_no_egress_and_no_proxy` is what makes
/// this evidence rather than an assertion: DIRECT_CONNECTED here, DIRECT_BLOCKED there, with one
/// variable changed (the package name). `/bin/sh` on macOS carries the `/dev/tcp` redirection, so
/// the probe needs no external binary the tight read set might not reach.
#[test]
fn a_catalogued_package_reaches_any_host_directly_and_gets_no_proxy() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    // The cell is spelled for the catalogued name below. The gate reads only the resolved
    // NAME, so a mismatch here would still pass — and would read as if the fixture were the
    // variable under test, which it is not.
    let package_dir = project.join("node_modules/.aube/cypress@1.0.0/node_modules/cypress");
    std::fs::create_dir_all(&package_dir).unwrap();

    // Bound but never accepted: a TCP connect completes off the listen backlog, so the
    // reachability differential does not need an accept loop racing the assertions.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let script = format!(
        r#"
        (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo DIRECT_CONNECTED || echo DIRECT_BLOCKED
        case "${{HTTP_PROXY:-}}" in http://*) echo PROXY_ENV_SET ;; *) echo PROXY_ENV_MISSING ;; esac
        "#
    );

    let control = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "(exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo DIRECT_CONNECTED || echo DIRECT_BLOCKED"
        ))
        .output()
        .expect("run the probe unconfined");
    assert!(
        String::from_utf8_lossy(&control.stdout).contains("DIRECT_CONNECTED"),
        "control: the probe must reach the listener unconfined, or the confined failure \
         below says nothing about the jail"
    );

    let ambient: BTreeMap<String, String> = [("PATH".to_string(), "/usr/bin:/bin".to_string())]
        .into_iter()
        .collect();
    // A name the catalog DOES carry — the arm's only variable against the uncatalogued twin.
    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        Some("cypress"),
        Some("1.0.0"),
        vec![home.join("Library/Caches/nub/node/bin/node")],
        Vec::new(),
        ambient,
    )
    .expect("compile build-jail");
    // The IR half of the contract, asserted before the OS half so a failure names which layer
    // moved: a catalogued package compiles to a COARSE grant carrying no host rule at all.
    assert!(
        !policy.net.enforce && policy.net.rules.is_empty(),
        "a catalogued package must compile to coarse-allow with no per-host rule, got \
         enforce={} rules={:?}",
        policy.net.enforce,
        policy.net.rules
    );

    let spec = nub_sandbox::CommandSpec::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .cwd(&package_dir);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let prepared = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail (fail-closed on error)");
    let out = prepared.output().expect("run confined script");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("DIRECT_CONNECTED") && !stdout.contains("DIRECT_BLOCKED"),
        "a catalogued package gets coarse egress — any host must be dialable directly, \
         including one no host list would have carried:\n{stdout}"
    );
    assert!(
        stdout.contains("PROXY_ENV_MISSING"),
        "no proxy may be started for a build-jail policy — a coarse grant needs none, and a \
         bind failure would be a hard apply error:\n{stdout}"
    );
}

/// The enforcing-OS twin of the test above: the SAME jail, the SAME probe, a package the catalog
/// does not name — and it reaches its listener too, because the BASELINE grants coarse egress.
///
/// ⛔⛔ THIS TEST ASSERTED THE OPPOSITE UNTIL 2026-08-17, AND HAD BEEN RED ON THE BRANCH. It read "an
/// uncatalogued package must reach nothing — this is the whole defense", which was true when it was
/// written (2026-07-30, `f35f427b10`) and stopped being true on 2026-08-16, when
/// `4001cec5c5 sandbox: give an uncatalogued package a baseline grant instead of nothing` set
/// `baseline_caps().network = true`. That commit did not update this test, so the suite carried a
/// security assertion contradicting the policy it shipped. The fix is to pin what the baseline
/// actually grants; reversing the baseline instead is a posture decision, not a test repair.
///
/// ⛔ SO THE NET AXIS NO LONGER DISCRIMINATES CATALOGUED FROM UNCATALOGUED, and nothing here should
/// pretend it does. Both arms connect and both report `PROXY_ENV_MISSING`. What the jail withholds
/// from an unknown package is on the FILESYSTEM axis — no read of the real `$HOME`, no write to the
/// project — which is what the sibling tests in this file cover. Egress denial is not the defense
/// against exfiltration here; denying the READ of anything worth exfiltrating is.
///
/// The listener still gets bound and dialed rather than assumed: a coarse grant that silently failed
/// to apply would leave a proxy-only assertion passing, since `PROXY_ENV_MISSING` is now true either
/// way. The connect is the only thing that proves the policy was applied at all.
#[test]
fn an_uncatalogued_package_gets_the_baselines_coarse_egress_and_no_proxy() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/.aube/unvetted@1.0.0/node_modules/unvetted");
    std::fs::create_dir_all(&package_dir).unwrap();

    // Bound but never accepted, as in the catalogued arm: a TCP connect completes off the listen
    // backlog, so the reachability differential needs no accept loop racing the assertions.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let script = format!(
        r#"
        (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo DIRECT_CONNECTED || echo DIRECT_BLOCKED
        case "${{HTTP_PROXY:-}}" in http://*) echo PROXY_ENV_SET ;; *) echo PROXY_ENV_MISSING ;; esac
        "#
    );

    let control = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "(exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo DIRECT_CONNECTED || echo DIRECT_BLOCKED"
        ))
        .output()
        .expect("run the probe unconfined");
    assert!(
        String::from_utf8_lossy(&control.stdout).contains("DIRECT_CONNECTED"),
        "control: the probe must reach the listener unconfined, or the confined failure \
         below says nothing about the jail"
    );

    let ambient: BTreeMap<String, String> = [("PATH".to_string(), "/usr/bin:/bin".to_string())]
        .into_iter()
        .collect();
    // A name no catalog entry carries — the Shai-Hulud shape: an ordinary package that
    // acquired a lifecycle script nobody reviewed.
    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        Some("unvetted"),
        Some("1.0.0"),
        vec![home.join("Library/Caches/nub/node/bin/node")],
        Vec::new(),
        ambient,
    )
    .expect("compile build-jail");
    // Coarse-ALLOW with no host rule, which is byte-for-byte what the catalogued arm above compiles
    // to. `enforce` is the tell that discriminated the two before the baseline changed, and pinning
    // both halves (the flag AND the empty rule list) is what keeps a future per-host regression from
    // passing here on the flag alone.
    assert!(
        !policy.net.enforce && policy.net.rules.is_empty(),
        "an uncatalogued package must compile to coarse-allow with no per-host rule, because the \
         baseline grants network, got enforce={} rules={:?}",
        policy.net.enforce,
        policy.net.rules
    );

    let spec = nub_sandbox::CommandSpec::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .cwd(&package_dir);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let prepared = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail (fail-closed on error)");
    let out = prepared.output().expect("run confined script");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("DIRECT_CONNECTED") && !stdout.contains("DIRECT_BLOCKED"),
        "an uncatalogued package gets the BASELINE grant, and the baseline allows egress — so the \
         probe must reach its own listener. A block here means either the baseline stopped granting \
         network or the two decision sites disagree again (`build_jail_net` decides egress, \
         `compile_build_jail` decides the filesystem, and both must read the same baseline):\n{stdout}"
    );
    assert!(
        stdout.contains("PROXY_ENV_MISSING"),
        "no proxy may be started for a build-jail policy, catalogued or not:\n{stdout}"
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
///
/// The control's probe sits in the sibling's `node_modules` because that is the ONLY thing
/// a project anchor now reaches — the anchor grants the dependency tree, not the project.
/// A probe outside it would be denied in both runs and the control would prove nothing.
#[test]
fn a_fetched_checkout_reads_nothing_outside_itself_through_a_symlink_or_a_steered_anchor() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let checkout = root.path().join("checkout");
    // Another package's scratch copy, sibling to this one — what `../**` reaches for.
    let sibling = root.path().join("sibling-scratch");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::create_dir_all(sibling.join("node_modules")).unwrap();
    std::fs::write(home.join(".nub-secret"), b"HOME-SECRET-DO-NOT-LEAK").unwrap();
    std::fs::write(
        sibling.join("node_modules/scratch.txt"),
        b"SIBLING-SCRATCH-DO-NOT-LEAK",
    )
    .unwrap();
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
        sibling_file = sibling.join("node_modules/scratch.txt").display(),
    );

    let run = |project: &std::path::Path| -> String {
        let policy = nub_sandbox::compile_build_jail(
            homes(&home, project),
            &checkout,
            None,
            None,
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
        None,
        None,
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

/// A real native compile must still work under the narrowed `$tmp`.
///
/// `$tmp` narrowed from "the whole confstr scratch" to "a private per-run dir plus Apple's
/// fixed compiler cache", which is the shape the design specifies. This is the regression
/// guard for that narrowing: it already caught the tmp deny — emitted after `emit_fs` so it
/// can override a generous base read — swallowing the jail's own package-dir grant for any
/// tree living under `$TMPDIR`. Judges on the OBJECT FILE, never an exit code.
///
/// MEASURED, and worth knowing before anyone tightens further: this passes with the
/// `xcrun_db` carve-out SUPPRESSED. That file is a lookup CACHE — `xcrun` falls back to the
/// slow toolchain search on a miss rather than failing — so the carve-out is a performance
/// affordance here, not what makes this compile succeed. It is kept because the design
/// specifies it and it costs one file, but do not read this test as proving it load-bearing;
/// a heavier node-gyp/xcodebuild path may yet depend on it.
#[test]
fn a_native_compile_still_works_under_the_narrowed_tmp() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/.aube/native@1.0.0/node_modules/native");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        package_dir.join("addon.c"),
        b"int nub_probe(int x) { return x + 1; }\n",
    )
    .unwrap();

    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        None,
        None,
        vec![home.join("Library/Caches/nub/node/bin/node")],
        Vec::new(),
        [("PATH", "/usr/bin:/bin")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
    .expect("compile build-jail");

    let spec = nub_sandbox::CommandSpec::new("/usr/bin/cc")
        .args(["-c", "addon.c", "-o", "addon.o"])
        .cwd(&package_dir);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let out = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail (fail-closed on error)")
        .output()
        .expect("run confined compile");

    // The marker, not the status: an object file the compiler actually produced.
    assert!(
        package_dir.join("addon.o").exists(),
        "a native compile must still succeed under the narrowed $tmp — the xcrun_db \
         carve-out is what makes that possible.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Canary for the `/etc` narrowing: the Seatbelt base stopped granting `(subpath "/etc")`,
/// so anything a confined script legitimately reads there must be an enumerated leaf. Date
/// formatting is the most common such read (`/etc/localtime`), and TLS/name resolution the
/// most consequential.
#[test]
fn common_etc_reads_survive_the_leaf_narrowing() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/.aube/p@1.0.0/node_modules/p");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        None,
        None,
        vec![home.join("Library/Caches/nub/node/bin/node")],
        Vec::new(),
        [("PATH", "/usr/bin:/bin")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
    .expect("compile build-jail");

    let script = "date > date.txt 2>/dev/null && echo DATE_OK || echo DATE_FAIL; \
                  head -c 1 /etc/ssl/cert.pem >/dev/null 2>&1 && echo TLS_OK || echo TLS_FAIL; \
                  head -c 1 /etc/hosts >/dev/null 2>&1 && echo HOSTS_OK || echo HOSTS_FAIL; \
                  head -c 1 /etc/passwd >/dev/null 2>&1 && echo PASSWD_OK || echo PASSWD_FAIL; \
                  head -c 1 /etc/bashrc >/dev/null 2>&1 && echo UNLISTED_LEAK || echo UNLISTED_HIDDEN";
    let spec = nub_sandbox::CommandSpec::new("/bin/sh")
        .args(["-c", script])
        .cwd(&package_dir);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let out = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail")
        .output()
        .expect("run confined probe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("--- /etc probe ---\n{stdout}");
    for ok in ["DATE_OK", "TLS_OK", "HOSTS_OK", "PASSWD_OK"] {
        assert!(stdout.contains(ok), "{ok} missing:\n{stdout}");
    }
    // The narrowing actually bites: `/etc/bashrc` is world-readable (0444) yet outside the
    // enumerated leaves, so it is refused. Deliberately NOT probed with `/etc/master.passwd`
    // — that file is mode 0600 root-only, so a refusal there proves filesystem permissions,
    // not the sandbox, and would be a control that passes for the wrong reason.
    assert!(
        stdout.contains("UNLISTED_HIDDEN"),
        "an unlisted, world-readable /etc file must be refused by the leaf narrowing:\n{stdout}"
    );
}

/// The PM-cache grant reaches the store and the toolchain, and STOPS at the git checkouts.
///
/// A git dependency is cloned to `$cache/nub/pm/git/<key>`, whose `.git/config` records the
/// fetch URL — for a private dep fetched over HTTPS, with the token in it. The grant used to
/// be the cache ROOT, so that token was readable by every lifecycle script on the machine
/// (reproduced under the real jail on macOS and Linux, 2026-07-28). It is now two named
/// subtrees.
///
/// The two positive probes are what make the negative one mean something: an assertion that
/// the git config is refused would pass equally if the PM-cache grant had been dropped
/// altogether, which breaks every native build (no node-gyp) and every dependency resolution
/// through the global virtual store.
#[test]
fn build_jail_reads_the_pm_store_and_toolchain_but_not_a_git_dep_clone() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/somepkg");
    std::fs::create_dir_all(&package_dir).unwrap();

    // `$cache` is `~/Library/Caches` on macOS, matching `homes()` above and the NUB
    // embedder's `cache_namespace = "nub/pm"`.
    let pm = home.join("Library/Caches/nub/pm");
    let dep = pm.join("store/left-pad@1.3.0-abc/node_modules/left-pad");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(dep.join("index.js"), b"STORE-CONTENT-VISIBLE").unwrap();
    let gyp = pm.join("tools/node-gyp/lazy-bin");
    std::fs::create_dir_all(&gyp).unwrap();
    std::fs::write(gyp.join("node-gyp"), b"TOOLCHAIN-VISIBLE").unwrap();
    let clone = pm.join("git/aube-git-deadbeef/.git");
    std::fs::create_dir_all(&clone).unwrap();
    std::fs::write(
        clone.join("config"),
        b"[remote \"origin\"]\n\turl = https://x-access-token:GIT-URL-TOKEN-DO-NOT-LEAK@github.com/acme/private.git\n",
    )
    .unwrap();

    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        None,
        None,
        vec![home.join("Library/Caches/nub/node/bin/node")],
        Vec::new(),
        [("PATH", "/usr/bin:/bin")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
    .expect("compile build-jail");

    let script = format!(
        "cat '{store}' 2>/dev/null || echo STORE_DENIED; \
         cat '{tools}' 2>/dev/null || echo TOOLS_DENIED; \
         cat '{git}' 2>/dev/null || echo GIT_CONFIG_DENIED",
        store = dep.join("index.js").display(),
        tools = gyp.join("node-gyp").display(),
        git = clone.join("config").display(),
    );
    let spec = nub_sandbox::CommandSpec::new("/bin/sh")
        .args(["-c", &script])
        .cwd(&package_dir);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let out = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail")
        .output()
        .expect("run confined probe");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("STORE-CONTENT-VISIBLE"),
        "the global virtual store holds the dependency tree a project's node_modules \
         symlinks into; without it nothing resolves:\n{stdout}"
    );
    assert!(
        stdout.contains("TOOLCHAIN-VISIBLE"),
        "nub's bootstrapped node-gyp is the only one a confined native build can reach:\n{stdout}"
    );
    assert!(
        stdout.contains("GIT_CONFIG_DENIED") && !stdout.contains("GIT-URL-TOKEN-DO-NOT-LEAK"),
        "a git dependency's clone config carries its fetch URL — a private dep's token — \
         and must be outside the jail's read set:\n{stdout}"
    );
}

/// `cfprefsd` reads preference domains on the child's behalf from OUTSIDE the sandbox, so
/// the file allowlist does not bound it: with `~/Library/Preferences` ungranted, a confined
/// script still got the global domain back byte-for-byte. Closed by withholding the
/// `user-preference-read` grant (`macos_seatbelt_base.sbpl`) — not by a deny; the operation
/// falls to `(deny default)`.
///
/// The unconfined run is the control. `defaults` reports a missing domain and a refused one
/// the same way, so without proving the domain is non-empty on THIS host the confined
/// refusal would be indistinguishable from there being nothing to read.
#[test]
fn build_jail_cannot_read_preferences_through_the_cfprefs_broker() {
    let unconfined = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "-globalDomain"])
        .output()
        .expect("run defaults unconfined");
    assert!(
        unconfined.status.success() && !unconfined.stdout.is_empty(),
        "control: the global preference domain must be readable unconfined, else the \
         confined refusal below proves nothing"
    );

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let package_dir = project.join("node_modules/somepkg");
    std::fs::create_dir_all(&package_dir).unwrap();

    let policy = nub_sandbox::compile_build_jail(
        homes(&home, &project),
        &package_dir,
        None,
        None,
        vec![home.join("Library/Caches/nub/node/bin/node")],
        Vec::new(),
        [("PATH", "/usr/bin:/bin")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
    .expect("compile build-jail");

    let spec = nub_sandbox::CommandSpec::new("/usr/bin/defaults")
        .args(["read", "-globalDomain"])
        .cwd(&package_dir);
    let runtime = nub_sandbox::earliest_bootstrap().expect("bootstrap");
    let out = nub_sandbox::apply_with_runtime(&policy, spec, &runtime)
        .expect("apply build-jail")
        .output()
        .expect("run confined probe");

    assert!(
        out.stdout.is_empty(),
        "the cfprefs broker read {} bytes of the user's global preference domain from \
         inside the jail:\n{}",
        out.stdout.len(),
        String::from_utf8_lossy(&out.stdout)
    );
}
