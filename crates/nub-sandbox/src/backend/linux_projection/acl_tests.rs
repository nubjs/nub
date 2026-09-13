//! Capable-host discriminator for the current one-ID native-open boundary.
//!
//! The fixture is deliberately prepared outside this process: it needs a
//! foreign-owned inode, which an ordinary captured user must not create.  The
//! test only consumes that fixture through `Sandbox -> Prepared`, so it neither
//! changes host policy nor turns on FUSE POSIX ACL support.

use crate::backend::{CommandSpec, Sandbox};
use crate::policy::{
    CanonGlob, Effect, EnvPolicy, FsAccess, FsOrigin, FsRule, FsRuleSet, SandboxPolicy,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const FIXTURE_ENV: &str = "NUB_ACL_MOUNTED_FIXTURE";
const ROLE_ENV: &str = "NUB_ACL_MOUNTED_ROLE";
const ROLE: &str = "prepared-child";
const HELPER: &str = "backend::linux_projection::acl_tests::mounted_native_acl_helper";
const ACL_ACCESS: &str = "system.posix_acl_access";
const USER_XATTR: &str = "user.nub_acl_discriminator";
const READONLY_USER_XATTR: &str = "user.nub_acl_readonly";

const ACL_VERSION: u32 = 0x0002;
const ACL_USER_OBJ: u16 = 0x0001;
const ACL_USER: u16 = 0x0002;
const ACL_GROUP_OBJ: u16 = 0x0004;
const ACL_MASK: u16 = 0x0010;
const ACL_OTHER: u16 = 0x0020;

fn rule(path: impl Into<String>, access: FsAccess) -> FsRule {
    FsRule {
        matcher: CanonGlob(path.into()),
        access,
        effect: Effect::Allow,
        origin: FsOrigin::Authored,
    }
}

fn errno(result: isize, operation: &str) -> i32 {
    assert_eq!(result, -1, "{operation} unexpectedly succeeded");
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_else(|| panic!("{operation} did not report an errno"))
}

fn path_c(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("fixture path has no NUL")
}

fn getxattr_path(path: &Path, name: &str) -> io::Result<Vec<u8>> {
    let path = path_c(path);
    let name = CString::new(name).unwrap();
    let size = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = vec![0; size as usize];
    let count = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(count as usize);
    Ok(bytes)
}

fn getxattr_fd(fd: i32, name: &str) -> io::Result<Vec<u8>> {
    let name = CString::new(name).unwrap();
    let size = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = vec![0; size as usize];
    let count =
        unsafe { libc::fgetxattr(fd, name.as_ptr(), bytes.as_mut_ptr().cast(), bytes.len()) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(count as usize);
    Ok(bytes)
}

fn setxattr_path(path: &Path, name: &str, value: &[u8]) -> i32 {
    let path = path_c(path);
    let name = CString::new(name).unwrap();
    unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    }
}

fn setxattr_fd(fd: i32, name: &str, value: &[u8]) -> i32 {
    let name = CString::new(name).unwrap();
    unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0) }
}

fn open_read(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .unwrap_or_else(|error| panic!("open read {}: {error}", path.display()))
}

