//! Windows filesystem-semantics probe for nubjs/nub#576.
//!
//! Every assumption the isolated linker makes about NTFS junctions and
//! `std::fs` error classification, exercised on a real Windows kernel and
//! PRINTED. Nothing here asserts — the output IS the deliverable.
//!
//! Assumptions under test (file:line in the nub tree at the time of writing):
//!   - `read_link(<real dir>)` == `ErrorKind::InvalidInput`
//!       aube-linker/src/link.rs:377-383, aube-linker/src/sweep.rs:291-306
//!   - `read_link(<junction>)` returns a target that compares EQUAL to the
//!     absolute path handed to `junction::create`
//!       aube-linker/src/sweep.rs:293 (`classify_entry_state`)
//!   - `junction::create` over an existing entry yields os error 183, which
//!     `is_retriable_link_error` (aube-linker/src/sys.rs:113) does NOT tolerate
//!   - `Path::exists()` is a sound "is anything here?" guard
//!       aube-linker/src/link.rs:562-570, :1137-1140

fn main() {
    #[cfg(not(windows))]
    {
        eprintln!("win576-probe: Windows-only");
        std::process::exit(1);
    }
    #[cfg(windows)]
    win::run();
}

#[cfg(windows)]
mod win {
    use std::fs;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const GENERIC_READ: u32 = 0x8000_0000;

    fn hdr(n: &str) {
        println!("\n================ {n} ================");
    }

    fn show_err(label: &str, e: &io::Error) {
        println!(
            "  {label}: Err(kind={:?}, raw_os_error={:?}, msg={:?})",
            e.kind(),
            e.raw_os_error(),
            e.to_string()
        );
    }

    fn show_read_link(label: &str, p: &Path) {
        match fs::read_link(p) {
            Ok(t) => println!("  {label}: Ok({:?})", t),
            Err(e) => show_err(label, &e),
        }
    }

    fn mkdir(p: &Path) {
        fs::create_dir_all(p).unwrap_or_else(|e| panic!("create_dir_all {p:?}: {e}"));
    }

