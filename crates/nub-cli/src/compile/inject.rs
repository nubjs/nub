//! Per-format payload injection and verification for `nub compile`.
//!
//! Three container formats, three legs, dispatched on the TARGET platform and
//! never on the host — that dispatch is the whole of cross-compilation on the
//! writer side. Each leg has a matching STATIC SCANNER that finds the payload
//! back in a produced file, because the compile-time smoke check (which execs
//! the artifact in launcher probe mode) only works when target == host: a cross-compiled
//! artifact is verified by locating and decoding its payload instead of running
//! it.
//!
//! The scanners parse the file the same way each platform's loader will. The ELF
//! one walks program headers exactly as the launcher's runtime `dl_iterate_phdr`
//! scan does, and the PE one walks the resource directory `FindResourceA`
//! resolves — so a scan that finds the payload is real evidence the loader will,
//! not a re-read of whatever the writer happened to emit.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::{ContainerFormat, TargetArch, TargetPlatform};

/// Inject `payload` into a copy of `template`, writing the artifact to `out`.
///
/// Signing policy is per format and lives here because it is inseparable from
/// the write: Mach-O is ad-hoc signed by libsui's pure-Rust signer (arm64 refuses
/// to execute an unsigned image, and Gatekeeper rejects an invalid one), while
/// ELF and PE are never signed — an unsigned ELF is normal, and Authenticode is
/// deliberately out of scope (an unsigned PE runs; SmartScreen may warn).
pub fn inject(target: &TargetPlatform, template: &[u8], payload: &[u8], out: &Path) -> Result<()> {
    let format = target.format();
    verify_template(target, template)?;

    let mut file = fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    match format {
        ContainerFormat::MachO => {
            let macho = libsui::Macho::from(template.to_vec())
                .map_err(|e| anyhow!("parsing the launcher template as Mach-O: {e:?}"))?;
            macho
                .write_section(nub_core::compile::SECTION_NAME, payload.to_vec())
                .map_err(|e| anyhow!("injecting the payload section: {e:?}"))?
                .build_and_sign(&mut file)
                .map_err(|e| anyhow!("building + ad-hoc signing the executable: {e:?}"))
        }
        ContainerFormat::Elf => libsui::Elf::new(template)
            .append(nub_core::compile::SECTION_NAME, payload, &mut file)
            .map_err(|e| anyhow!("appending the payload note: {e:?}")),
        ContainerFormat::Pe => {
            // libsui rebuilds the PE's resource directory from scratch, so any
            // resource the template carried (icons, version info) is dropped.
            // Harmless today — the launcher template has none — but it is why an
            // `--icon` implementation must set the icon through libsui in THIS
            // call rather than baking it into the template.
            libsui::PortableExecutable::from(template)
                .map_err(|e| anyhow!("parsing the launcher template as PE: {e:?}"))?
                .write_resource(nub_core::compile::SECTION_NAME, payload.to_vec())
                .map_err(|e| anyhow!("injecting the payload resource: {e:?}"))?
                .build(&mut file)
                .map_err(|e| anyhow!("building the executable: {e:?}"))
        }
    }
}

/// The container format `image` actually is, by magic bytes.
pub fn detect_format(image: &[u8]) -> Option<ContainerFormat> {
    if libsui::utils::is_macho(image) {
        Some(ContainerFormat::MachO)
    } else if libsui::utils::is_elf(image) {
        Some(ContainerFormat::Elf)
    } else if libsui::utils::is_pe(image) {
        Some(ContainerFormat::Pe)
    } else {
        None
    }
}

/// Reject a template that is not the target's executable BEFORE libsui parses it.
/// Points at the real mistake (an override, a sibling, or a fetched asset built
/// for the wrong platform) instead of an opaque parse error from the injector.
///
/// FORMAT ALONE IS NOT ENOUGH, and the gap is silent rather than loud: the three
/// platforms nub targets are two architectures each, so a darwin-arm64 template
/// is a perfectly well-formed Mach-O for a `--platform darwin-x64` build. The
/// injection succeeds, every payload check passes, and the user ships an arm64
/// binary labelled x64 that dies on the machine it was built for. Only the
/// ARCHITECTURE field distinguishes them, so it is checked here.
///
/// What this still cannot see: glibc vs musl. Both are ELF with the same
/// `e_machine`, so `linux-x64` and `linux-x64-musl` templates are
/// indistinguishable from the header. Only the asset's NAME separates them.
pub fn verify_template(target: &TargetPlatform, template: &[u8]) -> Result<()> {
    let expected = target.format();
    match detect_format(template) {
        Some(actual) if actual == expected => {}
        Some(actual) => bail!(
            "the launcher template is {} but --platform {} needs {}. \
             The template must be the launcher built FOR the target platform.",
            format_name(actual),
            target.triple(),
            format_name(expected)
        ),
        None => bail!(
            "the launcher template is not a recognized executable \
             (expected {} for --platform {})",
            format_name(expected),
            target.triple()
        ),
    }

    match detect_arch(expected, template) {
        Some(actual) if actual == target.arch => Ok(()),
        Some(actual) => bail!(
            "the launcher template is {} {} but --platform {} needs {}. \
             The template must be the launcher built FOR the target platform.",
            arch_name(actual),
            format_name(expected),
            target.triple(),
            arch_name(target.arch)
        ),
        None => bail!(
            "the launcher template has an unsupported or unreadable {} architecture; \
             --platform {} needs {}. The template must be the launcher built FOR the target platform.",
            format_name(expected),
            target.triple(),
            arch_name(target.arch)
        ),
    }
}

