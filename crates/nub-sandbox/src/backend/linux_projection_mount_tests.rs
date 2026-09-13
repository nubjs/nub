//! Explicit real-kernel probe; not production launch wiring or full filesystem coverage.

use super::linux_projection::Projection;
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, FsRuleSet};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[path = "linux_projection_exec_tests.rs"]
mod exec_tests;
#[path = "linux_projection_native_tests.rs"]
mod native_tests;

const HELPER: &str = "backend::linux_projection_mount_tests::projection_mount_helper";
const ROLE: &str = "NUB_PROJECTION_TEST_ROLE";

fn helper(exe: &Path, role: &str, root: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", HELPER, "--nocapture", "--test-threads=1"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env(ROLE, role)
        .env("NUB_PROJECTION_TEST_ROOT", root)
        .stdin(Stdio::null());
    cmd
}

fn checked(rc: i32) -> io::Result<()> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn unmounted_session_result(result: io::Result<()>) -> io::Result<()> {
    match result {
        // Linux may report connection abort to a pending FUSE read when the
        // owner unmounts. This classification is valid only after that unmount.
        Err(error) if error.raw_os_error() == Some(libc::ECONNABORTED) => Ok(()),
        result => result,
    }
}

fn unmount_projection(target: &std::ffi::CStr, server: std::thread::JoinHandle<io::Result<()>>) {
    assert!(
        !server.is_finished(),
        "FUSE server ended before owner unmount"
    );
    checked(unsafe { libc::umount2(target.as_ptr(), 0) }).expect("normal FUSE unmount");
    unmounted_session_result(server.join().expect("FUSE server panicked")).unwrap();
}

#[test]
fn owner_unmount_classifies_only_connection_abort() {
    assert!(unmounted_session_result(Ok(())).is_ok());
    assert!(
        unmounted_session_result(Err(io::Error::from_raw_os_error(libc::ECONNABORTED))).is_ok()
    );
    for errno in [libc::EIO, libc::EACCES, libc::ENOTCONN] {
        assert_eq!(
            unmounted_session_result(Err(io::Error::from_raw_os_error(errno)))
                .unwrap_err()
                .raw_os_error(),
            Some(errno)
        );
    }
}

// All pre_exec callers precompute strings and IDs. No allocation, filesystem
// wrappers, locks, or arbitrary Rust callbacks run between fork and exec.
unsafe fn parent_death(parent: libc::pid_t) -> io::Result<()> {
    unsafe {
        checked(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0))?;
    }
    if unsafe { libc::getppid() } != parent {
        return Err(io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

unsafe fn write_map(path: &std::ffi::CStr, value: &[u8]) -> io::Result<()> {
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    checked(fd)?;
    let n = unsafe { libc::write(fd, value.as_ptr().cast(), value.len()) };
    let error = io::Error::last_os_error();
    unsafe {
        libc::close(fd);
    }
    if n == value.len() as isize {
        Ok(())
    } else if n >= 0 {
        Err(io::Error::from_raw_os_error(libc::EIO))
    } else {
        Err(error)
    }
}

fn namespace_launch(cmd: &mut Command) {
    // These are the ordinary caller's IDs, not an elevated mapping helper.
    let uid = format!("0 {} 1\n", unsafe { libc::getuid() });
    let gid = format!("0 {} 1\n", unsafe { libc::getgid() });
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || {
            parent_death(parent)?;
            checked(libc::unshare(libc::CLONE_NEWUSER))?;
            write_map(c"/proc/self/setgroups", b"deny\n")?;
            write_map(c"/proc/self/uid_map", uid.as_bytes())?;
            write_map(c"/proc/self/gid_map", gid.as_bytes())?;
            checked(libc::unshare(libc::CLONE_NEWNS))?;
            checked(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ))?;
            // Credential changes can clear PDEATHSIG; arm it again after mapping.
            parent_death(parent)
        });
    }
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}
#[derive(Default)]
#[repr(C)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn caps() -> [CapData; 2] {
    let mut header = CapHeader {
        version: 0x20080522,
        pid: 0,
    };
    let mut data = [CapData::default(), CapData::default()];
    assert_eq!(
        unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) },
        0
    );
    data
}

