//! libsui's own minimal portable-executable resource writer.
//!
//! This module was reduced from the BSD-2-Clause `editpe` crate by Christian
//! Sdunek (<https://github.com/Systemcluster/editpe>), which is unmaintained.
//! Only the portable-executable resource-*writing* path that libsui actually
//! exercises is kept: parsing enough of the PE headers to locate the resource
//! data directory, building a fresh resource tree, and rewriting the image with
//! a new/extended `.rsrc`/`.pedata` section. Everything else from upstream
//! (resource *parsing*, icon/version/manifest readers, the `image`-crate
//! integration) has been removed.
//!
//! Local changes relative to upstream are marked with `// libsui:` comments.
//! See LICENSE-editpe for the upstream license and attribution.

// The raw header structs mirror the Windows PE type names (DWORD, WORD, ...).
#![allow(clippy::upper_case_acronyms)]

use std::borrow::{Borrow, Cow};
use std::{mem, slice};

use indexmap::IndexMap;
use thiserror::Error;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

// ---------------------------------------------------------------------------
// errors (from editpe/errors.rs)
// ---------------------------------------------------------------------------

/// Error that can occur when reading and parsing bytes.
#[derive(Error, Debug)]
#[error("{0}")]
pub struct ReadError(pub String);

/// Errors that can occur when reading a PE image.
#[derive(Error, Debug)]
pub enum ImageReadError {
    #[error("invalid bytes: {0}")]
    InvalidBytes(#[from] ReadError),
    #[error("invalid header: {0}")]
    InvalidHeader(String),
}

/// Errors that can occur when writing a PE image.
#[derive(Error, Debug)]
pub enum ImageWriteError {
    #[error("not enough space in file header")]
    NotEnoughSpaceInHeader,
    #[error("section points outside image: {0} > {1}")]
    InvalidSectionRange(u64, u64),
}

// ---------------------------------------------------------------------------
// util (from editpe/util.rs)
// ---------------------------------------------------------------------------

pub fn read<T: FromBytes + Copy>(resource: &[u8]) -> Result<T, ReadError> {
    T::read_from_prefix(resource).ok_or_else(|| ReadError(std::any::type_name::<T>().to_string()))
}

pub fn aligned_to<T>(value: T, alignment: T) -> T
where
    T: std::ops::Add<Output = T>
        + std::ops::Sub<Output = T>
        + std::ops::Rem<Output = T>
        + Eq
        + Copy
        + Default,
{
    if value % alignment == T::default() {
        return value;
    }
    value + alignment - (value % alignment)
}

// ---------------------------------------------------------------------------
// constants (from editpe/constants.rs)
// ---------------------------------------------------------------------------

#[allow(non_camel_case_types)]
pub type DWORD = u32;
#[allow(non_camel_case_types)]
pub type WORD = u16;
#[allow(non_camel_case_types)]
pub type LANGID = WORD;

// https://docs.microsoft.com/en-us/openspecs/windows_protocols/ms-ucoderef/28fefe92-d66c-4b03-90a9-97b473223d43
pub const CODE_PAGE_ID_EN_US: LANGID = 1200; // 0x04B0, UTF-16LE

// https://docs.microsoft.com/en-us/windows/win32/debug/pe-format
pub const PE_DOS_MAGIC: WORD = 0x5a4d; // MZ
pub const PE_PTR_OFFSET: DWORD = 0x03c;
pub const PE_NT_SIGNATURE: DWORD = 0x00004550; // PE00
pub const PE_32_MAGIC: WORD = 0x010b;
pub const PE_64_MAGIC: WORD = 0x020b;

// https://docs.microsoft.com/en-us/windows/win32/menurc/resource-types
pub const RT_ICON: WORD = 0x03;
pub const RT_RCDATA: WORD = 0x0A;
pub const RT_GROUP_ICON: WORD = 0x0E;

// https://docs.microsoft.com/en-us/windows/win32/debug/pe-format#section-flags
pub const IMAGE_SCN_CNT_INITIALIZED_DATA: DWORD = 0x00000040;
pub const IMAGE_SCN_MEM_READ: DWORD = 0x40000000;

// ---------------------------------------------------------------------------
// types (from editpe/types.rs)
//
// One-to-one mappings of the data described in
// <https://docs.microsoft.com/en-us/windows/win32/debug/pe-format>.
// ---------------------------------------------------------------------------

#[repr(C, packed(1))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct VersionU8 {
    pub major: u8,
    pub minor: u8,
}
#[repr(C, packed(2))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct VersionU16 {
    pub major: u16,
    pub minor: u16,
}
#[repr(C, packed(2))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct CoffHeader {
    pub machine:                 u16,
    pub number_of_sections:      u16,
    pub time_date_stamp:         u32,
    pub pointer_to_symbol_table: u32,
    pub number_of_symbols:       u32,
    pub size_of_optional_header: u16,
    pub characteristics:         u16,
}
#[repr(C, packed(2))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct StandardHeader {
    pub magic:                      u16,
    pub linker_version:             VersionU8,
    pub size_of_code:               u32,
    pub size_of_initialized_data:   u32,
    pub size_of_uninitialized_data: u32,
    pub address_of_entry_point:     u32,
    pub base_of_code:               u32,
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, Default)]
pub struct WindowsHeader<UXX> {
    pub image_base:               UXX,
    pub section_alignment:        u32,
    pub file_alignment:           u32,
    pub operating_system_version: VersionU16,
    pub image_version:            VersionU16,
    pub subsystem_version:        VersionU16,
    pub win32_version_value:      u32,
    pub size_of_image:            u32,
    pub size_of_headers:          u32,
    pub check_sum:                u32,
    pub subsystem:                u16,
    pub dll_characteristics:      u16,
    pub size_of_stack_reserve:    UXX,
    pub size_of_stack_commit:     UXX,
    pub size_of_heap_reserve:     UXX,
    pub size_of_heap_commit:      UXX,
    pub loader_flags:             u32,
    pub number_of_rva_and_sizes:  u32,
}
impl<UXX> WindowsHeader<UXX>
where
    UXX: AsBytes,
{
    pub fn as_bytes(&self) -> &[u8] {
        // manually implement this here because zerocopy doesn't support derive for generic types
        unsafe {
            let len = mem::size_of_val(self);
            slice::from_raw_parts(self as *const Self as *const u8, len)
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum GenericWindowsHeader {
    WindowsHeader32(WindowsHeader<u32>),
    WindowsHeader64(WindowsHeader<u64>),
}
// Only `as_bytes` and `section_alignment` are exercised by the retained code;
// the remaining accessors are kept verbatim from upstream for completeness.
#[allow(dead_code)]
impl GenericWindowsHeader {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.as_bytes(),
            GenericWindowsHeader::WindowsHeader64(header) => header.as_bytes(),
        }
    }

    pub const fn section_alignment(&self) -> u32 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.section_alignment,
            GenericWindowsHeader::WindowsHeader64(header) => header.section_alignment,
        }
    }

    pub const fn file_alignment(&self) -> u32 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.file_alignment,
            GenericWindowsHeader::WindowsHeader64(header) => header.file_alignment,
        }
    }

    pub const fn operating_system_version(&self) -> VersionU16 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.operating_system_version,
            GenericWindowsHeader::WindowsHeader64(header) => header.operating_system_version,
        }
    }

    pub const fn image_version(&self) -> VersionU16 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.image_version,
            GenericWindowsHeader::WindowsHeader64(header) => header.image_version,
        }
    }

    pub const fn subsystem_version(&self) -> VersionU16 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.subsystem_version,
            GenericWindowsHeader::WindowsHeader64(header) => header.subsystem_version,
        }
    }

    pub const fn win32_version_value(&self) -> u32 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.win32_version_value,
            GenericWindowsHeader::WindowsHeader64(header) => header.win32_version_value,
        }
    }

    pub const fn size_of_image(&self) -> u32 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.size_of_image,
            GenericWindowsHeader::WindowsHeader64(header) => header.size_of_image,
        }
    }

    pub const fn size_of_headers(&self) -> u32 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.size_of_headers,
            GenericWindowsHeader::WindowsHeader64(header) => header.size_of_headers,
        }
    }

    pub const fn check_sum(&self) -> u32 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.check_sum,
            GenericWindowsHeader::WindowsHeader64(header) => header.check_sum,
        }
    }

    pub const fn subsystem(&self) -> u16 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.subsystem,
            GenericWindowsHeader::WindowsHeader64(header) => header.subsystem,
        }
    }

    pub const fn dll_characteristics(&self) -> u16 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.dll_characteristics,
            GenericWindowsHeader::WindowsHeader64(header) => header.dll_characteristics,
        }
    }

    pub const fn loader_flags(&self) -> u32 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.loader_flags,
            GenericWindowsHeader::WindowsHeader64(header) => header.loader_flags,
        }
    }

    pub const fn number_of_rva_and_sizes(&self) -> u32 {
        match self {
            GenericWindowsHeader::WindowsHeader32(header) => header.number_of_rva_and_sizes,
            GenericWindowsHeader::WindowsHeader64(header) => header.number_of_rva_and_sizes,
        }
    }
}

