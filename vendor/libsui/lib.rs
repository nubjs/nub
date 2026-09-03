/*
 * "Sui: embed and extract auxillary data from binaries"
 *
 * Copyright (c) 2024 Divy Srivastava
 * Copyright (c) 2024 the Deno authors
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */

//! Sui is a library for embedding and extracting auxillary data from binary files.
//!
//! # Examples
//!
//! Embedding data in a Mach-O binary:
//! ```rust
//! use libsui::Macho;
//! use std::fs::File;
//!
//! # fn main() -> Result<(), libsui::Error> {
//! let data = b"Hello, world!";
//! let exe = std::fs::read("tests/exec_mach64")?;
//! let mut out = File::create("tests/out")?;
//!
//! Macho::from(exe)?
//!     .write_section("__SECTION", data.to_vec())?
//!     .build(&mut out)?;
//! #     Ok(())
//! # }
//! ```
//!
//! # Section name limits
//!
//! Mach-O stores section names in a fixed 16-byte field, so names passed to
//! [`Macho::write_section`] must be at most 16 bytes long; longer names
//! return [`Error::InvalidObject`]. PE resource names ([`PortableExecutable::write_resource`])
//! and ELF note section names ([`Elf::append`]) have no comparable 16-byte
//! limit.
//!
//! # Packers (UPX, etc.)
//!
//! Runtime packers compress the original program and decompress it back into
//! memory at startup, replacing the on-disk section layout with the packer's
//! own envelope. Embedded sections, notes, and (in many cases) resources are
//! hidden inside the packed payload, so [`find_section`] will not see data
//! that was injected *before* packing.
//!
//! There is no fully packer-proof embedding scheme that works the way
//! NativeAOT does on .NET (NativeAOT owns both the producer and the runtime
//! extractor, so it can decompress its own payload after the stub restores
//! memory). The recommended workaround with libsui is to **pack first, then
//! inject** — libsui's writers operate on the final on-disk layout, so a
//! resource/note/segment added after packing is not touched by the
//! unpacker stub at startup. See the project README for per-format details.

use core::mem::size_of;
use pe_edit::{
    IconDirectory, IconDirectoryEntry, ResourceData, ResourceEntry, ResourceEntryName,
    ResourceTable, CODE_PAGE_ID_EN_US, RT_GROUP_ICON, RT_ICON, RT_RCDATA, RT_VERSION,
};

// libsui: the resource-directory language for the version resource. US English
// rather than neutral, matching the `040904B0` StringTable the encoder emits;
// Windows' resource loader falls back to whatever language is present, so this
// is found under any UI locale.
const LANG_EN_US: u32 = 0x0409;
use image::{imageops::FilterType::Lanczos3, ImageFormat, ImageReader};
use std::io::Cursor;
use std::io::Write;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

pub mod apple_codesign;
pub mod intel_mac;

// libsui's own minimal PE resource writer, reduced from the BSD-2-Clause
// `editpe` crate. See pe_edit.rs and LICENSE-editpe.
mod pe_edit;

#[cfg(all(unix, not(target_vendor = "apple")))]
pub use elf::find_section;
#[cfg(all(unix, not(target_vendor = "apple")))]
pub use elf::find_section_in_current_image;
#[cfg(target_vendor = "apple")]
pub use macho::find_section;
#[cfg(target_vendor = "apple")]
pub use macho::find_section_in_current_image;
#[cfg(windows)]
pub use pe::find_section;
#[cfg(windows)]
pub use pe::find_section_in_current_image;

#[derive(Debug)]
pub enum Error {
    InvalidObject(&'static str),
    ImageError(image::ImageError),
    InternalError,
    IoError(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::InvalidObject(msg) => write!(f, "Invalid object: {}", msg),
            Error::InternalError => write!(f, "Internal error"),
            Error::ImageError(err) => write!(f, "Image error: {}", err),
            Error::IoError(err) => write!(f, "I/O error: {}", err),
        }
    }
}

impl From<image::ImageError> for Error {
    fn from(err: image::ImageError) -> Self {
        Error::ImageError(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::IoError(err)
    }
}

impl std::error::Error for Error {}

/// A portable executable (PE)
///
/// Build a new PE from existing PE and write auxillary data as
/// a resource in the .rsrc section.
pub struct PortableExecutable<'a> {
    image: pe_edit::Image<'a>,
    resource_dir: pe_edit::ResourceDirectory,
    icons: Vec<IconDirectoryEntry>,
    // libsui: deferred to build() rather than inserted on call, because the root
    // table is emitted in insertion order and RT_VERSION must sort after
    // RT_GROUP_ICON. See set_version_info.
    version_info: Option<Vec<u8>>,
}

impl<'a> PortableExecutable<'a> {
    /// Parse from a PE file
    pub fn from(data: &'a [u8]) -> Result<Self, Error> {
        Ok(Self {
            image: pe_edit::Image::parse(data)
                .map_err(|_| Error::InvalidObject("Failed to parse PE"))?,
            resource_dir: pe_edit::ResourceDirectory::default(),
            icons: Vec::new(),
            version_info: None,
        })
    }

    /// Write a resource to the PE file
    pub fn write_resource(mut self, name: &str, sectdata: Vec<u8>) -> Result<Self, Error> {
        let root = self.resource_dir.root_mut();
        if root.get(ResourceEntryName::ID(RT_RCDATA as u32)).is_none() {
            root.insert(
                ResourceEntryName::ID(RT_RCDATA as u32),
                ResourceEntry::Table(ResourceTable::default()),
            );
        }
        let rc_table = match root
            .get_mut(ResourceEntryName::ID(RT_RCDATA as u32))
            .unwrap()
        {
            ResourceEntry::Table(table) => table,
            ResourceEntry::Data(_) => {
                return Err(Error::InvalidObject("RCDATA is not a table"));
            }
        };
        let name = name.to_uppercase();
        rc_table.insert(
            pe_edit::ResourceEntryName::from_string(name.clone()),
            ResourceEntry::Table(ResourceTable::default()),
        );

        let rc_table = match rc_table
            .get_mut(pe_edit::ResourceEntryName::from_string(name))
            .unwrap()
        {
            ResourceEntry::Table(table) => table,
            ResourceEntry::Data(_) => {
                return Err(Error::InvalidObject("Resource entry is not a table"));
            }
        };
        let mut entry = pe_edit::ResourceData::default();
        entry.set_data(sectdata);

        rc_table.insert(ResourceEntryName::ID(0), ResourceEntry::Data(entry));

        Ok(self)
    }

    /// Set the icon of the executable.
    pub fn set_icon<T: AsRef<[u8]>>(mut self, icon: T) -> Result<Self, Error> {
        let root = self.resource_dir.root_mut();
        let icon = icon.as_ref();
        let icon = ImageReader::new(Cursor::new(icon))
            .with_guessed_format()?
            .decode()?;

        // find the main icon table
        if root.get(ResourceEntryName::ID(RT_ICON as u32)).is_none() {
            root.insert(
                ResourceEntryName::ID(RT_ICON as u32),
                ResourceEntry::Table(ResourceTable::default()),
            );
        }
        let icon_table = match root.get_mut(ResourceEntryName::ID(RT_ICON as u32)).unwrap() {
            ResourceEntry::Table(table) => table,
            ResourceEntry::Data(_) => {
                return Err(Error::InvalidObject("icon table is not a table"));
            }
        };

        // find the first free icon id
        let first_free_icon_id = icon_table
            .entries()
            .into_iter()
            .filter_map(|k| match k {
                ResourceEntryName::ID(id) => Some(id),
                _ => None,
            })
            .max()
            .unwrap_or(&0)
            + 1;

        // add the icon to the icon table
        let mut icon_directory_entries = Vec::new();
        let resolutions = [256, 128, 48, 32, 24, 16];
        for (i, &size) in resolutions.iter().enumerate() {
            let id = first_free_icon_id + i as u32;
            let mut inner_table = ResourceTable::default();
            let data = {
                let mut data = Vec::new();
                icon.resize_exact(size, size, Lanczos3)
                    .to_rgba8()
                    .write_to(&mut Cursor::new(&mut data), ImageFormat::Ico)?;

                let mut entry = IconDirectoryEntry::read_from_prefix(&data[6..20]).unwrap();
                entry.id = id as u16;
                icon_directory_entries.push(entry);
                data[22..].to_owned()
            };

            let mut resource_data = ResourceData::default();
            resource_data.set_codepage(CODE_PAGE_ID_EN_US as u32);
            resource_data.set_data(data);

            inner_table.insert(ResourceEntryName::ID(0), ResourceEntry::Data(resource_data));
            icon_table.insert(ResourceEntryName::ID(id), ResourceEntry::Table(inner_table));
        }
        self.icons = icon_directory_entries;

        Ok(self)
    }