fn confined_launch(cmd: &mut Command, mountpoint: &Path, fuse_fd: i32) {
    let mountpoint = CString::new(mountpoint.as_os_str().as_bytes()).unwrap();
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || {
            parent_death(parent)?;
            // Preserve std's exec-error pipe until exec, so a launch failure
            // reports its errno. No backing authority survives into user code.
            checked(libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) as i32)?;
            // A child blocked before exec must not keep the connection alive.
            checked(libc::close(fuse_fd))?;
            checked(libc::chroot(mountpoint.as_ptr()))?;
            checked(libc::chdir(c"/".as_ptr()))?;
            checked(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            let header = CapHeader {
                version: 0x20080522,
                pid: 0,
            };
            let data = [CapData::default(), CapData::default()];
            checked(libc::syscall(libc::SYS_capset, &header, data.as_ptr()) as i32)?;
            parent_death(parent)
        });
    }
}

fn wait(child: &mut Child) -> ExitStatus {
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    panic!("projection helper {} exceeded 30 seconds", child.id());
}

fn event(reader: &mut impl BufRead, prefix: &str) -> String {
    // Helpers carry alarms and PDEATHSIG; EOF is a failure, never readiness.
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0, "missing {prefix}");
        print!("{line}");
        if let Some(value) = line.trim().strip_prefix(prefix) {
            return value.to_owned();
        }
    }
}

fn copy(src: &Path, dst: &Path) {
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::copy(src, dst).unwrap();
}

fn fixture(root: &Path, exe: &Path) -> FsRuleSet {
    let raw = root.join("raw");
    fs::create_dir_all(raw.join("app")).unwrap();
    fs::create_dir(root.join("view")).unwrap();
    let libm = [
        "/lib/x86_64-linux-gnu/libm.so.6",
        "/lib/aarch64-linux-gnu/libm.so.6",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
    .expect("glibc libm fixture");
    // Test-only ELF closure, not production executable discovery or policy widening.
    let mut libraries = BTreeSet::new();
    let mut pending = vec![exe.to_owned(), libm.clone()];
    while let Some(binary) = pending.pop() {
        let output = Command::new("ldd").arg(&binary).output().unwrap();
        assert!(
            output.status.success(),
            "ldd {}: {:?}",
            binary.display(),
            output
        );
        for word in String::from_utf8(output.stdout).unwrap().split_whitespace() {
            if word.starts_with('/') && libraries.insert(PathBuf::from(word)) {
                pending.push(PathBuf::from(word));
            }
        }
    }
    let mut grants = vec![
        ("/app/run".to_owned(), FsAccess::Read),
        ("/app/math.so".to_owned(), FsAccess::Read),
        ("/app/*.dat".to_owned(), FsAccess::ReadWrite),
        ("/app/shared-read".to_owned(), FsAccess::Read),
        ("/app/mapped-read".to_owned(), FsAccess::Read),
    ];
    for path in libraries {
        copy(&path, &raw.join(path.strip_prefix("/").unwrap()));
        grants.push((path.to_str().unwrap().to_owned(), FsAccess::Read));
    }
    copy(exe, &raw.join("app/run"));
    copy(&libm, &raw.join("app/math.so"));
    copy(exe, &raw.join("app/run-near"));
    fs::write(raw.join("app/near.txt"), b"untouched").unwrap();
    fs::write(raw.join("app/shared.dat"), b"before").unwrap();
    fs::hard_link(raw.join("app/shared.dat"), raw.join("app/shared-read")).unwrap();
    fs::hard_link(raw.join("app/shared.dat"), raw.join("app/omitted-hardlink")).unwrap();
    fs::write(raw.join("app/held.dat"), b"old").unwrap();
    let mut page = vec![0; 4096];
    page[..8].copy_from_slice(b"initial!");
    fs::write(raw.join("app/mmap.dat"), &page).unwrap();
    fs::write(raw.join("app/mapped-alias.dat"), &page).unwrap();
    fs::hard_link(
        raw.join("app/mapped-alias.dat"),
        raw.join("app/mapped-read"),
    )
    .unwrap();
    FsRuleSet {
        entries: grants
            .into_iter()
            .map(|(path, access)| FsRule {
                matcher: CanonGlob(path),
                access,
                effect: Effect::Allow,
                origin: FsOrigin::Authored,
            })
            .collect(),
        default_effect: Effect::Deny,
    }
}

fn dlopen_call(path: &Path) {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        assert!(
            !handle.is_null(),
            "dlopen {} failed",
            path.to_string_lossy()
        );
        let symbol = libc::dlsym(handle, c"cos".as_ptr());
        assert!(!symbol.is_null());
        let cos: unsafe extern "C" fn(f64) -> f64 = std::mem::transmute(symbol);
        assert_eq!(cos(0.0), 1.0);
        assert_eq!(libc::dlclose(handle), 0);
    }
}