/// The architecture `image` is built for, read from the format's own machine
/// field. `None` when the header is truncated or carries a machine nub has no
/// target for (including a Mach-O fat binary, whose magic is not a thin header).
fn detect_arch(format: ContainerFormat, image: &[u8]) -> Option<TargetArch> {
    let le16 = |at: usize| -> Option<u16> {
        Some(u16::from_le_bytes(image.get(at..at + 2)?.try_into().ok()?))
    };
    let le32 = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(image.get(at..at + 4)?.try_into().ok()?))
    };
    match format {
        // Mach-O `cputype`, the word after the magic. The high `CPU_ARCH_ABI64`
        // bit (0x0100_0000) is masked off, leaving CPU_TYPE_X86 (7) / _ARM (12).
        ContainerFormat::MachO => match le32(4)? & 0x00ff_ffff {
            7 => Some(TargetArch::X64),
            12 => Some(TargetArch::Arm64),
            _ => None,
        },
        // ELF `e_machine`. Read little-endian: both architectures nub targets are
        // little-endian, and a big-endian ELF is not one of them anyway.
        ContainerFormat::Elf => match le16(18)? {
            0x3e => Some(TargetArch::X64),
            0xb7 => Some(TargetArch::Arm64),
            _ => None,
        },
        // PE COFF `Machine`, reached through the `e_lfanew` offset at 0x3c.
        ContainerFormat::Pe => {
            let pe = le32(0x3c)? as usize;
            if image.get(pe..pe + 4)? != b"PE\0\0" {
                return None;
            }
            match le16(pe + 4)? {
                0x8664 => Some(TargetArch::X64),
                0xaa64 => Some(TargetArch::Arm64),
                _ => None,
            }
        }
    }
}

pub fn format_name(format: ContainerFormat) -> &'static str {
    match format {
        ContainerFormat::MachO => "Mach-O (macOS)",
        ContainerFormat::Elf => "ELF (Linux)",
        ContainerFormat::Pe => "PE (Windows)",
    }
}

fn arch_name(arch: TargetArch) -> &'static str {
    match arch {
        TargetArch::X64 => "x64",
        TargetArch::Arm64 => "arm64",
    }
}

/// Find the injected payload in a produced artifact, mirroring how each
/// platform's loader locates it at runtime. `Ok(None)` = well-formed image with
/// no payload (a bare template); `Err` = the image could not be parsed.
pub fn find_payload(format: ContainerFormat, image: &[u8]) -> Result<Option<&[u8]>> {
    let name = nub_core::compile::SECTION_NAME;
    match format {
        ContainerFormat::MachO => match find_in_macho(image, name)? {
            Some(found) => Ok(Some(found)),
            // libsui writes a real Mach-O section for arm64 but APPENDS the
            // payload behind a sentinel for x86_64 — its own documented split,
            // because the Intel path strips the signature with `codesign` first.
            // Reading only the section made every darwin-x64 build fail this
            // check with "the injection did not take", which is the verifier
            // being right about the wrong format rather than a broken injection.
            None => Ok(find_appended_payload(image)),
        },
        ContainerFormat::Elf => find_in_elf(image, name),
        ContainerFormat::Pe => find_in_pe(image, name),
    }
}

// ---- little-endian readers ----------------------------------------------------
//
// Every target nub compiles for is little-endian, so the scanners read LE
// unconditionally; a big-endian image is rejected at its format check instead of
// being mis-parsed. Every offset these take is attacker-controllable in principle
// (it comes out of the file being parsed), so reads are bounds- AND
// overflow-checked and computed offsets saturate — a malformed image must return
// None, never panic.

