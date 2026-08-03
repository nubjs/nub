/*
 * "Sui experiments that may make into the main library"
 *
 * Copyright (c) 2024 Divy Srivastaa
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

use libc::{dl_iterate_phdr, dl_phdr_info, strncmp, Elf64_Phdr, PT_NOTE};
use std::mem::size_of;
use std::os::raw::{c_char, c_int, c_void};

fn roundup(x: usize, y: usize) -> usize {
    ((x + y - 1) / y) * y
}

unsafe extern "C" fn sui_dl_iterate_phdr_callback(
    info: *mut dl_phdr_info,
    _size: usize,
    data: *mut c_void,
) -> c_int {
    // Snag the dl_phdr_info struct then stop iterating.
    *(data as *mut dl_phdr_info) = *info;
    1
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct Elf64_Nhdr {
    pub n_namesz: u32,
    pub n_descsz: u32,
    pub n_type: u32,
}

pub fn find_section2(elf_section_name: &str) -> Option<&[u8]> {
    unsafe {
        let mut main_program_info: dl_phdr_info = std::mem::zeroed();
        dl_iterate_phdr(
            Some(sui_dl_iterate_phdr_callback),
            &mut main_program_info as *mut dl_phdr_info as *mut c_void,
        );

        let mut p = main_program_info.dlpi_phdr as *const u8;
        let mut n = main_program_info.dlpi_phnum as usize;
        let base = main_program_info.dlpi_addr as usize;

        loop {
            if n <= 0 {
                break;
            }
            let phdr = p as *const Elf64_Phdr;
            if (*phdr).p_type == PT_NOTE {
                let mut pos = base + (*phdr).p_vaddr as usize;
                let end = pos + (*phdr).p_memsz as usize;

                while pos < end {
                    if pos + size_of::<Elf64_Nhdr>() > end {
                        break; // invalid
                    }

                    let note = pos as *const Elf64_Nhdr;
                    if (*note).n_namesz != 0
                        && (*note).n_descsz != 0
                        && strncmp(
                            (pos + size_of::<Elf64_Nhdr>()) as *const c_char,
                            elf_section_name.as_ptr() as *const c_char,
                            elf_section_name.len(),
                        ) == 0
                    {
                        let size = (*note).n_descsz as usize;

                        let data =
                            pos + size_of::<Elf64_Nhdr>() + roundup((*note).n_namesz as usize, 4);
                        return Some(std::slice::from_raw_parts(data as *const u8, size));
                    }

                    pos += size_of::<Elf64_Nhdr>()
                        + roundup((*note).n_namesz as usize, 4)
                        + roundup((*note).n_descsz as usize, 4);
                }
            }

            n -= 1;
            p = p.add(size_of::<Elf64_Phdr>());
        }

        None
    }
}

pub fn inject_elf(
    elf: &[u8],
    name: &str,
    sectdata: &[u8],
    outfile: &str,
) -> Result<(), String> {
    use object::build::elf as e;
    
    let mut builder = e::Builder::read(elf).unwrap();
    
    let section = builder.sections.add();
    section.sh_type = object::elf::SHT_NOTE;
    section.sh_flags = object::elf::SHF_ALLOC as u64;
    section.sh_offset = 0;
    section.sh_size = sectdata.len() as u64;
    section.name = "__SUI".into();
    section.data = e::SectionData::Note(sectdata.into());
    let id = section.id();

    builder.set_section_sizes();

    let segment = builder.segments.add();
    segment.p_type = object::elf::PT_NOTE;
    segment.p_flags = object::elf::PF_R;
    segment.p_align = 4;
    segment.p_filesz = sectdata.len() as u64;
    segment.p_memsz = sectdata.len() as u64;
    segment.append_section(builder.sections.get_mut(id));

    let mut out = Vec::new();
    builder.write(&mut out).unwrap();
    std::fs::write(outfile, out).unwrap();

    Ok(())
}