fn denied<T>(result: io::Result<T>, label: &str) {
    match result {
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::EACCES)) => {}
        Err(error) => panic!("{label}: wrong denial {error}"),
        Ok(_) => panic!("{label}: unauthorized access succeeded"),
    }
}

fn replace_held(raw: &Path) {
    fs::rename(raw.join("app/held.dat"), raw.join("app/held-hidden")).unwrap();
    fs::write(raw.join("app/held.dat"), b"replacement").unwrap();
}

struct Mapping(*mut libc::c_void);

impl Mapping {
    fn new(file: &File, protection: i32, flags: i32) -> io::Result<Self> {
        // Fixture files are a full page and remain live for the mapping's use.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                protection,
                flags,
                file.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(address))
        }
    }

    fn prefix(&self) -> [u8; 8] {
        // Read through the actual mapping each time, including after another
        // descriptor changes its backing inode; no snapshot or file reread.
        std::array::from_fn(|index| unsafe {
            std::ptr::read_volatile(self.0.cast::<u8>().add(index))
        })
    }

    unsafe fn write_prefix(&mut self, bytes: &[u8; 8]) {
        // Caller must have constructed this mapping with PROT_WRITE.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.0.cast::<u8>(), bytes.len());
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        assert_eq!(unsafe { libc::munmap(self.0, 4096) }, 0);
    }
}

fn exercise_mappings(app: &Path, projected: bool) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(app.join("mmap.dat"))
        .unwrap();
    let mut private =
        Mapping::new(&file, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE).unwrap();
    assert_eq!(private.prefix(), *b"initial!");
    unsafe {
        private.write_prefix(b"private!");
    }
    assert_eq!(private.prefix(), *b"private!");
    assert_eq!(&fs::read(app.join("mmap.dat")).unwrap()[..8], b"initial!");
    drop(private);
    let mut shared =
        Mapping::new(&file, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED).unwrap();
    assert_eq!(shared.prefix(), *b"initial!");
    unsafe {
        shared.write_prefix(b"shared!!");
    }
    assert_eq!(unsafe { libc::msync(shared.0, 4096, libc::MS_SYNC) }, 0);
    assert_eq!(&fs::read(app.join("mmap.dat")).unwrap()[..8], b"shared!!");
    drop(shared);
    println!("MMAP_PRIVATE_SHARED_MSYNC_OK projected={projected}");

    let read_only = File::open(app.join("mapped-read")).unwrap();
    // A read-only descriptor cannot create a writable shared map in either arm.
    denied(
        Mapping::new(
            &read_only,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
        ),
        "writable shared map through read-only handle",
    );
    let writable_alias = OpenOptions::new()
        .read(true)
        .write(true)
        .open(app.join("mapped-read"));
    if projected {
        denied(writable_alias, "read-only alias writable mapping open");
    } else {
        let writable_alias = writable_alias.unwrap();
        drop(
            Mapping::new(
                &writable_alias,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
            )
            .unwrap(),
        );
    }

    let alias = Mapping::new(&read_only, libc::PROT_READ, libc::MAP_SHARED).unwrap();
    assert_eq!(alias.prefix(), *b"initial!");
    let mut writer = OpenOptions::new()
        .write(true)
        .open(app.join("mapped-alias.dat"))
        .unwrap();
    writer.write_all(b"changed!").unwrap();
    // An ordinary sequential write must be visible through the prefaulted
    // read-only alias. No msync, invalidation call, sleep, or reopen repairs it.
    assert_eq!(
        alias.prefix(),
        *b"changed!",
        "prefaulted mapped hardlink alias must observe ordinary writes"
    );
    println!("MMAP_ALIAS_COHERENCE_OK projected={projected}");
}