fn u16le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn u64le(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// A NUL-padded fixed-width name field (Mach-O `sectname`/`segname`).
fn fixed_name(b: &[u8], at: usize, len: usize) -> Option<&str> {
    let raw = b.get(at..at.checked_add(len)?)?;
    let end = raw.iter().position(|&c| c == 0).unwrap_or(len);
    std::str::from_utf8(&raw[..end]).ok()
}

fn slice_at(image: &[u8], offset: u64, size: u64) -> Result<&[u8]> {
    let start = usize::try_from(offset).ok();
    let len = usize::try_from(size).ok();
    start
        .zip(len)
        .and_then(|(s, l)| image.get(s..s.checked_add(l)?))
        .context("the payload's recorded extent lies outside the file")
}

// ---- Mach-O -------------------------------------------------------------------

const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_SEGMENT_64: u32 = 0x19;

/// Walk `LC_SEGMENT_64` load commands for a section named `name`. libsui writes
/// it under its own `__SUI` segment; matching on the SECTION name alone keeps the
/// scanner tolerant of the segment libsui chooses.
/// libsui's x86_64 Mach-O layout: the payload appended to the end of the file
/// behind a 16-byte sentinel, then its length, then the bytes.
///
/// Searched from the END, because the marker is only guaranteed to be the last
/// one — an image can legitimately contain the same bytes earlier, which is why
/// libsui builds the marker at run time rather than embedding it as a literal.
fn find_appended_payload(image: &[u8]) -> Option<&[u8]> {
    const SENTINEL: &[u8] = b"<~sui-data~>\xef\xbe\xad\xde";
    let start = image
        .windows(SENTINEL.len())
        .rposition(|w| w == SENTINEL)?
        .checked_add(SENTINEL.len())?;
    let len_end = start.checked_add(8)?;
    let len = u64::from_le_bytes(image.get(start..len_end)?.try_into().ok()?);
    let end = len_end.checked_add(usize::try_from(len).ok()?)?;
    image.get(len_end..end)
}

fn find_in_macho<'a>(image: &'a [u8], name: &str) -> Result<Option<&'a [u8]>> {
    if u32le(image, 0) != Some(MH_MAGIC_64) {
        bail!("not a 64-bit little-endian Mach-O image");
    }
    let ncmds = u32le(image, 16).context("truncated Mach-O header")? as usize;
    let mut cursor = 32usize; // sizeof(mach_header_64)
    for _ in 0..ncmds {
        let cmd = u32le(image, cursor).context("truncated Mach-O load command")?;
        let cmdsize = u32le(image, cursor + 4).context("truncated Mach-O load command")? as usize;
        if cmdsize == 0 {
            bail!("Mach-O load command has a zero size");
        }
        if cmd == LC_SEGMENT_64 {
            let nsects = u32le(image, cursor + 64).context("truncated Mach-O segment")? as usize;
            for i in 0..nsects {
                // section_64 entries follow the 72-byte segment_command_64.
                let sect = cursor
                    .saturating_add(72)
                    .saturating_add(i.saturating_mul(80));
                if fixed_name(image, sect, 16) == Some(name) {
                    let offset = u32le(image, sect + 48).context("truncated Mach-O section")?;
                    let size = u64le(image, sect + 40).context("truncated Mach-O section")?;
                    return slice_at(image, u64::from(offset), size).map(Some);
                }
            }
        }
        cursor = cursor
            .checked_add(cmdsize)
            .context("Mach-O load commands overflow")?;
    }
    Ok(None)
}

// ---- ELF ----------------------------------------------------------------------

const PT_NOTE: u32 = 4;
const ELF_NOTE_NAME: &[u8] = b"SUI";
/// libsui's note type for an embedded section payload.
const ELF_NOTE_TYPE: u32 = 0x5355_4901;

