/*
 * "Ad-hoc code signing for Mach-O binaries"
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
use sha2::{Digest, Sha256};
use zerocopy::byteorder::big_endian;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::{Error, Header64, SegmentCommand64, LC_CODE_SIGNATURE, LC_SEGMENT_64};
use core::mem::size_of;

#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
struct SuperBlob {
    magic: big_endian::U32,
    length: big_endian::U32,
    count: big_endian::U32,
}

#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
#[repr(C)]
struct Blob {
    typ: big_endian::U32,
    offset: big_endian::U32,
}

#[repr(C)]
#[derive(Debug, Clone, FromBytes, FromZeroes, AsBytes)]
struct CodeDirectory {
    magic: big_endian::U32,           // magic number (CSMAGIC_CODEDIRECTORY)
    length: big_endian::U32,          // total length of CodeDirectory blob
    version: big_endian::U32,         // compatibility version
    flags: big_endian::U32,           // setup and mode flags
    hash_offset: big_endian::U32,     // offset of hash slot element at index zero
    ident_offset: big_endian::U32,    // offset of identifier string
    n_special_slots: big_endian::U32, // number of special hash slots
    n_code_slots: big_endian::U32,    // number of ordinary (code) hash slots
    code_limit: big_endian::U32,      // limit to main image signature range
    hash_size: u8,                    // size of each hash in bytes
    hash_type: u8,                    // type of hash (cdHashType* constants)
    _pad1: u8,                        // unused (must be zero)
    page_size: u8,                    // log2(page size in bytes); 0 => infinite
    _pad2: big_endian::U32,           // unused (must be zero)
    scatter_offset: big_endian::U32,
    team_offset: big_endian::U32,
    _pad3: big_endian::U32,
    code_limit64: big_endian::U64,
    exec_seg_base: big_endian::U64,
    exec_seg_limit: big_endian::U64,
    exec_seg_flags: big_endian::U64,
}

#[derive(FromBytes, FromZeroes, AsBytes, Debug)]
#[repr(C)]
struct LinkeditDataCommand {
    cmd: u32,
    cmdsize: u32,
    dataoff: u32,
    datasize: u32,
}

pub struct MachoSigner {
    data: Vec<u8>,

    sig_off: usize,
    sig_sz: usize,
    cs_cmd_off: usize,
    linkedit_off: usize,

    linkedit_seg: SegmentCommand64,
    text_seg: SegmentCommand64,
}

const CSMAGIC_CODEDIRECTORY: u32 = 0xfade0c02; // CodeDirectory blob
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade0cc0; // embedded form of signature data
const CSSLOT_CODEDIRECTORY: u32 = 0; // slot index for CodeDirectory

const SEC_CODE_SIGNATURE_HASH_SHA256: u8 = 2;

const CS_EXECSEG_MAIN_BINARY: u64 = 0x1; // executable segment denotes main binary

impl MachoSigner {
    pub fn new(obj: Vec<u8>) -> Result<Self, Error> {
        let header = Header64::read_from_prefix(&obj)
            .ok_or(Error::InvalidObject("Invalid Mach-O header"))?;

        let mut offset = size_of::<Header64>();
        let mut sig_off = 0;
        let mut sig_sz = 0;
        let mut cs_cmd_off = 0;
        let mut linkedit_off = 0;

        let mut text_seg = SegmentCommand64::new_zeroed();
        let mut linkedit_seg = SegmentCommand64::new_zeroed();

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

            if cmd == LC_CODE_SIGNATURE {
                let cmd = LinkeditDataCommand::read_from_prefix(&obj[offset..])
                    .ok_or(Error::InvalidObject("Failed to read linkedit data command"))?;
                sig_off = cmd.dataoff as usize;
                sig_sz = cmd.datasize as usize;
                cs_cmd_off = offset;
            }
            if cmd == LC_SEGMENT_64 {
                let segcmd = SegmentCommand64::read_from_prefix(&obj[offset..])
                    .ok_or(Error::InvalidObject("Failed to read segment command"))?;
                // Convert fixed size array terminated by null byte to string
                let segname = String::from_utf8_lossy(&segcmd.segname);
                let segname = segname.trim_end_matches('\0');

                if segname == "__LINKEDIT" {
                    linkedit_off = offset;
                    linkedit_seg = segcmd;
                } else if segname == "__TEXT" {
                    text_seg = segcmd;
                }
            }

            offset += cmdsize as usize;
        }

        Ok(Self {
            data: obj,
            sig_off,
            sig_sz,
            cs_cmd_off,
            linkedit_off,
            linkedit_seg,
            text_seg,
        })
    }

    pub fn sign<W: std::io::Write>(mut self, mut writer: W) -> Result<(), Error> {
        const PAGE_SIZE: usize = 1 << 12;

        let id = b"a.out\0";
        let n_hashes = self.sig_off.div_ceil(PAGE_SIZE);
        let id_off = size_of::<CodeDirectory>();
        let hash_off = id_off + id.len();
        let c_dir_sz = hash_off + n_hashes * 32;
        let sz = size_of::<SuperBlob>() + size_of::<Blob>() + c_dir_sz;

        if self.sig_sz != sz {
            // Update the load command
            let cs_cmd = LinkeditDataCommand::mut_from_prefix(&mut self.data[self.cs_cmd_off..])
                .ok_or(Error::InvalidObject("Failed to read linkedit data command"))?;
            cs_cmd.datasize = sz as u32;

            // Update __LINKEDIT segment
            let seg_sz = self.sig_off + sz - self.linkedit_seg.fileoff as usize;
            let linkedit_seg =
                SegmentCommand64::mut_from_prefix(&mut self.data[self.linkedit_off..])
                    .ok_or(Error::InvalidObject("Failed to read linkedit segment"))?;
            linkedit_seg.filesize = seg_sz as u64;
            linkedit_seg.vmsize = seg_sz as u64;
        }

        let sb = SuperBlob {
            magic: CSMAGIC_EMBEDDED_SIGNATURE.into(),
            length: (sz as u32).into(),
            count: 1.into(),
        };
        let blob = Blob {
            typ: CSSLOT_CODEDIRECTORY.into(),
            offset: (size_of::<SuperBlob>() as u32 + size_of::<Blob>() as u32).into(),
        };
        let c_dir = CodeDirectory::new_zeroed();
        let c_dir = CodeDirectory {
            magic: CSMAGIC_CODEDIRECTORY.into(),
            length: (sz as u32 - (size_of::<SuperBlob>() as u32 + size_of::<Blob>() as u32)).into(),
            version: 0x20400.into(),
            flags: 0x20002.into(), // adhoc | linkerSigned
            hash_offset: (hash_off as u32).into(),
            ident_offset: (id_off as u32).into(),
            n_code_slots: (n_hashes as u32).into(),
            code_limit: (self.sig_off as u32).into(),
            hash_size: sha2::Sha256::output_size() as u8,
            hash_type: SEC_CODE_SIGNATURE_HASH_SHA256,
            page_size: 12,
            exec_seg_base: self.text_seg.fileoff.into(),
            exec_seg_limit: self.text_seg.filesize.into(),
            exec_seg_flags: CS_EXECSEG_MAIN_BINARY.into(),
            ..c_dir
        };

        let mut out = Vec::with_capacity(sz);
        out.extend_from_slice(sb.as_bytes());
        out.extend_from_slice(blob.as_bytes());
        out.extend_from_slice(c_dir.as_bytes());
        out.extend_from_slice(id);

        let mut fileoff = 0;

        let mut hasher = Sha256::new();
        while fileoff < self.sig_off {
            let mut n = PAGE_SIZE;
            if fileoff + n > self.sig_off {
                n = self.sig_off - fileoff;
            }
            let chunk = &self.data[fileoff..fileoff + n];
            hasher.update(chunk);
            out.extend_from_slice(&hasher.finalize_reset());
            fileoff += n;
        }

        if self.data.len() < self.sig_off + sz {
            self.data.resize(self.sig_off + sz, 0);
        }

        self.data[self.sig_off..self.sig_off + sz].copy_from_slice(&out);
        self.data.truncate(self.sig_off + sz);

        writer.write_all(&self.data)?;

        Ok(())
    }
}