fn exercise(base: &Path, projected: bool) {
    let app = base.join("app");
    dlopen_call(&app.join("math.so"));
    let mut out = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(app.join("future.dat"))
        .unwrap();
    out.write_all(b"host-visible-tail").unwrap();
    out.set_len(4).unwrap();
    drop(out);
    assert_eq!(fs::read(app.join("future.dat")).unwrap(), b"host");
    let mut out = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(app.join("future.dat"))
        .unwrap();
    out.write_all(b"final").unwrap();
    drop(out);
    let mut alias = File::open(app.join("shared-read")).unwrap();
    let mut initial = String::new();
    alias.read_to_string(&mut initial).unwrap();
    assert_eq!(initial, "before");
    let mut shared = OpenOptions::new()
        .write(true)
        .open(app.join("shared.dat"))
        .unwrap();
    shared.write_all(b"shared").unwrap();
    drop(shared);
    alias.seek(SeekFrom::Start(0)).unwrap();
    let mut updated = String::new();
    alias.read_to_string(&mut updated).unwrap();
    assert_eq!(
        updated, "shared",
        "already-open hardlink alias cache must not hide writes"
    );
    drop(alias);
    assert_eq!(fs::read(app.join("shared-read")).unwrap(), b"shared");
    let mut held = OpenOptions::new()
        .read(true)
        .write(true)
        .open(app.join("held.dat"))
        .unwrap();
    if projected {
        println!("HELD");
        io::stdout().flush().unwrap();
        let mut ack = String::new();
        io::stdin().read_line(&mut ack).unwrap();
        assert_eq!(ack.trim(), "replaced");
    } else {
        replace_held(base);
    }
    held.write_all(b"retained").unwrap();
    held.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    held.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"retained");
    assert_eq!(fs::read(app.join("held.dat")).unwrap(), b"replacement");
    for name in ["near.txt", "omitted-hardlink", "run-near", "held-hidden"] {
        if projected {
            denied(File::open(app.join(name)), name);
            denied(OpenOptions::new().write(true).open(app.join(name)), name);
        } else {
            assert!(File::open(app.join(name)).is_ok(), "raw {name}");
            assert!(
                OpenOptions::new().write(true).open(app.join(name)).is_ok(),
                "raw writable {name}"
            );
        }
    }
    if projected {
        denied(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(app.join("future.txt")),
            "nonmatching create",
        );
        denied(
            OpenOptions::new().write(true).open(app.join("shared-read")),
            "read-only hardlink",
        );
        denied(
            fs::read_dir(&app),
            "traversal is not directory read authority",
        );
        denied(File::open("/proc/self/root"), "host procfs escape");
        assert!(
            caps()
                .iter()
                .all(|c| c.effective == 0 && c.permitted == 0 && c.inheritable == 0)
        );
        assert_eq!(
            unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) },
            1
        );
        println!("PROJECTED_EXEC_DLOPEN_IO_OK CAPS=0 NNP=1");
    } else {
        println!("RAW_EXEC_DLOPEN_IO_OK");
    }
}