#[repr(C, packed(4))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct ImageDataDirectory {
    pub virtual_address: u32,
    pub size:            u32,
}

#[repr(C, packed(4))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct SectionHeader {
    pub name:                   u64,
    pub virtual_size:           u32,
    pub virtual_address:        u32,
    pub size_of_raw_data:       u32,
    pub pointer_to_raw_data:    u32,
    pub pointer_to_relocations: u32,
    pub pointer_to_linenumbers: u32,
    pub number_of_relocations:  u16,
    pub number_of_linenumbers:  u16,
    pub characteristics:        u32,
}

#[repr(C, packed(2))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct ResourceDirectoryTable {
    pub characteristics:        u32,
    pub time_date_stamp:        u32,
    pub version:                VersionU16,
    pub number_of_name_entries: u16,
    pub number_of_id_entries:   u16,
}

#[repr(C, packed(4))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct ResourceDirectoryEntry {
    pub name_offset_or_integer_id:         u32,
    pub data_entry_or_subdirectory_offset: u32,
}

#[repr(C, packed(4))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct ResourceDataEntry {
    pub data_rva: u32,
    pub size:     u32,
    pub codepage: u32,
    pub reserved: u32,
}

#[repr(C, packed(2))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct IconDirectory {
    pub reserved: u16,
    pub type_:    u16,
    pub count:    u16,
}

