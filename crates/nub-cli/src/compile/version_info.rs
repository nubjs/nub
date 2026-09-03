//! `VS_VERSIONINFO` — the Windows version resource, written from any host.
//!
//! This is the metadata Explorer's Details tab shows and `(Get-Item x.exe).VersionInfo`
//! reads: product name, file/product version, company, copyright, description.
//! It is a `RT_VERSION` resource, so it goes into the same libsui builder chain
//! as `--icon` (see [`super::inject`]) and is written for a Windows target from
//! macOS or Linux exactly as it is from Windows — nothing here shells out to a
//! resource compiler, which is what confines the equivalent feature in other
//! toolchains to a Windows host.
//!
//! ## Why an encoder rather than a struct definition
//!
//! `VS_VERSIONINFO` is a tree of variable-length nodes that share one header
//! shape — `wLength`, `wValueLength`, `wType`, a NUL-terminated UTF-16LE key,
//! then a value and/or children. Three rules decide whether Windows reads the
//! resource or silently ignores it, and only the third is stated plainly in the
//! documentation:
//!
//! 1. **Every node body is 32-bit aligned.** Zero padding goes between the key
//!    and the value, and between the value and the children.
//! 2. **`wLength` is the node's whole span**, its own padding included, so a
//!    parent's length is the sum of its children's.
//! 3. **`wValueLength` changes unit by node type.** For a `String` it counts
//!    *words* — UTF-16 code units, NUL included. Everywhere else it counts
//!    *bytes*. Getting this wrong truncates or overruns the value with no error
//!    anywhere: the resource is still structurally walkable, so Windows reads
//!    garbage or nothing rather than rejecting the file.
//!
//! The layout is not expressible as Rust structs, so [`Node`] builds it and
//! [`parse`] walks it back the way Windows does. That round trip is both the
//! unit test and the post-write check on a real artifact: a length or padding
//! mistake that a writer-only check would miss shows up as a parse failure or a
//! changed value.
//!
//! Reference: [VS_VERSIONINFO], [StringFileInfo], [StringTable], [String], [VarFileInfo],
//! [Var], [VS_FIXEDFILEINFO].
//!
//! [VS_VERSIONINFO]: https://learn.microsoft.com/en-us/windows/win32/menurc/vs-versioninfo
//! [StringFileInfo]: https://learn.microsoft.com/en-us/windows/win32/menurc/stringfileinfo
//! [StringTable]: https://learn.microsoft.com/en-us/windows/win32/menurc/stringtable
//! [String]: https://learn.microsoft.com/en-us/windows/win32/menurc/string-str
//! [VarFileInfo]: https://learn.microsoft.com/en-us/windows/win32/menurc/varfileinfo
//! [Var]: https://learn.microsoft.com/en-us/windows/win32/menurc/var-str
//! [VS_FIXEDFILEINFO]: https://learn.microsoft.com/en-us/windows/win32/api/verrsrc/ns-verrsrc-vs_fixedfileinfo

use std::collections::BTreeMap;

use anyhow::{Result, bail};

/// The `String` keys Windows documents, and the only ones `--metadata` accepts.
///
/// Closed on purpose. An unrecognized key would encode fine and then be invisible
/// everywhere — Explorer shows a fixed set of names and nothing surfaces the rest —
/// so a typo would cost a full build to discover. Alphabetical because that is
/// also the emission order, which keeps a build byte-reproducible regardless of
/// the order the flags were typed in.
pub const KEYS: [&str; 12] = [
    "Comments",
    "CompanyName",
    "FileDescription",
    "FileVersion",
    "InternalName",
    "LegalCopyright",
    "LegalTrademarks",
    "OriginalFilename",
    "PrivateBuild",
    "ProductName",
    "ProductVersion",
    "SpecialBuild",
];

/// `0x0409` (US English) / `0x04B0` (1200, UTF-16). The pair appears twice and
/// must agree: as the `StringTable` key `"040904B0"` and as the `Var`
/// translation word. Nub emits one table, and English/Unicode is what every
/// producer of a single-language resource uses — Windows' resource loader falls
/// back to whatever language is present, so this reads correctly under any UI
/// locale.
const LANG_ID: u16 = 0x0409;
const CODE_PAGE: u16 = 0x04B0;