fn verify_backing(root: &Path) {
    let app = root.join("raw/app");
    assert_eq!(fs::read(app.join("future.dat")).unwrap(), b"final");
    assert_eq!(fs::read(app.join("omitted-hardlink")).unwrap(), b"shared");
    assert_eq!(fs::read(app.join("held-hidden")).unwrap(), b"retained");
    assert_eq!(fs::read(app.join("near.txt")).unwrap(), b"untouched");
    assert!(!app.join("future.txt").exists());
    assert_eq!(
        fs::metadata(app.join("shared.dat")).unwrap().ino(),
        fs::metadata(app.join("omitted-hardlink")).unwrap().ino()
    );
}

fn verify_mapped_backing(root: &Path) {
    let app = root.join("raw/app");
    assert_eq!(&fs::read(app.join("mmap.dat")).unwrap()[..8], b"shared!!");
    assert_eq!(
        &fs::read(app.join("mapped-read")).unwrap()[..8],
        b"changed!"
    );
}

fn provider(root: &Path, owner_loss: bool, mappings: bool) {
    unsafe {
        libc::alarm(20);
        libc::umask(0);
    }
    let rules: FsRuleSet =
        serde_json::from_slice(&fs::read(root.join("rules.json")).unwrap()).unwrap();
    // Keep these allowed paths absent during acquisition and mount. The real
    // command executes/dlopen's them only after the fixed policy is installed.
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
    let mountpoint = root.join("view");
    let target = CString::new(mountpoint.as_os_str().as_bytes()).unwrap();
    let options = CString::new(format!(
        "fd={},rootmode=40000,user_id=0,group_id=0",
        fuse.as_raw_fd()
    ))
    .unwrap();
    checked(unsafe {
        libc::mount(
            c"nub-projection-test".as_ptr(),
            target.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    })
    .expect("direct FUSE mount prerequisite");
    println!(
        "DIRECT_MOUNT_OK namespace={:?}",
        fs::read_link("/proc/self/ns/mnt").unwrap()
    );
    for name in ["run", "math.so"] {
        fs::rename(root.join(name), root.join("raw/app").join(name)).unwrap();
    }
    let fuse_fd = fuse.as_raw_fd();
    let server = std::thread::spawn(move || projection.serve(fuse.into()));
    let mut cmd = helper(
        Path::new("/app/run"),
        if mappings {
            "mapping-command"
        } else if owner_loss {
            "owner-command"
        } else {
            "command"
        },
        Path::new("/"),
    );
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    confined_launch(&mut cmd, &mountpoint, fuse_fd);
    let mut command = cmd.spawn().unwrap();
    let mut output = BufReader::new(command.stdout.take().unwrap());
    if owner_loss {
        let descendants = event(&mut output, "COMMAND_READY ");
        let holders = fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|path| path == Path::new("/dev/fuse"))
            .count();
        assert_eq!(holders, 1, "provider must be sole FUSE descriptor owner");
        println!("PROVIDER_FUSE_FDS=1");
        println!("OWNER_READY {} {descendants}", command.id());
        io::stdout().flush().unwrap();
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    if mappings {
        event(&mut output, "MAPPINGS_OK");
    } else {
        event(&mut output, "HELD");
        replace_held(&root.join("raw"));
        command
            .stdin
            .take()
            .unwrap()
            .write_all(b"replaced\n")
            .unwrap();
        event(&mut output, "PROJECTED_EXEC_DLOPEN_IO_OK");
    }
    assert!(wait(&mut command).success());
    if mappings {
        verify_mapped_backing(root);
    } else {
        verify_backing(root);
    }
    unmount_projection(&target, server);
    fs::remove_dir(&mountpoint).unwrap();
    println!("NORMAL_UNMOUNT_BACKING_OK");
}

fn audit_command() {
    // close_range(CLOEXEC) covers the entire descriptor range before exec. This
    // post-exec sample also catches ordinary loader/reentry descriptor leaks.
    for fd in 3..1024 {
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) },
            -1,
            "unexpected fd {fd}"
        );
    }
    assert!(
        caps()
            .iter()
            .all(|c| c.effective == 0 && c.permitted == 0 && c.inheritable == 0)
    );
    assert_eq!(
        unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) },
        1
    );
    println!("COMMAND_FDS_3_TO_1023_EMPTY CAPS=0 NNP=1");
}

