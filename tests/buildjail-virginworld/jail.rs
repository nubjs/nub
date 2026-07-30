// Prototype spike — NOT nub code. Standalone; builds with plain `rustc jail.rs -O -o jail`,
// zero crates, so a CI runner needs no cargo registry access.
//
// Design under test (wiki/research/build-jail-virgin-world.md §2): instead of an allowlist over the
// real machine, REROOT. `unshare(NEWUSER|NEWNS|NEWPID|NEWUTS[|NEWNET])` + `pivot_root` onto a base
// image rootfs. Inside there is no policy at all — no allowlist, no denylist — because the world
// contains nothing worth denying. The one bind mount is /work, which holds BOTH the project and the
// project-local CAS store so hardlinks between them stay intra-mount (§2b: do_linkat() compares
// vfsmount, not superblock, so a two-mount topology returns EXDEV).
//
// Freshness comes from overlayfs: lowerdir is the prepared, reusable rootfs; upperdir is a per-run
// tmpfs. Every run therefore starts from an identical world and leaves nothing behind. The
// `--no-overlay` path exists to A/B that against copying the rootfs into tmpfs, and to build the
// lower itself (prep writes directly through).

#![allow(non_camel_case_types)]

use std::ffi::CString;
use std::fs;
use std::io::{Error, Write};
use std::os::raw::{c_char, c_int, c_long, c_ulong, c_void};
use std::time::Instant;

extern "C" {
    fn unshare(flags: c_int) -> c_int;
    fn mount(
        source: *const c_char,
        target: *const c_char,
        fstype: *const c_char,
        flags: c_ulong,
        data: *const c_void,
    ) -> c_int;
    fn umount2(target: *const c_char, flags: c_int) -> c_int;
    fn syscall(num: c_long, ...) -> c_long;
    fn fork() -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn chdir(path: *const c_char) -> c_int;
    fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
    fn geteuid() -> u32;
    fn getuid() -> u32;
    fn getgid() -> u32;
    fn sethostname(name: *const c_char, len: usize) -> c_int;
    fn _exit(code: c_int) -> !;
}

const CLONE_NEWNS: c_int = 0x0002_0000;
const CLONE_NEWUTS: c_int = 0x0400_0000;
const CLONE_NEWIPC: c_int = 0x0800_0000;
const CLONE_NEWUSER: c_int = 0x1000_0000;
const CLONE_NEWPID: c_int = 0x2000_0000;
const CLONE_NEWNET: c_int = 0x4000_0000;

const MS_RDONLY: c_ulong = 1;
const MS_NOSUID: c_ulong = 2;
const MS_NODEV: c_ulong = 4;
const MS_NOEXEC: c_ulong = 8;
const MS_BIND: c_ulong = 4096;
const MS_REC: c_ulong = 16384;
const MS_PRIVATE: c_ulong = 1 << 18;
const MNT_DETACH: c_int = 2;

#[cfg(target_arch = "x86_64")]
const SYS_PIVOT_ROOT: c_long = 155;
#[cfg(target_arch = "aarch64")]
const SYS_PIVOT_ROOT: c_long = 41;

fn cs(s: &str) -> CString {
    CString::new(s).expect("NUL in path")
}

fn die(ctx: &str) -> ! {
    eprintln!("jail: {}: {}", ctx, Error::last_os_error());
    std::process::exit(70);
}

fn do_mount(src: &str, tgt: &str, fs_ty: Option<&str>, flags: c_ulong, data: Option<&str>) {
    let (s, t) = (cs(src), cs(tgt));
    let f = fs_ty.map(cs);
    let d = data.map(cs);
    let rc = unsafe {
        mount(
            s.as_ptr(),
            t.as_ptr(),
            f.as_ref().map_or(std::ptr::null(), |x| x.as_ptr()),
            flags,
            d.as_ref()
                .map_or(std::ptr::null(), |x| x.as_ptr() as *const c_void),
        )
    };
    if rc != 0 {
        die(&format!(
            "mount({} -> {}, {:?}, {:#x}, {:?})",
            src, tgt, fs_ty, flags, data
        ));
    }
}

fn try_mount(src: &str, tgt: &str, fs_ty: Option<&str>, flags: c_ulong, data: Option<&str>) -> bool {
    let (s, t) = (cs(src), cs(tgt));
    let f = fs_ty.map(cs);
    let d = data.map(cs);
    0 == unsafe {
        mount(
            s.as_ptr(),
            t.as_ptr(),
            f.as_ref().map_or(std::ptr::null(), |x| x.as_ptr()),
            flags,
            d.as_ref()
                .map_or(std::ptr::null(), |x| x.as_ptr() as *const c_void),
        )
    }
}

