//! Exercise the native Mount Manager ABI, without importing adapter code.

use serde_json::json;
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_NT};

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
    fn NtDeviceIoControlFile(
        handle: *mut c_void,
        event: *mut c_void,
        apc: *mut c_void,
        context: *mut c_void,
        io: *mut IoStatus,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
    ) -> i32;
}

pub(super) fn read_directory_node(path: &Path) -> i32 {
    use std::os::windows::ffi::OsStrExt as _;
    let name: Vec<u16> = r"\??\"
        .encode_utf16()
        .chain(path.as_os_str().encode_wide())
        .collect();
    let name = UnicodeString {
        length: (name.len() * 2) as u16,
        maximum_length: (name.len() * 2) as u16,
        buffer: name.as_ptr(),
    };
    let attrs = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root: std::ptr::null_mut(),
        name: &name,
        attributes: 0,
        security: std::ptr::null_mut(),
        quality: std::ptr::null_mut(),
    };
    let mut io = IoStatus::default();
    let mut handle = std::ptr::null_mut();
    let status = unsafe { NtOpenFile(&mut handle, 0x001200a9, &attrs, &mut io, 7, 0x21) };
    if status >= 0 {
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let file = std::fs::File::from(handle);
        assert!(file.metadata().unwrap().is_dir());
    }
    status
}

pub(super) fn query_for_file(path: &Path, create: bool) -> serde_json::Value {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    // Derive the selector from an authorized object, not a newly granted path.
    let file = std::fs::File::open(path).unwrap();
    let mut native = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            native.as_mut_ptr(),
            native.len() as u32,
            VOLUME_NAME_NT,
        )
    } as usize;
    assert!(length > 0 && length < native.len());
    let native = String::from_utf16(&native[..length]).unwrap();
    let suffix = native.strip_prefix(r"\Device\").unwrap();
    let device = &native[..8 + suffix.find('\\').unwrap()];
    let device: Vec<u8> = device.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut input = vec![0u8; 24]; // MOUNTMGR_MOUNT_POINT with only DeviceName populated.
    input[16..20].copy_from_slice(&24u32.to_le_bytes());
    input[20..22].copy_from_slice(&u16::try_from(device.len()).unwrap().to_le_bytes());
    input.extend_from_slice(&device);
    let name: Vec<u16> = r"\??\MountPointManager".encode_utf16().collect();
    let name = UnicodeString {
        length: (name.len() * 2) as u16,
        maximum_length: (name.len() * 2) as u16,
        buffer: name.as_ptr(),
    };
    let attrs = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root: std::ptr::null_mut(),
        name: &name,
        attributes: 0,
        security: std::ptr::null_mut(),
        quality: std::ptr::null_mut(),
    };
    let mut io = IoStatus::default();
    let mut handle = std::ptr::null_mut();
    let handle_count = || {
        let mut count = 0;
        assert_ne!(
            unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
            0
        );
        count
    };
    let before_handles = handle_count();
    // SYNCHRONIZE, read/write/delete sharing, synchronous non-directory open.
    let open = unsafe {
        if create {
            // The pinned Bun/Zig call uses FILE_ATTRIBUTE_NORMAL with FILE_OPEN.
            NtCreateFile(
                &mut handle,
                0x100000,
                &attrs,
                &mut io,
                std::ptr::null(),
                0x80,
                7,
                1,
                0x60,
                std::ptr::null(),
                0,
            )
        } else {
            NtOpenFile(&mut handle, 0x100000, &attrs, &mut io, 7, 0x60)
        }
    };
    if open < 0 {
        return json!({"open": open, "query": null, "matched": false});
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut output = vec![0u8; 4096];
    let issue_query = |handle, io: &mut IoStatus, output: &mut [u8]| unsafe {
        NtDeviceIoControlFile(
            handle,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            io,
            0x006d0008,
            input.as_ptr().cast(),
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
        )
    };
    let query = issue_query(handle.as_raw_handle(), &mut io, &mut output);
    let mut links = Vec::new();
    if query == 0 {
        assert_eq!(io.status as i32, 0);
        assert!((8..=output.len()).contains(&io.information));
        output.truncate(io.information);
        let read_u32 =
            |offset| u32::from_le_bytes(output[offset..offset + 4].try_into().unwrap()) as usize;
        assert_eq!(read_u32(0), output.len());
        for i in 0..read_u32(4) {
            let start = 8 + i * 24;
            let offset = read_u32(start);
            let size = u16::from_le_bytes(output[start + 4..start + 6].try_into().unwrap());
            let bytes = &output[offset..offset + usize::from(size)];
            assert_eq!(bytes.len() % 2, 0);
            let text: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes(pair.try_into().unwrap()))
                .collect();
            links.push(String::from_utf16(&text).unwrap());
        }
    }
    let drive = &path.to_str().unwrap()[..2];
    let matched = links.iter().any(|link| {
        link.eq_ignore_ascii_case(&format!(r"\DosDevices\{drive}"))
            || link.eq_ignore_ascii_case(&format!(r"\??\{drive}"))
    });
    output.resize(4096, 0);
    // Exercise the actual NtDuplicateObject/NtClose detours, not Bridge helpers.
    let duplicate = handle.try_clone().unwrap();
    let duplicated_query = issue_query(duplicate.as_raw_handle(), &mut io, &mut output);
    let original_after_duplicate = issue_query(handle.as_raw_handle(), &mut io, &mut output);
    drop(duplicate);
    drop(handle);
    // A missed NtClose hook leaves a private identity behind even when handle
    // numbers are recycled. Repeated opens must return to the original count.
    for _ in 0..64 {
        let mut current = std::ptr::null_mut();
        assert_eq!(
            unsafe { NtOpenFile(&mut current, 0x100000, &attrs, &mut io, 7, 0x60) },
            0
        );
        let current = unsafe { OwnedHandle::from_raw_handle(current) };
        assert_eq!(
            issue_query(current.as_raw_handle(), &mut io, &mut output),
            0
        );
    }
    let handles_balanced = handle_count() == before_handles;
    json!({"open": open, "query": query, "matched": matched, "links": links,
        "duplicated_query": duplicated_query, "original_after_duplicate": original_after_duplicate,
        "handles_balanced": handles_balanced})
}