    pub fn run() {
        let root = std::env::temp_dir().join(format!("win576-probe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        mkdir(&root);
        println!("probe root: {}", root.display());
        println!("rustc host: windows");

        probe1_read_link_classification(&root);
        probe2_read_link_junction_target(&root);
        probe3_junction_create_over_existing(&root);
        probe4_exists(&root);
        probe5_remove_dir(&root);
        probe6_aube_shape(&root);
        probe7_exists_false_while_occupied(&root);
        probe8_reparse_depth(&root);

        println!("\nprobe complete (root left in place for the workflow to list): {}", root.display());
    }

    // ---------------------------------------------------------------- probe 1
    fn probe1_read_link_classification(root: &Path) {
        hdr("PROBE 1 — read_link() error classification");

        let real_dir = root.join("p1_real_dir");
        mkdir(&real_dir);
        show_read_link("read_link(REAL EMPTY DIR)", &real_dir);

        let nonempty = root.join("p1_real_dir_nonempty");
        mkdir(&nonempty);
        fs::write(nonempty.join("a.txt"), b"x").unwrap();
        show_read_link("read_link(REAL NON-EMPTY DIR)", &nonempty);

        let file = root.join("p1_file.txt");
        fs::write(&file, b"x").unwrap();
        show_read_link("read_link(REAL FILE)", &file);

        let missing = root.join("p1_missing");
        show_read_link("read_link(MISSING PATH)", &missing);

        println!(
            "  >>> the linker treats Err(InvalidInput) as \"real directory\"; \
             anything else is classified Stale/Missing"
        );
    }

    // ---------------------------------------------------------------- probe 2
    fn probe2_read_link_junction_target(root: &Path) {
        hdr("PROBE 2 — read_link() on a junction: exact target string");

        let target = root.join("p2_target");
        mkdir(&target);
        fs::write(target.join("marker.txt"), b"t").unwrap();
        let link = root.join("p2_link");

        match junction::create(&target, &link) {
            Ok(()) => println!("  junction::create(target={:?}, link={:?}) => Ok", target, link),
            Err(e) => {
                show_err("junction::create", &e);
                return;
            }
        }

        let got = fs::read_link(&link);
        match &got {
            Ok(t) => {
                println!("  read_link(JUNCTION)          = Ok({:?})", t);
                println!("  absolute target passed in    =    {:?}", target);
                println!("  read_link(..) == target ?    => {}", *t == target);
                println!(
                    "  read_link(..) == verbatim(target) ? => {}",
                    t.as_os_str().to_string_lossy()
                        == format!(r"\\?\{}", target.display())
                );
                println!(
                    "  target string starts with: {:?}",
                    t.to_string_lossy().chars().take(8).collect::<String>()
                );
            }
            Err(e) => show_err("read_link(JUNCTION)", e),
        }

        match junction::get_target(&link) {
            Ok(t) => println!("  junction::get_target()       = Ok({:?})", t),
            Err(e) => show_err("junction::get_target", &e),
        }

        // classify_entry_state compares read_link(link) == expected, where
        // `expected` is what the linker computed — an ordinary absolute path.
        println!(
            "  >>> classify_entry_state(link, expected) is Fresh ONLY when this comparison is true"
        );

        // Same question, but with a junction created from a path that itself
        // came back out of read_link (i.e. round-trip stability).
        let link2 = root.join("p2_link_roundtrip");
        if let Ok(t) = &got {
            match junction::create(t, &link2) {
                Ok(()) => show_read_link("read_link(junction made FROM read_link output)", &link2),
                Err(e) => show_err("junction::create(from read_link output)", &e),
            }
        }

        // is_symlink() / metadata on a junction, for contrast.
        println!(
            "  Path::is_symlink(junction)   = {}",
            link.is_symlink()
        );
        println!(
            "  symlink_metadata.is_dir()    = {:?}",
            fs::symlink_metadata(&link).map(|m| m.is_dir())
        );
        println!("  metadata.is_dir()            = {:?}", fs::metadata(&link).map(|m| m.is_dir()));
    }

    // ---------------------------------------------------------------- probe 3
    fn probe3_junction_create_over_existing(root: &Path) {
        hdr("PROBE 3 — junction::create over an EXISTING link path");

        let target = root.join("p3_target");
        mkdir(&target);
        fs::write(target.join("t.txt"), b"t").unwrap();

        // (a) existing EMPTY real directory
        let a = root.join("p3_a_empty_dir");
        mkdir(&a);
        match junction::create(&target, &a) {
            Ok(()) => println!("  (a) over EMPTY real dir      => Ok  [!! surprising]"),
            Err(e) => show_err("(a) over EMPTY real dir", &e),
        }
        println!("      after: read_link => {:?}", fs::read_link(&a).map_err(|e| e.raw_os_error()));

        // (b) existing NON-EMPTY real directory
        let b = root.join("p3_b_nonempty_dir");
        mkdir(&b);
        fs::write(b.join("keep.txt"), b"k").unwrap();
        match junction::create(&target, &b) {
            Ok(()) => println!("  (b) over NON-EMPTY real dir  => Ok  [!! surprising]"),
            Err(e) => show_err("(b) over NON-EMPTY real dir", &e),
        }
        println!(
            "      after: keep.txt still there? {}",
            b.join("keep.txt").exists()
        );

        // (c) existing junction (same target)
        let c = root.join("p3_c_junction");
        junction::create(&target, &c).expect("seed junction");
        match junction::create(&target, &c) {
            Ok(()) => println!("  (c) over EXISTING junction (same target) => Ok"),
            Err(e) => show_err("(c) over EXISTING junction (same target)", &e),
        }

        // (c2) existing junction, DIFFERENT target
        let other = root.join("p3_other_target");
        mkdir(&other);
        match junction::create(&other, &c) {
            Ok(()) => println!("  (c2) over EXISTING junction (diff target) => Ok"),
            Err(e) => show_err("(c2) over EXISTING junction (diff target)", &e),
        }
        println!("      after: read_link => {:?}", fs::read_link(&c));

        // (d) existing FILE
        let d = root.join("p3_d_file");
        fs::write(&d, b"f").unwrap();
        match junction::create(&target, &d) {
            Ok(()) => println!("  (d) over EXISTING file       => Ok  [!! surprising]"),
            Err(e) => show_err("(d) over EXISTING file", &e),
        }

        // (e) existing junction whose TARGET has been deleted (dangling)
        let dangling_target = root.join("p3_e_target_to_delete");
        mkdir(&dangling_target);
        let e_link = root.join("p3_e_dangling_junction");
        junction::create(&dangling_target, &e_link).expect("seed dangling junction");
        fs::remove_dir_all(&dangling_target).expect("delete target");
        match junction::create(&target, &e_link) {
            Ok(()) => println!("  (e) over DANGLING junction   => Ok"),
            Err(e) => show_err("(e) over DANGLING junction", &e),
        }

        println!(
            "  >>> sys.rs:113 is_retriable_link_error tolerates ONLY raw os 5 and 32; \
             183 propagates as a hard failure"
        );
    }

    // ---------------------------------------------------------------- probe 4
    fn probe4_exists(root: &Path) {
        hdr("PROBE 4 — Path::exists() / try_exists() soundness");

        // (a) junction whose target directory was deleted
        let t = root.join("p4_target");
        mkdir(&t);
        let dangling = root.join("p4_dangling_junction");
        junction::create(&t, &dangling).expect("seed");
        fs::remove_dir_all(&t).expect("rm target");
        println!("  (a) dangling junction:");
        println!("      exists()      = {}", dangling.exists());
        println!("      try_exists()  = {:?}", dangling.try_exists());
        println!("      symlink_metadata().is_ok() = {}", fs::symlink_metadata(&dangling).is_ok());
        show_read_link("      read_link", &dangling);
        println!(
            "      >>> link.rs:562 `if aube_entry.exists() {{ cached; return }}` sees {} here",
            dangling.exists()
        );

        // (b) a path under a dangling junction
        let under = dangling.join("child");
        println!("  (b) path UNDER a dangling junction:");
        println!("      exists()      = {}", under.exists());
        println!("      try_exists()  = {:?}", under.try_exists());

        // (c) file held open with share_mode(0)
        let held_dir = root.join("p4_held");
        mkdir(&held_dir);
        let held_file = held_dir.join("held.txt");
        fs::write(&held_file, b"h").unwrap();
        let guard = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&held_file);
        println!("  (c) FILE held open with share_mode(0) (same process):");
        match &guard {
            Ok(_) => {
                println!("      exists()      = {}", held_file.exists());
                println!("      try_exists()  = {:?}", held_file.try_exists());
                println!("      metadata()    = {:?}", fs::metadata(&held_file).map(|m| m.len()).map_err(|e| (e.kind(), e.raw_os_error())));
                show_read_link("      read_link", &held_file);
            }
            Err(e) => show_err("      open(share_mode 0)", e),
        }
        drop(guard);

        // (d) DIRECTORY held open with share_mode(0)
        let held_dir2 = root.join("p4_held_dir");
        mkdir(&held_dir2);
        let dguard = fs::OpenOptions::new()
            .access_mode(GENERIC_READ)
            .share_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&held_dir2);
        println!("  (d) DIRECTORY held open with share_mode(0):");
        match &dguard {
            Ok(_) => {
                println!("      exists()      = {}", held_dir2.exists());
                println!("      try_exists()  = {:?}", held_dir2.try_exists());
                show_read_link("      read_link", &held_dir2);
                let inner = held_dir2.join("inner");
                println!(
                    "      create_dir(inner) = {:?}",
                    fs::create_dir(&inner).map_err(|e| (e.kind(), e.raw_os_error()))
                );
                println!(
                    "      junction::create(->target, held dir) = {:?}",
                    junction::create(root.join("p4_held"), &held_dir2)
                        .map_err(|e| (e.kind(), e.raw_os_error()))
                );
            }
            Err(e) => show_err("      open dir(share_mode 0)", e),
        }
        drop(dguard);

        // (e) path longer than MAX_PATH without an explicit \\?\ prefix
        let mut deep = root.join("p4_long");
        mkdir(&deep);
        while deep.as_os_str().len() < 300 {
            deep = deep.join("abcdefghijklmnopqrstuvwxyz0123456789");
        }
        let mk = fs::create_dir_all(&deep);
        println!("  (e) long path ({} chars):", deep.as_os_str().len());
        println!("      create_dir_all = {:?}", mk.as_ref().map_err(|e| (e.kind(), e.raw_os_error())));
        if mk.is_ok() {
            println!("      exists()       = {}", deep.exists());
            println!("      try_exists()   = {:?}", deep.try_exists());
            let verbatim = PathBuf::from(format!(r"\\?\{}", deep.display()));
            println!("      verbatim exists() = {}", verbatim.exists());
            show_read_link("      read_link(long real dir)", &deep);
            let lj = deep.join("j");
            println!(
                "      junction::create(target, <long path>) = {:?}",
                junction::create(root.join("p4_target_ok"), &lj)
                    .map_err(|e| (e.kind(), e.raw_os_error()))
            );
        }

        println!(
            "  >>> `exists()` returning FALSE for an entry that IS on disk is what would \
             let the linker enter materialize_into() with the path occupied"
        );
    }