fn owner_command() {
    audit_command();
    let mut cmd = helper(Path::new("/app/run"), "descendant", Path::new("/"));
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || parent_death(parent));
    }
    // No /dev/null grant is needed: keep the already-authorized stdio channel.
    let mut child = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    event(&mut output, "DESCENDANT_READY");
    println!("COMMAND_READY {}", child.id());
    io::stdout().flush().unwrap();
    loop {
        unsafe {
            libc::pause();
        }
    }
}

fn reap_owned(pid: libc::pid_t) {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            assert!(libc::WIFSIGNALED(status));
            assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
            assert!(!Path::new(&format!("/proc/{pid}/ns/mnt")).exists());
            return;
        }
        assert!(result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD));
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("owned command {pid} did not terminate within five seconds");
}

fn supervisor(root: &Path, mappings: bool) {
    unsafe {
        libc::alarm(75);
    }
    assert_ne!(
        unsafe { libc::getuid() },
        0,
        "probe must start as an ordinary non-root user"
    );
    assert!(caps().iter().all(|c| c.effective == 0 && c.permitted == 0));
    checked(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }).unwrap();
    let exe = std::env::current_exe().unwrap();
    let roles: &[&str] = if mappings {
        &["raw", "normal"]
    } else {
        &["raw", "normal", "owner"]
    };
    for &role in roles {
        let case = root.join(role);
        fs::create_dir(&case).unwrap();
        let rules = fixture(&case, &exe);
        fs::write(case.join("rules.json"), serde_json::to_vec(&rules).unwrap()).unwrap();
        let mut cmd = if role == "raw" {
            helper(
                &case.join("raw/app/run"),
                if mappings { "mapping-raw" } else { "raw" },
                &case.join("raw"),
            )
        } else {
            helper(&exe, if mappings { "mapping-normal" } else { role }, &case)
        };
        if role != "raw" {
            namespace_launch(&mut cmd);
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .spawn()
            .expect("unprivileged namespace mapping prerequisite");
        let mut output = BufReader::new(child.stdout.take().unwrap());
        if role == "owner" {
            let pids = event(&mut output, "OWNER_READY ")
                .split_whitespace()
                .map(|pid| pid.parse::<libc::pid_t>().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(pids.len(), 2);
            child.kill().unwrap();
            assert_eq!(wait(&mut child).signal(), Some(libc::SIGKILL));
            assert!(!Path::new(&format!("/proc/{}/ns/mnt", child.id())).exists());
            for pid in pids {
                reap_owned(pid);
            }
            // Namespace-private mount never covered this directory in the observer.
            assert!(fs::read_dir(case.join("view")).unwrap().next().is_none());
            fs::remove_dir(case.join("view")).unwrap();
            println!("OWNER_LOSS_COMMAND_DESCENDANT_NAMESPACE_OK");
        } else {
            event(
                &mut output,
                if role == "raw" {
                    if mappings {
                        "MAPPINGS_OK"
                    } else {
                        "RAW_EXEC_DLOPEN_IO_OK"
                    }
                } else {
                    "NORMAL_UNMOUNT_BACKING_OK"
                },
            );
            assert!(wait(&mut child).success(), "{role}");
            if mappings {
                verify_mapped_backing(&case);
            } else {
                verify_backing(&case);
            }
        }
        fs::remove_dir_all(&case).unwrap();
    }
    println!(
        "{}",
        if mappings {
            "MMAP_PROBE_OK"
        } else {
            "MOUNT_PROBE_OK"
        }
    );
}

#[test]
fn projection_mount_helper() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("NUB_PROJECTION_TEST_ROOT").unwrap());
    // libtest prints its test-name prefix before calling this function.
    println!();
    match role.as_str() {
        "supervisor" => supervisor(&root, false),
        "mapping-supervisor" => supervisor(&root, true),
        "raw" => exercise(&root, false),
        "normal" => provider(&root, false, false),
        "owner" => provider(&root, true, false),
        "mapping-normal" => provider(&root, false, true),
        "mapping-raw" => {
            exercise_mappings(&root.join("app"), false);
            println!("MAPPINGS_OK");
        }
        "mapping-command" => {
            audit_command();
            exercise_mappings(&root.join("app"), true);
            println!("MAPPINGS_OK");
        }
        "command" => {
            audit_command();
            exercise(&root, true);
        }
        "owner-command" => owner_command(),
        "descendant" => {
            audit_command();
            println!("DESCENDANT_READY");
            io::stdout().flush().unwrap();
            loop {
                unsafe {
                    libc::pause();
                }
            }
        }
        _ if native_tests::run_role(&role, &root) => {}
        _ if exec_tests::run_role(&role, &root) => {}
        _ => panic!("unknown projection helper role {role}"),
    }
}