/// `VS_FIXEDFILEINFO::dwSignature`.
const VS_FFI_SIGNATURE: u32 = 0xFEEF_04BD;
/// `VS_FIXEDFILEINFO::dwStrucVersion` — 1.0.
const VS_FFI_STRUCVERSION: u32 = 0x0001_0000;
/// `VOS_NT_WINDOWS32`.
const VOS_NT_WINDOWS32: u32 = 0x0004_0004;
/// `VFT_APP`.
const VFT_APP: u32 = 0x0000_0001;
/// `VS_FFI_FILEFLAGSMASK` — which `dwFileFlags` bits are meaningful. Nub sets no
/// flags, so the mask is the documented full set and the flags are zero.
const VS_FFI_FILEFLAGSMASK: u32 = 0x0000_003F;

/// A version resource ready to encode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VersionInfo {
    /// `VS_FIXEDFILEINFO::dwFileVersionMS`/`LS`, as `[major, minor, patch, build]`.
    pub file_version: [u16; 4],
    /// `VS_FIXEDFILEINFO::dwProductVersionMS`/`LS`.
    pub product_version: [u16; 4],
    /// The `StringTable` entries. A `BTreeMap` so emission is alphabetical and
    /// a repeated key resolves to one entry rather than two Windows would
    /// disagree about.
    pub strings: BTreeMap<String, String>,
}

impl VersionInfo {
    /// Encode the `RT_VERSION` resource bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut root = Node::new("VS_VERSION_INFO", NodeType::Binary);
        root.value(&self.fixed_file_info());

        // Both children are optional in the format, but a StringTable with no
        // String children is malformed rather than empty, so it is only emitted
        // when something goes in it. VarFileInfo rides along with it because the
        // translation it declares is exactly that table's key.
        if !self.strings.is_empty() {
            let mut table = Node::new(&format!("{LANG_ID:04X}{CODE_PAGE:04X}"), NodeType::Text);
            for (key, value) in &self.strings {
                let mut entry = Node::new(key, NodeType::Text);
                entry.text_value(value);
                table.child(entry);
            }
            let mut string_file_info = Node::new("StringFileInfo", NodeType::Text);
            string_file_info.child(table);
            root.child(string_file_info);

            let mut var = Node::new("Translation", NodeType::Binary);
            let translation = u32::from(LANG_ID) | (u32::from(CODE_PAGE) << 16);
            var.value(&translation.to_le_bytes());
            let mut var_file_info = Node::new("VarFileInfo", NodeType::Text);
            var_file_info.child(var);
            root.child(var_file_info);
        }

