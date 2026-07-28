//! WINDOWS VERIFICATION PROBE for the four unverified build-jail landings.
//! THROWAWAY — never committed to nub. Modeled on `tests/windows_enforcement.rs`
//! (self-reexec child, `harness = false`), but drives the REAL `compile_build_jail`
//! preset rather than a hand-rolled IR, so what is measured is the shipping policy.
//!
//! Every confinement claim is paired with (a) an UNJAILED control proving the target is
//! reachable at all, and (b) where possible an IN-JAIL positive control proving the
//! surrounding grant works — so a denial can never be "the file was never there".
//! All probe paths are ABSOLUTE and computed by the PARENT; the child never resolves a
//! relative path. Env-var-resolved paths are additionally echoed by the child and
//! re-checked by the parent against the filesystem.

#[cfg(not(target_os = "windows"))]
fn main() {}

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__probe__") {
        std::process::exit(win::child_main(&args[2..]));
    }
    win::run();
}

#[cfg(target_os = "windows")]
mod win {
    use nub_sandbox::{CommandSpec, Homes, SandboxPolicy, apply, compile_build_jail};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    // ────────────────────────── the probe child ──────────────────────────
    // exit contract: 0 ok · 4 env-absent · 5 access-denied · 8 not-found · 9 other

    fn io_code(e: &std::io::Error) -> i32 {
        match e.kind() {
            std::io::ErrorKind::PermissionDenied => 5,
            std::io::ErrorKind::NotFound => 8,
            _ => {
                eprintln!("  child io error: {e} os={:?}", e.raw_os_error());
                9
            }
        }
    }

    pub fn child_main(a: &[String]) -> i32 {
        match a.first().map(String::as_str) {
            // read <abs>
            Some("read") => match std::fs::read(&a[1]) {
                Ok(b) => {
                    println!("  CHILD read {} -> {} bytes", a[1], b.len());
                    0
                }
                Err(e) => io_code(&e),
            },
            // write <abs>
            Some("write") => match std::fs::write(&a[1], b"nub-probe") {
                Ok(()) => 0,
                Err(e) => io_code(&e),
            },
            // listdir <abs>
            Some("listdir") => match std::fs::read_dir(&a[1]) {
                Ok(it) => {
                    let names: Vec<String> = it
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    println!("  CHILD listdir {} -> {} entries", a[1], names.len());
                    for n in names.iter().take(40) {
                        println!("    · {n}");
                    }
                    0
                }
                Err(e) => io_code(&e),
            },
            // getenv <NAME> — echo the value the CHILD actually sees
            Some("getenv") => match std::env::var(&a[1]) {
                Ok(v) => {
                    println!("  CHILD env {}={}", a[1], v);
                    0
                }
                Err(_) => {
                    println!("  CHILD env {} ABSENT", a[1]);
                    4
                }
            },
            // envwrite <NAME> <leafname> — resolve the env var, write <value>\<leaf>,
            // and ECHO the absolute path written, so the parent can verify on disk.
            Some("envwrite") => {
                let Ok(v) = std::env::var(&a[1]) else {
                    println!("  CHILD env {} ABSENT", a[1]);
                    return 4;
                };
                let p = PathBuf::from(&v).join(&a[2]);
                println!("  CHILD envwrite target={}", p.display());
                match std::fs::write(&p, b"nub-probe-marker") {
                    Ok(()) => 0,
                    Err(e) => io_code(&e),
                }
            },
            // walk <abs> <maxdepth> — bounded recursive reachability census. Prints a
            // tally of dirs listed / files whose CONTENT was read / denials, plus the
            // first 60 readable file paths. This is the LOCALAPPDATA quantifier.
            Some("walk") => {
                let root = PathBuf::from(&a[1]);
                let maxd: usize = a[2].parse().unwrap_or(2);
                let mut dirs_ok = 0usize;
                let mut dirs_denied = 0usize;
                let mut files_read = 0usize;
                let mut files_denied = 0usize;
                let mut bytes = 0usize;
                let mut samples: Vec<String> = Vec::new();
                let mut queue = vec![(root.clone(), 0usize)];
                let mut visited = 0usize;
                while let Some((d, depth)) = queue.pop() {
                    visited += 1;
                    if visited > 4000 {
                        break;
                    }
                    match std::fs::read_dir(&d) {
                        Ok(it) => {
                            dirs_ok += 1;
                            for e in it.flatten() {
                                let p = e.path();
                                let ft = match e.file_type() {
                                    Ok(t) => t,
                                    Err(_) => continue,
                                };
                                if ft.is_dir() {
                                    if depth + 1 <= maxd {
                                        queue.push((p, depth + 1));
                                    }
                                } else if ft.is_file() {
                                    match std::fs::read(&p) {
                                        Ok(b) => {
                                            files_read += 1;
                                            bytes += b.len();
                                            if samples.len() < 60 {
                                                samples.push(format!(
                                                    "{} ({} bytes)",
                                                    p.display(),
                                                    b.len()
                                                ));
                                            }
                                        }
                                        Err(_) => files_denied += 1,
                                    }
                                }
                            }
                        }
                        Err(_) => dirs_denied += 1,
                    }
                }
                println!(
                    "  CHILD walk {} depth<={maxd}: dirs_listed={dirs_ok} dirs_denied={dirs_denied} \
                     files_read={files_read} files_denied={files_denied} bytes_read={bytes}",
                    root.display()
                );
                for s in &samples {
                    println!("    + {s}");
                }
                if dirs_ok == 0 { 5 } else { 0 }
            }
            _ => 2,
        }
    }