#[test]
#[ignore = "requires an ordinary Linux user with direct user namespaces and /dev/fuse"]
fn mounted_projection_enforces_paths_and_owns_commands() {
    run_mount_probe("supervisor", "MOUNT_PROBE_OK");
}

#[test]
#[ignore = "requires an ordinary Linux user with direct user namespaces and /dev/fuse"]
fn mounted_projection_preserves_mapping_semantics() {
    run_mount_probe("mapping-supervisor", "MMAP_PROBE_OK");
}

#[test]
#[ignore = "requires an ordinary Linux user with direct user namespaces and /dev/fuse"]
fn mounted_projection_delivers_native_regular_files() {
    run_mount_probe("native-supervisor", "NATIVE_PROJECTION_ACCEPTANCE_OK");
}

#[test]
#[ignore = "requires an ordinary Linux user, /dev/fuse, and a C compiler on the remote fixture host"]
fn mounted_projection_exec_and_dlopen_acceptance() {
    run_mount_probe("exec-supervisor", "LINUX_EXEC_ACCEPTANCE_COMPLETE");
}

fn run_mount_probe(role: &str, marker: &str) {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("probe.log");
    let mut cmd = helper(&std::env::current_exe().unwrap(), role, root.path());
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || {
            parent_death(parent)?;
            checked(libc::setpgid(0, 0))
        });
    }
    let output = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&log)
        .unwrap();
    let mut child = cmd
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= end {
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // Isolated process group belongs only to this probe, including failure paths.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    if status.is_none() {
        let _ = wait(&mut child);
        writeln!(
            OpenOptions::new().append(true).open(&log).unwrap(),
            "MOUNTED_PROBE_TIMEOUT role={role}"
        )
        .unwrap();
    }
    let output = fs::read_to_string(&log).unwrap();
    if role == "exec-supervisor"
        && let Some(destination) = std::env::var_os("NUB_PROJECTION_TEST_ARTIFACTS")
    {
        let destination = PathBuf::from(destination);
        fs::create_dir_all(&destination).unwrap();
        fs::copy(&log, destination.join("probe.log")).unwrap();
        let payloads = root.path().join("payloads");
        if payloads.is_dir() {
            for file in fs::read_dir(payloads).unwrap() {
                let file = file.unwrap();
                assert!(file.file_type().unwrap().is_file());
                fs::copy(file.path(), destination.join(file.file_name())).unwrap();
            }
        }
    }
    println!("{output}");
    assert!(
        status.is_some_and(|status| status.success()),
        "mounted projection failed: {status:?}\n{output}"
    );
    assert!(output.contains(marker), "missing mounted acceptance marker");
}
