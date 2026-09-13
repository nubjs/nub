//! Focused tests for the retained namespace-pair bootstrap contract.

use super::namespace::{
    BootstrapFault, NamespacePair, Reply, UnmountResult, decode_packet, decode_packet_expected,
    decode_unmount_packet, receive_reply,
};
use std::io;
use std::os::fd::RawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn assert_errno<T>(result: io::Result<T>, expected: i32) {
    match result {
        Err(error) => assert_eq!(error.raw_os_error(), Some(expected)),
        Ok(_) => panic!("expected errno {expected}, got success"),
    }
}

#[test]
fn namespace_packet_accepts_only_the_fixed_two_fd_success_shape() {
    assert_eq!(
        decode_packet(&[b'N', b'P', 1, 0, 0, 0, 0, 0], 2).unwrap(),
        Reply::Success
    );
    assert_errno(
        decode_packet(&[b'N', b'P', 1, 0, 0, 0, 0, 0], 1),
        libc::EPROTO,
    );
    assert_errno(
        decode_packet(&[b'N', b'P', 2, 0, 0, 0, 0, 0], 0),
        libc::EPROTO,
    );
}

#[test]
fn namespace_packet_rejects_unknown_or_malformed_reply() {
    let errno = libc::EIO.to_ne_bytes();
    assert_eq!(
        decode_packet(
            &[b'N', b'P', 2, 0, errno[0], errno[1], errno[2], errno[3]],
            0
        )
        .unwrap(),
        Reply::Failure(libc::EIO)
    );
    assert_errno(
        decode_packet(&[b'N', b'P', 99, 0, 0, 0, 0, 0], 0),
        libc::EPROTO,
    );
    assert_errno(
        decode_packet(&[b'X', b'P', 1, 0, 0, 0, 0, 0], 2),
        libc::EPROTO,
    );
}

#[test]
fn projected_mount_packet_requires_all_six_descriptors() {
    let packet = [b'N', b'P', 1, 0, 0, 0, 0, 0];
    assert_eq!(
        decode_packet_expected(&packet, 6, 6).unwrap(),
        Reply::Success
    );
    assert_errno(decode_packet_expected(&packet, 5, 6), libc::EPROTO);
    assert_errno(decode_packet_expected(&packet, 7, 6), libc::EPROTO);
}

#[test]
fn namespace_cleanup_packet_requires_a_rights_free_success() {
    let packet = [b'N', b'P', 1, 0, 0, 0, 0, 0];
    assert_eq!(
        decode_packet_expected(&packet, 0, 0).unwrap(),
        Reply::Success
    );
    assert_errno(decode_packet_expected(&packet, 1, 0), libc::EPROTO);
}

#[test]
fn forced_view_abort_distinguishes_vfs_busy_after_abort_from_pre_unmount_errors() {
    assert_eq!(
        decode_unmount_packet(&[b'N', b'P', 1, 0, 0, 0, 0, 0]).unwrap(),
        UnmountResult::Unmounted
    );
    let busy = libc::EBUSY.to_ne_bytes();
    assert_eq!(
        decode_unmount_packet(&[b'N', b'P', 2, 1, busy[0], busy[1], busy[2], busy[3],]).unwrap(),
        UnmountResult::ForceIssuedBusy
    );
    assert_errno(
        decode_unmount_packet(&[b'N', b'P', 2, 0, busy[0], busy[1], busy[2], busy[3]]),
        libc::EBUSY,
    );
    assert_errno(
        decode_unmount_packet(&[b'N', b'P', 2, 1, 0, 0, 0, 0]),
        libc::EPROTO,
    );
}

const CMSG_ALIGN: usize = std::mem::size_of::<usize>();

const fn align_cmsg(value: usize) -> usize {
    (value + CMSG_ALIGN - 1) & !(CMSG_ALIGN - 1)
}

const CMSG_DATA_OFFSET: usize = align_cmsg(std::mem::size_of::<libc::cmsghdr>());
const CMSG_LEN_FD: usize = CMSG_DATA_OFFSET + std::mem::size_of::<RawFd>();
const CMSG_SPACE_FD: usize = align_cmsg(CMSG_LEN_FD);

#[repr(C, align(8))]
struct CmsgBuffer([u8; CMSG_SPACE_FD]);

fn socket_pair() -> [RawFd; 2] {
    let mut sockets = [0; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                sockets.as_mut_ptr(),
            )
        },
        0
    );
    sockets
}