    // ────────────────────────── fixture ──────────────────────────

    struct Fx {
        root: PathBuf,
        child: PathBuf,
        user_home: PathBuf,
        cache: PathBuf,
        project: PathBuf,
        package_dir: PathBuf,
    }

    fn icacls(args: &[&std::ffi::OsStr]) {
        let st = std::process::Command::new("icacls")
            .args(args)
            .output()
            .expect("run icacls");
        if !st.status.success() {
            eprintln!(
                "  icacls failed: {}",
                String::from_utf8_lossy(&st.stderr).trim()
            );
        }
    }

    fn secure_root(root: &Path) {
        let user = std::env::var("USERNAME").expect("USERNAME");
        let grant = format!("{user}:(OI)(CI)F");
        icacls(&[
            root.as_os_str(),
            "/inheritance:r".as_ref(),
            "/grant:r".as_ref(),
            grant.as_ref(),
        ]);
    }

    impl Fx {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("nub-gap-{nonce:x}"));
            std::fs::create_dir_all(&root).unwrap();
            // PROTECTED DACL before children so nothing inherits a stray AAP ACE.
            secure_root(&root);

            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let child = bin.join("child.exe");
            std::fs::copy(std::env::current_exe().unwrap(), &child).unwrap();

            // The "user's real home" stand-in.
            let user_home = root.join("home");
            std::fs::create_dir_all(user_home.join(".ssh")).unwrap();
            std::fs::write(user_home.join(".ssh/id_rsa"), b"SSH-PRIVATE-KEY-DO-NOT-LEAK").unwrap();
            std::fs::write(
                user_home.join(".npmrc"),
                b"//registry.npmjs.org/:_authToken=HOME-NPM-TOKEN-DO-NOT-LEAK",
            )
            .unwrap();

            // nub's PM cache. `store` + `tools` are what the narrowing keeps; `git` is
            // what it drops. All three planted so a denial is never "absent file".
            let cache = root.join("cache");
            let pm = cache.join("nub/pm");
            std::fs::create_dir_all(pm.join("store/left-pad@1.3.0/node_modules/left-pad")).unwrap();
            std::fs::write(
                pm.join("store/left-pad@1.3.0/node_modules/left-pad/index.js"),
                b"module.exports=1",
            )
            .unwrap();
            std::fs::create_dir_all(pm.join("tools/node-gyp/lazy-bin")).unwrap();
            std::fs::write(pm.join("tools/node-gyp/lazy-bin/node-gyp"), b"#!/bin/sh").unwrap();
            std::fs::create_dir_all(pm.join("git/aube-git-deadbeef/.git")).unwrap();
            std::fs::write(
                pm.join("git/aube-git-deadbeef/.git/config"),
                b"[remote \"origin\"]\n\turl = https://x-access-token:GIT-FETCH-TOKEN-DO-NOT-LEAK@github.com/acme/private.git\n",
            )
            .unwrap();

            let project = root.join("project");
            let package_dir = project.join("node_modules/native");
            std::fs::create_dir_all(&package_dir).unwrap();
            std::fs::write(project.join("package.json"), b"{}").unwrap();
            std::fs::write(package_dir.join("binding.gyp"), b"{}").unwrap();

