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

fn parse_c_str(buf: &[u8]) -> Option<String> {
    for (i, &byte) in buf.iter().enumerate() {
        if byte == 0 {
            return String::from_utf8(buf[..i].to_vec()).ok();
        }
    }
    None
}

fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    let bytes: [u8; 8] = buf[offset..offset + 8].try_into().unwrap();
    u64::from_le_bytes(bytes)
}

fn write_u64_le(buf: &mut [u8], offset: usize, value: u64) {
    let bytes = value.to_le_bytes();
    buf[offset..offset + 8].copy_from_slice(&bytes);
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    let bytes: [u8; 4] = buf[offset..offset + 4].try_into().unwrap();
    u32::from_le_bytes(bytes)
}

fn write_u32_le(buf: &mut [u8], offset: usize, value: u32) {
    let bytes = value.to_le_bytes();
    buf[offset..offset + 4].copy_from_slice(&bytes);
}

/// Patch a Mach-O load command
fn patch_command(cmd_type: u32, buf: &mut [u8], file_len: usize) {
    // LC_SEGMENT_64
    if cmd_type == 0x19 {
        if let Some(name) = parse_c_str(&buf[..16]) {
            if name == "__LINKEDIT" {
                let fileoff = read_u64_le(buf, 32);
                let vmsize_patched = file_len as u64 - fileoff;
                let filesize_patched = vmsize_patched;

                write_u64_le(buf, 24, vmsize_patched);
                write_u64_le(buf, 40, filesize_patched);
            }
        }
    }

    // LC_SYMTAB
    if cmd_type == 0x2 {
        let stroff = read_u32_le(buf, 8);
        let strsize_patched = file_len as u32 - stroff;

        write_u32_le(buf, 12, strsize_patched);
    }
}

/// Find section data in an Intel Mac Mach-O executable.
///
/// Searches the current executable for the `<~sui-data~>` sentinel and
/// returns the embedded payload, or `None` if no payload is present.
pub fn find_section() -> std::io::Result<Option<&'static [u8]>> {
    let exe = std::env::current_exe()?;
    find_section_in_file(&exe)
}

/// Find section data in an Intel Mac Mach-O image on disk.
///
/// Identical to [`find_section`] but reads from `path` instead of the current
/// executable, so it can also recover the payload from a dylib loaded into
/// the process. Used by `find_section_in_current_image` on x86_64 macOS,
/// where the payload is appended past the Mach-O sections (rather than
/// written as a real named section) and so cannot be read with
/// `getsectiondata`.
pub fn find_section_in_file(path: &std::path::Path) -> std::io::Result<Option<&'static [u8]>> {
    use std::io::{Read, Seek, SeekFrom};

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
    let sentinel = sentinel.as_slice();

    let mut file = std::fs::File::open(path)?;

    // Get file size
    let file_size = file.seek(SeekFrom::End(0))?;

    // Search backwards for sentinel in chunks to avoid allocating huge buffer
    const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
    let overlap = sentinel.len() + 8; // Overlap to handle sentinel across chunk boundaries

    let mut pos = file_size;
    let mut prev_chunk_tail = vec![0u8; 0];

    while pos > 0 {
        let chunk_start = pos.saturating_sub(CHUNK_SIZE as u64);
        let chunk_len = (pos - chunk_start) as usize;

        file.seek(SeekFrom::Start(chunk_start))?;
        let mut chunk = vec![0u8; chunk_len];
        file.read_exact(&mut chunk)?;

        // Append previous chunk tail for overlap handling
        chunk.extend_from_slice(&prev_chunk_tail);

        // Find sentinel from the end of this chunk
        for i in (0..=(chunk.len().saturating_sub(sentinel.len()))).rev() {
            if &chunk[i..i + sentinel.len()] == sentinel {
                // Found sentinel, read data length (u64 after sentinel)
                if i + sentinel.len() + 8 > chunk.len() {
                    return Ok(None);
                }

                let len_bytes: [u8; 8] = chunk[i + sentinel.len()..i + sentinel.len() + 8]
                    .try_into()
                    .map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid length")
                    })?;
                let data_len = u64::from_le_bytes(len_bytes) as usize;

                // Read the actual data
                let data_start = i + sentinel.len() + 8;
                if data_start + data_len > chunk.len() {
                    // We need to read the rest of the data from the file
                    let mut data = chunk[data_start..].to_vec(); // data we already have
                    let remaining = data_len - (chunk.len() - data_start); // how much we need to read
                    file.seek(SeekFrom::Start(chunk_start + chunk.len() as u64))?;
                    if chunk.len() < remaining {
                        chunk.extend(std::iter::repeat_n(0, remaining - chunk.len()));
                    }
                    file.read_exact(&mut chunk[..remaining])?;
                    data.extend_from_slice(&chunk[..remaining]);
                    return Ok(Some(Box::leak(data.into_boxed_slice())));
                }
                let data = chunk[data_start..data_start + data_len].to_vec();
                return Ok(Some(Box::leak(data.into_boxed_slice())));
            }
        }

        // Save tail of current chunk for next iteration (for overlap)
        prev_chunk_tail = if chunk_len > overlap {
            chunk[..overlap].to_vec()
        } else {
            chunk
        };

        pos = chunk_start;
    }

    Ok(None)
}