    // ---------------------------------------------------------------- probe 5
    fn probe5_remove_dir(root: &Path) {
        hdr("PROBE 5 — remove_dir / remove_dir_all / remove_file on junctions");

        let target = root.join("p5_target");
        mkdir(&target);
        fs::write(target.join("payload.txt"), b"payload").unwrap();

        let j1 = root.join("p5_j_remove_dir");
        junction::create(&target, &j1).expect("seed j1");
        println!(
            "  remove_dir(JUNCTION)         = {:?}",
            fs::remove_dir(&j1).map_err(|e| (e.kind(), e.raw_os_error()))
        );
        println!(
            "      target payload survived? {}",
            target.join("payload.txt").exists()
        );

        let j2 = root.join("p5_j_remove_file");
        junction::create(&target, &j2).expect("seed j2");
        println!(
            "  remove_file(JUNCTION)        = {:?}",
            fs::remove_file(&j2).map_err(|e| (e.kind(), e.raw_os_error()))
        );

        let j3 = root.join("p5_j_remove_dir_all");
        junction::create(&target, &j3).expect("seed j3");
        println!(
            "  remove_dir_all(JUNCTION)     = {:?}",
            fs::remove_dir_all(&j3).map_err(|e| (e.kind(), e.raw_os_error()))
        );
        println!(
            "      target payload survived? {}  <<< true means remove_dir_all did NOT follow the junction",
            target.join("payload.txt").exists()
        );

        let nonempty = root.join("p5_nonempty_real");
        mkdir(&nonempty);
        fs::write(nonempty.join("x.txt"), b"x").unwrap();
        println!(
            "  remove_dir(NON-EMPTY REAL)   = {:?}",
            fs::remove_dir(&nonempty).map_err(|e| (e.kind(), e.raw_os_error()))
        );

        let empty = root.join("p5_empty_real");
        mkdir(&empty);
        println!(
            "  remove_dir(EMPTY REAL)       = {:?}",
            fs::remove_dir(&empty).map_err(|e| (e.kind(), e.raw_os_error()))
        );

        let jd_target = root.join("p5_dangling_target");
        mkdir(&jd_target);
        let jd = root.join("p5_dangling_junction");
        junction::create(&jd_target, &jd).expect("seed jd");
        fs::remove_dir_all(&jd_target).expect("rm");
        println!(
            "  remove_dir(DANGLING JUNCTION) = {:?}",
            fs::remove_dir(&jd).map_err(|e| (e.kind(), e.raw_os_error()))
        );
    }

