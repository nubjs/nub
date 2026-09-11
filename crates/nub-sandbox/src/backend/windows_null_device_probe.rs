//! Probe direct NT null-device opens through the production native adapter.

use serde_json::json;
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, GetHandleInformation, HANDLE_FLAG_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_TYPE_CHAR, GetFileType, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

const STATUS_SUCCESS: i32 = 0;
const ACCESS_DENIED: i32 = 0xc000_0022_u32 as i32;
const OBJ_INHERIT: u32 = 0x2;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const FILE_OPEN: u32 = 1;
const FILE_OPEN_IF: u32 = 3;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x4000;
const FILE_SHARE_ALL: u32 = 7;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const READ_CONTROL: u32 = 0x0002_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WRITE_DAC: u32 = 0x0004_0000;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: *mut c_void,
    name: *const UnicodeString,
    attributes: u32,
    security: *mut c_void,
    quality: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct IoStatus {
    status: isize,
    information: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut *mut c_void,
        access: u32,
        attrs: *const ObjectAttributes,
        io: *mut IoStatus,
        allocation: *const i64,
        attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn NtOpenFile(
        handle: *mut *mut c_void,
        access: u32,
        attrs: *const ObjectAttributes,
        io: *mut IoStatus,
        share: u32,
        options: u32,
    ) -> i32;
}

fn handle_count() -> u32 {
    let mut count = 0;
    assert_ne!(
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
        0
    );
    count
}