#[repr(C, packed(1))]
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, FromBytes, FromZeroes, AsBytes, Default,
)]
pub struct IconDirectoryEntry {
    pub width:       u8,
    pub height:      u8,
    pub color_count: u8,
    pub reserved:    u8,
    pub planes:      u16,
    pub bit_count:   u16,
    pub bytes:       u32,
    pub id:          u16,
}

// ---------------------------------------------------------------------------
// resource directory (from editpe/resource.rs)
//
// Data types for building the resource section. See
// <https://learn.microsoft.com/en-us/windows/win32/debug/pe-format#the-rsrc-section>.
// ---------------------------------------------------------------------------

/// Portable executable resource directory.
#[derive(Debug, Clone, Default)]
pub struct ResourceDirectory {
    pub(crate) root: ResourceTable,
}
impl ResourceDirectory {
    /// Returns the mutable root resource table.
    /// The root resource table contains the top-level resource entries.
    pub fn root_mut(&mut self) -> &mut ResourceTable { &mut self.root }

    /// Returns the size of the resulting resource directory in bytes.
    pub fn size(&self) -> u32 { self.root.size() }

    /// Build the resource directory into raw bytes to be included in an image.
    /// The virtual address is used to compute the resource data offsets and has to correspond to the virtual address in the section table header of the target image.
    pub fn build(&self, virtual_address: u32) -> Vec<u8> { self.root.build(virtual_address) }
}

/// Portable executable resource table data.
enum TableData {
    Table(ResourceDirectoryTable),
    Entry(ResourceDirectoryEntry),
}

/// Portable executable resource table.
#[derive(Debug, Clone, Default)]
pub struct ResourceTable {
    pub(crate) data:    ResourceDirectoryTable,
    pub(crate) entries: IndexMap<ResourceEntryName, ResourceEntry>,
}
impl ResourceTable {
    fn build(&self, virtual_address: u32) -> Vec<u8> {
        let mut tables_offset = 0;
        let mut strings_offset = 0;
        let mut descriptions_offset = 0;
        let mut data_offset = 0;
        let (mut tables_data, strings_data, mut descriptions_data, data_data) = self.build_table(
            virtual_address,
            &mut tables_offset,
            &mut strings_offset,
            &mut descriptions_offset,
            &mut data_offset,
        );

        let mut data = Vec::new();
        data.extend(tables_data.iter_mut().flat_map(|data| match data {
            TableData::Table(table) => table.as_bytes(),
            TableData::Entry(entry) => {
                if entry.data_entry_or_subdirectory_offset & 0x80000000 == 0 {
                    entry.data_entry_or_subdirectory_offset += tables_offset + strings_offset;
                }
                if entry.name_offset_or_integer_id & 0x80000000 != 0 {
                    entry.name_offset_or_integer_id += tables_offset;
                }
                entry.as_bytes()
            }
        }));
        data.extend(strings_data.iter());
        data.extend(descriptions_data.iter_mut().flat_map(|data| {
            data.data_rva += tables_offset + strings_offset + descriptions_offset;
            data.as_bytes()
        }));
        data.extend(data_data);

        data
    }

    fn build_table(
        &self, virtual_address: u32, tables_offset: &mut u32, strings_offset: &mut u32,
        descriptions_offset: &mut u32, data_offset: &mut u32,
    ) -> (Vec<TableData>, Vec<u8>, Vec<ResourceDataEntry>, Vec<u8>) {
        let mut tables_data = Vec::<TableData>::new();
        let mut strings_data = Vec::<u8>::new();
        let mut descriptions_data = Vec::<ResourceDataEntry>::new();
        let mut data_data = Vec::<u8>::new();

        tables_data.push(TableData::Table(self.data));
        *tables_offset += 16;

        let mut next_table_offset = 0u32;
        let mut next_table_sizes = 0u32;
        for (name, entry) in &self.entries {
            strings_data.extend(name.string_data());
            let name_offset_or_integer_id = if name.string_size() > 0 {
                *strings_offset | 0x80000000
            } else {
                name.id()
            };
            *strings_offset += name.string_size();

            match entry {
                ResourceEntry::Table(table) => {
                    let entry_data = ResourceDirectoryEntry {
                        name_offset_or_integer_id,
                        data_entry_or_subdirectory_offset: (*tables_offset
                            + self.entries.len() as u32 * 8
                            + next_table_sizes)
                            | 0x80000000,
                    };
                    tables_data.push(TableData::Entry(entry_data));
                    next_table_offset += 8;
                    next_table_sizes += table.tables_size();
                }
                ResourceEntry::Data(data) => {
                    let entry_data = ResourceDirectoryEntry {
                        name_offset_or_integer_id,
                        data_entry_or_subdirectory_offset: *descriptions_offset,
                    };
                    tables_data.push(TableData::Entry(entry_data));
                    next_table_offset += 8;

                    data_data.extend(&data.data);
                    let description_data = ResourceDataEntry {
                        data_rva: *data_offset + virtual_address,
                        size:     data.data.len() as u32,
                        codepage: data.codepage,
                        reserved: data.reserved,
                    };
                    descriptions_data.push(description_data);
                    *descriptions_offset += 16;
                    *data_offset += data.data.len() as u32;
                }
            }
        }
        *tables_offset += next_table_offset;

        for (_, entry) in &self.entries {
            match entry {
                ResourceEntry::Table(table) => {
                    let (t_tables_data, t_strings_data, t_descriptions_data, t_data_data) = table
                        .build_table(
                            virtual_address,
                            tables_offset,
                            strings_offset,
                            descriptions_offset,
                            data_offset,
                        );
                    tables_data.extend(t_tables_data);
                    strings_data.extend(t_strings_data);
                    descriptions_data.extend(t_descriptions_data);
                    data_data.extend(t_data_data);
                }
                ResourceEntry::Data(_) => {}
            }
        }

        (tables_data, strings_data, descriptions_data, data_data)
    }

