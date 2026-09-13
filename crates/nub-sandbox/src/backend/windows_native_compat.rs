//! Embedded native compatibility assets owned by the persistent Windows lease.

use super::windows::windows_registry::{self, Acquired};
use sha2::{Digest as _, Sha256};
use std::io;
#[cfg(target_env = "msvc")]
use std::io::Write as _;
#[cfg(target_env = "msvc")]
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[cfg(target_env = "msvc")]
const ASSETS: &[(&str, &[u8])] = &[
    (
        "compat-x64.dll",
        include_bytes!(concat!(env!("OUT_DIR"), "/compat-x64.dll")),
    ),
    (
        "compat-arm64.dll",
        include_bytes!(concat!(env!("OUT_DIR"), "/compat-arm64.dll")),
    ),
];
#[cfg(not(target_env = "msvc"))]
const ASSETS: &[(&str, &[u8])] = &[];

pub(super) fn version() -> &'static str {
    static VERSION: LazyLock<String> = LazyLock::new(|| {
        let mut hash = Sha256::new();
        hash.update(b"native-adapter/socket-broker-v1");
        for (name, bytes) in ASSETS {
            hash.update(name.as_bytes());
            hash.update(bytes);
        }
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    });
    &VERSION
}

pub(super) fn asset_path(profile: &str) -> io::Result<PathBuf> {
    windows_registry::native_assets_path(profile)
}

#[cfg(target_env = "msvc")]
pub(super) fn install(resource: &mut Acquired, path: &Path) -> io::Result<()> {
    // The protected registry parent prevents an AppContainer from replacing the
    // directory or its DLLs. The launcher later grants only this leaf read/execute.
    resource.record_private_path(path)?;
    #[cfg(test)]
    super::windows::launch::test_crash_transition(
        "native-assets-journaled",
        &resource.entry.profile_name,
        path,
    );
    std::fs::create_dir(path)?;
    #[cfg(test)]
    super::windows::launch::test_crash_transition(
        "native-assets-before-identity",
        &resource.entry.profile_name,
        path,
    );
    resource.record_mutation(windows_registry::AclMutation {
        path: path.to_string_lossy().into_owned(),
        kind: windows_registry::AclKind::Subtree,
        access: windows_sys::Win32::Foundation::GENERIC_READ
            | windows_sys::Win32::Foundation::GENERIC_EXECUTE,
    })?;
    #[cfg(test)]
    super::windows::launch::test_crash_transition(
        "native-assets-created",
        &resource.entry.profile_name,
        path,
    );
    for (name, bytes) in ASSETS {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.join(name))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(test)]
        super::windows::launch::test_crash_transition(
            "native-asset-written",
            &resource.entry.profile_name,
            path,
        );
    }
    #[cfg(test)]
    super::windows::launch::test_crash_transition(
        "native-assets-installed",
        &resource.entry.profile_name,
        path,
    );
    Ok(())
}

#[cfg(not(target_env = "msvc"))]
pub(super) fn install(_resource: &mut Acquired, _path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native compatibility requires an MSVC build",
    ))
}

pub(super) struct SocketBroker {
    #[cfg(target_env = "msvc")]
    native: std::ptr::NonNull<std::ffi::c_void>,
    #[cfg(target_env = "msvc")]
    endpoint: Vec<u16>,
}

// SAFETY: the native allocation is stable across moves and its worker threads
// synchronize through a cancellation event. Drop stops and joins them all.
unsafe impl Send for SocketBroker {}

impl SocketBroker {
    pub(super) fn start(
        process: *mut std::ffi::c_void,
        job: *mut std::ffi::c_void,
    ) -> io::Result<Self> {
        #[cfg(target_env = "msvc")]
        {
            unsafe extern "C" {
                fn sandbox_socket_broker_start(
                    process: *mut std::ffi::c_void,
                    job: *mut std::ffi::c_void,
                    endpoint: *const u16,
                    broker: *mut *mut std::ffi::c_void,
                ) -> u32;
            }
            let mut nonce = [0u8; 16];
            getrandom::getrandom(&mut nonce)
                .map_err(|error| io::Error::other(error.to_string()))?;
            let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
            let endpoint: Vec<u16> = format!(r"\\.\pipe\LOCAL\sandbox-sockets-{nonce}")
                .encode_utf16()
                .chain([0])
                .collect();
            let mut native = std::ptr::null_mut();
            // SAFETY: WindowsChild owns this suspended process and the Job. It
            // destroys the broker before closing the borrowed Job handle.
            let code = unsafe {
                sandbox_socket_broker_start(process, job, endpoint.as_ptr(), &mut native)
            };
            if code != 0 {
                return Err(io::Error::other(format!(
                    "native sandbox socket broker: {}",
                    io::Error::from_raw_os_error(code as i32)
                )));
            }
            let native = std::ptr::NonNull::new(native)
                .ok_or_else(|| io::Error::other("native socket broker returned no owner"))?;
            Ok(Self { native, endpoint })
        }
        #[cfg(not(target_env = "msvc"))]
        {
            let _ = (process, job);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native socket broker requires an MSVC build",
            ))
        }
    }
}