fn open_null(
    create: bool,
    disposition: u32,
    access: u32,
    options: u32,
    name: &str,
    attrs_length: u32,
    spare_terminator: bool,
) -> serde_json::Value {
    let mut encoded_name: Vec<u16> = name.encode_utf16().collect();
    let length = (encoded_name.len() * 2) as u16;
    if spare_terminator {
        encoded_name.push(0);
    }
    let name = UnicodeString {
        length,
        maximum_length: (encoded_name.len() * 2) as u16,
        buffer: encoded_name.as_ptr(),
    };
    let attrs = ObjectAttributes {
        length: attrs_length,
        root: std::ptr::null_mut(),
        name: &name,
        attributes: OBJ_CASE_INSENSITIVE | OBJ_INHERIT,
        security: std::ptr::null_mut(),
        quality: std::ptr::null_mut(),
    };
    let before = handle_count();
    let mut io = IoStatus::default();
    let mut handle = std::ptr::null_mut();
    let call = |handle: &mut *mut c_void, io: &mut IoStatus| unsafe {
        if create {
            NtCreateFile(
                handle,
                access,
                &attrs,
                io,
                std::ptr::null(),
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_ALL,
                disposition,
                options,
                std::ptr::null(),
                0,
            )
        } else {
            NtOpenFile(handle, access, &attrs, io, FILE_SHARE_ALL, options)
        }
    };
    let status = call(&mut handle, &mut io);
    if status != STATUS_SUCCESS {
        let repeated_status_matches = (1..64).all(|_| {
            let mut repeated_handle = std::ptr::null_mut();
            let mut repeated_io = IoStatus::default();
            let repeated = call(&mut repeated_handle, &mut repeated_io);
            if repeated == STATUS_SUCCESS {
                drop(unsafe { OwnedHandle::from_raw_handle(repeated_handle) });
            }
            repeated == status
        });
        return json!({"status": status, "opened": false, "repeated_status_matches": repeated_status_matches, "handles_balanced": handle_count() == before});
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut read = [1u8];
    let mut read_bytes = 1;
    let mut write_bytes = 0;
    let read_ok = unsafe {
        ReadFile(
            handle.as_raw_handle(),
            read.as_mut_ptr().cast(),
            read.len() as u32,
            &mut read_bytes,
            std::ptr::null_mut(),
        ) != 0
    };
    let write_ok = unsafe {
        WriteFile(
            handle.as_raw_handle(),
            b"x".as_ptr().cast(),
            1,
            &mut write_bytes,
            std::ptr::null_mut(),
        ) != 0
    };
    let file_type = unsafe { GetFileType(handle.as_raw_handle()) };
    let mut flags = 0u32;
    let inherited = unsafe { GetHandleInformation(handle.as_raw_handle(), &mut flags) != 0 }
        && flags & HANDLE_FLAG_INHERIT != 0;
    drop(handle);
    let repeated_status_matches = (1..64).all(|_| {
        let mut repeated_handle = std::ptr::null_mut();
        let mut repeated_io = IoStatus::default();
        let repeated = call(&mut repeated_handle, &mut repeated_io);
        if repeated == STATUS_SUCCESS {
            drop(unsafe { OwnedHandle::from_raw_handle(repeated_handle) });
        }
        repeated == status
    });
    json!({
        "status": status,
        "opened": true,
        "io_status": io.status,
        "read": read_ok && read_bytes == 0,
        "write": write_ok && write_bytes == 1,
        "file_type": file_type == FILE_TYPE_CHAR,
        "inherited": inherited,
        "repeated_status_matches": repeated_status_matches,
        "handles_balanced": handle_count() == before,
    })
}

pub(super) fn probe() -> serde_json::Value {
    let supported_access =
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL | SYNCHRONIZE | FILE_READ_ATTRIBUTES;
    let supported_options =
        FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE | FILE_OPEN_FOR_BACKUP_INTENT;
    json!({
        "open": open_null(false, FILE_OPEN, supported_access, supported_options, r"\Device\Null", std::mem::size_of::<ObjectAttributes>() as u32, false),
        "spare_terminator": open_null(false, FILE_OPEN, supported_access, supported_options, r"\Device\Null", std::mem::size_of::<ObjectAttributes>() as u32, true),
        "create": open_null(true, FILE_OPEN_IF, supported_access, supported_options, r"\Device\Null", std::mem::size_of::<ObjectAttributes>() as u32, false),
        "create_open": open_null(true, FILE_OPEN, supported_access, supported_options, r"\Device\Null", std::mem::size_of::<ObjectAttributes>() as u32, false),
        "msys_access": open_null(false, FILE_OPEN, 0x4012_0080, FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_FOR_BACKUP_INTENT, r"\Device\Null", std::mem::size_of::<ObjectAttributes>() as u32, false),
        "async": open_null(false, FILE_OPEN, supported_access, FILE_NON_DIRECTORY_FILE, r"\Device\Null", std::mem::size_of::<ObjectAttributes>() as u32, false),
        "unknown_device": open_null(false, FILE_OPEN, supported_access, supported_options, r"\Device\NullX", std::mem::size_of::<ObjectAttributes>() as u32, false),
        "security_access": open_null(false, FILE_OPEN, supported_access | WRITE_DAC, supported_options, r"\Device\Null", std::mem::size_of::<ObjectAttributes>() as u32, false),
        "invalid_shape": open_null(false, FILE_OPEN, supported_access, supported_options, r"\Device\Null", 0, false),
    })
}

pub(super) fn assert_mode(result: &serde_json::Value, mode: &str) {
    for operation in [
        "open",
        "spare_terminator",
        "create",
        "create_open",
        "msys_access",
        "async",
        "unknown_device",
        "security_access",
        "invalid_shape",
    ] {
        assert_eq!(
            result[operation]["handles_balanced"], true,
            "{mode}: {result}"
        );
        assert_eq!(
            result[operation]["repeated_status_matches"], true,
            "{mode}: {result}"
        );
    }
    for operation in ["open", "spare_terminator", "create", "create_open"] {
        let current = &result[operation];
        if mode == "raw" && current["opened"] == false {
            assert_eq!(current["status"], ACCESS_DENIED, "{mode}: {result}");
        } else {
            assert_eq!(current["opened"], true, "{mode}: {result}");
            for property in [
                "read",
                "write",
                "file_type",
                "inherited",
                "repeated_status_matches",
                "handles_balanced",
            ] {
                assert_eq!(
                    current[property], true,
                    "{mode} {operation} {property}: {result}"
                );
            }
        }
    }
    // Windows 11 permits these harmless raw device opens; Server 2022 denies
    // them. The adapter repairs denial, not an OS-independent Null policy.
    if mode != "raw" || result["msys_access"]["opened"] == true {
        assert_eq!(result["msys_access"]["opened"], true, "{mode}: {result}");
        assert_eq!(result["msys_access"]["write"], true, "{mode}: {result}");
        assert_eq!(result["msys_access"]["read"], false, "{mode}: {result}");
    } else {
        assert_eq!(
            result["msys_access"]["status"], ACCESS_DENIED,
            "{mode}: {result}"
        );
    }
}

pub(super) fn assert_unsupported_matches(result: &serde_json::Value, raw: &serde_json::Value) {
    for operation in [
        "async",
        "unknown_device",
        "security_access",
        "invalid_shape",
    ] {
        assert_eq!(
            result[operation]["status"], raw[operation]["status"],
            "adapter changed unsupported {operation}: raw={raw}, adapter={result}"
        );
        assert_eq!(
            result[operation]["opened"], raw[operation]["opened"],
            "adapter changed unsupported {operation}: raw={raw}, adapter={result}"
        );
        assert_eq!(
            result[operation]["handles_balanced"], true,
            "adapter leaked unsupported {operation}: {result}"
        );
    }
}