    /// Get a resource entry from the table.
    /// # Returns
    /// The resource entry.
    pub fn get<N: Borrow<ResourceEntryName>>(&self, name: N) -> Option<&ResourceEntry> {
        self.entries.get(name.borrow())
    }

    /// Get a mutable resource entry from the table.
    /// # Returns
    /// The resource entry.
    pub fn get_mut<N: Borrow<ResourceEntryName>>(&mut self, name: N) -> Option<&mut ResourceEntry> {
        self.entries.get_mut(name.borrow())
    }

    /// Insert a resource entry into the table.
    /// If an entry with the given name already exists, it will be replaced.
    /// # Returns
    /// The replaced entry.
    pub fn insert<N: Borrow<ResourceEntryName>>(
        &mut self, name: N, entry: ResourceEntry,
    ) -> Option<ResourceEntry> {
        let name = name.borrow();
        let entry = self.entries.insert(name.clone(), entry);
        if entry.is_none() {
            if name.string_size() > 0 {
                self.data.number_of_name_entries += 1;
            } else {
                self.data.number_of_id_entries += 1;
            }
        }
        entry
    }

    /// Insert a resource entry into the table at the specified position.
    /// If an entry with the given name already exists, it will be replaced.
    /// # Returns
    /// The replaced entry.
    pub fn insert_at<N: Borrow<ResourceEntryName>>(
        &mut self, name: N, entry: ResourceEntry, position: usize,
    ) -> Option<ResourceEntry> {
        let name = name.borrow();
        let len = self.entries.len();
        let old_entry = self.entries.get(name).cloned();
        let new_entry = self
            .entries
            .entry(name.clone())
            .and_modify(|old_entry| *old_entry = entry.clone());
        let index = new_entry.index();
        new_entry.or_insert(entry);
        self.entries.move_index(index, position);
        if index >= len {
            if name.string_size() > 0 {
                self.data.number_of_name_entries += 1;
            } else {
                self.data.number_of_id_entries += 1;
            }
        }
        old_entry
    }

    /// Returns the entries in the table.
    pub fn entries(&self) -> Vec<&ResourceEntryName> { self.entries.keys().collect() }

    /// Returns the complete size of the table, its resources and its children in the resource table.
    pub fn size(&self) -> u32 {
        self.tables_size() + self.strings_size() + self.descriptions_size() + self.data_size()
    }

    /// Returns the size of the table and its children in the resource table.
    pub fn tables_size(&self) -> u32 {
        self.entries.iter().map(|(_, entry)| entry.table_size()).sum::<u32>() + 16
    }

    /// Returns the size of the strings in the entry and its children in the resource table.
    pub fn strings_size(&self) -> u32 {
        self.entries
            .iter()
            .map(|(name, entry)| name.string_size() + entry.strings_size())
            .sum::<u32>()
    }

    /// Returns the size of the descriptions in the tables children in the resource table.
    pub fn descriptions_size(&self) -> u32 {
        self.entries.iter().map(|(_, entry)| entry.description_size()).sum::<u32>()
    }

    /// Returns the size of the data in in the tables children in the resource table.
    pub fn data_size(&self) -> u32 {
        self.entries.iter().map(|(_, entry)| entry.data_size()).sum::<u32>()
    }
}

/// Raw resource data.
#[derive(Clone, Default)]
pub struct ResourceData {
    data:     Vec<u8>,
    codepage: u32,
    reserved: u32,
}
impl std::fmt::Debug for ResourceData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceData")
            .field("data", &format_args!("[{} bytes]", self.data.len()))
            .field("codepage", &self.codepage)
            .field("reserved", &self.reserved)
            .finish()
    }
}
impl ResourceData {
    /// Returns the raw data.
    #[allow(dead_code)]
    pub fn data(&self) -> &[u8] { &self.data }

    /// Returns the codepage of the data.
    #[allow(dead_code)]
    pub fn codepage(&self) -> u32 { self.codepage }

    /// Set the raw data.
    pub fn set_data(&mut self, data: Vec<u8>) { self.data = data; }