    /// Set the `VS_VERSIONINFO` resource — the fields Explorer's Details tab and
    /// `GetFileVersionInfo` read.
    ///
    /// libsui: `resource` is the already-encoded `RT_VERSION` payload; this
    /// method only places it in the tree, because the encoding is a
    /// self-contained binary format with its own alignment rules and belongs
    /// with the tests that exercise it.
    ///
    /// The insertion is DEFERRED to [`Self::build`]. Entries are emitted in the
    /// order they are inserted and the PE resource directory requires ID entries
    /// in ascending order — `FindResource` binary-searches them — so `RT_VERSION`
    /// (16) has to follow the `RT_GROUP_ICON` (14) entry that `build` adds last.
    /// Inserting here would place 16 before 14 and lose both resources.
    pub fn set_version_info(mut self, resource: Vec<u8>) -> Self {
        self.version_info = Some(resource);
        self
    }

    /// Build and write the modified PE file
    pub fn build<W: std::io::Write>(mut self, writer: &mut W) -> Result<(), Error> {
        // TODO: the order of the table entries matters. this works for now.
        if !self.icons.is_empty() {
            let root = self.resource_dir.root_mut();

            // find the group icon table
            if root
                .get(ResourceEntryName::ID(RT_GROUP_ICON as u32))
                .is_none()
            {
                root.insert(
                    ResourceEntryName::ID(RT_GROUP_ICON as u32),
                    ResourceEntry::Table(ResourceTable::default()),
                );
            }
            let group_table = match root
                .get_mut(ResourceEntryName::ID(RT_GROUP_ICON as u32))
                .unwrap()
            {
                ResourceEntry::Table(table) => table,
                ResourceEntry::Data(_) => {
                    return Err(Error::InvalidObject("group icon table is not a table"));
                }
            };

            let data = {
                let mut data = Vec::new();
                let icon_directory = IconDirectory {
                    reserved: 0,
                    type_: 1,
                    count: self.icons.len() as u16,
                };
                data.extend(icon_directory.as_bytes());
                for entry in self.icons {
                    data.extend(&entry.as_bytes()[..14]);
                }
                data
            };
            let mut resource_data = ResourceData::default();
            resource_data.set_codepage(CODE_PAGE_ID_EN_US as u32);
            resource_data.set_data(data);
            // insert the main icon directory table
            let mut inner_table = ResourceTable::default();
            inner_table.insert(ResourceEntryName::ID(0), ResourceEntry::Data(resource_data));
            group_table.insert_at(
                ResourceEntryName::from_string("MAINICON"),
                ResourceEntry::Table(inner_table),
                0,
            );
        }

        // libsui: last, so the root table's ID entries stay ascending — RT_ICON
        // (3), RT_RCDATA (10), RT_GROUP_ICON (14), RT_VERSION (16).
        if let Some(resource) = self.version_info.take() {
            let root = self.resource_dir.root_mut();
            let mut data = ResourceData::default();
            // The version resource is UTF-16 throughout, and its language must
            // agree with the StringTable key the encoder wrote.
            data.set_codepage(CODE_PAGE_ID_EN_US as u32);
            data.set_data(resource);
            let mut language_table = ResourceTable::default();
            language_table.insert(ResourceEntryName::ID(LANG_EN_US), ResourceEntry::Data(data));
            // Name 1 is VS_VERSION_INFO — the only id GetFileVersionInfo looks up.
            let mut name_table = ResourceTable::default();
            name_table.insert(
                ResourceEntryName::ID(1),
                ResourceEntry::Table(language_table),
            );
            root.insert(
                ResourceEntryName::ID(RT_VERSION as u32),
                ResourceEntry::Table(name_table),
            );
        }

        self.image
            .set_resource_directory(self.resource_dir)
            .map_err(|_| Error::InternalError)?;

        let data = self.image.data();
        writer.write_all(data)?;
        Ok(())
    }
}

#[cfg(windows)]
mod pe {
    use std::ffi::CString;
    use windows::core::PCSTR;
    use windows::Win32::System::LibraryLoader::{
        FindResourceA, LoadResource, LockResource, SizeofResource,
    };

    // RT_RCDATA passed as a MAKEINTRESOURCE-style integer in the low word.
    const RT_RCDATA: PCSTR = PCSTR::from_raw(10 as *const u8);

    pub fn find_section(section_name: &str) -> std::io::Result<Option<&[u8]>> {
        let section_name = section_name.to_uppercase();
        let section_name = CString::new(section_name)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        // SAFETY: `section_name` is a valid C string for the duration of this
        // call. `None` for the module handle targets the current process. The
        // typed `windows` wrappers return `Result`s and strongly-typed handles
        // that we propagate without further raw-handle arithmetic.
        unsafe {
            let resource_handle = match FindResourceA(
                None,
                PCSTR::from_raw(section_name.as_ptr() as *const u8),
                RT_RCDATA,
            ) {
                Ok(h) => h,
                // Resource not found: surface as `Ok(None)`, matching the
                // semantics on other platforms.
                Err(_) => return Ok(None),
            };

            let resource_data =
                LoadResource(None, resource_handle).map_err(std::io::Error::other)?;

            let resource_size = SizeofResource(None, resource_handle);
            if resource_size == 0 {
                return Ok(Some(&[]));
            }

            let resource_ptr = LockResource(resource_data);
            if resource_ptr.is_null() {
                return Err(std::io::Error::other(
                    "LockResource returned a null pointer",
                ));
            }
            Ok(Some(std::slice::from_raw_parts(
                resource_ptr as *const u8,
                resource_size as usize,
            )))
        }
    }