        root.finish()
    }

    /// The 52-byte `VS_FIXEDFILEINFO`. `dwFileDate*` stays zero — the field is
    /// vestigial (the SDK's own tools leave it zero) and a build timestamp here
    /// would make otherwise identical inputs produce different bytes.
    fn fixed_file_info(&self) -> [u8; 52] {
        let pack = |v: [u16; 4]| {
            (
                (u32::from(v[0]) << 16) | u32::from(v[1]),
                (u32::from(v[2]) << 16) | u32::from(v[3]),
            )
        };
        let (file_ms, file_ls) = pack(self.file_version);
        let (product_ms, product_ls) = pack(self.product_version);
        let words = [
            VS_FFI_SIGNATURE,
            VS_FFI_STRUCVERSION,
            file_ms,
            file_ls,
            product_ms,
            product_ls,
            VS_FFI_FILEFLAGSMASK,
            0, // dwFileFlags
            VOS_NT_WINDOWS32,
            VFT_APP,
            0, // dwFileSubtype — unused for VFT_APP
            0, // dwFileDateMS
            0, // dwFileDateLS
        ];
        let mut out = [0u8; 52];
        for (slot, word) in out.chunks_exact_mut(4).zip(words) {
            slot.copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// `wType`: 1 for a text value, 0 for binary. It describes the node's own
/// `Value`, so `StringFileInfo` (no value at all) is still Text — that is what
/// `rc.exe` emits and what parsers expect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NodeType {
    Binary = 0,
    Text = 1,
}

/// One `VS_VERSIONINFO` node under construction.
///
/// The three headers are written last, once the body is known, so nothing has to
/// predict a length. Every `Node` ends 32-bit aligned and reports that aligned
/// span as its `wLength`; a parent therefore concatenates children with no
/// arithmetic of its own, and the alignment a reader applies before each sibling
/// is a no-op. (The documentation permits excluding trailing padding from
/// `wLength`; including it is what `rc.exe` and every writer in the wild do, and
/// both are read correctly, since a reader aligns after every node regardless.)
struct Node {
    buf: Vec<u8>,
    value_length: u16,
}

impl Node {
    fn new(key: &str, ty: NodeType) -> Self {
        let mut buf = vec![0u8; 6]; // wLength, wValueLength, wType — backfilled by finish().
        buf[4..6].copy_from_slice(&(ty as u16).to_le_bytes());
        buf.extend(utf16z(key));
        pad4(&mut buf);
        Self {
            buf,
            value_length: 0,
        }
    }

    /// A binary value; `wValueLength` counts BYTES.
    fn value(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        self.value_length = bytes.len() as u16;
        pad4(&mut self.buf);
    }

    /// A text value; `wValueLength` counts WORDS, terminator included. This unit
    /// change is the single likeliest way to produce a resource Windows accepts
    /// structurally and then reads wrongly.
    fn text_value(&mut self, text: &str) {
        let encoded = utf16z(text);
        self.value_length = (encoded.len() / 2) as u16;
        self.buf.extend(encoded);
        pad4(&mut self.buf);
    }

    fn child(&mut self, child: Node) {
        self.buf.extend(child.finish());
    }

    fn finish(mut self) -> Vec<u8> {
        pad4(&mut self.buf);
        let len = u16::try_from(self.buf.len()).unwrap_or(u16::MAX);
        self.buf[0..2].copy_from_slice(&len.to_le_bytes());
        self.buf[2..4].copy_from_slice(&self.value_length.to_le_bytes());
        self.buf
    }
}

/// UTF-16LE, NUL-terminated. Lone surrogates cannot occur: the input is `str`,
/// so `encode_utf16` emits well-formed pairs.
fn utf16z(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity((s.len() + 1) * 2);
    for unit in s.encode_utf16() {
        out.extend(unit.to_le_bytes());
    }
    out.extend([0, 0]);
    out
}

fn pad4(buf: &mut Vec<u8>) {
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

/// Parse `text` as a dotted version into `[major, minor, patch, build]`.
///
/// `VS_FIXEDFILEINFO` holds four `u16`s, and npm versions are neither four
/// components nor necessarily numeric, so leading numeric components are taken
/// and the rest is dropped: `1.2.3` → `1.2.3.0`, `1.2.3-beta.4` → `1.2.3.0`.
/// The string form the user typed still reaches the `FileVersion` string
/// verbatim, so the prerelease tag is not lost — only the numeric block, which
/// has nowhere to put it, is truncated.
pub fn parse_version(text: &str) -> Result<[u16; 4]> {
    let mut out = [0u16; 4];
    for (slot, part) in out.iter_mut().zip(text.split('.')) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        // A component too large for the field is refused rather than wrapped:
        // silently shipping 65536 as 0 is worse than failing the build.
        *slot = digits.parse::<u16>().map_err(|_| {
            anyhow::anyhow!("version component {digits} in {text:?} does not fit in 16 bits")
        })?;
        if digits.len() != part.len() {
            break;
        }
    }
    Ok(out)
}

/// Match `key` against [`KEYS`] case-insensitively, returning the canonical
/// spelling. The canonical form is what gets encoded: Windows matches the key
/// byte-for-byte, so `filedescription` would encode to a field nothing reads.
pub fn canonical_key(key: &str) -> Result<&'static str> {
    KEYS.iter()
        .find(|k| k.eq_ignore_ascii_case(key))
        .copied()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{key:?} is not a Windows version-resource field.\n\
                 \x20\x20Known fields: {}",
                KEYS.join(", ")
            )
        })
}