            Fx {
                root,
                child,
                user_home,
                cache,
                project,
                package_dir,
            }
        }
    }
    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    // ────────────────────────── runners ──────────────────────────

    /// Run the child UNDER the jail. Returns the exit code (or a negative sentinel).
    fn jailed(policy: &SandboxPolicy, fx: &Fx, args: &[&str]) -> i32 {
        let mut v = vec!["__probe__"];
        v.extend_from_slice(args);
        let spec = CommandSpec::new(fx.child.as_os_str())
            .args(v.iter().copied())
            .cwd(&fx.package_dir);
        let prepared = match apply(policy, spec) {
            Ok(p) => p,
            Err(d) => {
                eprintln!("  [apply Err] lost={:?} reason={:?}", d.lost, d.reason);
                return -100;
            }
        };
        match prepared.status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                eprintln!("  [status Err] {e} os={:?}", e.raw_os_error());
                -101
            }
        }
    }

    /// Run the child WITHOUT any sandbox — the negative control.
    fn unjailed(fx: &Fx, args: &[&str]) -> i32 {
        let mut c = std::process::Command::new(&fx.child);
        c.arg("__probe__").args(args).current_dir(&fx.package_dir);
        match c.status() {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => {
                eprintln!("  [unjailed spawn Err] {e}");
                -101
            }
        }
    }

    fn name(c: i32) -> &'static str {
        match c {
            0 => "OK",
            4 => "ENV-ABSENT",
            5 => "DENIED",
            8 => "NOT-FOUND",
            9 => "OTHER-ERR",
            -100 => "APPLY-REFUSED",
            -101 => "SPAWN-ERR",
            _ => "?",
        }
    }

    struct Report {
        fails: u32,
    }
    impl Report {
        /// One measurement: run jailed AND unjailed, print both, judge against `want`.
        fn pair(&mut self, label: &str, policy: &SandboxPolicy, fx: &Fx, args: &[&str], want: i32) {
            println!("── {label}");
            let u = unjailed(fx, args);
            let j = jailed(policy, fx, args);
            println!(
                "   CONTROL(unjailed)={} ({u})   JAILED={} ({j})   expect JAILED={} ({want})",
                name(u),
                name(j),
                name(want)
            );
            if u != 0 {
                self.fails += 1;
                println!("   !! CONTROL DID NOT SUCCEED — this measurement proves nothing");
            }
            if j == want {
                println!("   PASS");
            } else {
                self.fails += 1;
                println!("   FAIL");
            }
        }
        /// Observational only — no expected value; the output is the finding.
        fn observe(&mut self, label: &str, policy: &SandboxPolicy, fx: &Fx, args: &[&str]) {
            println!("── {label}  [OBSERVATION]");
            let u = unjailed(fx, args);
            let j = jailed(policy, fx, args);
            println!(
                "   CONTROL(unjailed)={} ({u})   JAILED={} ({j})",
                name(u),
                name(j)
            );
        }
    }

    fn ambient() -> BTreeMap<String, String> {
        std::env::vars().collect()
    }

    /// The jail-home the compile just materialized for this package (exactly one dir).
    fn jail_home_of(cache: &Path) -> Option<PathBuf> {
        let root = cache.join("nub/jail-home");
        let mut v: Vec<PathBuf> = std::fs::read_dir(&root).ok()?.flatten().map(|e| e.path()).collect();
        if v.len() != 1 {
            println!("   (jail-home root holds {} entries: {v:?})", v.len());
        }
        v.pop()
    }

    pub fn run() {
        let fx = Fx::new();
        let mut r = Report { fails: 0 };

        println!("=========================================================");
        println!("nub Windows build-jail gap probe");
        println!("  branch marker : {}", "private-home-d05890baf7");
        println!("  exe           : {}", std::env::current_exe().unwrap().display());
        println!("  fixture root  : {}", fx.root.display());
        println!("  USERPROFILE   : {:?}", std::env::var("USERPROFILE"));
        println!("  LOCALAPPDATA  : {:?}", std::env::var("LOCALAPPDATA"));
        println!("=========================================================");

        let homes = Homes {
            home: fx.user_home.clone(),
            tmp: std::env::temp_dir(),
            cache: fx.cache.clone(),
            project: fx.project.clone(),
        };
        let policy = match compile_build_jail(
            homes,
            &fx.package_dir,
            Vec::new(),
            Vec::new(),
            ambient(),
        ) {
            Ok(p) => p,
            Err(e) => {
                println!("!!! compile_build_jail FAILED: {e:?}");
                std::process::exit(2);
            }
        };

        println!("\n--- COMPILED POLICY (fs rules) ---");
        for e in &policy.fs.rules.entries {
            println!("   {:?}", e);
        }
        println!("   default_effect = {:?}", policy.fs.rules.default_effect);
        println!("--- COMPILED POLICY (env constructed) ---");
        for (k, v) in &policy.env.constructed {
            println!("   {k}={v}");
        }
        println!("   env.enforce = {}", policy.env.enforce);
        println!("--- net: enforce={} rules={:?}", policy.net.enforce, policy.net.rules);

        // ═══ SANITY: the jail actually engages ═══
        println!("\n════ SANITY ════");
        let pkg_file = fx.package_dir.join("binding.gyp");
        r.pair(
            "S1 own package dir is READABLE (grant works at all)",
            &policy, &fx, &["read", &pkg_file.to_string_lossy()], 0,
        );
        let pkg_write = fx.package_dir.join("built.node");
        r.pair(
            "S2 own package dir is WRITABLE",
            &policy, &fx, &["write", &pkg_write.to_string_lossy()], 0,
        );
        let outside = fx.root.join("bin/child.exe.copytarget");
        std::fs::write(&outside, b"x").unwrap();
        r.pair(
            "S3 an UNGRANTED path is DENIED (the jail is on)",
            &policy, &fx, &["write", &outside.to_string_lossy()], 5,
        );

        // ═══ ITEM 1: private per-package HOME ═══
        println!("\n════ ITEM 1 — private per-package $HOME ════");
        let jh = jail_home_of(&fx.cache);
        match &jh {
            Some(p) => println!("   materialized jail-home: {}", p.display()),
            None => println!("   !! NO jail-home materialized (private_home_dir returned None)"),
        }
        r.pair(
            "1a REAL home secret ~/.ssh/id_rsa is UNREADABLE",
            &policy, &fx,
            &["read", &fx.user_home.join(".ssh/id_rsa").to_string_lossy()], 5,
        );
        r.pair(
            "1b REAL home secret ~/.npmrc is UNREADABLE",
            &policy, &fx,
            &["read", &fx.user_home.join(".npmrc").to_string_lossy()], 5,
        );
        r.observe("1c what USERPROFILE the child sees", &policy, &fx, &["getenv", "USERPROFILE"]);
        r.observe("1d what HOME the child sees", &policy, &fx, &["getenv", "HOME"]);
        // The compat half: write through USERPROFILE and verify the marker ON DISK.
        println!("── 1e write through %USERPROFILE% and verify the marker on disk");
        let jcode = jailed(&policy, &fx, &["envwrite", "USERPROFILE", "nub-probe-marker.txt"]);
        println!("   jailed envwrite -> {} ({jcode})", name(jcode));
        match &jh {
            Some(p) => {
                let marker = p.join("nub-probe-marker.txt");
                let landed = marker.is_file();
                println!("   marker at jail-home {} : {}", marker.display(), landed);
                if landed { println!("   PASS (write landed INSIDE the private home)"); }
                else { r.fails += 1; println!("   FAIL (no marker in the private home)"); }
            }
            None => {
                r.fails += 1;
                println!("   FAIL (no private home to check)");
            }
        }
        // Did anything land in the REAL user home? (the leak direction)
        let leaked = fx.user_home.join("nub-probe-marker.txt");
        println!("   marker in REAL user home {} : {}", leaked.display(), leaked.is_file());

        // ═══ ITEM 2: pm-cache grant narrowing ═══
        println!("\n════ ITEM 2 — pm-cache grant narrowing ════");
        let store = fx.cache.join("nub/pm/store/left-pad@1.3.0/node_modules/left-pad/index.js");
        let tools = fx.cache.join("nub/pm/tools/node-gyp/lazy-bin/node-gyp");
        let gitcfg = fx.cache.join("nub/pm/git/aube-git-deadbeef/.git/config");
        r.pair("2a POSITIVE CONTROL: pm store content IS readable",
            &policy, &fx, &["read", &store.to_string_lossy()], 0);
        r.pair("2b POSITIVE CONTROL: pm tools content IS readable",
            &policy, &fx, &["read", &tools.to_string_lossy()], 0);
        r.observe("2c git-dep .git/config (NARROWED branch expects DENIED; pre-narrowing expects OK)",
            &policy, &fx, &["read", &gitcfg.to_string_lossy()]);
        r.observe("2d list the pm cache root",
            &policy, &fx, &["listdir", &fx.cache.join("nub/pm").to_string_lossy()]);

        // ═══ THE %LOCALAPPDATA% STRUCTURAL GAP ═══
        println!("\n════ %LOCALAPPDATA% STRUCTURAL GAP ════");
        r.observe("L1 what LOCALAPPDATA the child sees", &policy, &fx, &["getenv", "LOCALAPPDATA"]);
        if let Ok(lad) = std::env::var("LOCALAPPDATA") {
            let lad = PathBuf::from(lad);
            // Plant a credential-shaped file the way real tooling would.
            let planted_dir = lad.join("nub-probe-fakecache");
            let _ = std::fs::create_dir_all(&planted_dir);
            let planted = planted_dir.join("credentials.json");
            let _ = std::fs::write(&planted, b"{\"token\":\"LOCALAPPDATA-SECRET-DO-NOT-LEAK\"}");
            r.observe("L2 READ a planted secret under the REAL %LOCALAPPDATA%",
                &policy, &fx, &["read", &planted.to_string_lossy()]);
            r.observe("L3 WRITE into the REAL %LOCALAPPDATA%",
                &policy, &fx, &["write", &lad.join("nub-probe-write.txt").to_string_lossy()]);
            r.observe("L4 LIST the REAL %LOCALAPPDATA%",
                &policy, &fx, &["listdir", &lad.to_string_lossy()]);
            r.observe("L5 CENSUS: bounded walk of the REAL %LOCALAPPDATA% (depth<=2)",
                &policy, &fx, &["walk", &lad.to_string_lossy(), "2"]);
            r.observe("L6 LIST %LOCALAPPDATA%\\Packages (the AppContainer profile root)",
                &policy, &fx, &["listdir", &lad.join("Packages").to_string_lossy()]);
            r.observe("L7 envwrite through %LOCALAPPDATA%",
                &policy, &fx, &["envwrite", "LOCALAPPDATA", "nub-probe-envwrite.txt"]);
            let _ = std::fs::remove_dir_all(&planted_dir);
            let _ = std::fs::remove_file(lad.join("nub-probe-write.txt"));
            let _ = std::fs::remove_file(lad.join("nub-probe-envwrite.txt"));
        }
        // The REAL user profile — production faithfulness for "are the user's secrets safe".
        if let Ok(up) = std::env::var("USERPROFILE") {
            let up = PathBuf::from(up);
            let real_npmrc = up.join(".npmrc");
            let planted = !real_npmrc.exists();
            if planted {
                let _ = std::fs::write(&real_npmrc, b"//registry.npmjs.org/:_authToken=REAL-PROFILE-TOKEN-DO-NOT-LEAK");
            }
            r.observe("U1 READ the REAL %USERPROFILE%\\.npmrc",
                &policy, &fx, &["read", &real_npmrc.to_string_lossy()]);
            r.observe("U2 LIST the REAL %USERPROFILE%",
                &policy, &fx, &["listdir", &up.to_string_lossy()]);
            r.observe("U3 CENSUS: bounded walk of the REAL %USERPROFILE% (depth<=1)",
                &policy, &fx, &["walk", &up.to_string_lossy(), "1"]);
            if planted { let _ = std::fs::remove_file(&real_npmrc); }
        }
        // Windows dirs that are NOT under any grant but are classically world-readable.
        r.observe("W1 READ C:\\Windows\\System32\\drivers\\etc\\hosts",
            &policy, &fx, &["read", "C:\\Windows\\System32\\drivers\\etc\\hosts"]);
        r.observe("W2 LIST C:\\Users",
            &policy, &fx, &["listdir", "C:\\Users"]);

        println!("\n=========================================================");
        if r.fails == 0 {
            println!("PROBE COMPLETE — 0 judged failures");
        } else {
            println!("PROBE COMPLETE — {} judged FAILURE(S)", r.fails);
        }
        println!("=========================================================");
    }
}