    // ---------------------------------------------------------------- probe 6
    fn probe6_aube_shape(root: &Path) {
        hdr("PROBE 6 — the exact #576 shape: <store>/<entry>/node_modules/@types/connect");

        let store = root.join("p6_store");
        let entry = store.join("@types+body-parser@1.19.6");
        let nm = entry.join("node_modules");
        let scope = nm.join("@types");
        mkdir(&scope);
        let connect_target = store.join("@types+connect@3.4.38").join("node_modules").join("@types").join("connect");
        mkdir(&connect_target);
        fs::write(connect_target.join("index.d.ts"), b"// connect").unwrap();

        let link = scope.join("connect");

        println!("  first  create_dir_link => {:?}", junction::create(&connect_target, &link).map_err(|e| (e.kind(), e.raw_os_error())));
        println!("  exists() after first   => {}", link.exists());
        println!("  second create_dir_link => {:?}", junction::create(&connect_target, &link).map_err(|e| (e.kind(), e.raw_os_error())));
        println!("     ^^^ this is the #576 error signature if it prints raw_os_error 183");

        // The guard the linker actually relies on, at the ENTRY level.
        println!("  entry.exists() (link.rs:562 guard) = {}", entry.exists());

        // What if `connect` is a REAL directory rather than a junction?
        let link2 = scope.join("connect2");
        mkdir(&link2);
        println!(
            "  create_dir_link over a REAL dir => {:?}",
            junction::create(&connect_target, &link2).map_err(|e| (e.kind(), e.raw_os_error()))
        );

        // create_dir_all over an existing junction — the parent-scope call.
        println!(
            "  create_dir_all(<existing junction>) = {:?}",
            fs::create_dir_all(&link).map_err(|e| (e.kind(), e.raw_os_error()))
        );
        println!(
            "  create_dir(<existing junction>)     = {:?}",
            fs::create_dir(&link).map_err(|e| (e.kind(), e.raw_os_error()))
        );
    }

