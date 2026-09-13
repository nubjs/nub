//! Mounted acceptance for ordinary projected namespace operations.
//!
//! This is a direct-user-namespace FUSE fixture, not production wiring. Its
//! raw arm establishes that the host supports each syscall; its mounted arm
//! requires the provider to enforce the fixed path policy through the kernel.

use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};

const RENAME_NOREPLACE: u32 = 1;
const RENAME_EXCHANGE: u32 = 2;

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    inode: u64,
    mode: u32,
    bytes: Vec<u8>,
}

fn snapshot(path: &Path) -> Snapshot {
    let metadata = fs::metadata(path).unwrap();
    Snapshot {
        inode: metadata.ino(),
        mode: metadata.mode(),
        bytes: fs::read(path).unwrap(),
    }
}

fn assert_unchanged(path: &Path, before: &Snapshot, label: &str) {
    assert_eq!(
        snapshot(path),
        *before,
        "{label} changed backing canary {}",
        path.display()
    );
}

fn rule(path: &str, access: FsAccess) -> FsRule {
    FsRule {
        matcher: CanonGlob(path.into()),
        access,
        effect: Effect::Allow,
        origin: FsOrigin::Authored,
    }
}

fn namespace_fixture(root: &Path, exe: &Path) -> FsRuleSet {
    let mut rules = fixture(root, exe);
    rules.entries.extend([
        rule("/app/namespace/*.json", FsAccess::ReadWrite),
        rule("/app/namespace/*.dir", FsAccess::ReadWrite),
        rule("/app/namespace/read-only.locked", FsAccess::Read),
        // `/app` remains traversal-only: this exact future leaf must not grant
        // authority to its siblings or parent directory.
        rule("/app/exact-leaf.json", FsAccess::ReadWrite),
    ]);
    let namespace = root.join("raw/app/namespace");
    fs::create_dir(&namespace).unwrap();
    for (name, bytes) in [
        ("rename-source.json", b"rename-source".as_slice()),
        ("noreplace-source.json", b"noreplace-source".as_slice()),
        (
            "noreplace-destination.json",
            b"noreplace-destination".as_slice(),
        ),
        ("exchange-left.json", b"exchange-left".as_slice()),
        ("exchange-right.json", b"exchange-right".as_slice()),
        ("unlink.json", b"unlink".as_slice()),
        ("held.json", b"held-old".as_slice()),
        ("target.json", b"target".as_slice()),
        ("read-only.locked", b"read-only".as_slice()),
        ("nearest.txt", b"nearest-canary".as_slice()),
    ] {
        fs::write(namespace.join(name), bytes).unwrap();
    }
    rules
}

fn assert_errno<T>(result: io::Result<T>, expected: i32, label: &str) {
    match result {
        Err(error) if error.raw_os_error() == Some(expected) => {}
        Err(error) => panic!("{label}: expected errno {expected}, got {error}"),
        Ok(_) => panic!("{label}: unexpectedly succeeded"),
    }
}

fn create(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)
}

fn renameat2(old: &Path, new: &Path, flags: u32) -> io::Result<()> {
    let old = CString::new(old.as_os_str().as_bytes()).unwrap();
    let new = CString::new(new.as_os_str().as_bytes()).unwrap();
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            flags,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_existing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(bytes)
}

fn case_marker(case: &str, projected: bool) {
    println!("NAMESPACE_CASE {case} projected={projected}");
    io::stdout().flush().unwrap();
}

