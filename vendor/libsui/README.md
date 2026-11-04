# `libsui`

[![Crates.io](https://img.shields.io/crates/v/libsui.svg)](https://crates.io/crates/libsui)

_Sui (सुई)_ is a injection tool for executable formats (ELF, PE, Mach-O) that
allows you to embed files into existing binary and extract them at runtime.

It produces valid executables that can be code signed on macOS and Windows.

[Documentation](https://docs.rs/libsui) | [Usage](cli.rs)

## Usage

```
cargo add libsui
```

Embedding data into binaries:

```rust
use libsui::{Macho, PortableExecutable};

let exe = std::fs::read("tests/exec_mach64")?;
let mut out = std::fs::File::create("out")?;

Macho::from(exe)?
    .write_section("__hello", b"Hello, World!".to_vec())?
    .build(&mut out)?;

let exe = std::fs::read("tests/exec_pe64")?;
let mut out = std::fs::File::create("out.exe")?;

PortableExecutable::from(exe)?
    .write_resource("hello.txt", b"Hello, World!".to_vec())?
    .build(&mut out)?;
```

Extracting from self:

```rust
use libsui::find_section;

let data = find_section("hello.txt")?;
```

## Design

### Mach-O

#### ARM64 (Apple Silicon)

Resource is added as section in a new segment, load commands are updated and
offsets are adjusted. `__LINKEDIT` is kept at the end of the file.

It is similar to linker's `-sectcreate,__FOO,__foo,hello.txt` option.

Note that `Macho::build` will invalidate existing code signature. on Apple
sillicon, kernel refuses to run executables with bad signatures.

#### x86_64 (Intel Mac)

For Intel Macs, a simpler approach is used:

1. The existing code signature is stripped using `codesign --remove-signature`
2. Data is appended to the end of the binary with a sentinel marker.
3. The `__LINKEDIT` segment and symbol table sizes are patched to account for
   the appended data.

At runtime, the data is extracted by searching backwards from the end of the
file for the sentinel marker.

#### Code Signing

Use `Macho::build_and_sign` to re-sign the binary with ad-hoc signature. See
[`apple_codesign.rs`](./apple_codesign.rs) for details. This is similar to
`codesign -s - ./out` command.

```rust
Macho::from(exe)?
    .write_section("__sect", data)?
    .build_and_sign(&mut out)?;
```

```
$ codesign -d -vvv ./out

Executable=/Users/divy/gh/sui/out
Identifier=a.out
Format=Mach-O thin (arm64)
CodeDirectory v=20400 size=10238 flags=0x20002(adhoc,linker-signed) hashes=317+0 location=embedded
Hash type=sha256 size=32
CandidateCDHash sha256=6b1abb20f2291dd9b0dbcd0659a918cb2d0e6b18
CandidateCDHashFull sha256=6b1abb20f2291dd9b0dbcd0659a918cb2d0e6b1876153efa17f90dc8b3a8f177
Hash choices=sha256
CMSDigest=6b1abb20f2291dd9b0dbcd0659a918cb2d0e6b1876153efa17f90dc8b3a8f177
CMSDigestType=2
CDHash=6b1abb20f2291dd9b0dbcd0659a918cb2d0e6b18
Signature=adhoc
Info.plist=not bound
TeamIdentifier=not set
Sealed Resources=none
Internal requirements=none
```

### PE

Resource is added into a new PE resource directory as `RT_RCDATA` type and
extracted using `FindResource` and `LoadResource` at run-time.

### ELF

Data is stored in ELF notes using a section of type `SHT_NOTE` and a program
header of type `PT_NOTE`. The `.note.sui` section is placed inside a `PT_LOAD`
segment so it is mapped at runtime, while the `PT_NOTE` program header points
to the same mapped range. Existing ELF notes are preserved by appending the
new SUI note to the note segment data.

At run-time, data is extracted from note segments in memory using
`dl_iterate_phdr`.

## Packers (UPX, etc.)

Runtime executable packers like [UPX](https://upx.github.io/) compress the
original program and prepend a small stub that decompresses it back into
memory at startup. The packed file on disk no longer contains the original
section layout — sections, segments, notes, and (in many cases) resources
are replaced by the packer's own envelope, so libsui's runtime lookup will
fail on a packed binary that was injected *before* packing.

There is no fully packer-proof embedding scheme that works the way
NativeAOT does on .NET: NativeAOT owns both the producer and the runtime
extractor, so it can decompress its own payload after the stub has
restored memory. libsui is a generic injection tool and has no hook into
the unpacker, so it cannot recover data that the packer has hidden inside
its compressed envelope.

The recommended workaround is **pack first, then inject**. libsui's
writers operate on the final on-disk layout, so injecting after packing
adds a fresh section/resource/note that the unpacker's stub never
touches:

| Format        | Behavior of `libsui` injection on top of a packed binary               |
|---------------|------------------------------------------------------------------------|
| PE (UPX)      | A new `RT_RCDATA` resource is added to `.rsrc`; UPX preserves the resource directory, so `find_section` continues to work. |
| ELF (UPX)     | A new `.note.sui` is placed in a fresh `PT_LOAD` past the packed payload, with a matching `PT_NOTE` program header. `dl_iterate_phdr` enumerates the program headers after the unpacker hands control back, so the note remains visible. |
| Mach-O arm64  | A new `__SUI` segment is appended after `__LINKEDIT`; `getsectdata` reads it directly from the mapped image. (Most packers on Apple Silicon are blocked by code signing anyway.) |
| Mach-O x86_64 | Data is appended past the end of the file with a sentinel marker; this survives any packer that does not truncate or rewrite the file tail. |

Injecting *before* packing is not supported: the packer is free to
discard or relocate the embedded data, and on ELF/Mach-O the runtime
lookup will not find it. If you must pack a binary that already contains
libsui data, treat it as a build step ordering issue and move the
injection to after the pack step.

## Testing

This crate is fuzzed with LLVM's libFuzzer. See [fuzz/](fuzz/).

`exec_*` executables in `tests/` are compiled from `tests/exec.rs`:

```
rustc exec.rs -o exec_elf64 --target x86_64-unknown-linux-gnu
```

## License

MIT
