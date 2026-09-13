//! Bounded Linux exec/dlopen discriminator for the FUSE and native-export paths.
//!
//! This is deliberately a test fixture, not an executable broker or a cache repair
//! mechanism.  It uses freshly compiled, disposable ELF files and reports every
//! arm before judging parity, so a native-only discrepancy retains its controls.

use super::super::linux_projection::Projection;
use super::super::linux_supervisor::{
    EgressPolicy, ProjectedLaunch, SupervisedLaunch, SupervisedStdio, spawn_supervised_projected,
};
use super::*;
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule};
use std::ffi::{CStr, CString};
use std::io::{BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

const EXEC_A: &[u8; 6] = b"EXEC_A";
const EXEC_B: &[u8; 6] = b"EXEC_B";
const DSO_A: &[u8; 5] = b"DSO_A";
const DSO_B: &[u8; 5] = b"DSO_B";
const FUSE_SUPER_MAGIC: libc::c_long = 0x6573_5546;

fn rule(path: &str, access: FsAccess) -> FsRule {
    FsRule {
        matcher: CanonGlob(path.into()),
        access,
        effect: Effect::Allow,
        origin: FsOrigin::Authored,
    }
}

#[derive(Clone)]
struct Payloads {
    executable: PathBuf,
    dso: PathBuf,
    executable_sha256: String,
    dso_sha256: String,
    executable_source_sha256: String,
    dso_source_sha256: String,
    closure: Vec<PathBuf>,
}

fn sha256(path: &Path) -> String {
    let output = Command::new("sha256sum").arg(path).output().unwrap();
    assert!(output.status.success(), "sha256sum {}", path.display());
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

fn compile_payloads(root: &Path) -> Payloads {
    let payload_root = root.join("payloads");
    fs::create_dir(&payload_root).unwrap();
    let src = payload_root.join("exec.c");
    let dso = payload_root.join("dso.c");
    fs::write(
        &src,
        r#"#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
int main(void) { puts("EXEC_A"); fflush(stdout); if (getenv("NUB_EXEC_HOLD")) { puts("READY"); fflush(stdout); char x; read(0,&x,1); } return 0; }
"#,
    )
    .unwrap();
    fs::write(
        &dso,
        "const char marker[] = \"DSO_A\"; const char *nub_exec_marker(void) { return marker; }\n",
    )
    .unwrap();
    let executable = payload_root.join("exec-r");
    let library = payload_root.join("exec-r.so");
    println!(
        "EXEC_PAYLOAD_COMMAND cc -O0 -Wl,-z,now -o {} {}",
        executable.display(),
        src.display()
    );
    let status = Command::new("cc")
        .args(["-O0", "-Wl,-z,now", "-o"])
        .arg(&executable)
        .arg(&src)
        .status()
        .expect("remote fixture C compiler prerequisite");
    assert!(status.success(), "fixture executable compiler failed");
    println!(
        "EXEC_PAYLOAD_COMMAND cc -O0 -fPIC -shared -Wl,-z,now -o {} {}",
        library.display(),
        dso.display()
    );
    let status = Command::new("cc")
        .args(["-O0", "-fPIC", "-shared", "-Wl,-z,now", "-o"])
        .arg(&library)
        .arg(&dso)
        .status()
        .expect("remote fixture C compiler prerequisite");
    assert!(status.success(), "fixture DSO compiler failed");
    for (path, marker) in [
        (&executable, EXEC_A.as_slice()),
        (&library, DSO_A.as_slice()),
    ] {
        let bytes = fs::read(path).unwrap();
        assert_eq!(
            bytes
                .windows(marker.len())
                .filter(|window| *window == marker)
                .count(),
            1,
            "marker must be unique in {}",
            path.display()
        );
        assert_eq!(
            &bytes[..6],
            b"\x7fELF\x02\x01",
            "ELF64 little-endian payload"
        );
        let u16_at = |offset| u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
        let u32_at = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let u64_at = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let marker_offset = bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .unwrap() as u64;
        let table = u64_at(32) as usize;
        let stride = u16_at(54) as usize;
        assert!(stride >= 56);
        let readonly_load = (0..u16_at(56) as usize).any(|index| {
            let entry = table + index * stride;
            let start = u64_at(entry + 8);
            let size = u64_at(entry + 32);
            u32_at(entry) == 1
                && u32_at(entry + 4) & 2 == 0
                && marker_offset >= start
                && marker_offset + marker.len() as u64 <= start + size
        });
        assert!(
            readonly_load,
            "marker must belong to a non-writable PT_LOAD"
        );
        let headers = Command::new("readelf")
            .args(["-W", "-l"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            headers.status.success() && String::from_utf8_lossy(&headers.stdout).contains("LOAD"),
            "ELF PT_LOAD prerequisite {}",
            path.display()
        );
        fs::write(
            payload_root.join(format!(
                "{}.segments.txt",
                path.file_name().unwrap().to_string_lossy()
            )),
            &headers.stdout,
        )
        .unwrap();
        let sections = Command::new("readelf")
            .args(["-W", "-S"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            sections.status.success()
                && String::from_utf8_lossy(&sections.stdout).contains(".rodata"),
            "read-only marker section prerequisite {}",
            path.display()
        );
        fs::write(
            payload_root.join(format!(
                "{}.sections.txt",
                path.file_name().unwrap().to_string_lossy()
            )),
            &sections.stdout,
        )
        .unwrap();
    }
    let dynamic = Command::new("readelf")
        .args(["-W", "-d"])
        .arg(&library)
        .output()
        .unwrap();
    assert!(dynamic.status.success());
    assert!(
        !String::from_utf8_lossy(&dynamic.stdout).contains("(NEEDED)"),
        "fixture DSO must have no dependencies"
    );
    fs::write(payload_root.join("dso-dynamic.txt"), &dynamic.stdout).unwrap();
    let closure = Command::new("ldd").arg(&executable).output().unwrap();
    assert!(
        closure.status.success(),
        "payload loader closure prerequisite"
    );
    fs::write(payload_root.join("exec-r.ldd.txt"), &closure.stdout).unwrap();
    let closure_text = String::from_utf8(closure.stdout).unwrap();
    assert!(
        !closure_text.contains("not found"),
        "incomplete payload closure"
    );
    let closure: Vec<_> = closure_text
        .split_whitespace()
        .filter(|word| word.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    assert!(
        !closure.is_empty(),
        "dynamic fixture needs its observed loader closure"
    );
    let payloads = Payloads {
        executable,
        dso: library,
        executable_sha256: sha256(&payload_root.join("exec-r")),
        dso_sha256: sha256(&payload_root.join("exec-r.so")),
        executable_source_sha256: sha256(&src),
        dso_source_sha256: sha256(&dso),
        closure,
    };
    println!(
        "EXEC_PAYLOAD_PROVENANCE exec_source_sha256={} dso_source_sha256={} executable_sha256={} dso_sha256={} retained={}",
        payloads.executable_source_sha256,
        payloads.dso_source_sha256,
        payloads.executable_sha256,
        payloads.dso_sha256,
        payload_root.display()
    );
    payloads
}

fn exec_fixture(root: &Path, exe: &Path, native: bool, payloads: &Payloads) -> FsRuleSet {
    let mut rules = if native {
        native_tests::native_fixture(root, exe)
    } else {
        fixture(root, exe)
    };
    rules.entries.extend([
        rule("/app/exec-r", FsAccess::Read),
        rule("/app/exec-rw", FsAccess::ReadWrite),
        rule("/app/exec-rw-alias", FsAccess::ReadWrite),
        rule("/app/exec-r.so", FsAccess::Read),
        rule("/app/exec-rw.so", FsAccess::ReadWrite),
    ]);
    let app = root.join("raw/app");
    for path in &payloads.closure {
        let target = root.join("raw").join(path.strip_prefix("/").unwrap());
        copy(path, &target);
        assert_eq!(sha256(&target), sha256(path), "payload closure bytes");
        rules
            .entries
            .push(rule(path.to_str().unwrap(), FsAccess::Read));
    }
    fs::copy(&payloads.executable, app.join("exec-r")).unwrap();
    fs::copy(&payloads.dso, app.join("exec-r.so")).unwrap();
    assert_eq!(sha256(&app.join("exec-r")), payloads.executable_sha256);
    assert_eq!(sha256(&app.join("exec-r.so")), payloads.dso_sha256);
    fs::hard_link(app.join("exec-r"), app.join("exec-rw")).unwrap();
    fs::hard_link(app.join("exec-r"), app.join("exec-rw-alias")).unwrap();
    fs::hard_link(app.join("exec-r"), app.join("exec-denied")).unwrap();
    fs::hard_link(app.join("exec-r.so"), app.join("exec-rw.so")).unwrap();
    for names in [["exec-r", "exec-rw", "exec-rw-alias"]] {
        let inode = fs::metadata(app.join(names[0])).unwrap().ino();
        assert!(
            names
                .iter()
                .all(|name| fs::metadata(app.join(name)).unwrap().ino() == inode),
            "hardlink fixture inode mismatch"
        );
    }
    assert_eq!(
        fs::metadata(app.join("exec-r.so")).unwrap().ino(),
        fs::metadata(app.join("exec-rw.so")).unwrap().ino(),
        "DSO hardlink fixture inode mismatch"
    );
    fs::write(app.join("exec-neighbour"), b"unchanged").unwrap();
    rules
}

fn replace(path: &Path, from: &[u8], to: &[u8], native: bool) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .unwrap_or_else(|e| panic!("OPEN_ERR {}: {e}", path.display()));
    let before = file.metadata().unwrap();
    let mut bytes = Vec::new();
    (&file).read_to_end(&mut bytes).unwrap();
    let offset = bytes
        .windows(from.len())
        .position(|b| b == from)
        .unwrap_or_else(|| panic!("marker missing in {}", path.display()));
    let written = unsafe {
        libc::pwrite(
            file.as_raw_fd(),
            to.as_ptr().cast(),
            to.len(),
            offset as libc::off_t,
        )
    };
    assert_eq!(
        written,
        to.len() as isize,
        "short write {}: {}",
        path.display(),
        io::Error::last_os_error()
    );
    let mut readback = vec![0; to.len()];
    assert_eq!(
        unsafe {
            libc::pread(
                file.as_raw_fd(),
                readback.as_mut_ptr().cast(),
                readback.len(),
                offset as libc::off_t,
            )
        },
        to.len() as isize,
        "readback {}",
        path.display()
    );
    assert_eq!(readback, to, "pwrite readback {}", path.display());
    let after = file.metadata().unwrap();
    assert_eq!(
        (before.dev(), before.ino(), before.len()),
        (after.dev(), after.ino(), after.len()),
        "mutation replaced {}",
        path.display()
    );
    if native {
        let mut stat = unsafe { std::mem::zeroed::<libc::statfs>() };
        assert_eq!(
            unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) },
            0,
            "native fstatfs"
        );
        assert_ne!(
            stat.f_type as libc::c_long, FUSE_SUPER_MAGIC,
            "native write remained FUSE-backed"
        );
        println!(
            "EXEC_NATIVE_OBJECT name={} dev={} ino={} type={}",
            path.file_name().unwrap().to_string_lossy(),
            after.dev(),
            after.ino(),
            stat.f_type
        );
    }
}

fn child_output(mut cmd: Command, label: &str) -> Result<String, io::Error> {
    // Keep the inherited channel: opening /dev/null here would require an
    // unrelated path grant inside the already-confined controller.
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{label} exit {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8(output.stdout).unwrap())
}

fn exec_marker(path: &Path) -> Result<String, io::Error> {
    child_output(Command::new(path), "exec")
}

fn loader_marker(exe: &Path, root: &Path, dso: &Path) -> Result<String, io::Error> {
    let mut cmd = helper(exe, "exec-loader", root);
    cmd.env("NUB_EXEC_DSO", dso);
    child_output(cmd, "dlopen")
}

fn loader() {
    let path = std::env::var_os("NUB_EXEC_DSO").expect("loader path");
    let path = CString::new(Path::new(&path).as_os_str().as_bytes()).unwrap();
    unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        assert!(
            !handle.is_null(),
            "LOADER_ERR {}",
            CStr::from_ptr(libc::dlerror()).to_string_lossy()
        );
        let symbol = libc::dlsym(handle, c"nub_exec_marker".as_ptr());
        assert!(!symbol.is_null(), "missing marker symbol");
        let marker: unsafe extern "C" fn() -> *const libc::c_char = std::mem::transmute(symbol);
        println!("LOADER {}", CStr::from_ptr(marker()).to_string_lossy());
        assert_eq!(libc::dlclose(handle), 0);
    }
}

fn held_exec(path: &Path) -> Child {
    let mut child = Command::new(path)
        .env("NUB_EXEC_HOLD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("EXEC_ERR {}: {e}", path.display()));
    let mut out = BufReader::new(child.stdout.take().unwrap());
    assert_eq!(event(&mut out, ""), "EXEC_A");
    assert_eq!(event(&mut out, ""), "READY");
    child.stdout = Some(out.into_inner());
    child
}

fn errno(result: io::Result<File>) -> Option<i32> {
    result.err().and_then(|e| e.raw_os_error())
}

fn open_writer(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
}

fn open_writer_rw(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
}

fn exec_error(path: &Path) -> Option<i32> {
    match Command::new(path).spawn() {
        Ok(mut child) => {
            assert!(wait(&mut child).success(), "unexpected child failure");
            None
        }
        Err(error) => error.raw_os_error(),
    }
}

fn row(arm: &str, case: &str, result: impl std::fmt::Display) {
    println!("EXEC_ROW arm={arm} case={case} result={result}");
}

fn freshness_row(arm: &str, case: &str, kind: &str, result: &io::Result<String>, expected: &[u8]) {
    let expected = std::str::from_utf8(expected).unwrap();
    match result {
        Ok(output) if output.contains(expected) => row(arm, case, "MATCH"),
        Ok(output) => row(arm, case, format_args!("WRONG_MARKER output={output:?}")),
        Err(error) if error.raw_os_error().is_some() => row(
            arm,
            case,
            format_args!("EXEC_ERR errno={:?}", error.raw_os_error()),
        ),
        Err(error) if kind == "dlopen" => row(arm, case, format_args!("LOADER_ERR {error}")),
        Err(error) => row(arm, case, format_args!("EXEC_EXIT_ERROR {error}")),
    }
}

fn etxtbsy(arm: &str, app: &Path) {
    for (case, writer, runner) in [
        ("writer_then_exec_same", "exec-rw", "exec-rw"),
        ("writer_then_exec_alias", "exec-rw-alias", "exec-r"),
    ] {
        let held = open_writer(&app.join(writer))
            .unwrap_or_else(|error| panic!("writer acquisition {case}: {error}"));
        let got = exec_error(&app.join(runner));
        row(arm, case, format_args!("errno={got:?}"));
        drop(held);
        assert!(
            open_writer(&app.join(writer)).is_ok(),
            "writer recovery {case}"
        );
        assert!(
            exec_marker(&app.join(runner)).is_ok(),
            "exec recovery {case}"
        );
    }
    for (case, running, writer) in [
        ("exec_then_writer_same", "exec-rw", "exec-rw"),
        ("exec_then_writer_alias", "exec-r", "exec-rw-alias"),
    ] {
        let mut child = held_exec(&app.join(running));
        let got = errno(open_writer(&app.join(writer)));
        row(arm, case, format_args!("errno={got:?}"));
        child.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(wait(&mut child).success(), "held executable recovery");
        assert!(
            open_writer(&app.join(writer)).is_ok(),
            "writer recovery {case}"
        );
        assert!(
            exec_marker(&app.join(running)).is_ok(),
            "exec recovery {case}"
        );
    }
    let file = open_writer_rw(&app.join("exec-rw")).unwrap();
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(
        map,
        libc::MAP_FAILED,
        "shared mapping: {}",
        io::Error::last_os_error()
    );
    drop(file);
    row(
        arm,
        "shared_mapping_closed_fd",
        format_args!("errno={:?}", exec_error(&app.join("exec-r"))),
    );
    assert_eq!(unsafe { libc::munmap(map, 4096) }, 0);
    row(
        arm,
        "shared_mapping_unmapped",
        format_args!("errno={:?}", exec_error(&app.join("exec-r"))),
    );
}

fn client(arm: &str, root: &Path, mediated: bool) {
    let app = if mediated {
        PathBuf::from("/app")
    } else {
        root.join("app")
    };
    let loader_exe = if mediated {
        PathBuf::from("/app/run")
    } else {
        std::env::current_exe().unwrap()
    };
    let loader_root = if mediated {
        PathBuf::from("/")
    } else {
        root.to_owned()
    };
    for (kind, r, rw, a, b) in [
        (
            "exec",
            "exec-r",
            "exec-rw-alias",
            EXEC_A.as_slice(),
            EXEC_B.as_slice(),
        ),
        (
            "dlopen",
            "exec-r.so",
            "exec-rw.so",
            DSO_A.as_slice(),
            DSO_B.as_slice(),
        ),
    ] {
        let first = if kind == "exec" {
            exec_marker(&app.join(r))
        } else {
            loader_marker(&loader_exe, &loader_root, &app.join(r))
        };
        row(arm, &format!("{kind}_A"), format_args!("{first:?}"));
        freshness_row(arm, &format!("{kind}_A_DIAGNOSTIC"), kind, &first, a);
        row(
            arm,
            &format!("{kind}_INITIAL_MARKER"),
            first
                .as_ref()
                .is_ok_and(|v| v.contains(std::str::from_utf8(a).unwrap())),
        );
        replace(&app.join(rw), a, b, arm == "native");
        let second = if kind == "exec" {
            exec_marker(&app.join(r))
        } else {
            loader_marker(&loader_exe, &loader_root, &app.join(r))
        };
        row(arm, &format!("{kind}_B"), format_args!("{second:?}"));
        freshness_row(arm, &format!("{kind}_B_DIAGNOSTIC"), kind, &second, b);
        let restored = second
            .as_ref()
            .is_ok_and(|v| v.contains(std::str::from_utf8(b).unwrap()));
        row(
            arm,
            &format!("{kind}_FRESHNESS"),
            if restored {
                "B"
            } else {
                "WRONG_MARKER_OR_ERROR"
            },
        );
        replace(&app.join(rw), b, a, arm == "native");
        let third = if kind == "exec" {
            exec_marker(&app.join(r))
        } else {
            loader_marker(&loader_exe, &loader_root, &app.join(r))
        };
        row(arm, &format!("{kind}_A_AGAIN"), format_args!("{third:?}"));
        freshness_row(arm, &format!("{kind}_RESTORE_DIAGNOSTIC"), kind, &third, a);
        row(
            arm,
            &format!("{kind}_A_RESTORED"),
            third
                .as_ref()
                .is_ok_and(|v| v.contains(std::str::from_utf8(a).unwrap())),
        );
    }
    etxtbsy(arm, &app);
    let denied_exec = exec_marker(&app.join("exec-denied"));
    if mediated {
        assert!(
            matches!(
                denied_exec.as_ref().err().and_then(io::Error::raw_os_error),
                Some(libc::EACCES | libc::ENOENT)
            ),
            "ungranted executable must be denied: {denied_exec:?}"
        );
    } else {
        assert!(
            denied_exec
                .as_ref()
                .is_ok_and(|output| output.contains("EXEC_A")),
            "raw executable canary: {denied_exec:?}"
        );
    }
    row(arm, "executable_canary", format_args!("{denied_exec:?}"));
    if mediated {
        denied(
            File::open(app.join("exec-neighbour")),
            "ungranted exec neighbour",
        );
    } else {
        assert_eq!(fs::read(app.join("exec-neighbour")).unwrap(), b"unchanged");
    }
    println!("EXEC_ARM_COMPLETE {arm} mapping_attribution=unavailable_without_procfs");
}

fn fuse_provider(root: &Path) {
    unsafe {
        libc::alarm(60);
        libc::umask(0);
    }
    let rules: FsRuleSet =
        serde_json::from_slice(&fs::read(root.join("rules.json")).unwrap()).unwrap();
    for name in [
        "run",
        "math.so",
        "exec-r",
        "exec-rw",
        "exec-rw-alias",
        "exec-r.so",
        "exec-rw.so",
    ] {
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
        .expect("/dev/fuse");
    let view = root.join("view");
    let target = CString::new(view.as_os_str().as_bytes()).unwrap();
    let options = CString::new(format!(
        "fd={},rootmode=40000,user_id=0,group_id=0",
        fuse.as_raw_fd()
    ))
    .unwrap();
    checked(unsafe {
        libc::mount(
            c"nub-exec-test".as_ptr(),
            target.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    })
    .unwrap();
    for name in [
        "run",
        "math.so",
        "exec-r",
        "exec-rw",
        "exec-rw-alias",
        "exec-r.so",
        "exec-rw.so",
    ] {
        fs::rename(root.join(name), root.join("raw/app").join(name)).unwrap();
    }
    let fuse_fd = fuse.as_raw_fd();
    let server = std::thread::spawn(move || projection.serve(fuse.into()));
    let mut cmd = helper(Path::new("/app/run"), "exec-fuse-command", Path::new("/"));
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    confined_launch(&mut cmd, &view, fuse_fd);
    let output = cmd.output().unwrap();
    println!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    unmount_projection(&target, server);
    fs::remove_dir(view).unwrap();
    assert!(output.status.success(), "FUSE arm failed: {output:?}");
}

fn native_provider(root: &Path) {
    unsafe {
        libc::alarm(60);
        libc::umask(0);
    }
    let rules: FsRuleSet =
        serde_json::from_slice(&fs::read(root.join("rules.json")).unwrap()).unwrap();
    let raw = root.join("raw");
    let rw = root.join("rw");
    let read = root.join("read");
    let rw_root = native_tests::recursive_view(&raw, &rw, false);
    let read_root = native_tests::recursive_view(&raw, &read, true);
    let projection = Projection::acquire_native(&rules, rw_root, read_root).unwrap();
    let fuse = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/dev/fuse")
        .expect("/dev/fuse");
    let view = root.join("view");
    let view_c = CString::new(view.as_os_str().as_bytes()).unwrap();
    let options = CString::new(format!(
        "fd={},rootmode=40000,user_id=0,group_id=0",
        fuse.as_raw_fd()
    ))
    .unwrap();
    checked(unsafe {
        libc::mount(
            c"nub-native-exec-test".as_ptr(),
            view_c.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    })
    .unwrap();
    let serve = projection.clone();
    let server = std::thread::spawn(move || serve.serve(fuse.into()));
    let service = projection.native_opener(&view).unwrap();
    let exe = [
        CString::new("/app/run").unwrap(),
        CString::new("--exact").unwrap(),
        CString::new(HELPER).unwrap(),
        CString::new("--nocapture").unwrap(),
        CString::new("--test-threads=1").unwrap(),
    ];
    let env = [
        CString::new("PATH=/usr/bin:/bin").unwrap(),
        CString::new(format!("{ROLE}=exec-native-command")).unwrap(),
        CString::new("NUB_PROJECTION_TEST_ROOT=/").unwrap(),
    ];
    let launch = SupervisedLaunch {
        argv: &exe,
        envp: &env,
        cwd: Some(c"/"),
        ruleset_fd: -1,
        seccomp_ceiling: None,
        stdin: SupervisedStdio::Null,
        stdout: SupervisedStdio::Piped,
        stderr: SupervisedStdio::Piped,
        inherited_fds: &[],
    };
    let policy = EgressPolicy {
        self_proc: BTreeSet::new(),
        allow_all: false,
        allow: vec![],
        write_policy: None,
        proxy_port: None,
        proxy_token: None,
    };
    let mut child = spawn_supervised_projected(
        policy,
        launch,
        ProjectedLaunch {
            root: view_c.clone(),
            opener: service.client(),
        },
    )
    .unwrap();
    let stderr = child.take_stderr().unwrap();
    let stderr_drain = std::thread::spawn(move || {
        let mut errors = String::new();
        BufReader::new(stderr)
            .read_to_string(&mut errors)
            .map(|_| errors)
    });
    let mut text = String::new();
    child
        .take_stdout()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let errors = stderr_drain.join().unwrap().unwrap();
    let status = child.wait().unwrap();
    println!("{text}{errors}");
    service.shutdown().unwrap();
    drop(projection);
    unmount_projection(&view_c, server);
    for path in [&read, &rw] {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        checked(unsafe { libc::umount2(c.as_ptr(), 0) }).unwrap();
        fs::remove_dir(path).unwrap();
    }
    fs::remove_dir(view).unwrap();
    assert!(status.success(), "native arm {status:?}");
}

fn supervisor(root: &Path) {
    unsafe {
        libc::alarm(180);
    }
    let exe = std::env::current_exe().unwrap();
    let payloads = compile_payloads(root);
    let mut failures = Vec::new();
    let mut reports = Vec::new();
    for (name, role, native) in [
        ("raw", "exec-raw", false),
        ("fuse", "exec-fuse-provider", false),
        ("native", "exec-native-provider", true),
    ] {
        println!("EXEC_ARM_START {name}");
        io::stdout().flush().unwrap();
        let case = root.join(name);
        fs::create_dir(&case).unwrap();
        let rules = exec_fixture(&case, &exe, native, &payloads);
        fs::write(case.join("rules.json"), serde_json::to_vec(&rules).unwrap()).unwrap();
        let mut cmd = if role == "exec-raw" {
            helper(&case.join("raw/app/run"), role, &case.join("raw"))
        } else {
            helper(&exe, role, &case)
        };
        if role != "exec-raw" {
            namespace_launch(&mut cmd);
        }
        let output = cmd.output().unwrap();
        println!(
            "--- {name} ---\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            if output.status.signal() == Some(libc::SIGALRM) {
                println!("EXEC_TIMEOUT arm={name}");
            }
            failures.push(format!("{name}: {:?}", output.status));
        }
        if name == "native" && output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            for name in ["exec-rw-alias", "exec-rw.so"] {
                let meta = fs::metadata(case.join("raw/app").join(name)).unwrap();
                let prefix = format!(
                    "EXEC_NATIVE_OBJECT name={name} dev={} ino={} type=",
                    meta.dev(),
                    meta.ino()
                );
                assert_eq!(
                    text.lines().filter(|line| line.contains(&prefix)).count(),
                    2,
                    "native payload identity must match backing across both mutations"
                );
            }
        }
        assert_eq!(
            fs::read(case.join("raw/app/exec-neighbour")).unwrap(),
            b"unchanged",
            "host neighbour snapshot {name}"
        );
        reports.push((name, String::from_utf8_lossy(&output.stdout).into_owned()));
        fs::remove_dir_all(&case).unwrap();
    }
    assert!(
        failures.is_empty(),
        "LINUX_EXEC_ACCEPTANCE_FAILURE {failures:?}"
    );
    for (arm, report) in &reports {
        for case in ["exec_FRESHNESS", "dlopen_FRESHNESS"] {
            assert!(
                report.contains(&format!("arm={arm} case={case} result=B")),
                "LINUX_EXEC_ACCEPTANCE_FAILURE {arm} lacks fresh-byte result for {case}:\\n{report}"
            );
        }
        for case in [
            "exec_INITIAL_MARKER",
            "exec_A_RESTORED",
            "dlopen_INITIAL_MARKER",
            "dlopen_A_RESTORED",
        ] {
            assert!(
                report.contains(&format!("arm={arm} case={case} result=true")),
                "LINUX_EXEC_ACCEPTANCE_FAILURE {arm} lacks marker control for {case}:\\n{report}"
            );
        }
    }
    let raw = reports
        .iter()
        .find(|(arm, _)| *arm == "raw")
        .unwrap()
        .1
        .as_str();
    for case in [
        "writer_then_exec_same",
        "writer_then_exec_alias",
        "exec_then_writer_same",
        "exec_then_writer_alias",
    ] {
        assert!(
            raw.contains(&format!(
                "arm=raw case={case} result=errno=Some({})",
                libc::ETXTBSY
            )),
            "raw control did not establish ETXTBSY for {case}:\\n{raw}"
        );
    }
    let result = |report: &str, arm: &str, case: &str| {
        report
            .lines()
            .find_map(|line| line.strip_prefix(&format!("EXEC_ROW arm={arm} case={case} result=")))
            .map(str::to_owned)
    };
    let mut parity_failures = Vec::new();
    for (arm, report) in reports.iter().filter(|(arm, _)| *arm != "raw") {
        for case in [
            "writer_then_exec_same",
            "writer_then_exec_alias",
            "exec_then_writer_same",
            "exec_then_writer_alias",
            "shared_mapping_closed_fd",
            "shared_mapping_unmapped",
        ] {
            let raw_result = result(raw, "raw", case);
            let projected_result = result(report, arm, case);
            if raw_result != projected_result {
                println!(
                    "EXEC_PARITY_MISMATCH arm={arm} case={case} raw={raw_result:?} projected={projected_result:?}"
                );
                parity_failures.push(format!("{arm}:{case}"));
            }
        }
    }
    assert!(
        parity_failures.is_empty(),
        "LINUX_EXEC_ACCEPTANCE_PARITY_FAIL {parity_failures:?}"
    );
    println!("LINUX_EXEC_ACCEPTANCE_COMPLETE");
}

pub(super) fn run_role(role: &str, root: &Path) -> bool {
    match role {
        "exec-supervisor" => supervisor(root),
        "exec-raw" => client("raw", root, false),
        "exec-fuse-provider" => fuse_provider(root),
        "exec-fuse-command" => client("fuse", root, true),
        "exec-native-provider" => native_provider(root),
        "exec-native-command" => client("native", root, true),
        "exec-loader" => loader(),
        _ => return false,
    }
    true
}