    // ---------------------------------------------------------------- probe 7
    //
    // The whole #576 hypothesis in one probe: can `Path::exists()` report FALSE
    // for a directory that is on disk AND whose children are still writable?
    // That combination is what lets `link.rs:562`'s `if aube_entry.exists()`
    // guard fall through into `materialize_into`, whose sibling-link loop then
    // hits an already-present junction and gets 183.
    fn probe7_exists_false_while_occupied(root: &Path) {
        hdr("PROBE 7 — can exists() be FALSE while the path is occupied?");

        let icacls = |args: &[&str]| {
            let out = std::process::Command::new("icacls").args(args).output();
            match out {
                Ok(o) => println!(
                    "      icacls {:?} -> status={:?} {}",
                    args,
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout).trim().replace('\n', " | ")
                ),
                Err(e) => println!("      icacls spawn failed: {e}"),
            }
        };

        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        println!("  running as USERNAME={user}");

        // (a) deny only ReadAttributes on the entry dir itself.
        let entry = root.join("p7_entry_deny_ra");
        let scope = entry.join("node_modules").join("@types");
        mkdir(&scope);
        let tgt = root.join("p7_target");
        mkdir(&tgt);
        junction::create(&tgt, scope.join("connect")).expect("seed connect junction");
        println!("  (a) deny RA (ReadAttributes) on the ENTRY dir:");
        println!("      before: exists() = {}", entry.exists());
        icacls(&[&entry.to_string_lossy(), "/deny", &format!("{user}:(RA)")]);
        println!("      after : exists()      = {}", entry.exists());
        println!("      after : try_exists()  = {:?}", entry.try_exists());
        println!(
            "      after : metadata()    = {:?}",
            fs::metadata(&entry).map(|m| m.is_dir()).map_err(|e| (e.kind(), e.raw_os_error()))
        );
        println!(
            "      after : create_dir_all(<entry>/node_modules/@types) = {:?}",
            fs::create_dir_all(&scope).map_err(|e| (e.kind(), e.raw_os_error()))
        );
        println!(
            "      after : junction::create over the existing connect = {:?}",
            junction::create(&tgt, scope.join("connect"))
                .map_err(|e| (e.kind(), e.raw_os_error()))
        );
        icacls(&[&entry.to_string_lossy(), "/remove:d", &user]);