/// Walk `PT_NOTE` program headers for libsui's `SUI` note carrying `name` — the
/// same traversal the launcher's `dl_iterate_phdr` scan performs at runtime,
/// against the file's own program header table.
fn find_in_elf<'a>(image: &'a [u8], name: &str) -> Result<Option<&'a [u8]>> {
    if image.get(0..4) != Some(b"\x7fELF") {
        bail!("not an ELF image");
    }
    if image.get(4) != Some(&2) || image.get(5) != Some(&1) {
        bail!("only 64-bit little-endian ELF is supported");
    }
    let phoff = u64le(image, 0x20).context("truncated ELF header")? as usize;
    let phentsize = u16le(image, 0x36).context("truncated ELF header")? as usize;
    let phnum = u16le(image, 0x38).context("truncated ELF header")? as usize;
    if phentsize < 56 {
        bail!("unexpected ELF program header size");
    }
    for i in 0..phnum {
        let ph = phoff.saturating_add(i.saturating_mul(phentsize));
        if u32le(image, ph) != Some(PT_NOTE) {
            continue;
        }
        let offset = u64le(image, ph + 8).context("truncated ELF program header")?;
        let filesz = u64le(image, ph + 32).context("truncated ELF program header")?;
        let align = u64le(image, ph + 48).unwrap_or(4).max(4) as usize;
        let segment = slice_at(image, offset, filesz)?;
        if let Some(found) = find_in_note_segment(segment, align, name) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Walk one note segment: `namesz`/`descsz`/`type`, then the name and desc each
/// padded up to the segment's alignment.
fn find_in_note_segment<'a>(segment: &'a [u8], align: usize, name: &str) -> Option<&'a [u8]> {
    let round_up = |v: usize| v.saturating_add(align - 1) & !(align - 1);
    let mut pos = 0usize;
    while pos + 12 <= segment.len() {
        let namesz = u32le(segment, pos)? as usize;
        let descsz = u32le(segment, pos + 4)? as usize;
        let note_type = u32le(segment, pos + 8)?;
        pos += 12;
        let note_name = segment.get(pos..pos.checked_add(namesz)?)?;
        let note_name = note_name.strip_suffix(&[0]).unwrap_or(note_name);
        pos = round_up(pos.checked_add(namesz)?);
        let desc = segment.get(pos..pos.checked_add(descsz)?)?;
        pos = round_up(pos.checked_add(descsz)?);

        if note_name == ELF_NOTE_NAME && note_type == ELF_NOTE_TYPE {
            // desc = [name_len u16][name][payload]
            let name_len = u16le(desc, 0)? as usize;
            if desc.get(2..2 + name_len)? == name.as_bytes() {
                return desc.get(2 + name_len..);
            }
        }
    }
    None
}

// ---- PE -----------------------------------------------------------------------

/// `RT_RCDATA` — the resource type libsui stores the payload under.
const RT_RCDATA: u32 = 10;

/// Walk the resource directory for `RT_RCDATA` → `<name>` → first language, the
/// path `FindResourceA` takes at runtime. libsui upper-cases the resource name on
/// both write and read, so the lookup does too.
fn find_in_pe<'a>(image: &'a [u8], name: &str) -> Result<Option<&'a [u8]>> {
    let pe = pe_header_offset(image)?;
    let opt = pe + 24;
    // The data directory sits after the optional header's fixed fields — 112 bytes
    // for PE32+, 96 for PE32 (which has an extra BaseOfData but 32-bit addresses).
    let dd_offset = match u16le(image, opt).context("truncated PE optional header")? {
        0x20b => opt + 112,
        0x10b => opt + 96,
        other => bail!("unrecognized PE optional header magic {other:#x}"),
    };
    // Data directory entry 2 = resources.
    let rsrc_rva = u32le(image, dd_offset + 16).context("truncated PE data directory")?;
    if rsrc_rva == 0 {
        return Ok(None);
    }
    let sections = pe_sections(image, pe)?;
    let base = rva_to_offset(&sections, rsrc_rva)
        .context("the PE resource directory RVA is outside every section")?;

    let name = name.to_uppercase();
    let Some(types) = resource_subdir(image, base, base, |e| e == ResourceKey::Id(RT_RCDATA))?
    else {
        return Ok(None);
    };
    let Some(named) =
        resource_subdir(image, base, types, |e| e == ResourceKey::Name(name.clone()))?
    else {
        return Ok(None);
    };
    // Language level: take the first entry, whatever its id.
    let Some(leaf) = resource_entries(image, named)?.first().copied() else {
        return Ok(None);
    };
    if leaf.1 & 0x8000_0000 != 0 {
        bail!("the PE payload resource is a directory, not data");
    }
    let data_entry = base + leaf.1 as usize;
    let data_rva = u32le(image, data_entry).context("truncated PE resource data entry")?;
    let size = u32le(image, data_entry + 4).context("truncated PE resource data entry")?;
    let offset = rva_to_offset(&sections, data_rva)
        .context("the PE payload RVA is outside every section")?;
    slice_at(image, offset as u64, u64::from(size)).map(Some)
}

fn pe_header_offset(image: &[u8]) -> Result<usize> {
    if image.get(0..2) != Some(b"MZ") {
        bail!("not a PE image");
    }
    let pe = u32le(image, 0x3c).context("truncated DOS header")? as usize;
    if image.get(pe..pe + 4) != Some(b"PE\0\0") {
        bail!("not a PE image (missing PE signature)");
    }
    Ok(pe)
}