struct Args {
    lower: String,
    binds: Vec<(String, String, bool)>, // host, inside, readonly
    cwd: String,
    netns: bool,
    overlay: bool,
    rw_lower: bool,
    scratch: String,
    keep_env: Vec<String>,
    extra_env: Vec<(String, String)>,
    timings: bool,
    inner_uid: u32,
    hostname: String,
    cmd: Vec<String>,
}

fn parse() -> Args {
    let mut a = Args {
        lower: String::new(),
        binds: vec![],
        cwd: "/".into(),
        netns: false,
        overlay: true,
        rw_lower: false,
        scratch: "/tmp".into(),
        keep_env: vec![],
        extra_env: vec![],
        timings: false,
        inner_uid: 0,
        hostname: "nub-world".into(),
        cmd: vec![],
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--root" => a.lower = it.next().expect("--root needs a value"),
            "--cwd" => a.cwd = it.next().expect("--cwd needs a value"),
            "--scratch" => a.scratch = it.next().expect("--scratch needs a value"),
            "--hostname" => a.hostname = it.next().expect("--hostname needs a value"),
            "--netns" => a.netns = true,
            "--no-overlay" => a.overlay = false,
            // Prep-only: writes land in the lowerdir itself. This is how the world gets BUILT.
            "--rw-lower" => {
                a.rw_lower = true;
                a.overlay = false;
            }
            "--timings" => a.timings = true,
            // Which uid the caller becomes inside. NOT cosmetic: a single-uid map can only express
            // one ownership, so anything that tries to chown to a second uid gets EINVAL. node-tar
            // (npm's and node-gyp's extractor) only attempts chown when it believes it is root, so
            // mapping to a NON-ZERO uid is what makes an unprivileged install work at all.
            "--uid" => a.inner_uid = it.next().expect("--uid needs a value").parse().unwrap(),
            "--keep-env" => a.keep_env.push(it.next().expect("--keep-env needs a value")),
            "--env" => {
                let kv = it.next().expect("--env needs K=V");
                let (k, v) = kv.split_once('=').expect("--env needs K=V");
                a.extra_env.push((k.into(), v.into()));
            }
            "--bind" | "--bind-ro" => {
                let ro = x == "--bind-ro";
                let spec = it.next().expect("--bind needs host:inside");
                let (h, i) = spec.rsplit_once(':').expect("--bind needs host:inside");
                a.binds.push((h.into(), i.into(), ro));
            }
            "--" => {
                a.cmd = it.collect();
                break;
            }
            other => panic!("unknown flag {}", other),
        }
    }
    assert!(!a.lower.is_empty(), "--root is required");
    assert!(!a.cmd.is_empty(), "a command is required after --");
    a
}

fn detach(tgt: &str) {
    let t = cs(tgt);
    unsafe { umount2(t.as_ptr(), MNT_DETACH) };
}

fn mkdirs(p: &str) {
    fs::create_dir_all(p).unwrap_or_else(|e| panic!("mkdir {}: {}", p, e));
}