/// Split one `KEY=VALUE` argument. An empty value is meaningful — it drops a
/// field that would otherwise be defaulted from `package.json` — so it is not an
/// error, matching how an empty `--install-message` suppresses the first-run
/// notice.
pub fn parse_assignment(arg: &str) -> Result<(&'static str, String)> {
    let Some((key, value)) = arg.split_once('=') else {
        bail!("--metadata expects KEY=VALUE, got {arg:?}");
    };
    Ok((canonical_key(key.trim())?, value.to_string()))
}

// ---- reader -------------------------------------------------------------------
//
// Not test-only. `verify_artifact` runs this over the file it just wrote, which
// for a cross-compiled Windows binary is the ONLY check available — nothing on
// the build host can execute it. A version resource the loader cannot reach
// fails silently, so a build that cannot read its own resource back has to fail
// rather than ship.

/// What a walk of an encoded resource recovered. Structurally the same
/// information [`VersionInfo`] encodes, so a round trip compares as equal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedVersionInfo {
    pub file_version: [u16; 4],
    pub product_version: [u16; 4],
    pub strings: BTreeMap<String, String>,
    /// The `Var` translation words, `(language, code page)`.
    pub translations: Vec<(u16, u16)>,
}

/// Walk an encoded `RT_VERSION` resource the way Windows does.
///
/// This is the artifact check, not a re-read of the writer's intent: it navigates
/// by the `wLength`/`wValueLength` fields and the 32-bit alignment rule, so a
/// wrong length or a missing pad puts the cursor in the middle of a node and the
/// parse fails. It is deliberately strict — a reader that resynchronized would
/// hide exactly the defect it exists to catch.
pub(crate) fn parse(bytes: &[u8]) -> Result<ParsedVersionInfo> {
    let root = read_node(bytes, 0)?;
    if root.key != "VS_VERSION_INFO" {
        bail!("root key is {:?}, not VS_VERSION_INFO", root.key);
    }
    if root.value.len() != 52 {
        bail!(
            "VS_FIXEDFILEINFO is {} bytes, expected 52",
            root.value.len()
        );
    }
    let word =
        |i: usize| u32::from_le_bytes(root.value[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
    if word(0) != VS_FFI_SIGNATURE {
        bail!("VS_FIXEDFILEINFO signature is {:#x}", word(0));
    }
    let unpack = |ms: u32, ls: u32| {
        [
            (ms >> 16) as u16,
            (ms & 0xFFFF) as u16,
            (ls >> 16) as u16,
            (ls & 0xFFFF) as u16,
        ]
    };

    let mut out = ParsedVersionInfo {
        file_version: unpack(word(2), word(3)),
        product_version: unpack(word(4), word(5)),
        ..Default::default()
    };

    for child in root.children {
        match child.key.as_str() {
            "StringFileInfo" => {
                for table in child.children {
                    for entry in table.children {
                        // wValueLength is in words here, and the value carries a
                        // NUL terminator plus any alignment padding. Reading the
                        // declared length back is what proves the unit was right.
                        let units = entry.value_length as usize;
                        if entry.value.len() < units * 2 {
                            bail!(
                                "String {:?} declares {units} words but carries {} bytes",
                                entry.key,
                                entry.value.len()
                            );
                        }
                        let text = decode_utf16(&entry.value[..units * 2])?;
                        out.strings
                            .insert(entry.key, text.trim_end_matches('\0').to_string());
                    }
                }
            }
            "VarFileInfo" => {
                for var in child.children {
                    for word in var.value.chunks_exact(4) {
                        let v = u32::from_le_bytes(word.try_into().expect("4 bytes"));
                        out.translations
                            .push(((v & 0xFFFF) as u16, (v >> 16) as u16));
                    }
                }
            }
            other => bail!("unexpected VS_VERSIONINFO child {other:?}"),
        }
    }
    Ok(out)
}

struct RawNode {
    key: String,
    value: Vec<u8>,
    value_length: u16,
    children: Vec<RawNode>,
    /// Bytes this node occupies BEFORE the caller re-aligns — its own `wLength`.
    length: usize,
}

fn read_node(bytes: &[u8], at: usize) -> Result<RawNode> {
    let word = |off: usize| -> Result<u16> {
        match off.checked_add(2).filter(|e| *e <= bytes.len()) {
            Some(e) => Ok(u16::from_le_bytes(
                bytes[off..e].try_into().expect("2 bytes"),
            )),
            None => bail!("truncated at offset {off}"),
        }
    };
    let length = word(at)? as usize;
    let value_length = word(at + 2)?;
    let ty = word(at + 4)?;
    let end = at
        .checked_add(length)
        .filter(|e| *e <= bytes.len() && length >= 6)
        .ok_or_else(|| anyhow::anyhow!("node at {at} declares an out-of-range length {length}"))?;

    let mut cursor = at + 6;
    let mut units = Vec::new();
    loop {
        let unit = word(cursor)?;
        cursor += 2;
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let key = String::from_utf16(&units)?;
    cursor = align4(cursor);

    // wValueLength is words for a text node and bytes for a binary one — the
    // asymmetry the writer has to get right, checked here by reading it back the
    // same way.
    let value_bytes = if ty == NodeType::Text as u16 {
        (value_length as usize) * 2
    } else {
        value_length as usize
    };
    let value_end = cursor
        .checked_add(value_bytes)
        .filter(|e| *e <= end)
        .ok_or_else(|| {
            anyhow::anyhow!("node {key:?} declares a value that overruns its own length")
        })?;
    let value = bytes[cursor..value_end].to_vec();
    cursor = align4(value_end);

    let mut children = Vec::new();
    while cursor < end {
        let child = read_node(bytes, cursor)?;
        cursor = align4(cursor + child.length);
        children.push(child);
    }
    if cursor != end {
        bail!("node {key:?} children overrun its declared length");
    }

    Ok(RawNode {
        key,
        value,
        value_length,
        children,
        length,
    })
}

/// Align an offset up to the next 32-bit boundary. Offsets are resource-relative
/// throughout, which is what the alignment is measured against — not the
/// enclosing node.
fn align4(cursor: usize) -> usize {
    cursor.div_ceil(4) * 4
}

fn decode_utf16(bytes: &[u8]) -> Result<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes(c.try_into().expect("2 bytes")))
        .collect();
    Ok(String::from_utf16(&units)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VersionInfo {
        VersionInfo {
            file_version: [1, 2, 3, 4],
            product_version: [1, 2, 0, 0],
            strings: [
                ("ProductName", "Example App"),
                ("FileDescription", "Does the thing"),
                ("CompanyName", "Example Inc."),
                ("FileVersion", "1.2.3-beta.4"),
                ("OriginalFilename", "example.exe"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        }
    }

    /// The round trip is the encoder's real test: [`parse`] navigates purely by
    /// the declared lengths and the alignment rule, so it only reaches the end
    /// if every length and pad is right.
    #[test]
    fn encoded_resource_parses_back_to_what_went_in() {
        let info = sample();
        let parsed = parse(&info.encode()).expect("the encoded resource must parse");
        assert_eq!(parsed.file_version, [1, 2, 3, 4]);
        assert_eq!(parsed.product_version, [1, 2, 0, 0]);
        assert_eq!(parsed.strings, info.strings);
        assert_eq!(parsed.translations, vec![(0x0409, 0x04B0)]);
    }

    /// A value whose character count makes the node land off a 4-byte boundary is
    /// where padding arithmetic breaks. Sweeping the lengths covers every
    /// residue for both the key and the value.
    #[test]
    fn every_key_and_value_length_residue_round_trips() {
        for key_len in 1..=8 {
            for value_len in 0..=8 {
                let key = "K".repeat(key_len);
                let value = "v".repeat(value_len);
                let info = VersionInfo {
                    file_version: [9, 9, 9, 9],
                    product_version: [9, 9, 9, 9],
                    strings: [(key.clone(), value.clone())].into_iter().collect(),
                };
                let bytes = info.encode();
                assert_eq!(
                    bytes.len() % 4,
                    0,
                    "resource for key {key_len} / value {value_len} is not 32-bit aligned"
                );
                let parsed = parse(&bytes).unwrap_or_else(|e| {
                    panic!("key {key_len} / value {value_len} failed to parse: {e}")
                });
                assert_eq!(
                    parsed.strings.get(&key).map(String::as_str),
                    Some(value.as_str()),
                    "key {key_len} / value {value_len} did not survive the round trip"
                );
            }
        }
    }

    /// Byte-level pins on the one header every reader looks at first. A change
    /// here means the resource moved, not that a field was renamed.
    #[test]
    fn the_root_header_matches_the_documented_layout() {
        let bytes = sample().encode();
        assert_eq!(
            u16::from_le_bytes(bytes[0..2].try_into().unwrap()) as usize,
            bytes.len(),
            "wLength must span the whole resource"
        );
        assert_eq!(
            u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
            52,
            "wValueLength must be sizeof(VS_FIXEDFILEINFO)"
        );
        assert_eq!(
            u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
            0,
            "the root value is binary"
        );
        // 6 header bytes + 32 for L"VS_VERSION_INFO" = 38, padded to 40.
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            VS_FFI_SIGNATURE,
            "VS_FIXEDFILEINFO must start on the 32-bit boundary after the key"
        );
    }

    /// A String's `wValueLength` is in WORDS while every other node's is in
    /// BYTES. Encoding a byte count there is the classic defect, and it is
    /// invisible without checking the field directly, because a reader that
    /// doubled the unit would still walk the tree.
    #[test]
    fn string_value_length_is_measured_in_words() {
        let info = VersionInfo {
            file_version: [1, 0, 0, 0],
            product_version: [1, 0, 0, 0],
            strings: [("ProductName".to_string(), "abcd".to_string())]
                .into_iter()
                .collect(),
        };
        let bytes = info.encode();
        // "ProductName" + NUL = 5 words; the value is 4 chars + NUL.
        let at = find_key(&bytes, "ProductName").expect("the String node must be present");
        assert_eq!(
            u16::from_le_bytes(bytes[at + 2..at + 4].try_into().unwrap()),
            5,
            "wValueLength counts UTF-16 code units including the terminator"
        );
    }

    #[test]
    fn a_version_string_keeps_its_numeric_head_and_drops_the_rest() {
        assert_eq!(parse_version("1.2.3").unwrap(), [1, 2, 3, 0]);
        assert_eq!(parse_version("1.2.3.4").unwrap(), [1, 2, 3, 4]);
        assert_eq!(parse_version("1.2.3-beta.4").unwrap(), [1, 2, 3, 0]);
        assert_eq!(parse_version("2026.1").unwrap(), [2026, 1, 0, 0]);
        assert_eq!(parse_version("").unwrap(), [0, 0, 0, 0]);
        assert_eq!(parse_version("v1.2.3").unwrap(), [0, 0, 0, 0]);
        assert!(parse_version("70000.0.0").is_err());
    }

    #[test]
    fn an_unknown_metadata_key_is_refused_and_a_known_one_is_canonicalized() {
        assert_eq!(
            parse_assignment("productname=My App").unwrap(),
            ("ProductName", "My App".to_string())
        );
        assert_eq!(
            parse_assignment("FileDescription=").unwrap(),
            ("FileDescription", String::new())
        );
        let err = parse_assignment("Copyright=x").unwrap_err().to_string();
        assert!(
            err.contains("LegalCopyright"),
            "the error must list the real field names: {err}"
        );
        assert!(parse_assignment("ProductName").is_err());
    }

    /// Offset of the `String` node whose key is `key`, by scanning for the
    /// UTF-16 key and backing up over the three header words.
    fn find_key(bytes: &[u8], key: &str) -> Option<usize> {
        let needle = utf16z(key);
        bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|at| at - 6)
    }
}