/// Patch a Mach-O executable file
///
/// This function modifies the Mach-O header to fix up segment and symbol table sizes.
/// It specifically patches:
/// - The __LINKEDIT segment's vmsize and filesize
/// - The symbol table's string size
///
/// # Arguments
///
/// * `file` - A mutable reference to the Mach-O executable bytes
///
/// # Returns
///
/// Returns `true` if patching was successful, `false` otherwise
pub fn patch_macho_executable(file: &mut [u8]) -> bool {
    const ALIGN: usize = 8;
    const HSIZE: usize = 32;

    if file.len() < HSIZE + 4 {
        return false;
    }

    let ncmds = read_u32_le(file, 16);
    let file_len = file.len();

    let mut offset = HSIZE;

    for _ in 0..ncmds {
        if offset + 8 > file.len() {
            return false;
        }

        let cmd_type = read_u32_le(file, offset);
        offset += 4;

        // A load command header is 8 bytes: a u32 `cmd` followed by a u32
        // `cmdsize`. A `cmdsize` smaller than the header is malformed, so
        // reject it instead of underflowing when computing the body size.
        let cmdsize = read_u32_le(file, offset) as usize;
        offset += 4;
        if cmdsize < 8 {
            return false;
        }
        let size = cmdsize - 8;

        if offset + size > file.len() {
            return false;
        }

        // Split the file slice to allow mutable access to the command buffer
        // while keeping file_len available
        let (_, rest) = file.split_at_mut(offset);
        let cmd_buf = &mut rest[..size];
        patch_command(cmd_type, cmd_buf, file_len);

        offset += size;
        if offset & ALIGN != 0 {
            offset += ALIGN - (offset & ALIGN);
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_c_str() {
        let buf = b"__LINKEDIT\0extra";
        assert_eq!(parse_c_str(buf), Some("__LINKEDIT".to_string()));

        let buf = b"test";
        assert_eq!(parse_c_str(buf), None);

        let buf = b"\0";
        assert_eq!(parse_c_str(buf), Some("".to_string()));
    }

    #[test]
    fn test_read_write_u64_le() {
        let mut buf = vec![0u8; 16];
        write_u64_le(&mut buf, 0, 0x0123456789ABCDEF);
        assert_eq!(read_u64_le(&buf, 0), 0x0123456789ABCDEF);

        write_u64_le(&mut buf, 8, 0xFEDCBA9876543210);
        assert_eq!(read_u64_le(&buf, 8), 0xFEDCBA9876543210);
    }

    #[test]
    fn test_read_write_u32_le() {
        let mut buf = vec![0u8; 8];
        write_u32_le(&mut buf, 0, 0x01234567);
        assert_eq!(read_u32_le(&buf, 0), 0x01234567);

        write_u32_le(&mut buf, 4, 0xFEDCBA98);
        assert_eq!(read_u32_le(&buf, 4), 0xFEDCBA98);
    }

    #[test]
    fn test_patch_macho_executable_invalid() {
        let mut buf = vec![0u8; 10];
        assert_eq!(patch_macho_executable(&mut buf), false);
    }

    #[test]
    fn test_patch_macho_executable_malformed_cmdsize() {
        // Header is 32 bytes; reserve room for one load command header plus a
        // little extra so the only thing wrong is the `cmdsize` value.
        let mut buf = vec![0u8; 64];
        // ncmds = 1
        write_u32_le(&mut buf, 16, 1);
        // First load command at offset 32: cmd = LC_SEGMENT_64 (0x19),
        // cmdsize = 4 which is smaller than the 8-byte command header.
        write_u32_le(&mut buf, 32, 0x19);
        write_u32_le(&mut buf, 36, 4);
        // Must reject the malformed command rather than underflowing.
        assert_eq!(patch_macho_executable(&mut buf), false);
    }
}
