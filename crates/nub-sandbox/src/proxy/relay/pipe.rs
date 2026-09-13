//! Preconnected, overlapped pipe endpoints. Only handles cross into the AppContainer;
//! it never opens a named service. Cancellation drains the operation before its buffers die.

use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
    SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_TYPE_PIPE, GetFileType,
    OPEN_EXISTING, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

fn event() -> io::Result<OwnedHandle> {
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: newly created, uniquely owned handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(crate) fn pair(parent_reads: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
    let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let name: Vec<u16> = format!(r"\\.\pipe\nub-relay-{nonce}")
        .encode_utf16()
        .chain([0])
        .collect();
    let sddl: Vec<u16> = "D:P(A;;GA;;;OW)".encode_utf16().chain([0]).collect();
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let server = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            FILE_FLAG_FIRST_PIPE_INSTANCE
                | FILE_FLAG_OVERLAPPED
                | if parent_reads {
                    PIPE_ACCESS_INBOUND
                } else {
                    PIPE_ACCESS_OUTBOUND
                },
            PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &attributes,
        )
    };
    let error = io::Error::last_os_error();
    unsafe {
        LocalFree(descriptor);
    }
    if server == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    let server = unsafe { OwnedHandle::from_raw_handle(server) };
    let client = unsafe {
        CreateFileW(
            name.as_ptr(),
            if parent_reads {
                GENERIC_WRITE
            } else {
                GENERIC_READ
            },
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if client == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let client = unsafe { OwnedHandle::from_raw_handle(client) };
    let ready = event()?;
    let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
    operation.hEvent = ready.as_raw_handle();
    if unsafe { ConnectNamedPipe(server.as_raw_handle(), &mut operation) } == 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_PIPE_CONNECTED) => {}
            Some(ERROR_IO_PENDING) => {
                let mut bytes = 0;
                if unsafe { GetOverlappedResult(server.as_raw_handle(), &operation, &mut bytes, 1) }
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            _ => return Err(error),
        }
    }
    if unsafe {
        SetHandleInformation(
            client.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((server, client))
}

pub(crate) struct Pipe {
    handle: OwnedHandle,
    event: OwnedHandle,
    stop: Arc<AtomicBool>,
}

impl Pipe {
    pub(crate) fn new(handle: OwnedHandle, stop: Arc<AtomicBool>) -> io::Result<Self> {
        if unsafe { GetFileType(handle.as_raw_handle()) } != FILE_TYPE_PIPE {
            return Err(io::Error::other("relay requires a pipe handle"));
        }
        if unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle,
            event: event()?,
            stop,
        })
    }

    fn transfer(
        &mut self,
        start: impl FnOnce(*mut OVERLAPPED, *mut u32) -> i32,
    ) -> io::Result<usize> {
        if self.stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        unsafe {
            ResetEvent(self.event.as_raw_handle());
        }
        let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
        operation.hEvent = self.event.as_raw_handle();
        let mut bytes = 0;
        if start(&mut operation, &mut bytes) != 0 {
            return Ok(bytes as usize);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error().map(|code| code as u32) != Some(ERROR_IO_PENDING) {
            return Err(error);
        }
        loop {
            if self.stop.load(Ordering::Acquire) {
                // CancelIoEx only requests cancellation. Even ERROR_NOT_FOUND can race
                // completion; always drain before releasing OVERLAPPED or caller buffers.
                unsafe {
                    CancelIoEx(self.handle.as_raw_handle(), &operation);
                    GetOverlappedResult(self.handle.as_raw_handle(), &operation, &mut bytes, 1);
                }
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            match unsafe { WaitForSingleObject(self.event.as_raw_handle(), 100) } {
                WAIT_OBJECT_0 => break,
                WAIT_TIMEOUT => {}
                _ => {
                    let error = io::Error::last_os_error();
                    unsafe {
                        CancelIoEx(self.handle.as_raw_handle(), &operation);
                        GetOverlappedResult(self.handle.as_raw_handle(), &operation, &mut bytes, 1);
                    }
                    return Err(error);
                }
            }
        }
        if unsafe { GetOverlappedResult(self.handle.as_raw_handle(), &operation, &mut bytes, 0) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(bytes as usize)
    }
}

impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let handle = self.handle.as_raw_handle();
        self.transfer(|operation, count| unsafe {
            ReadFile(
                handle,
                bytes.as_mut_ptr(),
                bytes.len().min(u32::MAX as usize) as u32,
                count,
                operation,
            )
        })
    }
}

impl Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let handle = self.handle.as_raw_handle();
        self.transfer(|operation, count| unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len().min(u32::MAX as usize) as u32,
                count,
                operation,
            )
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn cancellation_drains_pending_reads_and_saturated_writes() {
        for reading in [true, false] {
            let (parent, _peer) = pair(reading).expect("preconnected pipe");
            let stop = Arc::new(AtomicBool::new(false));
            let mut pipe = Pipe::new(parent, stop.clone()).unwrap();
            let (started, ready) = mpsc::channel();
            let (finished, done) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                started.send(()).unwrap();
                let result = if reading {
                    pipe.read(&mut [0; 1]).map(|_| ())
                } else {
                    pipe.write_all(&vec![0; 4 * 1024 * 1024])
                };
                finished.send(result).unwrap();
            });
            ready.recv_timeout(Duration::from_secs(10)).unwrap();
            // Either cancellation wins before submission, or it must drain the overlapped
            // operation. Both races are valid; no peer ever supplies data or drains writes.
            stop.store(true, Ordering::Release);
            assert_eq!(
                done.recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::ConnectionAborted
            );
            worker.join().unwrap();
        }
    }

    #[test]
    fn preconnected_endpoints_transfer_exact_bytes() {
        let (parent, peer) = pair(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut input = Pipe::new(parent, stop.clone()).unwrap();
        let mut output = Pipe::new(peer, stop).unwrap();
        output.write_all(b"relay-frame").unwrap();
        let mut bytes = [0; 11];
        input.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"relay-frame");
    }
}