    /// Set the codepage of the data.
    pub fn set_codepage(&mut self, codepage: u32) { self.codepage = codepage; }
}

/// Resource entry in a resource table.
/// This can be either a child table or raw data.
#[derive(Debug, Clone)]
pub enum ResourceEntry {
    Table(ResourceTable),
    Data(ResourceData),
}
impl ResourceEntry {
    /// Returns the size of the table entry and its children in the resource table.
    pub fn table_size(&self) -> u32 {
        match self {
            // entry + sub-table
            ResourceEntry::Table(table) => table.tables_size() + 8,
            // entry
            ResourceEntry::Data(_) => 8,
        }
    }

    /// Returns the size of the strings in the entry and its children in the resource table.
    /// This is the size of the resouorce names of child tables.
    pub fn strings_size(&self) -> u32 {
        match self {
            ResourceEntry::Table(table) => table.strings_size(),
            ResourceEntry::Data(_) => 0,
        }
    }

    /// Returns the size of the descriptions in the entry and its children in the resource table.
    /// This is the size of the resource data description of the entry or child entries.
    pub fn description_size(&self) -> u32 {
        match self {
            ResourceEntry::Table(table) => table.descriptions_size(),
            ResourceEntry::Data(_) => 16,
        }
    }

    /// Returns the size of the data in the entry and its children in the resource table.
    /// This is the size of the resource data of the entry or child entries.
    pub fn data_size(&self) -> u32 {
        match self {
            ResourceEntry::Table(table) => table.data_size(),
            ResourceEntry::Data(data) => data.data.len() as u32,
        }
    }
}

/// Resource directory entry name.
/// This can either be a raw id or a name.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum ResourceEntryName {
    // raw id
    ID(u32),
    // 2 byte size + data
    Name(Vec<u8>),
}
impl ResourceEntryName {
    pub fn from_string<S: AsRef<str>>(string: S) -> Self {
        let string = string.as_ref();
        let mut data = Vec::with_capacity(string.len() * 2 + 2);
        data.extend_from_slice(&(string.len() as u16).to_le_bytes());
        data.extend(string.encode_utf16().flat_map(|c| c.to_le_bytes().to_vec()));
        Self::Name(data)
    }

    fn string_size(&self) -> u32 {
        match self {
            Self::ID(_) => 0,
            Self::Name(name) => name.len() as u32,
        }
    }

    fn id(&self) -> u32 {
        match self {
            Self::ID(id) => *id,
            Self::Name(_) => unreachable!(),
        }
    }

    fn string_data(&self) -> &[u8] {
        match self {
            Self::ID(_) => &[],
            Self::Name(data) => data.as_bytes(),
        }
    }
}

// ---------------------------------------------------------------------------
// image (from editpe/image.rs)
//
// See <https://learn.microsoft.com/en-us/windows/win32/debug/pe-format>.
// ---------------------------------------------------------------------------

/// Image data directory type enumeration.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum DataDirectoryType {
    ExportTable,
    ImportTable,
    ResourceTable,
    ExceptionTable,
    CertificateTable,
    BaseRelocationTable,
    Debug,
    Architecture,
    GlobalPtr,
    TLSTable,
    LoadConfigTable,
    BoundImport,
    IAT,
    DelayImportDescriptor,
    CLRRuntimeHeader,
    Reserved,
}

/// Portable executable image representation.
///
/// This struct is the main entry point for parsing, querying and updating a portable executable image.
pub struct Image<'a> {
    pub(crate) image: Cow<'a, [u8]>,

    pub(crate) coff_header:           CoffHeader,
    pub(crate) standard_header:       StandardHeader,
    pub(crate) windows_header:        GenericWindowsHeader,
    pub(crate) header_data_directory: IndexMap<DataDirectoryType, ImageDataDirectory>,
    pub(crate) section_table:         Vec<SectionHeader>,

    pub(crate) resource_directory: Option<ResourceDirectory>,

    coff_header_offset:  u64,
    directories_offset:  u64,
}

impl<'a> Image<'a> {
    /// Parse a portable executable image from a byte slice.
    ///
    /// libsui only rewrites the resource tree wholesale, so — unlike upstream —
    /// this does not parse the existing resource directory. It parses just
    /// enough of the headers for [`Image::set_resource_directory`] and
    /// [`Image::data`]: the DOS/PE headers, COFF header, optional header
    /// (PE32 and PE32+), the data directory array, and the section table. The
    /// stored resource directory is left as `None`.
    ///
    /// # Returns
    /// Returns the `Image`, or an error if the byte slice is not a valid portable executable image or is missing required headers.
    pub fn parse<R: Into<Cow<'a, [u8]>>>(image: R) -> Result<Self, ImageReadError> {
        let image = image.into();

        let pe_dos_magic = read::<u16>(&image[0..])?;
        if pe_dos_magic != PE_DOS_MAGIC {
            return Err(ImageReadError::InvalidHeader("no dos magic".into()));
        }

        let pe_signature_offset = read::<u32>(&image[PE_PTR_OFFSET as usize..])?;

        let pe_signature = read::<u32>(&image[pe_signature_offset as usize..])?;
        if pe_signature != PE_NT_SIGNATURE {
            return Err(ImageReadError::InvalidHeader("no pe signature".into()));
        }