/// `(virtual_address, virtual_size, raw_offset, raw_size)` per section.
fn pe_sections(image: &[u8], pe: usize) -> Result<Vec<(u32, u32, u32, u32)>> {
    let count = u16le(image, pe + 6).context("truncated COFF header")? as usize;
    let opt_size = u16le(image, pe + 20).context("truncated COFF header")? as usize;
    let table = pe + 24 + opt_size;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let s = table.saturating_add(i.saturating_mul(40));
        out.push((
            u32le(image, s + 12).context("truncated PE section table")?,
            u32le(image, s + 8).context("truncated PE section table")?,
            u32le(image, s + 20).context("truncated PE section table")?,
            u32le(image, s + 16).context("truncated PE section table")?,
        ));
    }
    Ok(out)
}

fn rva_to_offset(sections: &[(u32, u32, u32, u32)], rva: u32) -> Option<usize> {
    sections.iter().find_map(|&(va, vsize, raw, rawsize)| {
        let span = vsize.max(rawsize);
        (rva >= va && rva < va.saturating_add(span)).then(|| (raw + (rva - va)) as usize)
    })
}

#[derive(PartialEq, Eq)]
enum ResourceKey {
    Id(u32),
    Name(String),
}

/// The `(key, offset_field)` pairs of one resource directory table.
fn resource_entries(image: &[u8], dir: usize) -> Result<Vec<(u32, u32)>> {
    let named = u16le(image, dir + 12).context("truncated PE resource directory")? as usize;
    let ids = u16le(image, dir + 14).context("truncated PE resource directory")? as usize;
    let mut out = Vec::with_capacity(named + ids);
    for i in 0..named + ids {
        let e = dir.saturating_add(16).saturating_add(i.saturating_mul(8));
        out.push((
            u32le(image, e).context("truncated PE resource entry")?,
            u32le(image, e + 4).context("truncated PE resource entry")?,
        ));
    }
    Ok(out)
}

/// The offset of the subdirectory whose key matches `want`, or `None`.
fn resource_subdir(
    image: &[u8],
    base: usize,
    dir: usize,
    mut want: impl FnMut(ResourceKey) -> bool,
) -> Result<Option<usize>> {
    for (key, offset) in resource_entries(image, dir)? {
        let key = if key & 0x8000_0000 != 0 {
            ResourceKey::Name(resource_string(image, base + (key & 0x7fff_ffff) as usize)?)
        } else {
            ResourceKey::Id(key)
        };
        if want(key) {
            if offset & 0x8000_0000 == 0 {
                bail!("a PE resource level that must be a directory is a data leaf");
            }
            return Ok(Some(base + (offset & 0x7fff_ffff) as usize));
        }
    }
    Ok(None)
}