        // (b) deny everything (F) on the entry dir.
        let entry2 = root.join("p7_entry_deny_full");
        let scope2 = entry2.join("node_modules").join("@types");
        mkdir(&scope2);
        junction::create(&tgt, scope2.join("connect")).expect("seed");
        println!("  (b) deny F (full) on the ENTRY dir:");
        icacls(&[&entry2.to_string_lossy(), "/deny", &format!("{user}:(F)")]);
        println!("      exists()     = {}", entry2.exists());
        println!("      try_exists() = {:?}", entry2.try_exists());
        println!(
            "      metadata()   = {:?}",
            fs::metadata(&entry2).map(|m| m.is_dir()).map_err(|e| (e.kind(), e.raw_os_error()))
        );
        println!(
            "      junction::create over the existing connect = {:?}",
            junction::create(&tgt, scope2.join("connect"))
                .map_err(|e| (e.kind(), e.raw_os_error()))
        );
        icacls(&[&entry2.to_string_lossy(), "/remove:d", &user]);

        // (c) deny RA on the PARENT, leaving the entry itself untouched.
        let parent = root.join("p7_parent_deny");
        let entry3 = parent.join("@types+body-parser@1.19.6");
        let scope3 = entry3.join("node_modules").join("@types");
        mkdir(&scope3);
        junction::create(&tgt, scope3.join("connect")).expect("seed");
        println!("  (c) deny RA on the PARENT (.store) dir:");
        icacls(&[&parent.to_string_lossy(), "/deny", &format!("{user}:(RA)")]);
        println!("      entry.exists() = {}", entry3.exists());
        println!(
            "      junction::create over the existing connect = {:?}",
            junction::create(&tgt, scope3.join("connect"))
                .map_err(|e| (e.kind(), e.raw_os_error()))
        );
        icacls(&[&parent.to_string_lossy(), "/remove:d", &user]);
    }

    // ---------------------------------------------------------------- probe 8
    //
    // Windows caps reparse-point traversal per path resolution. Past the cap a
    // path that genuinely exists resolves with ERROR_CANT_RESOLVE_FILENAME —
    // another way `exists()` can lie in the false direction.
    fn probe8_reparse_depth(root: &Path) {
        hdr("PROBE 8 — reparse-point traversal depth limit");

        let leaf = root.join("p8_leaf");
        mkdir(&leaf);
        fs::write(leaf.join("marker.txt"), b"m").unwrap();

        let mut current = leaf.clone();
        let chain = root.join("p8_chain");
        mkdir(&chain);
        for i in 0..40u32 {
            let link = chain.join(format!("j{i}"));
            if let Err(e) = junction::create(&current, &link) {
                println!("  create hop {i} failed: {e}");
                break;
            }
            let probe = link.join("marker.txt");
            let ex = probe.exists();
            let md = fs::metadata(&probe).map(|_| ()).map_err(|e| (e.kind(), e.raw_os_error()));
            if !ex || i % 8 == 0 {
                println!("  hop {i:>2}: marker exists()={ex} metadata={md:?}");
            }
            if !ex {
                println!("  >>> traversal cap hit at hop {i} — the file IS there, exists() says false");
                break;
            }
            current = link;
        }
    }
}