fn namespace_command(root: &Path, projected: bool) {
    let app = root.join("app");
    let namespace = app.join("namespace");
    let future = namespace.join("future.json");
    create(&future, b"future").unwrap();
    fs::create_dir(namespace.join("future.dir")).unwrap();
    case_marker("create_mkdir", projected);

    symlink("target.json", namespace.join("target-link.json")).unwrap();
    assert_eq!(
        fs::read(namespace.join("target-link.json")).unwrap(),
        b"target"
    );
    symlink("nearest.txt", namespace.join("denied-target.json")).unwrap();
    if projected {
        denied(
            File::open(namespace.join("denied-target.json")),
            "symlink target must receive fresh authority",
        );
    } else {
        assert_eq!(
            fs::read(namespace.join("denied-target.json")).unwrap(),
            b"nearest-canary"
        );
    }
    case_marker("symlink_authority", projected);

    let shared = namespace.join("shared.json");
    fs::hard_link(&future, &shared).unwrap();
    write_existing(&shared, b"shared-through-link").unwrap();
    assert_eq!(fs::read(&future).unwrap(), b"shared-through-link");
    case_marker("hardlink_contents", projected);

    let renamed = namespace.join("renamed.json");
    fs::rename(namespace.join("rename-source.json"), &renamed).unwrap();
    assert_eq!(fs::read(&renamed).unwrap(), b"rename-source");
    assert_errno(
        renameat2(
            &namespace.join("noreplace-source.json"),
            &namespace.join("noreplace-destination.json"),
            RENAME_NOREPLACE,
        ),
        libc::EEXIST,
        "rename no-replace collision",
    );
    assert_eq!(
        fs::read(namespace.join("noreplace-source.json")).unwrap(),
        b"noreplace-source"
    );
    assert_eq!(
        fs::read(namespace.join("noreplace-destination.json")).unwrap(),
        b"noreplace-destination"
    );
    renameat2(
        &namespace.join("exchange-left.json"),
        &namespace.join("exchange-right.json"),
        RENAME_EXCHANGE,
    )
    .unwrap();
    assert_eq!(
        fs::read(namespace.join("exchange-left.json")).unwrap(),
        b"exchange-right"
    );
    assert_eq!(
        fs::read(namespace.join("exchange-right.json")).unwrap(),
        b"exchange-left"
    );
    case_marker("rename_flags", projected);

    let mut held = OpenOptions::new()
        .read(true)
        .write(true)
        .open(namespace.join("held.json"))
        .unwrap();
    let held_name = namespace.join("held-renamed.json");
    fs::rename(namespace.join("held.json"), &held_name).unwrap();
    fs::remove_file(&held_name).unwrap();
    held.write_all(b"retained").unwrap();
    held.seek(SeekFrom::Start(0)).unwrap();
    let mut retained = Vec::new();
    held.read_to_end(&mut retained).unwrap();
    assert_eq!(retained, b"retained");
    assert!(!held_name.exists());
    case_marker("retained_handle", projected);

    fs::remove_file(namespace.join("unlink.json")).unwrap();
    fs::remove_dir(namespace.join("future.dir")).unwrap();
    create(&app.join("exact-leaf.json"), b"exact-parent-leaf").unwrap();

    if projected {
        assert_errno(
            create(&namespace.join("future.txt"), b"denied-create"),
            libc::EACCES,
            "nearest nonmatching create",
        );
        assert_errno(
            fs::create_dir(app.join("parent-near")),
            libc::EACCES,
            "traversal-only parent create",
        );
        assert_errno(
            fs::rename(&renamed, namespace.join("rename-near.txt")),
            libc::EACCES,
            "nonmatching rename destination",
        );
        denied(
            write_existing(&namespace.join("nearest.txt"), b"mutated"),
            "nearest hidden-name mutation",
        );
        assert_errno(
            write_existing(&namespace.join("read-only.locked"), b"mutated"),
            libc::EACCES,
            "read-only write",
        );
        assert_errno(
            fs::remove_file(namespace.join("read-only.locked")),
            libc::EACCES,
            "read-only unlink",
        );
        assert_errno(
            fs::rename(
                namespace.join("read-only.locked"),
                namespace.join("read-only-renamed.json"),
            ),
            libc::EACCES,
            "read-only rename",
        );
        assert_errno(
            fs::hard_link(
                namespace.join("read-only.locked"),
                namespace.join("read-only-link.json"),
            ),
            libc::EACCES,
            "read-only hardlink source",
        );
    } else {
        create(&namespace.join("future.txt"), b"raw-control").unwrap();
        fs::create_dir(app.join("parent-near")).unwrap();
        fs::rename(&renamed, namespace.join("rename-near.txt")).unwrap();
        write_existing(&namespace.join("nearest.txt"), b"raw-mutated").unwrap();
        write_existing(&namespace.join("read-only.locked"), b"raw-mutated").unwrap();
        fs::hard_link(
            namespace.join("read-only.locked"),
            namespace.join("read-only-link.json"),
        )
        .unwrap();
        fs::rename(
            namespace.join("read-only.locked"),
            namespace.join("read-only-renamed.json"),
        )
        .unwrap();
        fs::remove_file(namespace.join("read-only-renamed.json")).unwrap();
    }
    case_marker("authority_controls", projected);
    println!("NAMESPACE_COMMAND_OK projected={projected}");
}