        let coff_header_offset = (pe_signature_offset + 4) as u64;
        let coff_header = read::<CoffHeader>(&image[coff_header_offset as usize..])?;
        if coff_header.size_of_optional_header < 24 {
            return Err(ImageReadError::InvalidHeader("optional header too small".into()));
        }

        let standard_header_offset = coff_header_offset + 20;
        let standard_header = read::<StandardHeader>(&image[standard_header_offset as usize..])?;

        let (windows_header, number_of_rva_and_sizes, optional_header_dd_offset) = {
            if standard_header.magic == PE_32_MAGIC && coff_header.size_of_optional_header >= 96 {
                let windows_header_offset = standard_header_offset + 28;
                let windows_header =
                    read::<WindowsHeader<u32>>(&image[windows_header_offset as usize..])?;
                (
                    GenericWindowsHeader::WindowsHeader32(windows_header),
                    windows_header.number_of_rva_and_sizes,
                    standard_header_offset + 96,
                )
            } else if standard_header.magic == PE_64_MAGIC
                && coff_header.size_of_optional_header >= 112
            {
                let windows_header_offset = standard_header_offset + 24;
                let windows_header =
                    read::<WindowsHeader<u64>>(&image[windows_header_offset as usize..])?;
                (
                    GenericWindowsHeader::WindowsHeader64(windows_header),
                    windows_header.number_of_rva_and_sizes,
                    standard_header_offset + 112,
                )
            } else {
                return Err(ImageReadError::InvalidHeader("invalid optional header".into()));
            }
        };

        if image.len() <= optional_header_dd_offset as usize {
            return Err(ImageReadError::InvalidHeader(
                "image truncated after optional header".into(),
            ));
        }

        let mut header_data_directory = IndexMap::<DataDirectoryType, ImageDataDirectory>::new();
        use DataDirectoryType::*;
        for (index, &header) in [
            ExportTable,
            ImportTable,
            ResourceTable,
            ExceptionTable,
            CertificateTable,
            BaseRelocationTable,
            Debug,
            Architecture,
            GlobalPtr,
            TLSTable,
            LoadConfigTable,
            BoundImport,
            IAT,
            DelayImportDescriptor,
            CLRRuntimeHeader,
            Reserved,
        ]
        .iter()
        .enumerate()
        {
            if (index as u32) < number_of_rva_and_sizes {
                let offset = optional_header_dd_offset + (index * 8) as u64;
                let data = read::<ImageDataDirectory>(&image[offset as usize..])?;
                header_data_directory.insert(header, data);
            }
        }

        let section_table_offset =
            standard_header_offset + coff_header.size_of_optional_header as u64;
        let mut section_table = Vec::new();
        for index in 0..coff_header.number_of_sections {
            let section_table_offset = section_table_offset + (index * 40) as u64;
            let section_header = read::<SectionHeader>(&image[section_table_offset as usize..])?;
            section_table.push(section_header);
        }

        let directories_offset =
            section_table_offset + (coff_header.number_of_sections * 40) as u64;

        // libsui: do not parse the existing resource tree. libsui replaces the
        // resource directory wholesale and `set_resource_directory` reads only
        // the resource *data directory* entry (RVA + size) from
        // `header_data_directory`, never the parsed tree.
        let resource_directory = None;