fn open_rw(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .unwrap_or_else(|error| panic!("open rw {}: {error}", path.display()))
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn named_user_ids(bytes: &[u8]) -> Vec<u32> {
    assert!(
        bytes.len() >= 4 && (bytes.len() - 4) % 8 == 0,
        "ACL framing"
    );
    assert_eq!(read_u32(bytes, 0), ACL_VERSION, "ACL version");
    bytes[4..]
        .chunks_exact(8)
        .filter_map(|entry| (read_u16(entry, 0) == ACL_USER).then(|| read_u32(entry, 4)))
        .collect()
}

fn mapped_acl() -> Vec<u8> {
    // Child uid 0 is the captured host uid. The named ACL_USER entry is the
    // conversion discriminator; group/other carry no authority in this test.
    let entries: [(u16, u16, u32); 5] = [
        (ACL_USER_OBJ, 0o6, u32::MAX),
        (ACL_USER, 0o6, 0),
        (ACL_GROUP_OBJ, 0, u32::MAX),
        (ACL_MASK, 0o6, u32::MAX),
        (ACL_OTHER, 0, u32::MAX),
    ];
    let mut bytes = ACL_VERSION.to_le_bytes().to_vec();
    for (tag, permission, id) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&permission.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}

fn unmapped_acl() -> Vec<u8> {
    let mut acl = mapped_acl();
    // The second entry is the named user entry; only that ID is unrepresentable.
    acl[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    acl
}

fn expected_host_acl(host_uid: u32) -> Vec<u8> {
    let mut acl = mapped_acl();
    acl[16..20].copy_from_slice(&host_uid.to_le_bytes());
    acl
}

fn executable_closure(executable: &Path) -> BTreeSet<PathBuf> {
    let mut closure = BTreeSet::new();
    let mut pending = vec![executable.to_owned()];
    while let Some(binary) = pending.pop() {
        let output = Command::new("ldd")
            .arg(&binary)
            .output()
            .expect("fixture ldd invocation");
        assert!(
            output.status.success(),
            "ldd {}: {output:?}",
            binary.display()
        );
        for word in String::from_utf8(output.stdout)
            .expect("ldd output is utf-8")
            .split_whitespace()
        {
            if word.starts_with('/') {
                let library = PathBuf::from(word);
                if closure.insert(library.clone()) {
                    pending.push(library);
                }
            }
        }
    }
    closure
}

fn fixture_path_targets(grants: &mut BTreeSet<PathBuf>, path: &Path, links_left: u8) {
    assert!(path.is_absolute(), "fixture closure path must be absolute");
    let mut current = PathBuf::from("/");
    let mut components = path.components();
    while let Some(component) = components.next() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => {
                current.pop();
            }
            Component::Normal(name) => current.push(name),
            Component::Prefix(_) => unreachable!("Linux fixture path prefix"),
        }
        if fs::symlink_metadata(&current)
            .expect("fixture closure component")
            .file_type()
            .is_symlink()
        {
            assert!(links_left > 0, "fixture closure symlink limit");
            grants.insert(current.clone());
            let target = fs::read_link(&current).expect("fixture closure symlink");
            let mut target = if target.is_absolute() {
                target
            } else {
                current.parent().unwrap().join(target)
            };
            target.push(components.as_path());
            fixture_path_targets(grants, &target, links_left - 1);
            return;
        }
    }
    grants.insert(current);
}

fn policy(root: &Path, executable: &Path) -> SandboxPolicy {
    let mut closure = executable_closure(executable);
    closure.insert(executable.to_owned());
    let mut targets = BTreeSet::new();
    for path in &closure {
        fixture_path_targets(&mut targets, path, 40);
    }
    closure.extend(targets);

    let mut entries: Vec<_> = closure
        .into_iter()
        .map(|path| rule(path.to_string_lossy().into_owned(), FsAccess::Read))
        .collect();
    entries.extend([
        rule(
            root.join("allowed-read").to_string_lossy().into_owned(),
            FsAccess::Read,
        ),
        rule(
            root.join("allowed-rw").to_string_lossy().into_owned(),
            FsAccess::ReadWrite,
        ),
        rule(
            root.join("policy-read").to_string_lossy().into_owned(),
            FsAccess::Read,
        ),
        // This path is explicitly permitted by Rules. A child failure here is
        // host DAC, not the policy omission exercised by denied-read.
        rule(
            root.join("host-dac-denied").to_string_lossy().into_owned(),
            FsAccess::Read,
        ),
    ]);
    let mut environment = BTreeMap::new();
    environment.insert("PATH".into(), "/usr/bin:/bin".into());
    environment.insert(ROLE_ENV.into(), ROLE.into());
    environment.insert(FIXTURE_ENV.into(), root.to_string_lossy().into_owned());
    let mut policy = SandboxPolicy::default();
    policy.fs.rules = FsRuleSet {
        entries,
        default_effect: Effect::Deny,
    };
    policy.env = EnvPolicy::resolved(environment);
    policy.env.enforce = true;
    policy
}