    /// Like [`find_section`], but reads the resource from the module (DLL or
    /// EXE) that contains *this* code rather than the process's main
    /// executable. Used by the desktop runtime where the payload is embedded
    /// in the runtime library (denort.dll) loaded by the WEF host.
    pub fn find_section_in_current_image(
        section_name: &str,
    ) -> std::io::Result<Option<&'static [u8]>> {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::System::LibraryLoader::{
            GetModuleHandleExA, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };

        let section_name = section_name.to_uppercase();
        let section_name = CString::new(section_name)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        // SAFETY: mirrors `find_section`, but resolves the module that contains
        // this function's code (FROM_ADDRESS) so the lookup targets the current
        // image (e.g. denort.dll) instead of the process executable. The typed
        // `windows` wrappers return `Result`s and strongly-typed handles.
        unsafe {
            let mut hmodule = HMODULE::default();
            GetModuleHandleExA(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCSTR(find_section_in_current_image as *const u8),
                &mut hmodule,
            )
            .map_err(std::io::Error::other)?;

            let resource_handle = match FindResourceA(
                hmodule,
                PCSTR::from_raw(section_name.as_ptr() as *const u8),
                RT_RCDATA,
            ) {
                Ok(h) => h,
                Err(_) => return Ok(None),
            };

            let resource_data = LoadResource(hmodule, resource_handle)
                .map_err(std::io::Error::other)?;

            let resource_size = SizeofResource(hmodule, resource_handle);
            if resource_size == 0 {
                return Ok(Some(&[]));
            }

            let resource_ptr = LockResource(resource_data);
            if resource_ptr.is_null() {
                return Err(std::io::Error::other(
                    "LockResource returned a null pointer",
                ));
            }
            Ok(Some(std::slice::from_raw_parts(
                resource_ptr as *const u8,
                resource_size as usize,
            )))
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
pub(crate) struct SegmentCommand64 {
    cmd: u32,
    cmdsize: u32,
    segname: [u8; 16],
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: u32,
    initprot: u32,
    nsects: u32,
    flags: u32,
}

#[derive(FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
pub(crate) struct Header64 {
    magic: u32,
    cputype: i32,
    cpusubtype: u32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
pub(crate) struct Section64 {
    sectname: [u8; 16],
    segname: [u8; 16],
    addr: u64,
    size: u64,
    offset: u32,
    align: u32,
    reloff: u32,
    nreloc: u32,
    flags: u32,
    reserved1: u32,
    reserved2: u32,
    reserved3: u32,
}

const SEG_LINKEDIT: &[u8] = b"__LINKEDIT";

const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
pub(crate) const LC_CODE_SIGNATURE: u32 = 0x1d;
const LC_FUNCTION_STARTS: u32 = 0x26;
const LC_DATA_IN_CODE: u32 = 0x29;
const LC_DYLD_INFO: u32 = 0x22;
const LC_ATOM_INFO: u32 = 0x36;
const LC_DYLD_INFO_ONLY: u32 = 0x80000022;
const LC_DYLIB_CODE_SIGN_DRS: u32 = 0x2b;
const LC_LINKER_OPTIMIZATION_HINT: u32 = 0x2d;
const LC_DYLD_EXPORTS_TRIE: u32 = 0x80000033;
const LC_DYLD_CHAINED_FIXUPS: u32 = 0x80000034;
const LC_FUNCTION_VARIANTS: u32 = 0x37;
const LC_FUNCTION_VARIANT_FIXUPS: u32 = 0x38;

const CPU_TYPE_ARM_64: i32 = 0x0100000c;

fn align(size: u64, base: u64) -> u64 {
    let over = size % base;
    if over == 0 {
        size
    } else {
        size + (base - over)
    }
}

fn align_vmsize(size: u64, page_size: u64) -> u64 {
    align(if size > 0x4000 { size } else { 0x4000 }, page_size)
}

fn shift(value: u64, amount: u64, range_min: u64, range_max: u64) -> u64 {
    if value < range_min || value > (range_max + range_min) {
        return value;
    }
    value + amount
}

pub struct Macho {
    header: Header64,
    commands: Vec<(u32, u32, usize)>,
    linkedit_cmd: SegmentCommand64,
    rest_size: u64,
    data: Vec<u8>,
    seg: SegmentCommand64,
    sec: Section64,
    sectdata: Option<Vec<u8>>,
}

pub(crate) const SEGNAME: [u8; 16] = *b"__SUI\0\0\0\0\0\0\0\0\0\0\0";

impl Macho {
    pub fn from(obj: Vec<u8>) -> Result<Self, Error> {
        let header = Header64::read_from_prefix(&obj)
            .ok_or(Error::InvalidObject("Failed to read header"))?;

        // Atomically strip code signature first for intel binaries.
        #[cfg(target_vendor = "apple")]
        let obj = if header.cputype != CPU_TYPE_ARM_64 {
            use std::io::Write;

            let tmp_dir = std::env::temp_dir();
            std::fs::create_dir_all(&tmp_dir)?;
            let tmp_path = tmp_dir.join(format!("sui_tmp_{}", std::process::id()));

            let mut tmp_file = std::fs::File::create(&tmp_path)?;
            tmp_file.write_all(&obj)?;
            drop(tmp_file);

            match std::process::Command::new("codesign")
                .arg("--remove-signature")
                .arg(&tmp_path)
                .output()
            {
                Ok(output) => {
                    if !output.status.success() {
                        // If codesign fails, just use the original binary
                        eprintln!(
                            "Warning: Failed to remove code signature: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                        std::fs::remove_file(&tmp_path).ok();
                        obj
                    } else {
                        // Read the stripped binary
                        let stripped = std::fs::read(&tmp_path)?;
                        std::fs::remove_file(&tmp_path).ok();
                        stripped
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // codesign not found, skip it
                    std::fs::remove_file(&tmp_path).ok();
                    obj
                }
                Err(e) => {
                    std::fs::remove_file(&tmp_path).ok();
                    return Err(e.into());
                }
            }
        } else {
            obj
        };

        let mut commands: Vec<(u32, u32, usize)> = Vec::with_capacity(header.ncmds as usize);

        let mut offset = size_of::<Header64>();
        let mut linkedit_cmd = None;

        for _ in 0..header.ncmds as usize {
            let cmd = u32::from_le_bytes(
                obj[offset..offset + 4]
                    .try_into()
                    .map_err(|_| Error::InvalidObject("Failed to read command"))?,
            );
            let cmdsize = u32::from_le_bytes(
                obj[offset + 4..offset + 8]
                    .try_into()
                    .map_err(|_| Error::InvalidObject("Failed to read command size"))?,
            );

            if cmd == LC_SEGMENT_64 {
                let segcmd = SegmentCommand64::read_from_prefix(&obj[offset..])
                    .ok_or(Error::InvalidObject("Failed to read segment command"))?;
                if segcmd.segname[..SEG_LINKEDIT.len()] == *SEG_LINKEDIT {
                    linkedit_cmd = Some(segcmd);
                }
            }

            commands.push((cmd, cmdsize, offset));
            offset += cmdsize as usize;
        }

        let Some(linkedit_cmd) = linkedit_cmd else {
            return Err(Error::InvalidObject("Linkedit segment not found"));
        };
        let rest_size =
            linkedit_cmd.fileoff - size_of::<Header64>() as u64 - header.sizeofcmds as u64;
        Ok(Self {
            header,
            commands,
            linkedit_cmd,
            data: obj,
            rest_size,
            seg: SegmentCommand64::new_zeroed(),
            sec: Section64::new_zeroed(),
            sectdata: None,
        })
    }

    /// Write a section into the Mach-O file.
    ///
    /// `name` is the Mach-O section name. The Mach-O format stores section
    /// names in a fixed-size 16-byte field, so `name` must be at most
    /// **16 bytes** long. Names longer than 16 bytes return
    /// [`Error::InvalidObject`] instead of panicking.
    pub fn write_section(mut self, name: &str, sectdata: Vec<u8>) -> Result<Self, Error> {
        if name.len() > 16 {
            return Err(Error::InvalidObject(
                "Mach-O section name must be at most 16 bytes",
            ));
        }

        // One mechanism for both Darwin architectures. The x86_64 path used to
        // append the payload behind a sentinel instead, which cannot be code
        // signed and needs `codesign --remove-signature` at build time — so it
        // only ran on a macOS host. A real section has neither constraint, and
        // the segment construction below is generic Mach-O: the only
        // architecture-shaped value is the page size, and 0x10000 is a valid
        // multiple of x86_64's 0x1000.
        let page_size = 0x10000;

        self.seg = SegmentCommand64 {
            cmd: LC_SEGMENT_64,
            cmdsize: size_of::<SegmentCommand64>() as u32 + size_of::<Section64>() as u32,
            segname: SEGNAME,
            vmaddr: self.linkedit_cmd.vmaddr,
            vmsize: align_vmsize(sectdata.len() as u64, page_size),
            filesize: align_vmsize(sectdata.len() as u64, page_size),
            fileoff: self.linkedit_cmd.fileoff,
            maxprot: 0x01,
            initprot: 0x01,
            nsects: 1,
            flags: 0,
        };

        let mut sectname = [0; 16];
        sectname[..name.len()].copy_from_slice(name.as_bytes());

        self.sec = Section64 {
            addr: self.seg.vmaddr,
            size: sectdata.len() as u64,
            offset: self.linkedit_cmd.fileoff as u32,
            align: if sectdata.len() < 16 { 0 } else { 4 },
            segname: SEGNAME,
            sectname,
            ..self.sec
        };

        self.linkedit_cmd.vmaddr += self.seg.vmsize;
        let linkedit_fileoff = self.linkedit_cmd.fileoff;
        self.linkedit_cmd.fileoff += self.seg.filesize;

        macro_rules! shift_cmd {
            ($cmd:expr) => {
                $cmd = shift(
                    $cmd as _,
                    self.seg.filesize,
                    linkedit_fileoff,
                    self.linkedit_cmd.filesize,
                ) as _;
            };
        }

        for (cmd, _, offset) in self.commands.iter_mut() {
            match *cmd {
                LC_SYMTAB => {
                    #[derive(FromBytes, FromZeroes, AsBytes)]
                    #[repr(C)]
                    pub struct SymtabCommand {
                        pub cmd: u32,
                        pub cmdsize: u32,
                        pub symoff: u32,
                        pub nsyms: u32,
                        pub stroff: u32,
                        pub strsize: u32,
                    }

                    let cmd = SymtabCommand::mut_from_prefix(&mut self.data[*offset..])
                        .ok_or(Error::InvalidObject("Failed to read symtab command"))?;
                    shift_cmd!(cmd.symoff);
                    shift_cmd!(cmd.stroff);
                }
                LC_DYSYMTAB => {
                    #[derive(FromBytes, FromZeroes, AsBytes)]
                    #[repr(C)]
                    pub struct DysymtabCommand {
                        pub cmd: u32,
                        pub cmdsize: u32,
                        pub ilocalsym: u32,
                        pub nlocalsym: u32,
                        pub iextdefsym: u32,
                        pub nextdefsym: u32,
                        pub iundefsym: u32,
                        pub nundefsym: u32,
                        pub tocoff: u32,
                        pub ntoc: u32,
                        pub modtaboff: u32,
                        pub nmodtab: u32,
                        pub extrefsymoff: u32,
                        pub nextrefsyms: u32,
                        pub indirectsymoff: u32,
                        pub nindirectsyms: u32,
                        pub extreloff: u32,
                        pub nextrel: u32,
                        pub locreloff: u32,
                        pub nlocrel: u32,
                    }

                    let cmd = DysymtabCommand::mut_from_prefix(&mut self.data[*offset..])
                        .ok_or(Error::InvalidObject("Failed to read dysymtab command"))?;
                    shift_cmd!(cmd.tocoff);
                    shift_cmd!(cmd.modtaboff);
                    shift_cmd!(cmd.extrefsymoff);
                    shift_cmd!(cmd.indirectsymoff);
                    shift_cmd!(cmd.extreloff);
                    shift_cmd!(cmd.locreloff);
                }

                LC_CODE_SIGNATURE
                | LC_FUNCTION_STARTS
                | LC_DATA_IN_CODE
                | LC_DYLIB_CODE_SIGN_DRS
                | LC_ATOM_INFO
                | LC_LINKER_OPTIMIZATION_HINT
                | LC_DYLD_EXPORTS_TRIE
                | LC_DYLD_CHAINED_FIXUPS
                | LC_FUNCTION_VARIANTS
                | LC_FUNCTION_VARIANT_FIXUPS => {
                    #[derive(FromBytes, FromZeroes, AsBytes)]
                    #[repr(C)]
                    struct LinkeditDataCommand {
                        cmd: u32,
                        cmdsize: u32,
                        dataoff: u32,
                        datasize: u32,
                    }
                    let cmd = LinkeditDataCommand::mut_from_prefix(&mut self.data[*offset..])
                        .ok_or(Error::InvalidObject("Failed to read linkedit data command"))?;
                    shift_cmd!(cmd.dataoff);
                }

                LC_DYLD_INFO | LC_DYLD_INFO_ONLY => {
                    #[derive(FromBytes, FromZeroes, AsBytes)]
                    #[repr(C)]
                    pub struct DyldInfoCommand {
                        pub cmd: u32,
                        pub cmdsize: u32,
                        pub rebase_off: u32,
                        pub rebase_size: u32,
                        pub bind_off: u32,
                        pub bind_size: u32,
                        pub weak_bind_off: u32,
                        pub weak_bind_size: u32,
                        pub lazy_bind_off: u32,
                        pub lazy_bind_size: u32,
                        pub export_off: u32,
                        pub export_size: u32,
                    }
                    let dyld_info = DyldInfoCommand::mut_from_prefix(&mut self.data[*offset..])
                        .ok_or(Error::InvalidObject("Failed to read dyld info command"))?;
                    shift_cmd!(dyld_info.rebase_off);
                    shift_cmd!(dyld_info.bind_off);
                    shift_cmd!(dyld_info.weak_bind_off);
                    shift_cmd!(dyld_info.lazy_bind_off);
                    shift_cmd!(dyld_info.export_off);
                }

                _ => {}
            }
        }

        self.header.ncmds += 1;
        self.header.sizeofcmds += self.seg.cmdsize;

        self.sectdata = Some(sectdata);
        Ok(self)
    }

    /// Build and write the modified Mach-O file
    pub fn build<W: Write>(mut self, writer: &mut W) -> Result<(), Error> {
        // Retained for images that already carry an appended payload; nothing
        // this crate writes takes this path any more. See `write_section`.
        if false {
            let mut data = self.data;

            if let Some(sectdata) = self.sectdata {
                // Construct sentinel from reversed string to prevent it from existing as contiguous
                // bytes in the binary. Use black_box to prevent compiler from const-evaluating.
                let mut sentinel = Vec::with_capacity(16);
                let reversed = std::hint::black_box(b">~atad-ius~<"); // "<~sui-data~>" reversed
                for &byte in reversed.iter().rev() {
                    sentinel.push(byte);
                }
                // Add magic bytes in reverse order with black_box
                let magic = std::hint::black_box([0xEF, 0xBE, 0xAD, 0xDE]);
                sentinel.extend_from_slice(&magic);
                data.extend_from_slice(&sentinel);
                data.extend_from_slice(&(sectdata.len() as u64).to_le_bytes());
                data.extend_from_slice(&sectdata);
            }

            intel_mac::patch_macho_executable(&mut data);
            writer.write_all(&data)?;
            return Ok(());
        };

        writer.write_all(self.header.as_bytes())?;

        for (cmd, cmdsize, offset) in self.commands.iter_mut() {
            if *cmd == LC_SEGMENT_64 {
                let segcmd = SegmentCommand64::read_from_prefix(&self.data[*offset..])
                    .ok_or(Error::InvalidObject("Failed to read segment command"))?;
                if segcmd.segname[..SEG_LINKEDIT.len()] == *SEG_LINKEDIT {
                    writer.write_all(self.seg.as_bytes())?;
                    writer.write_all(self.sec.as_bytes())?;
                    writer.write_all(self.linkedit_cmd.as_bytes())?;
                    continue;
                }
            }
            writer.write_all(&self.data[*offset..*offset + *cmdsize as usize])?;
        }

        let mut off = self.header.sizeofcmds as usize + size_of::<Header64>();

        let len = self.rest_size as usize - self.seg.cmdsize as usize;
        writer.write_all(&self.data[off..off + len])?;

        off += len;

        if let Some(sectdata) = self.sectdata {
            writer.write_all(&sectdata)?;
            if self.seg.filesize > sectdata.len() as u64 {
                let padding = vec![0; (self.seg.filesize - sectdata.len() as u64) as usize];
                writer.write_all(&padding)?;
            }
        }

        writer.write_all(&self.data[off..off + self.linkedit_cmd.filesize as usize])?;

        Ok(())
    }

    pub fn build_and_sign<W: Write>(self, mut writer: W) -> Result<(), Error> {
        if self.header.cputype == CPU_TYPE_ARM_64 {
            let mut data = Vec::new();
            self.build(&mut data)?;
            let codesign = apple_codesign::MachoSigner::new(data)?;
            codesign.sign(writer)
        } else {
            // For Intel binaries, build to a temporary file and run adhoc codesign
            #[cfg(target_vendor = "apple")]
            {
                let tmp_dir = std::env::temp_dir();
                std::fs::create_dir_all(&tmp_dir)?;
                let tmp_path = tmp_dir.join(format!("sui_sign_{}", std::process::id()));

                {
                    let mut tmp_file = std::fs::File::create(&tmp_path)?;
                    self.build(&mut tmp_file)?;
                }

                // Run adhoc codesign
                match std::process::Command::new("codesign")
                    .arg("-s")
                    .arg("-")
                    .arg(&tmp_path)
                    .output()
                {
                    Ok(output) => {
                        if !output.status.success() {
                            eprintln!(
                                "Warning: Failed to adhoc codesign binary: {}",
                                String::from_utf8_lossy(&output.stderr)
                            );
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // codesign not found, skip it
                    }
                    Err(e) => {
                        std::fs::remove_file(&tmp_path).ok();
                        return Err(e.into());
                    }
                }

                // Read the (possibly signed) binary and write to output
                let signed_data = std::fs::read(&tmp_path)?;
                writer.write_all(&signed_data)?;
                std::fs::remove_file(&tmp_path).ok();

                Ok(())
            }

            #[cfg(not(target_vendor = "apple"))]
            {
                // On non-macOS, just build directly without codesign
                self.build(&mut writer)?;
                Ok(())
            }
        }
    }
}

#[cfg(target_vendor = "apple")]
mod macho {
    pub fn find_section(_section_name: &str) -> std::io::Result<Option<&[u8]>> {
        // `getsectdata` is standard macOS libc on both architectures, so one
        // reader answers for both. x86_64 used to search backwards for the
        // appended sentinel instead.
        {
            use super::SEGNAME;
            use std::ffi::CString;
            use std::os::raw::c_char;

            extern "C" {
                pub fn getsectdata(
                    segname: *const c_char,
                    sectname: *const c_char,
                    size: *mut usize,
                ) -> *mut c_char;

                pub fn _dyld_get_image_vmaddr_slide(image_index: usize) -> usize;
            }

            let mut section_size: usize = 0;
            let section_name = CString::new(_section_name)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

            unsafe {
                let mut ptr = getsectdata(
                    SEGNAME.as_ptr() as *const c_char,
                    section_name.as_ptr() as *const c_char,
                    &mut section_size as *mut usize,
                );

                if ptr.is_null() {
                    return Ok(None);
                }

                // Add the "virtual memory address slide" amount to ensure a valid pointer
                // in cases where the virtual memory address have been adjusted by the OS.
                ptr = ptr.wrapping_add(_dyld_get_image_vmaddr_slide(0));
                Ok(Some(std::slice::from_raw_parts(
                    ptr as *const u8,
                    section_size,
                )))
            }
        }
    }

    /// Find a section in the currently loaded dylib (the image containing this code).
    ///
    /// Unlike `find_section` which searches the main executable (image index 0),
    /// this function uses `dladdr` to find which Mach-O image the calling code
    /// belongs to, then searches for the section in that image using
    /// `getsectiondata`.
    pub fn find_section_in_current_image(
        _section_name: &str,
    ) -> std::io::Result<Option<&'static [u8]>> {
        #[cfg(target_arch = "x86_64")]
        {
            // Intel Mach-O embedding does not write a real named section
            // (see `Macho::build` for x86_64): the payload is appended past
            // __LINKEDIT and located at runtime by scanning for the
            // `<~sui-data~>` sentinel. `getsectiondata` therefore cannot
            // find it. Resolve the file backing the current image with
            // `dladdr` and run the sentinel scan against that file instead.
            use std::ffi::CStr;
            use std::os::raw::c_char;
            use std::path::PathBuf;

            #[repr(C)]
            #[allow(non_camel_case_types)]
            struct Dl_info {
                dli_fname: *const c_char,
                dli_fbase: *mut std::ffi::c_void,
                dli_sname: *const c_char,
                dli_saddr: *mut std::ffi::c_void,
            }

            extern "C" {
                fn dladdr(addr: *const std::ffi::c_void, info: *mut Dl_info) -> std::ffi::c_int;
            }

            let mut info: Dl_info = unsafe { std::mem::zeroed() };
            let self_addr = find_section_in_current_image as *const std::ffi::c_void;
            let ret = unsafe { dladdr(self_addr, &mut info) };
            if ret == 0 || info.dli_fname.is_null() {
                return Ok(None);
            }

            // SAFETY: `dladdr` succeeded with a non-null `dli_fname`, which
            // points at a NUL-terminated path owned by dyld for the lifetime
            // of the loaded image. We copy it out immediately.
            let fname = unsafe { CStr::from_ptr(info.dli_fname) };
            let Ok(fname) = fname.to_str() else {
                return Ok(None);
            };
            let path = PathBuf::from(fname);

            super::intel_mac::find_section_in_file(&path)
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            use super::SEGNAME;
            use std::ffi::CString;
            use std::os::raw::c_char;

            // Use the Mach-O header-aware getsectiondata instead of getsectdata.
            // getsectiondata takes a mach_header_64 pointer and returns the section
            // data for that specific image.
            #[repr(C)]
            #[allow(non_camel_case_types)]
            struct mach_header_64 {
                _data: [u8; 32],
            }

            #[repr(C)]
            #[allow(non_camel_case_types)]
            struct Dl_info {
                dli_fname: *const c_char,
                dli_fbase: *mut std::ffi::c_void,
                dli_sname: *const c_char,
                dli_saddr: *mut std::ffi::c_void,
            }

            extern "C" {
                fn getsectiondata(
                    mhp: *const mach_header_64,
                    segname: *const c_char,
                    sectname: *const c_char,
                    size: *mut usize,
                ) -> *mut u8;

                fn dladdr(addr: *const std::ffi::c_void, info: *mut Dl_info) -> std::ffi::c_int;
            }

            // Use dladdr on a function in this library to find our image's base address.
            // The base address (dli_fbase) is the mach_header_64 pointer for our image.
            let mut info: Dl_info = unsafe { std::mem::zeroed() };
            let self_addr = find_section_in_current_image as *const std::ffi::c_void;
            let ret = unsafe { dladdr(self_addr, &mut info) };
            if ret == 0 || info.dli_fbase.is_null() {
                return Ok(None);
            }

            let mh = info.dli_fbase as *const mach_header_64;
            let section_name = CString::new(_section_name)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

            let mut section_size: usize = 0;
            let ptr = unsafe {
                getsectiondata(
                    mh,
                    SEGNAME.as_ptr() as *const c_char,
                    section_name.as_ptr() as *const c_char,
                    &mut section_size,
                )
            };

            if ptr.is_null() || section_size == 0 {
                return Ok(None);
            }

            // SAFETY: The dylib is loaded for the lifetime of the process (or until
            // dlclose), and we expect the caller to keep the dylib loaded. The
            // section data is mapped by the kernel, so it's safe to return 'static.
            let data: &'static [u8] = unsafe { std::slice::from_raw_parts(ptr, section_size) };
            Ok(Some(data))
        }
    }
}

pub struct Elf<'a> {
    data: &'a [u8],
}

const ELF_NOTE_NAME: &[u8] = b"SUI\0";
const ELF_NOTE_TYPE_SECTION_DATA: u32 = 0x5355_4901;

fn align_up(value: usize, align: usize) -> usize {
    if align <= 1 {
        value
    } else {
        (value + (align - 1)) & !(align - 1)
    }
}

// ELF note payload
fn build_elf_note_payload(section_name: &str, section_data: &[u8]) -> Vec<u8> {
    let name_len = section_name.len();
    let name_len_u16 = u16::try_from(name_len).expect("section name too long");

    let mut desc = Vec::with_capacity(2 + name_len + section_data.len());
    desc.extend_from_slice(&name_len_u16.to_le_bytes());
    desc.extend_from_slice(section_name.as_bytes());
    desc.extend_from_slice(section_data);

    let mut note =
        Vec::with_capacity(12 + align_up(ELF_NOTE_NAME.len(), 4) + align_up(desc.len(), 4));
    note.extend_from_slice(&(ELF_NOTE_NAME.len() as u32).to_le_bytes());
    note.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    note.extend_from_slice(&ELF_NOTE_TYPE_SECTION_DATA.to_le_bytes());
    note.extend_from_slice(ELF_NOTE_NAME);
    note.resize(align_up(note.len(), 4), 0);
    note.extend_from_slice(&desc);
    note.resize(align_up(note.len(), 4), 0);
    note
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn parse_elf_note_desc<'a>(desc: &'a [u8], name: &str) -> Option<&'a [u8]> {
    if desc.len() < 2 {
        return None;
    }
    let name_len = u16::from_le_bytes(desc[0..2].try_into().ok()?) as usize;
    if desc.len() < 2 + name_len {
        return None;
    }
    if desc.get(2..2 + name_len)? != name.as_bytes() {
        return None;
    }
    Some(&desc[2 + name_len..])
}

impl<'a> Elf<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    /// Append `sectdata` to the ELF as a `.note.sui` note, leaving every
    /// original byte of the file untouched.
    ///
    /// The previous implementation round-tripped the whole binary through
    /// `object::build::elf::Builder`, which rebuilds the file purely from its
    /// section table. That is lossy for modern relocatable executables:
    /// `object` (0.36) has no `SHT_RELR` support, and any segment bytes not
    /// covered by a surviving section (e.g. `.relr.dyn` relative relocations
    /// in a binary whose section headers have been stripped) are silently
    /// dropped — leaving startup relocations un-applied and the program
    /// deadlocking in C++ static-init guards.
    ///
    /// Instead we keep the original image verbatim and graft the note on the
    /// end, in the style of `patchelf --add-note`:
    ///   1. Append the note payload and a fresh, enlarged copy of the program
    ///      header table at a page-aligned offset past EOF.
    ///   2. Add a `PT_LOAD` that maps that appended region, and a `PT_NOTE`
    ///      that points at the note within it, so `find_section`'s
    ///      `dl_iterate_phdr` scan can see it at runtime.
    ///   3. Repoint `e_phoff`/`e_phnum` (and `PT_PHDR`, if present) at the new
    ///      table. Every original byte is preserved, so relocations,
    ///      `.relr.dyn`, and everything else survive unchanged.
    ///
    /// The section header TABLE is extended, though, whenever the input still
    /// has a usable one: `.sui.phdrs` and `.note.sui` are appended and
    /// `.shstrtab` is copied and relocated to carry their names. That is not
    /// bookkeeping — both sections exist so a later `strip` cannot discard what
    /// it cannot see, and the comments on each name the stripper it defends
    /// against. A fully stripped input has no table to extend and keeps the
    /// note through `PT_NOTE` alone.
    pub fn append<W: Write>(
        &self,
        name: &str,
        sectdata: &[u8],
        writer: &mut W,
    ) -> Result<(), Error> {
        const PAGE: usize = 0x1000;
        // Program/segment header type and flag constants (ELF64).
        const PT_LOAD: u32 = 1;
        const PT_NOTE: u32 = 4;
        const PT_PHDR: u32 = 6;
        const PF_R: u32 = 4;
        const PHENTSIZE64: usize = 56;

        let data = self.data;
        if data.len() < 64 || data[0..4] != *b"\x7fELF" {
            return Err(Error::InvalidObject("Not an ELF file"));
        }
        if data[4] != 2 {
            return Err(Error::InvalidObject("Only 64-bit ELF is supported"));
        }
        let le = match data[5] {
            1 => true,
            2 => false,
            _ => return Err(Error::InvalidObject("Invalid ELF data encoding")),
        };

        // Endian-aware scalar readers/writers for the header fields we touch.
        let r16 = |b: &[u8]| -> u16 {
            let a = [b[0], b[1]];
            if le {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            }
        };
        let r32 = |b: &[u8]| -> u32 {
            let a = [b[0], b[1], b[2], b[3]];
            if le {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }
        };
        let r64 = |b: &[u8]| -> u64 {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[..8]);
            if le {
                u64::from_le_bytes(a)
            } else {
                u64::from_be_bytes(a)
            }
        };
        let w16 = |b: &mut [u8], v: u16| {
            let a = if le { v.to_le_bytes() } else { v.to_be_bytes() };
            b[..2].copy_from_slice(&a);
        };
        let w32 = |b: &mut [u8], v: u32| {
            let a = if le { v.to_le_bytes() } else { v.to_be_bytes() };
            b[..4].copy_from_slice(&a);
        };
        let w64 = |b: &mut [u8], v: u64| {
            let a = if le { v.to_le_bytes() } else { v.to_be_bytes() };
            b[..8].copy_from_slice(&a);
        };

        let e_phoff = r64(&data[0x20..0x28]) as usize;
        let e_phentsize = r16(&data[0x36..0x38]) as usize;
        let e_phnum = r16(&data[0x38..0x3a]) as usize;

        if e_phentsize < PHENTSIZE64 {
            return Err(Error::InvalidObject("Unexpected program header size"));
        }
        if e_phnum == 0
            || e_phoff == 0
            || e_phoff
                .checked_add(e_phnum * e_phentsize)
                .map(|end| end > data.len())
                .unwrap_or(true)
        {
            return Err(Error::InvalidObject("Invalid program header table"));
        }

        // Scan existing program headers: find the highest mapped virtual
        // address (so the note lands above everything), the PT_PHDR entry, and
        // the first PT_LOAD's `p_vaddr - p_offset` bias.
        let mut max_vaddr_end = 0u64;
        let mut pt_phdr_index = None;
        let mut first_load_bias: Option<i64> = None;
        for i in 0..e_phnum {
            let off = e_phoff + i * e_phentsize;
            let p = &data[off..off + PHENTSIZE64];
            match r32(&p[0..4]) {
                PT_LOAD => {
                    let p_offset = r64(&p[8..16]);
                    let p_vaddr = r64(&p[16..24]);
                    let end = p_vaddr.saturating_add(r64(&p[40..48]));
                    max_vaddr_end = max_vaddr_end.max(end);
                    // The kernel derives `AT_PHDR` from the first PT_LOAD's bias
                    // (see below); capture it in program-header order.
                    if first_load_bias.is_none() {
                        first_load_bias = Some(p_vaddr as i64 - p_offset as i64);
                    }
                }
                PT_PHDR => pt_phdr_index = Some(i),
                _ => {}
            }
        }
        if max_vaddr_end == 0 {
            return Err(Error::InvalidObject("No PT_LOAD segments"));
        }
        let first_load_bias = first_load_bias.unwrap_or(0);

        let note = build_elf_note_payload(name, sectdata);

        // Layout of the appended region, all within one new PT_LOAD:
        //   [page-aligned] new program header table | note
        //
        // The relocated program header table must stay reachable through
        // `AT_PHDR`. Loaders that recompute it from the file's segment table
        // (the kernel, ld.so via an explicit interpreter) cope with any
        // placement, but a loader that trusts the classic invariant
        //   AT_PHDR = load_bias + (first_load.p_vaddr - first_load.p_offset) + e_phoff
        // (e.g. gVisor / Cloud Run) only finds the table when the new segment
        // carries the *same* file-offset-to-vaddr bias as the first PT_LOAD. So
        // pin `load_vaddr = new_phoff + first_load_bias` rather than packing the
        // segment right above `max_vaddr_end`. `new_phoff` is bumped as needed
        // so the resulting vaddr still clears every existing segment.
        let new_phnum = e_phnum + 2;
        let phdr_table_size = new_phnum * e_phentsize;
        let mut new_phoff = align_up(data.len(), PAGE);
        // Keep the note's vaddr past the last mapped page (including .bss).
        let min_load_vaddr = align_up(max_vaddr_end as usize, PAGE) as i64;
        if new_phoff as i64 + first_load_bias < min_load_vaddr {
            // `min_load_vaddr` and `first_load_bias` are both page-aligned, so
            // the adjusted `new_phoff` stays page-aligned (and past EOF).
            new_phoff = (min_load_vaddr - first_load_bias) as usize;
        }
        let note_file_off = align_up(new_phoff + phdr_table_size, 4);
        let load_vaddr = (new_phoff as i64 + first_load_bias) as u64;
        let note_vaddr = load_vaddr + (note_file_off - new_phoff) as u64;
        let region_filesz = (note_file_off + note.len() - new_phoff) as u64;

        // Builds one program header entry of `e_phentsize` bytes (trailing
        // bytes, if the entry is larger than 56, stay zeroed).
        let make_phdr = |p_type: u32,
                         p_flags: u32,
                         p_offset: u64,
                         p_vaddr: u64,
                         p_filesz: u64,
                         p_memsz: u64,
                         p_align: u64|
         -> Vec<u8> {
            let mut e = vec![0u8; e_phentsize];
            w32(&mut e[0..4], p_type);
            w32(&mut e[4..8], p_flags);
            w64(&mut e[8..16], p_offset);
            w64(&mut e[16..24], p_vaddr);
            w64(&mut e[24..32], p_vaddr); // p_paddr = p_vaddr
            w64(&mut e[32..40], p_filesz);
            w64(&mut e[40..48], p_memsz);
            w64(&mut e[48..56], p_align);
            e
        };

        // New program header table = verbatim copy of the originals + the two
        // new segments. Copying raw preserves any per-entry padding.
        let mut table = data[e_phoff..e_phoff + e_phnum * e_phentsize].to_vec();
        // PT_PHDR, if present, must describe the (relocated) table and lie
        // inside a PT_LOAD — the new PT_LOAD below covers it.
        if let Some(i) = pt_phdr_index {
            let e = &mut table[i * e_phentsize..i * e_phentsize + PHENTSIZE64];
            w64(&mut e[8..16], new_phoff as u64);
            w64(&mut e[16..24], load_vaddr);
            w64(&mut e[24..32], load_vaddr);
            w64(&mut e[32..40], phdr_table_size as u64);
            w64(&mut e[40..48], phdr_table_size as u64);
        }
        table.extend_from_slice(&make_phdr(
            PT_LOAD,
            PF_R,
            new_phoff as u64,
            load_vaddr,
            region_filesz,
            region_filesz,
            PAGE as u64,
        ));
        table.extend_from_slice(&make_phdr(
            PT_NOTE,
            PF_R,
            note_file_off as u64,
            note_vaddr,
            note.len() as u64,
            note.len() as u64,
            4,
        ));

        let mut out = data.to_vec();
        out.resize(new_phoff, 0);
        out.extend_from_slice(&table);
        out.resize(note_file_off, 0);
        out.extend_from_slice(&note);

        // If the input still has a section header table, add a real allocated
        // SHT_NOTE section for the note as well. The PT_NOTE above is what the
        // runtime reads, but a section is what `strip` (and other BFD-based
        // tools, which rebuild the file from its sections) preserve — without
        // it a later strip would discard the note bytes. A fully stripped
        // binary has no section table to extend, and keeps the note via
        // PT_NOTE alone.
        const SHENTSIZE64: usize = 64;
        const SHT_PROGBITS: u32 = 1;
        const SHT_NOTE: u32 = 7;
        const SHF_ALLOC: u64 = 2;
        const SHN_XINDEX: usize = 0xffff;

        let e_shoff = r64(&data[0x28..0x30]) as usize;
        let e_shentsize = r16(&data[0x3a..0x3c]) as usize;
        let e_shnum = r16(&data[0x3c..0x3e]) as usize;
        let e_shstrndx = r16(&data[0x3e..0x40]) as usize;

        let mut new_shoff = e_shoff as u64;
        let mut new_shnum = e_shnum as u16;

        let shstr_range = (e_shoff != 0
            && e_shentsize >= SHENTSIZE64
            && e_shnum != 0
            && e_shstrndx != 0
            && e_shstrndx < e_shnum
            && e_shstrndx != SHN_XINDEX
            && e_shoff
                .checked_add(e_shnum * e_shentsize)
                .map(|end| end <= data.len())
                .unwrap_or(false))
        .then(|| {
            let hdr = &data[e_shoff + e_shstrndx * e_shentsize..];
            (r64(&hdr[24..32]) as usize, r64(&hdr[32..40]) as usize)
        })
        .filter(|&(off, size)| {
            off.checked_add(size)
                .map(|end| end <= data.len())
                .unwrap_or(false)
        });

        if let Some((shstr_off, shstr_size)) = shstr_range {
            // Relocate and grow .shstrtab so it carries the new section names.
            let mut new_shstr = data[shstr_off..shstr_off + shstr_size].to_vec();
            let note_name_index = new_shstr.len() as u32;
            new_shstr.extend_from_slice(b".note.sui\0");
            let phdr_name_index = new_shstr.len() as u32;
            new_shstr.extend_from_slice(b".sui.phdrs\0");

            // .shstrtab and the section header table are non-allocated
            // metadata: place them past the note, outside the new PT_LOAD.
            let shstr_new_off = out.len();
            out.extend_from_slice(&new_shstr);
            let sht_new_off = align_up(out.len(), 8);
            out.resize(sht_new_off, 0);

            // Copy the original section headers, repoint the relocated
            // .shstrtab entry, then append the new section headers.
            let mut sht = data[e_shoff..e_shoff + e_shnum * e_shentsize].to_vec();
            {
                let e = &mut sht[e_shstrndx * e_shentsize..e_shstrndx * e_shentsize + SHENTSIZE64];
                w64(&mut e[24..32], shstr_new_off as u64);
                w64(&mut e[32..40], new_shstr.len() as u64);
            }

            // The relocated program header table lives past the original EOF,
            // in the file gap before the note. Cover it with an allocated
            // section so section-based strip tools that keep the file's byte
            // layout preserve it. `eu-strip` in particular lays the output out
            // with ELF_F_LAYOUT and zero-fills every file gap that lies between
            // two allocated sections; without a section spanning the program
            // header table it treats those bytes as dead space and clobbers
            // them, leaving a binary with all-zero program headers that
            // segfaults on exec. This must be a plain PROGBITS section, not part
            // of the note: BFD `strip` parses SHT_NOTE contents, so folding the
            // (non-note) program header bytes into `.note.sui` corrupts the note
            // on strip and loses it.
            let mut phdr_sh = vec![0u8; e_shentsize];
            w32(&mut phdr_sh[0..4], phdr_name_index); // sh_name
            w32(&mut phdr_sh[4..8], SHT_PROGBITS); // sh_type
            w64(&mut phdr_sh[8..16], SHF_ALLOC); // sh_flags
            w64(&mut phdr_sh[16..24], load_vaddr); // sh_addr
            w64(&mut phdr_sh[24..32], new_phoff as u64); // sh_offset
            w64(&mut phdr_sh[32..40], (note_file_off - new_phoff) as u64); // sh_size
            w64(&mut phdr_sh[48..56], PAGE as u64); // sh_addralign
            sht.extend_from_slice(&phdr_sh);

            // The allocated SHT_NOTE section keeps the note reachable through the
            // section table (what BFD-based tools rebuild from); the runtime
            // reads it through its PT_NOTE program header regardless.
            let mut note_sh = vec![0u8; e_shentsize];
            w32(&mut note_sh[0..4], note_name_index); // sh_name
            w32(&mut note_sh[4..8], SHT_NOTE); // sh_type
            w64(&mut note_sh[8..16], SHF_ALLOC); // sh_flags
            w64(&mut note_sh[16..24], note_vaddr); // sh_addr
            w64(&mut note_sh[24..32], note_file_off as u64); // sh_offset
            w64(&mut note_sh[32..40], note.len() as u64); // sh_size
            w64(&mut note_sh[48..56], 4); // sh_addralign
            sht.extend_from_slice(&note_sh);
            out.extend_from_slice(&sht);

            new_shoff = sht_new_off as u64;
            new_shnum = (e_shnum + 2) as u16;
        }

        // Point the ELF header at the relocated, enlarged program header table
        // (and section header table, if one was added).
        w64(&mut out[0x20..0x28], new_phoff as u64);
        w16(&mut out[0x38..0x3a], new_phnum as u16);
        w64(&mut out[0x28..0x30], new_shoff);
        w16(&mut out[0x3c..0x3e], new_shnum);

        writer.write_all(&out)?;
        Ok(())
    }
}