fn send_packet(socket: RawFd, packet: &[u8], passed: Option<RawFd>) {
    let mut iov = libc::iovec {
        iov_base: packet.as_ptr().cast_mut().cast(),
        iov_len: packet.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    let mut control = CmsgBuffer([0; CMSG_SPACE_FD]);
    if let Some(passed) = passed {
        let header = control.0.as_mut_ptr().cast::<libc::cmsghdr>();
        unsafe {
            (*header).cmsg_len = CMSG_LEN_FD;
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (control.0.as_mut_ptr().add(CMSG_DATA_OFFSET).cast::<RawFd>()).write_unaligned(passed);
        }
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = CMSG_SPACE_FD;
    }
    assert_eq!(
        unsafe { libc::sendmsg(socket, &message, libc::MSG_NOSIGNAL) },
        packet.len() as isize
    );
}

#[test]
fn namespace_failure_packet_roundtrips_without_rights() {
    let sockets = socket_pair();
    let errno = libc::EIO.to_ne_bytes();
    send_packet(
        sockets[0],
        &[b'N', b'P', 2, 0, errno[0], errno[1], errno[2], errno[3]],
        None,
    );
    assert_errno(
        receive_reply(sockets[1], Instant::now() + Duration::from_secs(1)),
        libc::EIO,
    );
    unsafe {
        libc::close(sockets[0]);
        libc::close(sockets[1]);
    }
}

fn assert_receiver_closes_passed_pipe(packet: &[u8]) {
    let sockets = socket_pair();
    let mut pipe = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    // The queued SCM_RIGHTS duplicate is the only reader after `pipe[0]`
    // closes, so POLLERR proves receiver cleanup on every packet rejection.
    send_packet(sockets[0], packet, Some(pipe[0]));
    unsafe {
        libc::close(pipe[0]);
    }
    assert_errno(
        receive_reply(sockets[1], Instant::now() + Duration::from_secs(1)),
        libc::EPROTO,
    );
    let mut writer = libc::pollfd {
        fd: pipe[1],
        events: libc::POLLOUT,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut writer, 1, 0) }, 1);
    assert_ne!(writer.revents & libc::POLLERR, 0, "received pipe fd leaked");
    unsafe {
        libc::close(pipe[1]);
        libc::close(sockets[0]);
        libc::close(sockets[1]);
    }
}

#[test]
fn namespace_malformed_and_truncated_packets_close_received_rights() {
    assert_receiver_closes_passed_pipe(b"XP\x01\0\0\0\0\0");
    // The ninth byte forces MSG_TRUNC before the otherwise-valid success
    // payload reaches packet decoding.
    assert_receiver_closes_passed_pipe(b"NP\x01\0\0\0\0\0!");
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

fn has_sys_admin() -> bool {
    let mut header = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [CapData::default(), CapData::default()];
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0 {
        return false;
    }
    // Linux UAPI capability.h; libc does not export the capability numbers.
    let capability = 21usize;
    data[capability / 32].effective & (1 << (capability % 32)) != 0
}

unsafe fn child_enters_and_reports(pair: &NamespacePair, report: i32) -> ! {
    let result = unsafe { pair.enter() }.is_ok() && has_sys_admin();
    let marker = [u8::from(result)];
    unsafe {
        libc::write(report, marker.as_ptr().cast(), marker.len());
        libc::close(report);
        libc::_exit(if result { 0 } else { 1 });
    }
}

fn wait_success(pid: libc::pid_t) {
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert_eq!(status, 0, "namespace re-entry child failed: {status}");
}

/// This needs a kernel that permits and maps unprivileged user namespaces. It
/// deliberately starts a sibling thread before acquisition: the host library
/// parent remains multithreaded while the later raw fork child enters the pair.
#[test]
#[ignore = "requires an unprivileged user namespace-capable Linux kernel"]
fn namespace_pair_reenters_after_bootstrap_exit_from_multithreaded_parent() {
    assert!(
        !has_sys_admin(),
        "this probe requires an unprivileged parent to establish caps-only child authority"
    );
    let (started_tx, started_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let sibling = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let _ = stop_rx.recv();
    });
    started_rx.recv().unwrap();

    // This fault happens after user-namespace creation. The parent receives an
    // error only after the one-shot child is reaped, so it cannot leave a
    // capability-bearing bootstrap process behind.
    assert_errno(
        NamespacePair::create_with_test_fault(BootstrapFault::AfterUserNamespace),
        libc::EIO,
    );

    let pair = NamespacePair::create().unwrap();
    let mut report = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed: {}", io::Error::last_os_error());
    if pid == 0 {
        unsafe {
            libc::close(report[0]);
            child_enters_and_reports(&pair, report[1]);
        }
    }
    unsafe {
        libc::close(report[1]);
    }
    let mut marker = [0; 1];
    assert_eq!(
        unsafe { libc::read(report[0], marker.as_mut_ptr().cast(), marker.len()) },
        1
    );
    unsafe {
        libc::close(report[0]);
    }
    wait_success(pid);
    assert_eq!(marker, [1], "entered child lacked CAP_SYS_ADMIN");

    drop(stop_tx);
    sibling.join().unwrap();
}