fn command(executable: &Path) -> CommandSpec {
    CommandSpec::new(executable)
        .args(["--exact", HELPER, "--nocapture", "--test-threads=1"])
        .cwd("/")
        .redact_stdout(true)
        .redact_stderr(true)
}

fn raw_controls(root: &Path) {
    let read = root.join("allowed-read");
    let denied = root.join("host-dac-denied");
    let policy_denied = root.join("denied-read");
    let policy_read = root.join("policy-read");
    open_read(&read);
    let write = OpenOptions::new().read(true).write(true).open(&read);
    assert_eq!(
        write
            .expect_err("host ACL read-only control opened O_RDWR")
            .raw_os_error(),
        Some(libc::EACCES),
        "host ACL read-only control errno"
    );
    let denied = OpenOptions::new().read(true).open(&denied);
    assert_eq!(
        denied
            .expect_err("host DAC denied control opened")
            .raw_os_error(),
        Some(libc::EACCES),
        "host DAC denied control errno"
    );
    open_read(&policy_denied);
    open_rw(&policy_read);
    assert_eq!(
        fs::metadata(&policy_read).unwrap().ino(),
        fs::metadata(root.join("allowed-rw")).unwrap().ino(),
        "policy-read must be a hardlink to the RW backing inode"
    );
}

fn child_contract(root: &Path) {
    let read = open_read(&root.join("allowed-read"));
    let ids = named_user_ids(&getxattr_fd(read.as_raw_fd(), ACL_ACCESS).expect("native R ACL"));
    assert!(ids.contains(&0), "native ACL omitted mapped captured uid");
    assert!(
        ids.contains(&u32::MAX),
        "native ACL did not represent the foreign uid as 0xffffffff: {ids:?}"
    );
    let policy_read = root.join("policy-read");
    let read_alias = open_read(&policy_read);
    assert_eq!(
        errno(
            setxattr_fd(read_alias.as_raw_fd(), READONLY_USER_XATTR, b"denied") as isize,
            "R native FD user xattr mutation"
        ),
        libc::EROFS,
        "R native FD must carry a read-only backing view"
    );
    assert_eq!(
        getxattr_fd(read_alias.as_raw_fd(), READONLY_USER_XATTR)
            .expect_err("rejected R native xattr mutation became visible")
            .raw_os_error(),
        Some(libc::ENODATA),
        "R native FD rejection changed its backing user xattr"
    );
    assert_eq!(
        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&policy_read)
            .expect_err("R policy path unexpectedly opened O_WRONLY")
            .raw_os_error(),
        Some(libc::EACCES),
        "R policy path O_WRONLY denial errno"
    );

    let rw_path = root.join("allowed-rw");
    let rw = open_rw(&rw_path);
    assert_eq!(setxattr_fd(rw.as_raw_fd(), USER_XATTR, b"positive"), 0);
    assert_eq!(
        getxattr_fd(rw.as_raw_fd(), USER_XATTR).unwrap(),
        b"positive"
    );
    assert_eq!(setxattr_fd(rw.as_raw_fd(), ACL_ACCESS, &mapped_acl()), 0);
    let before = getxattr_fd(rw.as_raw_fd(), ACL_ACCESS).expect("mapped ACL after update");
    assert_eq!(
        errno(
            setxattr_fd(rw.as_raw_fd(), ACL_ACCESS, &unmapped_acl()) as isize,
            "unmapped native ACL update"
        ),
        libc::EINVAL,
    );
    assert_eq!(
        getxattr_fd(rw.as_raw_fd(), ACL_ACCESS).unwrap(),
        before,
        "EINVAL native ACL update changed backing bytes"
    );

    assert_eq!(
        getxattr_path(&rw_path, ACL_ACCESS)
            .expect_err("path ACL get unexpectedly succeeded")
            .raw_os_error(),
        Some(libc::EOPNOTSUPP),
        "path ACL get must stay unsupported while FUSE_POSIX_ACL is off"
    );
    assert_eq!(
        errno(
            setxattr_path(&rw_path, ACL_ACCESS, &mapped_acl()) as isize,
            "path ACL set"
        ),
        libc::EOPNOTSUPP,
        "path ACL set must stay unsupported while FUSE_POSIX_ACL is off"
    );
    let policy_denied = OpenOptions::new().read(true).open(root.join("denied-read"));
    assert_eq!(
        policy_denied
            .expect_err("unlisted ACL fixture path opened through projection")
            .raw_os_error(),
        Some(libc::EACCES),
        "projection policy denial errno"
    );
    assert_eq!(
        OpenOptions::new()
            .read(true)
            .open(root.join("host-dac-denied"))
            .expect_err("host-DAC-denied path opened through projection")
            .raw_os_error(),
        Some(libc::EACCES),
        "host DAC denial must remain distinct from policy omission"
    );
    println!("NATIVE_ACL_ONE_ID_MOUNTED_OK");
}