/// A `IMAGE_RESOURCE_DIR_STRING_U`: a u16 length followed by that many UTF-16LE
/// code units.
fn resource_string(image: &[u8], at: usize) -> Result<String> {
    let len = u16le(image, at).context("truncated PE resource name")? as usize;
    let mut units = Vec::with_capacity(len);
    for i in 0..len {
        let at = at.saturating_add(2).saturating_add(i.saturating_mul(2));
        units.push(u16le(image, at).context("truncated PE resource name")?);
    }
    String::from_utf16(&units).context("a PE resource name is not valid UTF-16")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nub_core::compile::{Manifest, Shape};

    fn payload(entry: &str) -> Vec<u8> {
        let manifest = Manifest {
            shape: Shape::Smol,
            entry: entry.into(),
            node_version: "24.10.0".into(),
            smol_exact_target: false,
            triple: "linux-x64".into(),
            node_sha256: String::new(),
            node_blake3: String::new(),
            node_size: 0,
            app_compressed: false,
            app_sha256: "aa".into(),
            minify: true,
            install_message: None,
        };
        nub_core::compile::encode(
            &manifest,
            &[nub_core::compile::AppFile::plain(
                entry,
                b"console.log(1)\n".to_vec(),
            )],
            &[],
        )
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nub-inject-{}-{name}", std::process::id()));
        let _ = fs::create_dir_all(&d);
        d.join(name)
    }

    /// Every format round-trips: inject into that format's template, then find and
    /// decode the payload back out of the produced file with the same traversal
    /// the platform loader uses. This is the cross-compile correctness gate — for
    /// the two foreign formats it is the ONLY gate, since they cannot be executed.
    fn round_trip(triple: &str, template: &[u8]) {
        let target = TargetPlatform::parse(triple).unwrap();
        let out = tmp(&format!("artifact-{triple}"));
        let payload = payload("main.js");
        inject(&target, template, &payload, &out).expect("inject");

        let produced = fs::read(&out).unwrap();
        assert_eq!(
            detect_format(&produced),
            Some(target.format()),
            "{triple}: the artifact must stay in the target's container format"
        );
        let found = find_payload(target.format(), &produced)
            .expect("scan")
            .unwrap_or_else(|| panic!("{triple}: no payload found in the produced artifact"));
        let view = nub_core::compile::decode(found).expect("decode");
        assert_eq!(view.manifest.entry, "main.js");
        assert_eq!(view.app_files[0].bytes, b"console.log(1)\n");
        let _ = fs::remove_file(&out);
    }

    #[test]
    fn elf_payload_round_trips() {
        round_trip("linux-x64", &fixtures::elf());
    }

    #[test]
    fn pe_payload_round_trips() {
        round_trip("win32-x64", &fixtures::pe());
    }

    /// The Mach-O leg is covered against a REAL template (the host launcher) by
    /// the end-to-end compile, which also execs the result; here we only pin the
    /// scanner against the injector so all three legs share one round-trip
    /// contract. Skipped when no launcher template is around to inject into.
    #[test]
    fn macho_payload_round_trips_when_a_template_is_available() {
        let Some(template) = fixtures::host_macho_template() else {
            return;
        };
        // The HOST's triple, not a hardcoded one: the template comes from this
        // machine's own launcher, and the arch gate would (correctly) refuse an
        // x64 template against a hardcoded darwin-arm64 on an Intel Mac.
        round_trip(&TargetPlatform::host().unwrap().triple(), &template);
    }

    #[test]
    fn a_bare_template_reports_no_payload() {
        assert!(
            find_payload(ContainerFormat::Elf, &fixtures::elf())
                .unwrap()
                .is_none()
        );
        assert!(
            find_payload(ContainerFormat::Pe, &fixtures::pe())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_template_of_the_wrong_format_is_rejected_by_name() {
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let err = inject(&target, &fixtures::pe(), &payload("main.js"), &tmp("wrong")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("PE (Windows)"),
            "should name what it got: {msg}"
        );
        assert!(
            msg.contains("ELF (Linux)"),
            "should name what it needs: {msg}"
        );
        assert!(msg.contains("linux-x64"), "should name the target: {msg}");
    }

    /// Format alone would let this through, and nothing downstream would notice:
    /// the container parses, the payload injects, the static scan finds it back.
    /// The user ships an arm64 binary named x64 and hears about it from whoever
    /// tries to run it, so the machine field has to be checked here.
    #[test]
    fn a_template_for_the_wrong_arch_is_rejected_even_though_the_format_matches() {
        let mut arm = fixtures::elf();
        arm[18..20].copy_from_slice(&0xb7u16.to_le_bytes()); // EM_AARCH64
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let err = inject(&target, &arm, &payload("main.js"), &tmp("badarch")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("arm64"), "should name what it got: {msg}");
        assert!(msg.contains("x64"), "should name what it needs: {msg}");

        // The matching arch is still accepted — the gate must not reject the
        // template every ordinary build uses.
        assert!(
            inject(
                &target,
                &fixtures::elf(),
                &payload("main.js"),
                &tmp("okarch")
            )
            .is_ok(),
            "an x86-64 ELF is what --platform linux-x64 wants"
        );
    }

    #[test]
    fn a_template_with_an_unknown_arch_is_rejected() {
        let mut unknown = fixtures::elf();
        unknown[18..20].copy_from_slice(&0x28u16.to_le_bytes()); // EM_ARM
        let target = TargetPlatform::parse("linux-x64").unwrap();
        let err = inject(&target, &unknown, &payload("main.js"), &tmp("unknownarch")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported or unreadable"), "{msg}");
        assert!(msg.contains("x64"), "{msg}");
    }

    #[test]
    fn macho_template_with_an_unknown_cpu_type_is_rejected() {
        let target = TargetPlatform::parse("darwin-x64").unwrap();
        let err = verify_template(&target, &fixtures::macho(18)).unwrap_err(); // CPU_TYPE_POWERPC
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported or unreadable"), "{msg}");
        assert!(msg.contains("Mach-O (macOS)"), "{msg}");
        assert!(msg.contains("darwin-x64"), "{msg}");
    }

    #[test]
    fn pe_template_with_an_unknown_machine_is_rejected() {
        let mut unknown = fixtures::pe();
        unknown[0x84..0x86].copy_from_slice(&0x1c0u16.to_le_bytes()); // ARM
        let target = TargetPlatform::parse("win32-x64").unwrap();
        let err = verify_template(&target, &unknown).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported or unreadable"), "{msg}");
        assert!(msg.contains("PE (Windows)"), "{msg}");
        assert!(msg.contains("win32-x64"), "{msg}");
    }

    /// The x86_64 Mach-O payload is appended, not sectioned, and the verifier
    /// has to read both shapes.
    ///
    /// libsui writes a real section for arm64 and appends behind a sentinel for
    /// x86_64 — its own documented split, because the Intel path strips the
    /// signature with `codesign` first. Reading only the section made every
    /// darwin-x64 build fail with "the injection did not take": the verifier was
    /// right that it found no section, and wrong that nothing had been injected.
    #[test]
    fn an_appended_payload_is_found_as_well_as_a_sectioned_one() {
        let sentinel = b"<~sui-data~>\xef\xbe\xad\xde";
        let body = b"the payload bytes".as_slice();

        let mut image = b"...a whole Mach-O image...".to_vec();
        image.extend_from_slice(sentinel);
        image.extend_from_slice(&(body.len() as u64).to_le_bytes());
        image.extend_from_slice(body);
        assert_eq!(find_appended_payload(&image), Some(body));

        // The LAST marker wins: an image may legitimately contain the same bytes
        // earlier, which is why libsui builds the marker at run time rather than
        // embedding it as a literal.
        let mut decoyed = b"a decoy: ".to_vec();
        decoyed.extend_from_slice(sentinel);
        decoyed.extend_from_slice(&(4u64).to_le_bytes());
        decoyed.extend_from_slice(b"nope");
        decoyed.extend_from_slice(&image);
        assert_eq!(find_appended_payload(&decoyed), Some(body));

        // Nothing to find, and a truncated trailer, must both be None rather
        // than a panic or a slice out of bounds.
        assert_eq!(find_appended_payload(b"no marker here"), None);
        let mut truncated = sentinel.to_vec();
        truncated.extend_from_slice(&[0u8; 3]);
        assert_eq!(find_appended_payload(&truncated), None);
        let mut overlong = sentinel.to_vec();
        overlong.extend_from_slice(&(999u64).to_le_bytes());
        overlong.extend_from_slice(b"short");
        assert_eq!(find_appended_payload(&overlong), None);
    }

    #[test]
    fn garbage_is_not_mistaken_for_an_executable() {
        assert_eq!(detect_format(b"not an executable at all"), None);
        assert!(find_payload(ContainerFormat::Elf, b"nope").is_err());
        assert!(find_payload(ContainerFormat::Pe, b"nope").is_err());
        assert!(find_payload(ContainerFormat::MachO, b"nope").is_err());
    }

    /// Minimal-but-real ELF and PE images to inject into. Hand-built rather than
    /// checked in as binaries: a fixture that is a few dozen lines of explicit
    /// header fields documents the layout the scanners depend on, and cannot rot
    /// into an opaque blob nobody can regenerate.
    mod fixtures {
        use super::super::MH_MAGIC_64;

        /// A thin 64-bit little-endian Mach-O header up through `cputype`.
        /// Template verification only needs the magic and CPU field; injection
        /// deliberately continues to require a real Mach-O image.
        pub fn macho(cpu_type: u32) -> Vec<u8> {
            let mut m = vec![0u8; 8];
            m[0..4].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
            m[4..8].copy_from_slice(&cpu_type.to_le_bytes());
            m
        }

        /// A 64-bit LE ELF executable with one `PT_LOAD` covering the file. Enough
        /// for libsui's `append`, which only reads the ELF + program headers.
        pub fn elf() -> Vec<u8> {
            const EHSIZE: usize = 64;
            const PHENTSIZE: usize = 56;
            let mut e = vec![0u8; EHSIZE + PHENTSIZE];
            e[0..4].copy_from_slice(b"\x7fELF");
            e[4] = 2; // ELFCLASS64
            e[5] = 1; // ELFDATA2LSB
            e[6] = 1; // EV_CURRENT
            e[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
            e[18..20].copy_from_slice(&0x3eu16.to_le_bytes()); // EM_X86_64
            e[20..24].copy_from_slice(&1u32.to_le_bytes());
            e[24..32].copy_from_slice(&0x400000u64.to_le_bytes()); // e_entry
            e[32..40].copy_from_slice(&(EHSIZE as u64).to_le_bytes()); // e_phoff
            e[52..54].copy_from_slice(&(EHSIZE as u16).to_le_bytes());
            e[54..56].copy_from_slice(&(PHENTSIZE as u16).to_le_bytes());
            e[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

            let p = EHSIZE;
            e[p..p + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
            e[p + 4..p + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R | PF_X
            e[p + 8..p + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
            e[p + 16..p + 24].copy_from_slice(&0x400000u64.to_le_bytes()); // p_vaddr
            e[p + 24..p + 32].copy_from_slice(&0x400000u64.to_le_bytes()); // p_paddr
            let sz = (EHSIZE + PHENTSIZE) as u64;
            e[p + 32..p + 40].copy_from_slice(&sz.to_le_bytes()); // p_filesz
            e[p + 40..p + 48].copy_from_slice(&sz.to_le_bytes()); // p_memsz
            e[p + 48..p + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
            e
        }

        /// A PE32+ executable with a DOS stub, a COFF/optional header, and one
        /// `.text` section. libsui rewrites the resource directory, so no `.rsrc`
        /// is needed up front.
        pub fn pe() -> Vec<u8> {
            const PE_OFF: usize = 0x80;
            const OPT_SIZE: usize = 240; // PE32+ optional header incl. 16 data dirs
            const SECT_ALIGN: u32 = 0x1000;
            const FILE_ALIGN: u32 = 0x200;
            let headers_end = PE_OFF + 24 + OPT_SIZE + 40;
            let text_raw = FILE_ALIGN as usize;
            let text_size = FILE_ALIGN as usize;
            let mut p = vec![0u8; text_raw + text_size];
            assert!(headers_end <= text_raw);

            p[0..2].copy_from_slice(b"MZ");
            p[0x3c..0x40].copy_from_slice(&(PE_OFF as u32).to_le_bytes());
            p[PE_OFF..PE_OFF + 4].copy_from_slice(b"PE\0\0");

            // COFF header.
            let c = PE_OFF + 4;
            p[c..c + 2].copy_from_slice(&0x8664u16.to_le_bytes()); // AMD64
            p[c + 2..c + 4].copy_from_slice(&1u16.to_le_bytes()); // NumberOfSections
            p[c + 16..c + 18].copy_from_slice(&(OPT_SIZE as u16).to_le_bytes());
            p[c + 18..c + 20].copy_from_slice(&0x22u16.to_le_bytes()); // EXECUTABLE | LARGE_ADDRESS_AWARE

            // Optional header (PE32+).
            let o = PE_OFF + 24;
            p[o..o + 2].copy_from_slice(&0x20bu16.to_le_bytes());
            p[o + 16..o + 20].copy_from_slice(&SECT_ALIGN.to_le_bytes()); // AddressOfEntryPoint
            p[o + 24..o + 32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes()); // ImageBase
            p[o + 32..o + 36].copy_from_slice(&SECT_ALIGN.to_le_bytes());
            p[o + 36..o + 40].copy_from_slice(&FILE_ALIGN.to_le_bytes());
            p[o + 40..o + 42].copy_from_slice(&6u16.to_le_bytes()); // MajorOSVersion
            p[o + 48..o + 50].copy_from_slice(&6u16.to_le_bytes()); // MajorSubsystemVersion
            p[o + 56..o + 60].copy_from_slice(&(2 * SECT_ALIGN).to_le_bytes()); // SizeOfImage
            p[o + 60..o + 64].copy_from_slice(&(text_raw as u32).to_le_bytes()); // SizeOfHeaders
            p[o + 68..o + 70].copy_from_slice(&3u16.to_le_bytes()); // SUBSYSTEM_WINDOWS_CUI
            p[o + 108..o + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

            // Section table: one .text covering the single raw block.
            let s = o + OPT_SIZE;
            p[s..s + 5].copy_from_slice(b".text");
            p[s + 8..s + 12].copy_from_slice(&(text_size as u32).to_le_bytes()); // VirtualSize
            p[s + 12..s + 16].copy_from_slice(&SECT_ALIGN.to_le_bytes()); // VirtualAddress
            p[s + 16..s + 20].copy_from_slice(&(text_size as u32).to_le_bytes()); // SizeOfRawData
            p[s + 20..s + 24].copy_from_slice(&(text_raw as u32).to_le_bytes()); // PointerToRawData
            p[s + 36..s + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes()); // CODE|EXECUTE|READ
            p
        }

        /// The host launcher template, when one is around — the Mach-O leg needs a
        /// real signed image (libsui parses far more of it than the other two
        /// legs), and hand-building one would be a second implementation of the
        /// format rather than a fixture.
        pub fn host_macho_template() -> Option<Vec<u8>> {
            if !cfg!(target_os = "macos") {
                return None;
            }
            let path = std::env::var_os("__NUB_LAUNCHER_TEMPLATE")?;
            std::fs::read(path).ok()
        }
    }
}