fn verify_backing(backing_root: &Path, projected: bool, nearest: &Snapshot, read_only: &Snapshot) {
    let app = backing_root.join("app");
    let namespace = app.join("namespace");
    assert_eq!(
        fs::read(namespace.join("future.json")).unwrap(),
        b"shared-through-link"
    );
    assert_eq!(
        fs::read(namespace.join("shared.json")).unwrap(),
        b"shared-through-link"
    );
    assert_eq!(
        fs::metadata(namespace.join("future.json")).unwrap().ino(),
        fs::metadata(namespace.join("shared.json")).unwrap().ino(),
        "hardlink aliases must share backing contents"
    );
    assert_eq!(
        fs::read_link(namespace.join("target-link.json")).unwrap(),
        Path::new("target.json")
    );
    assert_eq!(
        fs::read_link(namespace.join("denied-target.json")).unwrap(),
        Path::new("nearest.txt")
    );
    assert!(!namespace.join("rename-source.json").exists());
    assert_eq!(
        fs::read(namespace.join("noreplace-source.json")).unwrap(),
        b"noreplace-source"
    );
    assert_eq!(
        fs::read(namespace.join("noreplace-destination.json")).unwrap(),
        b"noreplace-destination"
    );
    assert_eq!(
        fs::read(namespace.join("exchange-left.json")).unwrap(),
        b"exchange-right"
    );
    assert_eq!(
        fs::read(namespace.join("exchange-right.json")).unwrap(),
        b"exchange-left"
    );
    assert!(!namespace.join("held.json").exists());
    assert!(!namespace.join("held-renamed.json").exists());
    assert!(!namespace.join("unlink.json").exists());
    assert!(!namespace.join("future.dir").exists());
    assert_eq!(
        fs::read(app.join("exact-leaf.json")).unwrap(),
        b"exact-parent-leaf"
    );

    if projected {
        assert_eq!(
            fs::read(namespace.join("renamed.json")).unwrap(),
            b"rename-source"
        );
        assert!(!namespace.join("future.txt").exists());
        assert!(!app.join("parent-near").exists());
        assert!(!namespace.join("rename-near.txt").exists());
        assert_unchanged(&namespace.join("nearest.txt"), nearest, "nearest name");
        assert_unchanged(
            &namespace.join("read-only.locked"),
            read_only,
            "read-only source",
        );
        assert!(!namespace.join("read-only-link.json").exists());
    } else {
        assert!(!namespace.join("renamed.json").exists());
        assert_eq!(
            fs::read(namespace.join("future.txt")).unwrap(),
            b"raw-control"
        );
        assert!(app.join("parent-near").is_dir());
        assert_eq!(
            fs::read(namespace.join("rename-near.txt")).unwrap(),
            b"rename-source"
        );
        assert_eq!(
            fs::read(namespace.join("nearest.txt")).unwrap(),
            b"raw-mutated"
        );
        assert!(!namespace.join("read-only.locked").exists());
        assert!(namespace.join("read-only-link.json").is_file());
    }
}