impl Drop for SocketBroker {
    fn drop(&mut self) {
        #[cfg(target_env = "msvc")]
        {
            unsafe extern "C" {
                fn sandbox_socket_broker_stop(broker: *mut std::ffi::c_void);
            }
            // SAFETY: this uniquely owns the allocation returned by start. Stop
            // cancels pending I/O and joins every worker before freeing it.
            unsafe { sandbox_socket_broker_stop(self.native.as_ptr()) };
        }
    }
}

pub(super) fn inject(
    process: *mut std::ffi::c_void,
    path: &Path,
    broker: Option<&SocketBroker>,
) -> io::Result<()> {
    #[cfg(target_env = "msvc")]
    {
        unsafe extern "C" {
            fn sandbox_native_inject(
                process: *mut std::ffi::c_void,
                directory: *const u16,
                broker: *const u16,
            ) -> u32;
        }
        let path = super::windows::strip_verbatim_prefix(path.to_path_buf());
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the launcher owns this suspended process handle, and the FFI
        // copies the terminated directory string before returning.
        let endpoint = broker.map_or(std::ptr::null(), |broker| broker.endpoint.as_ptr());
        let code = unsafe { sandbox_native_inject(process, wide.as_ptr(), endpoint) };
        if code != 0 {
            return Err(io::Error::other(format!(
                "native sandbox compatibility injection: {}",
                io::Error::from_raw_os_error(code as i32)
            )));
        }
        Ok(())
    }
    #[cfg(not(target_env = "msvc"))]
    {
        let _ = (process, path, broker);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native compatibility requires an MSVC build",
        ))
    }
}

#[cfg(all(test, target_env = "msvc"))]
mod tests {
    fn endpoint() -> Vec<u16> {
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).unwrap();
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        format!(r"\\.\pipe\LOCAL\sandbox-socket-test-{nonce}")
            .encode_utf16()
            .chain([0])
            .collect()
    }

    #[test]
    fn socket_broker_rejects_wrong_frame_lengths_and_drains_cancelled_read() {
        unsafe extern "C" {
            fn sandbox_socket_broker_test_frames(endpoint: *const u16) -> u32;
        }
        // SAFETY: the native test owns every pipe/event and drains each pending
        // operation before its stack buffers are released.
        let error = unsafe { sandbox_socket_broker_test_frames(endpoint().as_ptr()) };
        assert_eq!(
            error,
            0,
            "{}",
            std::io::Error::from_raw_os_error(error as i32)
        );
    }

    #[test]
    fn socket_broker_rejects_a_live_client_outside_its_job_and_cancels_idle_workers() {
        unsafe extern "C" {
            fn sandbox_socket_broker_test_foreign_client(endpoint: *const u16) -> u32;
        }
        // SAFETY: the native test copies this terminated name and creates only
        // its own pipe, empty Job, events and process-query handle.
        let error = unsafe { sandbox_socket_broker_test_foreign_client(endpoint().as_ptr()) };
        assert_eq!(
            error,
            0,
            "{}",
            std::io::Error::from_raw_os_error(error as i32)
        );
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Request {
        version: u32,
        family: i32,
        kind: i32,
        protocol: i32,
        flags: u32,
    }

    fn validate(request: Request) -> i32 {
        unsafe extern "C" {
            fn sandbox_socket_broker_validate(request: *const Request) -> i32;
        }
        // SAFETY: Request matches the pointer-free fixed-width native frame.
        unsafe { sandbox_socket_broker_validate(&request) }
    }

    #[test]
    fn socket_protocol_rejects_raw_privileged_unknown_and_malformed_requests() {
        let tcp = Request {
            version: 1,
            family: 2,
            kind: 1,
            protocol: 6,
            flags: 0x81,
        };
        for family in [2, 23] {
            for (kind, protocol) in [(1, 0), (1, 6), (2, 0), (2, 17)] {
                assert_eq!(
                    validate(Request {
                        family,
                        kind,
                        protocol,
                        ..tcp
                    }),
                    0
                );
            }
        }
        for request in [
            Request { version: 0, ..tcp },
            Request { family: 1, ..tcp },
            Request { kind: 3, ..tcp },
            Request { protocol: 1, ..tcp },
            Request { flags: 0x40, ..tcp }, // WSA_FLAG_ACCESS_SYSTEM_SECURITY
            Request {
                flags: u32::MAX,
                ..tcp
            },
        ] {
            assert_ne!(validate(request), 0);
        }
    }
}