#[cfg(all(unix, not(target_vendor = "apple")))]
mod elf {
    use libc::{dl_iterate_phdr, dl_phdr_info, Elf64_Phdr, PT_NOTE};
    use std::os::raw::{c_int, c_void};

    unsafe extern "C" fn sui_dl_iterate_phdr_callback(
        info: *mut dl_phdr_info,
        _size: usize,
        data: *mut c_void,
    ) -> c_int {
        *(data as *mut dl_phdr_info) = *info;
        1
    }

    fn find_in_note_segment<'a>(segment: &'a [u8], align: usize, name: &str) -> Option<&'a [u8]> {
        let mut pos = 0usize;
        while pos + 12 <= segment.len() {
            let namesz = u32::from_le_bytes(segment[pos..pos + 4].try_into().ok()?) as usize;
            let descsz = u32::from_le_bytes(segment[pos + 4..pos + 8].try_into().ok()?) as usize;
            let note_type = u32::from_le_bytes(segment[pos + 8..pos + 12].try_into().ok()?);
            pos += 12;

            if pos + namesz > segment.len() {
                break;
            }
            let mut note_name = &segment[pos..pos + namesz];
            while let [rest @ .., 0] = note_name {
                note_name = rest;
            }
            pos = super::align_up(pos + namesz, align);

            if pos + descsz > segment.len() {
                break;
            }
            let desc = &segment[pos..pos + descsz];
            pos = super::align_up(pos + descsz, align);

            if note_name == &super::ELF_NOTE_NAME[..super::ELF_NOTE_NAME.len() - 1]
                && note_type == super::ELF_NOTE_TYPE_SECTION_DATA
            {
                if let Some(section_data) = super::parse_elf_note_desc(desc, name) {
                    return Some(section_data);
                }
            }
        }
        None
    }

    pub fn find_section(name: &str) -> std::io::Result<Option<&[u8]>> {
        let mut main_program_info: dl_phdr_info = unsafe { std::mem::zeroed() };
        unsafe {
            dl_iterate_phdr(
                Some(sui_dl_iterate_phdr_callback),
                &mut main_program_info as *mut dl_phdr_info as *mut c_void,
            );
        }

        find_section_in_phdr_info(&main_program_info, name)
    }

    fn find_section_in_phdr_info(
        info: &dl_phdr_info,
        name: &str,
    ) -> std::io::Result<Option<&'static [u8]>> {
        let mut p = info.dlpi_phdr as *const u8;
        let mut n = info.dlpi_phnum as usize;
        let base = info.dlpi_addr as usize;

        while n > 0 {
            let phdr = unsafe { &*(p as *const Elf64_Phdr) };
            if phdr.p_type == PT_NOTE {
                let pos = base + phdr.p_vaddr as usize;
                let len = phdr.p_memsz as usize;
                if len > 0 {
                    let segment = unsafe { std::slice::from_raw_parts(pos as *const u8, len) };
                    let align = (phdr.p_align as usize).max(4);
                    if let Some(section_data) = find_in_note_segment(segment, align, name) {
                        // SAFETY: `segment` points into a mapped PT_LOAD range.
                        // That mapping stays valid for the process lifetime (or until dlclose),
                        // so it is safe to extend the slice's lifetime to 'static here.
                        let data: &'static [u8] = unsafe { std::mem::transmute(section_data) };
                        return Ok(Some(data));
                    }
                }
            }

            n -= 1;
            p = unsafe { p.add(core::mem::size_of::<Elf64_Phdr>()) };
        }

        Ok(None)
    }

    /// Find a section in the currently loaded shared library (the image
    /// containing this code).
    ///
    /// Unlike `find_section` which searches the main executable,
    /// this function uses `dladdr` to determine which shared object the
    /// calling code belongs to, then iterates `dl_iterate_phdr` to find
    /// the matching image and searches its PT_NOTE segments.
    pub fn find_section_in_current_image(name: &str) -> std::io::Result<Option<&'static [u8]>> {
        use libc::dladdr;

        #[repr(C)]
        #[allow(non_camel_case_types)]
        struct Dl_info {
            dli_fname: *const std::os::raw::c_char,
            dli_fbase: *mut c_void,
            dli_sname: *const std::os::raw::c_char,
            dli_saddr: *mut c_void,
        }

        // Find which shared object we are in by looking up our own function address
        let mut info: Dl_info = unsafe { std::mem::zeroed() };
        let self_addr = find_section_in_current_image as *const c_void;
        let ret = unsafe { dladdr(self_addr, &mut info as *mut Dl_info as *mut _) };
        if ret == 0 || info.dli_fbase.is_null() {
            return Ok(None);
        }
        let our_base = info.dli_fbase as usize;

        // Now iterate all loaded images to find the one matching our base address
        struct CallbackData {
            target_base: usize,
            result: dl_phdr_info,
            found: bool,
        }

        unsafe extern "C" fn callback(
            info: *mut dl_phdr_info,
            _size: usize,
            data: *mut c_void,
        ) -> c_int {
            let cb = &mut *(data as *mut CallbackData);
            if (*info).dlpi_addr as usize == cb.target_base {
                cb.result = *info;
                cb.found = true;
                return 1; // stop iteration
            }
            0 // continue
        }

        let mut cb_data = CallbackData {
            target_base: our_base,
            result: unsafe { std::mem::zeroed() },
            found: false,
        };

        unsafe {
            dl_iterate_phdr(
                Some(callback),
                &mut cb_data as *mut CallbackData as *mut c_void,
            );
        }

        if !cb_data.found {
            return Ok(None);
        }

        find_section_in_phdr_info(&cb_data.result, name)
    }
}