fn main() {
    let a = parse();
    let t0 = Instant::now();
    let (uid, gid) = unsafe { (getuid(), getgid()) };
    let already_root = unsafe { geteuid() } == 0;

    let mut flags = CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS | CLONE_NEWIPC;
    if !already_root {
        flags |= CLONE_NEWUSER;
    }
    if a.netns {
        flags |= CLONE_NEWNET;
    }
    if unsafe { unshare(flags) } != 0 {
        let e = Error::last_os_error();
        eprintln!("jail: unshare({:#x}) failed: {}", flags, e);
        eprintln!("jail: HINT — on Ubuntu 24.04+ check `sysctl kernel.apparmor_restrict_unprivileged_userns`");
        std::process::exit(71);
    }
    let t_unshare = t0.elapsed();

    // A single-uid map is enough: everything in the world is root-owned, and the one bind mount is
    // owned by the invoking user, who maps to uid 0 inside. setgroups must be denied before gid_map
    // is writable in an unprivileged userns.
    if !already_root {
        fs::write("/proc/self/setgroups", "deny").ok();
        if let Err(e) = fs::write("/proc/self/uid_map", format!("{} {} 1\n", a.inner_uid, uid)) {
            // The honest signal for "unprivileged userns is restricted". unshare(CLONE_NEWUSER)
            // itself SUCCEEDS on a restricted Ubuntu host — it returns an inert namespace — so the
            // uid_map write, not the unshare exit code, is what tells you the truth.
            eprintln!("jail: uid_map write failed: {}", e);
            eprintln!("jail: unprivileged user namespaces are restricted on this host.");
            eprintln!("jail: check `sysctl kernel.apparmor_restrict_unprivileged_userns`");
            std::process::exit(72);
        }
        fs::write("/proc/self/gid_map", format!("{} {} 1\n", a.inner_uid, gid))
            .unwrap_or_else(|e| panic!("gid_map: {}", e));
    }
    let t_map = t0.elapsed();

    // CLONE_NEWPID only takes effect for CHILDREN, so everything below runs in the child, which is
    // pid 1 of the new pid namespace. That matters for more than tidiness: procfs refuses to mount
    // (EPERM) unless the mounter's pid namespace is owned by its current user namespace, which is
    // only true for the child. bubblewrap and runc both build the whole rootfs from this position.
    let pid = unsafe { fork() };
    if pid < 0 {
        die("fork");
    }
    if pid > 0 {
        let mut st: c_int = 0;
        unsafe { waitpid(pid, &mut st, 0) };
        let code = if st & 0x7f == 0 { (st >> 8) & 0xff } else { 128 + (st & 0x7f) };
        std::process::exit(code);
    }

    do_mount("none", "/", None, MS_REC | MS_PRIVATE, None);

    let root = format!("{}/nub-world-root", a.scratch);
    let scratch = format!("{}/nub-world-scratch", a.scratch);
    mkdirs(&root);
    mkdirs(&scratch);

    // Freshness lives here. The upper layer is a tmpfs, so the world's every write — including
    // rm -rf /usr — is discarded at exit and the reusable lowerdir is physically unwritable through
    // this mount. pivot_root needs `root` to BE a mount, which both branches satisfy.
    let mut overlay_used = false;
    if a.overlay {
        do_mount("tmpfs", &scratch, Some("tmpfs"), 0, Some("size=8g,mode=0755"));
        let up = format!("{}/up", scratch);
        let wk = format!("{}/wk", scratch);
        mkdirs(&up);
        mkdirs(&wk);
        let opts = format!("lowerdir={},upperdir={},workdir={}", a.lower, up, wk);
        overlay_used = try_mount("overlay", &root, Some("overlay"), 0, Some(&opts))
            || try_mount(
                "overlay",
                &root,
                Some("overlay"),
                0,
                Some(&format!("{},userxattr", opts)),
            );
        if !overlay_used {
            eprintln!(
                "jail: overlayfs unavailable ({}); falling back to a tmpfs copy",
                Error::last_os_error()
            );
            detach(&scratch);
        }
    }
    if !overlay_used {
        if a.rw_lower {
            do_mount(&a.lower, &root, None, MS_BIND | MS_REC, None);
        } else {
            do_mount("tmpfs", &root, Some("tmpfs"), 0, Some("size=8g,mode=0755"));
            let st = std::process::Command::new("cp")
                .args(["-a", &format!("{}/.", a.lower), &root])
                .status()
                .expect("cp");
            assert!(st.success(), "cp -a of the rootfs failed");
        }
    }
    let t_root = t0.elapsed();

    for d in ["oldroot", "proc", "sys", "dev", "tmp", "work", "home/build", "etc"] {
        mkdirs(&format!("{}/{}", root, d));
    }

    for (h, i, ro) in &a.binds {
        let tgt = format!("{}{}", root, i);
        let meta = fs::metadata(h).unwrap_or_else(|e| panic!("bind source {}: {}", h, e));
        if meta.is_dir() {
            mkdirs(&tgt);
        } else {
            if let Some(p) = std::path::Path::new(&tgt).parent() {
                mkdirs(p.to_str().unwrap());
            }
            fs::write(&tgt, "").ok();
        }
        do_mount(h, &tgt, None, MS_BIND | MS_REC, None);
        if *ro {
            do_mount(
                h,
                &tgt,
                None,
                MS_BIND | MS_REC | MS_RDONLY | (1 << 5), /* MS_REMOUNT */
                None,
            );
        }
    }

    do_mount("tmpfs", &format!("{}/tmp", root), Some("tmpfs"), 0, Some("size=2g,mode=1777"));
    do_mount("tmpfs", &format!("{}/dev", root), Some("tmpfs"), MS_NOSUID, Some("mode=0755"));
    for dev in ["null", "zero", "full", "random", "urandom", "tty"] {
        let tgt = format!("{}/dev/{}", root, dev);
        fs::write(&tgt, "").ok();
        if !try_mount(&format!("/dev/{}", dev), &tgt, None, MS_BIND, None) {
            fs::remove_file(&tgt).ok();
        }
    }
    mkdirs(&format!("{}/dev/shm", root));
    do_mount("tmpfs", &format!("{}/dev/shm", root), Some("tmpfs"), MS_NOSUID | MS_NODEV, Some("size=1g,mode=1777"));
    for (l, t) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
        std::os::unix::fs::symlink(t, format!("{}/dev/{}", root, l)).ok();
    }

    // /etc is the whole "identity" of the world: one account, DNS, nothing else.
    let etc = format!("{}/etc", root);
    if !a.rw_lower {
        fs::write(
            format!("{}/passwd", etc),
            format!(
                "root:x:0:0:root:/root:/bin/sh\nbuild:x:{u}:{u}:build:/home/build:/bin/sh\n",
                u = a.inner_uid
            ),
        )
        .ok();
        fs::write(
            format!("{}/group", etc),
            format!("root:x:0:\nbuild:x:{}:\n", a.inner_uid),
        )
        .ok();
        fs::write(format!("{}/hostname", etc), format!("{}\n", a.hostname)).ok();
        fs::write(format!("{}/hosts", etc), "127.0.0.1 localhost nub-world\n::1 localhost\n").ok();
    }
    if !a.netns {
        let rc = fs::read_to_string("/etc/resolv.conf")
            .map(|s| {
                s.lines()
                    .filter(|l| l.starts_with("nameserver") || l.starts_with("search") || l.starts_with("options"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let rc = if rc.contains("nameserver") { rc } else { "nameserver 1.1.1.1\nnameserver 8.8.8.8".into() };
        fs::write(format!("{}/resolv.conf", etc), format!("{}\n", rc)).ok();
    }
    let t_world = t0.elapsed();

    do_mount(
        "proc",
        &format!("{}/proc", root),
        Some("proc"),
        MS_NOSUID | MS_NODEV | MS_NOEXEC,
        None,
    );

    let hn = cs(&a.hostname);
    unsafe { sethostname(hn.as_ptr(), a.hostname.len()) };

    // Ordering mirrors bubblewrap (bubblewrap.c ~3010-3030): chdir into the new root and pass a
    // RELATIVE put_old, then make the detached old root private before unmounting it — a shared
    // oldroot makes the MNT_DETACH fail with EINVAL.
    if unsafe { chdir(cs(&root).as_ptr()) } != 0 {
        die(&format!("chdir {}", root));
    }
    let (r, o) = (cs("."), cs("oldroot"));
    if unsafe { syscall(SYS_PIVOT_ROOT, r.as_ptr(), o.as_ptr()) } != 0 {
        die("pivot_root");
    }
    if unsafe { chdir(cs("/").as_ptr()) } != 0 {
        die("chdir /");
    }
    do_mount("oldroot", "oldroot", None, MS_REC | MS_PRIVATE, None);
    if unsafe { umount2(cs("/oldroot").as_ptr(), MNT_DETACH) } != 0 {
        die("umount2 /oldroot");
    }
    fs::remove_dir("/oldroot").ok();
    let t_pivot = t0.elapsed();

    let keep: Vec<(String, String)> = a
        .keep_env
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
        .collect();
    for (k, _) in std::env::vars() {
        std::env::remove_var(k);
    }
    std::env::set_var("HOME", "/home/build");
    std::env::set_var("PATH", "/opt/node/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    std::env::set_var("TMPDIR", "/tmp");
    std::env::set_var("USER", if a.inner_uid == 0 { "root" } else { "build" });
    std::env::set_var("SHELL", "/bin/sh");
    std::env::set_var("LANG", "C.UTF-8");
    std::env::set_var("NUB_WORLD", "1");
    for (k, v) in keep.into_iter().chain(a.extra_env.iter().cloned()) {
        std::env::set_var(k, v);
    }

    if a.timings {
        eprintln!(
            "TIMINGS_MS unshare={:.1} uidmap={:.1} rootfs={:.1} world={:.1} pivot={:.1} overlay={}",
            t_unshare.as_secs_f64() * 1e3,
            (t_map - t_unshare).as_secs_f64() * 1e3,
            (t_root - t_map).as_secs_f64() * 1e3,
            (t_world - t_root).as_secs_f64() * 1e3,
            (t_pivot - t_world).as_secs_f64() * 1e3,
            overlay_used
        );
        eprintln!("TIMINGS_TOTAL_MS {:.1}", t_pivot.as_secs_f64() * 1e3);
        std::io::stderr().flush().ok();
    }

    if unsafe { chdir(cs(&a.cwd).as_ptr()) } != 0 {
        die(&format!("chdir {}", a.cwd));
    }
    let argv: Vec<CString> = a.cmd.iter().map(|s| cs(s)).collect();
    let mut ptrs: Vec<*const c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { execvp(ptrs[0], ptrs.as_ptr()) };
    eprintln!("jail: exec {}: {}", a.cmd[0], Error::last_os_error());
    unsafe { _exit(127) }
}