/// The test executable re-enters here after `Prepared` has installed the
/// projection, native-open seccomp mediation, and the one-ID user namespace.
#[test]
fn mounted_native_acl_helper() {
    if std::env::var(ROLE_ENV).as_deref() == Ok(ROLE) {
        child_contract(Path::new(
            &std::env::var_os(FIXTURE_ENV).expect("ACL fixture root environment"),
        ));
    }
}

#[test]
#[ignore = "requires NUB_ACL_MOUNTED_FIXTURE pre-provisioned by a capable host, plus user namespaces and /dev/fuse"]
fn mounted_native_acl_preserves_host_dac_and_one_id_boundary() {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "probe must run as ordinary user"
    );
    let root = PathBuf::from(
        std::env::var_os(FIXTURE_ENV).expect("set NUB_ACL_MOUNTED_FIXTURE to the prepared root"),
    );
    for name in [
        "allowed-read",
        "allowed-rw",
        "policy-read",
        "denied-read",
        "host-dac-denied",
    ] {
        assert!(
            root.join(name).is_file(),
            "prepared ACL fixture missing {name}"
        );
    }
    raw_controls(&root);
    let raw = getxattr_path(&root.join("allowed-read"), ACL_ACCESS).expect("raw allowed ACL");
    let raw_ids = named_user_ids(&raw);
    assert!(
        raw_ids.contains(&(unsafe { libc::getuid() })),
        "prepared allowed-read ACL must name the captured host uid"
    );
    assert!(
        raw_ids.iter().any(|id| *id != unsafe { libc::getuid() }),
        "prepared allowed-read ACL must include a foreign named uid"
    );

    let executable = std::env::current_exe().expect("test executable");
    let sandbox = Sandbox::test_projected(&policy(&root, &executable), Path::new("/"))
        .expect("test-only projected session acquisition");
    let output = sandbox
        .prepare(command(&executable))
        .expect("prepared ACL discriminator")
        .output()
        .expect("prepared ACL discriminator reap");
    assert!(
        output.status.success(),
        "ACL discriminator failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("NATIVE_ACL_ONE_ID_MOUNTED_OK"),
        "missing ACL discriminator marker"
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
    let host_uid = unsafe { libc::getuid() };
    let host_acl = getxattr_path(&root.join("allowed-rw"), ACL_ACCESS).expect("raw mapped ACL");
    let host_ids = named_user_ids(&host_acl);
    assert!(
        host_ids.contains(&host_uid),
        "mapped child ACL update did not restore the captured host uid"
    );
    assert_eq!(
        host_acl,
        expected_host_acl(host_uid),
        "the rejected 0xffffffff update changed raw host ACL bytes"
    );
    assert_eq!(
        getxattr_path(&root.join("allowed-rw"), READONLY_USER_XATTR)
            .expect_err("rejected R xattr mutation reached the shared backing inode")
            .raw_os_error(),
        Some(libc::ENODATA),
        "R native FD mutation changed the hardlinked RW backing inode"
    );
}