        Ok(Self {
            image,
            coff_header,
            standard_header,
            windows_header,
            header_data_directory,
            section_table,
            resource_directory,
            coff_header_offset,
            directories_offset,
        })
    }

    /// Set the resource directory of the image.
    ///
    /// This will update the resource data directory and the resource section.
    /// If a section containing a resource directory already exists, it will be updated in place if the following conditions are met:
    /// - The new directory is not larger than the section containing the existing one, or that section is the last section in the image.
    /// - The section is not used by other directories.
    ///
    /// Otherwise, the existing section will be kept intact and a new section will be added after all other sections and before any other data at the end of the image.
    ///
    /// # Returns
    /// Returns the previous resource directory, or an error in the following cases:
    /// - Returns an error if the image could not be built. This can happen if there is not enough space in the image header to add a new section.
    /// - Returns an error if a section points to data outside the image.
    ///
    /// # Safety
    /// Replacing an existing resource directory may cause the resulting image to be invalid.
    /// Applications might reference data inside the resource directory that may not exist in the new one.
    /// Only set a resource directory originating from the same image with required resources intact unless you know what you are doing.
    ///
    /// Some packed images (e.g. packed with UPX) might not work with a modified resource directory or additional sections.
    pub fn set_resource_directory(
        &mut self, resource_directory: ResourceDirectory,
    ) -> Result<Option<ResourceDirectory>, ImageWriteError> {
        // copy to-be-modified data to allow erroring out without invalidating the image
        let mut coff_header = self.coff_header;
        let mut windows_header = self.windows_header;
        let mut header_data_directory = self.header_data_directory.clone();
        let mut section_table = self.section_table.clone();

        let mut required_header_space = 0;

        // ensure that the data directory entry for the resource table exists
        use DataDirectoryType::*;
        for (index, &header) in [ExportTable, ImportTable, ResourceTable].iter().enumerate() {
            if index + 1 > header_data_directory.len() {
                header_data_directory.insert(header, ImageDataDirectory::default());
                required_header_space += 8;
            }
        }
        let old_resource_data_directory =
            *header_data_directory.get(&DataDirectoryType::ResourceTable).unwrap();

        let new_resource_directory_size = resource_directory.size();
        let new_resource_directory_size_aligned =
            aligned_to(resource_directory.size(), windows_header.section_alignment());
        let mut resource_section_data = Vec::new();
        let mut new_section_data = Vec::new();

        let mut new_image = Vec::with_capacity(self.image.len());
        new_image.extend_from_slice(&self.image[..self.coff_header_offset as usize]);

        let first_section = section_table
            .iter()
            .filter(|section_header| section_header.size_of_raw_data > 0)
            .min_by_key(|section_header| section_header.pointer_to_raw_data)
            .copied();
        let first_section_start = first_section
            .map(|section| section.pointer_to_raw_data as usize)
            .unwrap_or(self.image.len());

        let last_section = section_table
            .iter()
            .filter(|section_header| section_header.size_of_raw_data > 0)
            .max_by_key(|section_header| {
                section_header.pointer_to_raw_data + section_header.size_of_raw_data
            })
            .copied();
        let last_section_end = last_section
            .map(|section| section.pointer_to_raw_data as usize + section.size_of_raw_data as usize)
            .unwrap_or(self.image.len());

        if last_section_end > self.image.len() {
            return Err(ImageWriteError::InvalidSectionRange(
                last_section_end as u64,
                self.image.len() as u64,
            ));
        }

        let mut old_resource_section_start = 0;
        let mut old_resource_section_end = 0;
        let mut old_resource_section = None;
        if old_resource_data_directory.size > 0 {
            // search for the section containing the resource directory
            for section_header in section_table.iter_mut() {
                if old_resource_data_directory.virtual_address >= section_header.virtual_address
                    && old_resource_data_directory.virtual_address
                        < section_header.virtual_address + section_header.virtual_size
                {
                    old_resource_section_start = section_header.pointer_to_raw_data as usize;
                    old_resource_section_end =
                        old_resource_section_start + section_header.size_of_raw_data as usize;
                    old_resource_section = Some(section_header);
                    break;
                }
            }
        }

        let mut add_new_section = true;
        let mut multiple_data_directories = false;
        if let Some(ref mut old_resource_section) = old_resource_section {
            // an existing resource section was found
            let last_section = last_section.unwrap();
            let is_last_section = last_section.pointer_to_raw_data + last_section.size_of_raw_data
                == old_resource_section.pointer_to_raw_data + old_resource_section.size_of_raw_data;

            // reuse the existing section in place if it is large enough to hold
            // the new resource directory, or if it is the last section and can
            // therefore be extended
            if old_resource_section.size_of_raw_data >= new_resource_directory_size
                || is_last_section
            {
                add_new_section = false;
            }

            if !add_new_section {
                // check for other sections also using the resource section
                for (header, directory) in header_data_directory.iter() {
                    if header != &ResourceTable
                        && directory.virtual_address >= old_resource_section.virtual_address
                        && directory.virtual_address
                            < old_resource_section.virtual_address
                                + old_resource_section.virtual_size
                    {
                        multiple_data_directories = true;
                    }
                }

                if !multiple_data_directories {
                    // only the resource directory uses the resource section, we can replace and extend it
                    let resource_dd =
                        header_data_directory.get_mut(&DataDirectoryType::ResourceTable).unwrap();

                    if old_resource_data_directory.size >= new_resource_directory_size {
                        resource_section_data =
                            resource_directory.build(old_resource_data_directory.virtual_address);

                        if !is_last_section
                            && old_resource_section.size_of_raw_data > new_resource_directory_size
                        {
                            // pad the section to the previous section size with existing data
                            resource_section_data.extend(
                                &self.image[(old_resource_section.pointer_to_raw_data as usize
                                    + new_resource_directory_size as usize)
                                    ..(old_resource_section.pointer_to_raw_data as usize
                                        + old_resource_section.size_of_raw_data as usize)],
                            );
                        } else if old_resource_section.size_of_raw_data
                            > new_resource_directory_size
                        {
                            // adjust section size and virtual size header values
                            resource_dd.size = new_resource_directory_size;
                            old_resource_section.size_of_raw_data = new_resource_directory_size;
                            old_resource_section.virtual_size = new_resource_directory_size_aligned;
                        }
                    } else {
                        resource_section_data =
                            resource_directory.build(old_resource_data_directory.virtual_address);
                        resource_dd.size = new_resource_directory_size;
                        // libsui: assign the grown section/virtual sizes directly.
                        // Upstream used `size_of_raw_data += new - size_of_raw_data`
                        // then `virtual_size += aligned_to(new - size_of_raw_data, ..)`,
                        // but the second read saw the already-mutated size_of_raw_data
                        // (== new), so the delta was 0 and virtual_size never grew,
                        // leaving SizeOfImage under-covering an extended-in-place
                        // section. Matches the truncate and append-new-section paths.
                        old_resource_section.size_of_raw_data = new_resource_directory_size;
                        old_resource_section.virtual_size = new_resource_directory_size_aligned;
                    }
                } else {
                    add_new_section = true;
                }
            }
        }

        if add_new_section {
            if let Some(ref mut old_resource_section) = old_resource_section {
                // copy existing resource section data that might be referenced by other data directories
                resource_section_data.extend(
                    &self.image[old_resource_section.pointer_to_raw_data as usize
                        ..(old_resource_section.pointer_to_raw_data
                            + old_resource_section.size_of_raw_data)
                            as usize],
                );
            }

            let virtual_address = {
                let last_virtual_section = section_table
                    .iter()
                    .max_by_key(|table| table.virtual_address + table.virtual_size);
                if let Some(last_virtual_section) = last_virtual_section {
                    last_virtual_section.virtual_address + last_virtual_section.virtual_size
                } else {
                    windows_header.section_alignment()
                }
            };
            let virtual_address = aligned_to(virtual_address, windows_header.section_alignment());

            let resource_dd =
                header_data_directory.get_mut(&DataDirectoryType::ResourceTable).unwrap();
            resource_dd.virtual_address = virtual_address;
            resource_dd.size = new_resource_directory_size;

            let pointer_to_raw_data = {
                if let Some(last_section) = last_section {
                    last_section.pointer_to_raw_data + last_section.size_of_raw_data
                } else {
                    self.directories_offset as u32
                }
            };
            let new_section = SectionHeader {
                name: u64::from_le_bytes(".pedata\0".as_bytes().try_into().unwrap()),
                virtual_size: new_resource_directory_size_aligned,
                virtual_address,
                size_of_raw_data: new_resource_directory_size,
                pointer_to_raw_data,
                characteristics: IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ,
                ..SectionHeader::default()
            };
            section_table.push(new_section);
            new_section_data = resource_directory.build(virtual_address);

            coff_header.number_of_sections += 1;
            required_header_space += 40;
        }

        let available_space = first_section_start - self.directories_offset as usize;
        if required_header_space as usize > available_space {
            return Err(ImageWriteError::NotEnoughSpaceInHeader);
        }

        // libsui: recompute SizeOfImage from the final section table instead of
        // `size_of_image += new_section_data.len()`. The old code added the
        // *unaligned raw* length of the appended resource data, which under-counts
        // the image by up to one SectionAlignment page (and did not update it at
        // all when an existing last section was merely extended). Per the PE spec,
        // SizeOfImage must cover the highest section's VirtualAddress+VirtualSize
        // rounded up to SectionAlignment; an undersized value leaves the tail of
        // the new resource section unmapped, so the Windows loader either faults
        // (access violation on stricter loaders) or FindResource fails to locate
        // the embedded section at runtime.
        let section_alignment = windows_header.section_alignment();
        let size_of_image = section_table
            .iter()
            .map(|s| aligned_to(s.virtual_address + s.virtual_size, section_alignment))
            .max()
            .unwrap_or_else(|| aligned_to(first_section_start as u32, section_alignment));
        match windows_header {
            GenericWindowsHeader::WindowsHeader32(ref mut header) => {
                header.number_of_rva_and_sizes = header_data_directory.len() as u32;
                header.size_of_image = size_of_image;
                header.check_sum = 0;
            }
            GenericWindowsHeader::WindowsHeader64(ref mut header) => {
                header.number_of_rva_and_sizes = header_data_directory.len() as u32;
                header.size_of_image = size_of_image;
                header.check_sum = 0;
            }
        }

        new_image.extend_from_slice(coff_header.as_bytes());
        new_image.extend_from_slice(self.standard_header.as_bytes());
        new_image.extend_from_slice(windows_header.as_bytes());

        for (_, data) in header_data_directory.iter() {
            new_image.extend_from_slice(data.as_bytes());
        }
        for section_header in section_table.iter() {
            new_image.extend_from_slice(section_header.as_bytes());
        }

        new_image.extend_from_slice(
            &self.image
                [(self.directories_offset + required_header_space) as usize..first_section_start],
        );

        if old_resource_section_start > 0 {
            // a resource section was found in the image, copy the data of sections around it
            new_image
                .extend_from_slice(&self.image[first_section_start..old_resource_section_start]);
            new_image.extend_from_slice(&resource_section_data);
            new_image.extend_from_slice(&self.image[old_resource_section_end..last_section_end]);
        } else {
            // no resource section was found in the image, copy data of all sections
            new_image.extend_from_slice(&self.image[first_section_start..last_section_end]);
        }
        new_image.extend_from_slice(&new_section_data);
        new_image.extend_from_slice(&self.image[last_section_end..]);

        self.coff_header = coff_header;
        self.windows_header = windows_header;
        self.header_data_directory = header_data_directory;
        self.section_table = section_table;

        let previous_resource_directory = self.resource_directory.take();
        self.resource_directory = Some(resource_directory);
        self.image = new_image.into();

        Ok(previous_resource_directory)
    }

    /// Returns the raw image data with all changes applied.
    pub fn data(&self) -> &[u8] { &self.image }
}