/// Utilities for detecting binary formats
pub mod utils {
    /// Check if the given data is an ELF64 binary
    pub fn is_elf(data: &[u8]) -> bool {
        if data.len() < 4 {
            return false;
        }
        let magic = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        magic == 0x7f454c46
    }

    /// Check if the given data is a 64-bit Mach-O binary
    pub fn is_macho(data: &[u8]) -> bool {
        if data.len() < 4 {
            return false;
        }
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        magic == 0xfeedfacf
    }

    /// Check if the given data is a PE32+ binary
    pub fn is_pe(data: &[u8]) -> bool {
        if data.len() < 2 {
            return false;
        }
        let magic = u16::from_le_bytes([data[0], data[1]]);
        magic == 0x5a4d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a minimal arm64 Mach-O containing __TEXT, __LINKEDIT, and the
    // dyld-1284.13 commands LC_FUNCTION_VARIANTS / LC_FUNCTION_VARIANT_FIXUPS,
    // then verify write_section shifts their dataoff into the new LINKEDIT
    // location.
    #[test]
    fn shifts_function_variants_offsets() {
        const HEADER_SIZE: usize = size_of::<Header64>();
        const SEG_SIZE: usize = size_of::<SegmentCommand64>();
        const LINKEDIT_CMD_SIZE: usize = 16;

        let sizeofcmds = SEG_SIZE * 2 + LINKEDIT_CMD_SIZE * 2;
        let padding = 256usize;
        let linkedit_fileoff = HEADER_SIZE + sizeofcmds + padding;
        let linkedit_filesize: u64 = 32;

        let header = Header64 {
            magic: 0xfeedfacf,
            cputype: CPU_TYPE_ARM_64,
            cpusubtype: 0,
            filetype: 2,
            ncmds: 4,
            sizeofcmds: sizeofcmds as u32,
            flags: 0,
            reserved: 0,
        };

        let text_seg = SegmentCommand64 {
            cmd: LC_SEGMENT_64,
            cmdsize: SEG_SIZE as u32,
            segname: *b"__TEXT\0\0\0\0\0\0\0\0\0\0",
            vmaddr: 0,
            vmsize: 0x4000,
            fileoff: 0,
            filesize: linkedit_fileoff as u64,
            maxprot: 5,
            initprot: 5,
            nsects: 0,
            flags: 0,
        };

        let linkedit_seg = SegmentCommand64 {
            cmd: LC_SEGMENT_64,
            cmdsize: SEG_SIZE as u32,
            segname: *b"__LINKEDIT\0\0\0\0\0\0",
            vmaddr: 0x4000,
            vmsize: 0x4000,
            fileoff: linkedit_fileoff as u64,
            filesize: linkedit_filesize,
            maxprot: 1,
            initprot: 1,
            nsects: 0,
            flags: 0,
        };

        let fv_dataoff = linkedit_fileoff as u32;
        let fvf_dataoff = linkedit_fileoff as u32 + 8;

        let mut obj = Vec::new();
        obj.extend_from_slice(header.as_bytes());
        obj.extend_from_slice(text_seg.as_bytes());
        obj.extend_from_slice(linkedit_seg.as_bytes());
        // LC_FUNCTION_VARIANTS as linkedit_data_command
        obj.extend_from_slice(&LC_FUNCTION_VARIANTS.to_le_bytes());
        obj.extend_from_slice(&(LINKEDIT_CMD_SIZE as u32).to_le_bytes());
        obj.extend_from_slice(&fv_dataoff.to_le_bytes());
        obj.extend_from_slice(&8u32.to_le_bytes());
        // LC_FUNCTION_VARIANT_FIXUPS as linkedit_data_command
        obj.extend_from_slice(&LC_FUNCTION_VARIANT_FIXUPS.to_le_bytes());
        obj.extend_from_slice(&(LINKEDIT_CMD_SIZE as u32).to_le_bytes());
        obj.extend_from_slice(&fvf_dataoff.to_le_bytes());
        obj.extend_from_slice(&8u32.to_le_bytes());
        // padding between commands and __LINKEDIT
        obj.resize(linkedit_fileoff, 0);
        // __LINKEDIT bytes
        obj.extend_from_slice(&vec![0xAB; linkedit_filesize as usize]);

        let sectdata = vec![0u8; 16];
        let macho = Macho::from(obj).unwrap();
        let macho = macho.write_section("__test", sectdata).unwrap();

        let expected_shift = align_vmsize(16, 0x10000);

        // Walk commands in the in-memory buffer to find the patched dataoffs.
        let mut found_fv = None;
        let mut found_fvf = None;
        for (cmd, _cmdsize, offset) in &macho.commands {
            if *cmd == LC_FUNCTION_VARIANTS {
                let dataoff = u32::from_le_bytes(
                    macho.data[offset + 8..offset + 12].try_into().unwrap(),
                );
                found_fv = Some(dataoff);
            } else if *cmd == LC_FUNCTION_VARIANT_FIXUPS {
                let dataoff = u32::from_le_bytes(
                    macho.data[offset + 8..offset + 12].try_into().unwrap(),
                );
                found_fvf = Some(dataoff);
            }
        }

        assert_eq!(
            found_fv,
            Some(fv_dataoff + expected_shift as u32),
            "LC_FUNCTION_VARIANTS dataoff was not shifted"
        );
        assert_eq!(
            found_fvf,
            Some(fvf_dataoff + expected_shift as u32),
            "LC_FUNCTION_VARIANT_FIXUPS dataoff was not shifted"
        );
    }
}