fn namespace_provider(root: &Path) {
    unsafe {
        libc::alarm(45);
        libc::umask(0);
    }
    let rules: FsRuleSet =
        serde_json::from_slice(&fs::read(root.join("rules.json")).unwrap()).unwrap();
    let namespace = root.join("raw/app/namespace");
    let nearest = snapshot(&namespace.join("nearest.txt"));
    let read_only = snapshot(&namespace.join("read-only.locked"));
    // Ensure the command's executable and its dependency are resolved through
    // the mounted provider rather than an inherited host file descriptor.
    for name in ["run", "math.so"] {
        fs::rename(root.join("raw/app").join(name), root.join(name)).unwrap();
    }
    let raw = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(root.join("raw"))
        .unwrap();
    let projection = Projection::acquire(&rules, raw).unwrap();
    let fuse = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/dev/fuse")
        .expect("direct unprivileged /dev/fuse prerequisite");
    let view = root.join("view");
    let target = CString::new(view.as_os_str().as_bytes()).unwrap();
    let options = CString::new(format!(
        "fd={},rootmode=40000,user_id=0,group_id=0",
        fuse.as_raw_fd()
    ))
    .unwrap();
    checked(unsafe {
        libc::mount(
            c"nub-namespace-test".as_ptr(),
            target.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    })
    .expect("direct FUSE mount prerequisite");
    for name in ["run", "math.so"] {
        fs::rename(root.join(name), root.join("raw/app").join(name)).unwrap();
    }
    let fuse_fd = fuse.as_raw_fd();
    let server = std::thread::spawn(move || projection.serve(fuse.into()));
    let mut cmd = helper(Path::new("/app/run"), "namespace-command", Path::new("/"));
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    confined_launch(&mut cmd, &view, fuse_fd);
    let output = cmd.output().unwrap();
    println!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The command has been reaped and its complete output retained before the
    // owner unmounts; a provider error is never reclassified as normal teardown.
    unmount_projection(&target, server);
    fs::remove_dir(&view).unwrap();
    assert!(
        output.status.success(),
        "namespace mounted command failed: {output:?}"
    );
    verify_backing(&root.join("raw"), true, &nearest, &read_only);
    println!("NAMESPACE_PROVIDER_OK");
}

fn namespace_raw(root: &Path) {
    let namespace = root.join("app/namespace");
    let nearest = snapshot(&namespace.join("nearest.txt"));
    let read_only = snapshot(&namespace.join("read-only.locked"));
    namespace_command(root, false);
    verify_backing(root, false, &nearest, &read_only);
    println!("NAMESPACE_RAW_OK");
}

fn namespace_supervisor(root: &Path) {
    unsafe {
        libc::alarm(90);
    }
    assert_ne!(
        unsafe { libc::getuid() },
        0,
        "probe must start as an ordinary non-root user"
    );
    assert!(
        caps()
            .iter()
            .all(|cap| cap.effective == 0 && cap.permitted == 0)
    );
    checked(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }).unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut failures = Vec::new();
    for (arm, role) in [("raw", "namespace-raw"), ("mounted", "namespace-provider")] {
        println!("NAMESPACE_ARM_START {arm}");
        io::stdout().flush().unwrap();
        let case = root.join(arm);
        fs::create_dir(&case).unwrap();
        let rules = namespace_fixture(&case, &exe);
        fs::write(case.join("rules.json"), serde_json::to_vec(&rules).unwrap()).unwrap();
        let mut cmd = if arm == "raw" {
            helper(&case.join("raw/app/run"), role, &case.join("raw"))
        } else {
            helper(&exe, role, &case)
        };
        if arm != "raw" {
            namespace_launch(&mut cmd);
        }
        let output = cmd.output().unwrap();
        println!(
            "--- namespace {arm} ---\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            failures.push(format!("{arm}: {:?}", output.status));
        }
        fs::remove_dir_all(&case).unwrap();
    }
    assert!(
        failures.is_empty(),
        "NAMESPACE_MOUNT_ACCEPTANCE_FAILURE {failures:?}"
    );
    println!("NAMESPACE_MOUNT_PROBE_OK");
}

pub(super) fn run_role(role: &str, root: &Path) -> bool {
    match role {
        "namespace-supervisor" => namespace_supervisor(root),
        "namespace-raw" => namespace_raw(root),
        "namespace-provider" => namespace_provider(root),
        "namespace-command" => {
            audit_command();
            namespace_command(Path::new("/"), true);
        }
        _ => return false,
    }
    true
}

#[test]
#[ignore = "requires an ordinary Linux user with direct user namespaces and /dev/fuse"]
fn mounted_projection_namespace_operations() {
    run_mount_probe("namespace-supervisor", "NAMESPACE_MOUNT_PROBE_OK");
}
