//! Locale trimming for the embedded Node's linked-in ICU data.
//!
//! An official Node is built `--with-intl=full-icu`, so one linked-in ICU data
//! package covering ~700 locales accounts for 31.6 MiB of a ~102 MiB stripped
//! binary — 31.7% of the compressed blob a default-shape artifact ships. ICU
//! reaches that package through a bare pointer (`U_ICUDATA_ENTRY_POINT`; a
//! full-icu Node never calls `udata_setCommonData`, which is `#ifdef
//! NODE_HAVE_SMALL_ICU`) and navigates it by its own table of contents. Nothing
//! anywhere records its length. So a SMALLER valid package written at the same
//! offset is served normally, and the zero padding behind it costs ~900 bytes
//! once zstd has seen it.
//!
//! DELIBERATELY FORMAT-BLIND. The package announces itself with a two-byte magic
//! plus a `CmnD` format tag, and that pair occurs exactly once per Node binary on
//! all three container formats — measured on darwin-arm64 Mach-O, linux-x64 ELF
//! and win32-x64 PE for 26.8.1, each yielding one header and an identical 4305-item
//! TOC. So this needs no Mach-O/ELF/PE parser and no symbol table, which is what
//! makes it work at all on ELF: `strip` removes the `.symtab` entry that would
//! otherwise name the blob, and a PE never exports it in the first place.
//!
//! What a trim COSTS is locale output, not APIs. Only locale-shaped resources are
//! dropped, so the charset converters, break iterators, normalization tables and
//! every supplemental resource stay — `Intl.Segmenter`, `TextDecoder('shift_jis')`,
//! `String.prototype.normalize` and time-zone arithmetic are unaffected. A locale
//! that was dropped falls back through ICU's normal chain to `root`, silently,
//! which is why this is opt-in and never a default.

use anyhow::{Result, bail};

/// ICU's `UDataInfo.dataFormat` for a common-data package, at header + 12.
const COMMON_DATA_FORMAT: &[u8; 4] = b"CmnD";
/// `DataHeader.magic1` / `magic2`, at header + 2 and + 3.
const MAGIC: (u8, u8) = (0xDA, 0x27);
/// Every item in the package is padded to this boundary, and the rebuilt package
/// reproduces that rather than packing tight — ICU memory-maps items in place and
/// several formats assume their own alignment.
const ITEM_ALIGN: usize = 16;

/// What a trim did, for the build report.
///
/// Counts only. The bytes freed inside the binary are not carried, because the
/// figure a caller would want — how much smaller the artifact gets — is decided by
/// zstd afterwards, and the raw number is about four times larger. Reporting it
/// promised a saving the build does not deliver.
pub struct TrimReport {
    /// Items retained.
    pub kept: usize,
    /// Items the original package held.
    pub total: usize,
}

/// The one ICU common-data package in `bytes`.
///
/// Scans for the magic rather than a symbol. Every resource INSIDE the package
/// carries the same two-byte magic, so the `CmnD` format tag is what disambiguates
/// — an inner resource declares `ResB`, `Cnv `, `Nrm2` and so on. Requiring exactly
/// one match is the check that this stayed true for the Node in hand, rather than
/// an assumption carried from the versions it was measured on.
fn find_package(bytes: &[u8]) -> Result<usize> {
    let mut found = None;
    let mut at = 0;
    while let Some(hit) = bytes[at..]
        .windows(4)
        .position(|w| w == COMMON_DATA_FORMAT)
        .map(|p| at + p)
    {
        at = hit + 1;
        let Some(header) = hit.checked_sub(12) else {
            continue;
        };
        if bytes[header + 2] != MAGIC.0 || bytes[header + 3] != MAGIC.1 {
            continue;
        }
        if found.replace(header).is_some() {
            bail!("this Node carries more than one ICU data package; refusing to guess which one");
        }
    }
    let Some(header) = found else {
        bail!("no ICU data package found in this Node — it may be a small-icu build");
    };

    // isBigEndian / charsetFamily, at header + 8 and + 9. Every Node target is
    // little-endian ASCII; a package that is not was built by a toolchain whose
    // layout this reader has never seen, so it declines rather than corrupting it.
    if bytes[header + 8] != 0 || bytes[header + 9] != 0 {
        bail!("the ICU data package is not little-endian ASCII; refusing to rewrite it");
    }
    Ok(header)
}

fn u16_at(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}

fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// ICU structural resources whose names collide with the locale-id shape below.
///
/// `res_index` is the whole list and it is not academic: a three-letter first
/// segment plus a lowercase second one is indistinguishable from a language plus a
/// variant subtag (`ca_ES_valencia`), so the shape test alone classifies each
/// tree's locale index as a droppable locale. A prototype that dropped them
/// produced a package that formats but reports `supportedLocalesOf` as empty.
const STRUCTURAL: &[&str] = &["res_index"];

/// Does this item name a locale, as opposed to supplemental data?
///
/// ICU locale ids are a language subtag of two or three lowercase letters plus
/// optional `_`-joined script/region/variant subtags. Every other resource in the
/// package fails that shape — `supplementalData`, `zoneinfo64`, `likelySubtags`,
/// `pool`, `nfkc`, `uemoji`, and the converter and break-iterator files — except
/// the [`STRUCTURAL`] names, which are excluded by hand. Getting this wrong in the
/// permissive direction only keeps a file that could have gone, which costs bytes
/// and never correctness.
fn is_locale(stem: &str) -> bool {
    if STRUCTURAL.contains(&stem) {
        return false;
    }
    let mut parts = stem.split('_');
    let Some(language) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&language.len()) || !language.bytes().all(|c| c.is_ascii_lowercase()) {
        return false;
    }
    parts.all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric()))
}

/// The language subtag a locale-shaped stem belongs to, which is what `--icu`
/// matches on: asking for `zh` keeps `zh_Hans_CN`, and asking for `en` keeps
/// `en_GB`, because a caller naming a language wants that language to work.
fn language_of(stem: &str) -> &str {
    stem.split('_').next().unwrap_or(stem)
}

/// Rewrite the ICU package in `bytes` to hold only `locales` (plus `root` and every
/// non-locale resource), zero-filling what it vacates.
///
/// `bytes` keeps its length: the package is overwritten in place because it is
/// referenced by absolute address, and everything after it in the binary must stay
/// where it is.
pub fn trim(bytes: &mut [u8], locales: &[String]) -> Result<TrimReport> {
    let header = find_package(bytes)?;
    let toc = header + u16_at(bytes, header);
    let count = u32_at(bytes, toc);
    if count == 0 {
        bail!("the ICU data package has an empty table of contents");
    }

    // (name, data offset), both relative to the TOC. The TOC is sorted by name and
    // the data is laid out in that same order, which is what makes an item's length
    // readable as the distance to the next one.
    let entry = |i: usize| -> (String, usize) {
        let at = toc + 4 + 8 * i;
        let name_at = toc + u32_at(bytes, at);
        let end = bytes[name_at..]
            .iter()
            .position(|&c| c == 0)
            .map(|p| name_at + p)
            .unwrap_or(name_at);
        (
            String::from_utf8_lossy(&bytes[name_at..end]).into_owned(),
            u32_at(bytes, at + 4),
        )
    };
    let mut items: Vec<(String, usize)> = (0..count).map(entry).collect();
    items.sort_by_key(|(_, offset)| *offset);

    // The final item's length is the one thing the format does not state, so the
    // rewrite stops at its start: it is never retained, and the bytes it occupies
    // are left untouched rather than zeroed. That forfeits the alphabetically last
    // resource of ~4300 (`zu_ZA.res` on 26.x) and a few dozen bytes of padding, and
    // buys a reader that needs no per-format length parser.
    let end = toc + items[count - 1].1;
    let sizes: Vec<usize> = (0..count - 1)
        .map(|i| items[i + 1].1 - items[i].1)
        .collect();
    // Two entries sharing one offset would make the earlier of them zero-length, and
    // the rewrite would emit it as an empty resource instead of refusing — silent
    // corruption of exactly the kind this whole rewrite has to be trusted not to
    // produce. Not observed in any Node measured; checked because the cost of being
    // wrong is unbounded and the cost of the check is one pass.
    if sizes.contains(&0) {
        bail!("the ICU data package aliases two resources to one offset; refusing to rewrite it");
    }

    let wanted = |name: &str| -> bool {
        let rel = name.split_once('/').map_or(name, |(_, r)| r);
        let base = rel.rsplit('/').next().unwrap_or(rel);
        let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
        if !is_locale(stem) {
            return true;
        }
        stem == "root" || locales.iter().any(|l| l == language_of(stem))
    };

    // Rebuilt in NAME order, which the TOC requires — ICU binary-searches it.
    let mut kept: Vec<(String, usize, usize)> = items[..count - 1]
        .iter()
        .zip(&sizes)
        .filter(|((name, _), _)| wanted(name))
        .map(|((name, offset), size)| (name.clone(), *offset, *size))
        .collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));

    let names_len: usize = kept.iter().map(|(n, _, _)| n.len() + 1).sum();
    let mut cursor = 4 + 8 * kept.len();
    let name_offsets: Vec<usize> = kept
        .iter()
        .map(|(n, _, _)| {
            let at = cursor;
            cursor += n.len() + 1;
            at
        })
        .collect();
    let data_start = cursor.div_ceil(ITEM_ALIGN) * ITEM_ALIGN;

    let mut body = Vec::with_capacity(data_start + names_len);
    body.extend_from_slice(&(kept.len() as u32).to_le_bytes());
    let mut at = data_start;
    let data_offsets: Vec<usize> = kept
        .iter()
        .map(|(_, _, size)| {
            let start = at;
            at += size.div_ceil(ITEM_ALIGN) * ITEM_ALIGN;
            start
        })
        .collect();
    for (name_at, data_at) in name_offsets.iter().zip(&data_offsets) {
        body.extend_from_slice(&(*name_at as u32).to_le_bytes());
        body.extend_from_slice(&(*data_at as u32).to_le_bytes());
    }
    for (name, _, _) in &kept {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
    }
    body.resize(data_start, 0);
    for (_, offset, size) in &kept {
        let from = toc + offset;
        body.extend_from_slice(&bytes[from..from + size]);
        body.resize(body.len().div_ceil(ITEM_ALIGN) * ITEM_ALIGN, 0);
    }

    // Dropping items can only shrink the package, so this cannot fire on any input
    // the filter above produces. It is here because the alternative to failing is
    // writing past `end` into whatever the linker put next.
    if (toc - header) + body.len() > end - header {
        bail!("the trimmed ICU package is larger than the one it replaces");
    }
    bytes[toc..toc + body.len()].copy_from_slice(&body);
    bytes[toc + body.len()..end].fill(0);

    Ok(TrimReport {
        kept: kept.len(),
        total: count,
    })
}

/// Parse an `--icu` value into the locales to retain.
///
/// `full` is the spelling for "change nothing" and is what a bare `--icu` supplies,
/// so the flag has an explicit form for the default as well as the trim.
pub fn parse_locales(value: &str) -> Result<Option<Vec<String>>> {
    if value.eq_ignore_ascii_case("full") {
        return Ok(None);
    }
    let mut out = Vec::new();
    for part in value.split(',') {
        let locale = part.trim();
        if locale.is_empty() {
            bail!("--icu: empty locale in {value:?}");
        }
        // Matched against a language subtag, so this is the whole grammar the flag
        // accepts. A region is fine to write (`--icu=en-US`) and narrows nothing.
        let language = locale.split(['-', '_']).next().unwrap_or(locale);
        if !is_locale(&language.to_ascii_lowercase()) {
            bail!("--icu: {locale:?} is not a locale");
        }
        let language = language.to_ascii_lowercase();
        if !out.contains(&language) {
            out.push(language);
        }
    }
    if out.is_empty() {
        bail!("--icu: no locales given");
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `root` is deliberately absent from the locale list below. Its four letters
    /// fail the language-subtag length bound, so it is classified as supplemental
    /// and retained by that path — and `wanted` names it explicitly as well, so
    /// widening the bound later could not start dropping it. Both trim tests assert
    /// `root.res` survives, which is the property that actually matters.
    #[test]
    fn locale_shape_separates_locales_from_supplemental_data() {
        for locale in ["en", "de", "zh_Hans_CN", "en_GB", "haw", "sr_Latn"] {
            assert!(is_locale(locale), "{locale} is a locale id");
        }
        for other in [
            "root",
            "supplementalData",
            "zoneinfo64",
            "likelySubtags",
            "res_index",
            "pool",
            "nfkc",
            "uemoji",
            "cnvalias",
            "metaZones",
        ] {
            assert!(!is_locale(other), "{other} is supplemental, not a locale");
        }
    }

    #[test]
    fn a_language_keeps_its_regional_variants() {
        assert_eq!(language_of("zh_Hans_CN"), "zh");
        assert_eq!(language_of("en"), "en");
    }

    #[test]
    fn full_is_the_spelling_for_no_trim() {
        assert!(parse_locales("full").unwrap().is_none());
        assert!(parse_locales("FULL").unwrap().is_none());
    }

    #[test]
    fn a_locale_list_normalizes_to_language_subtags() {
        assert_eq!(
            parse_locales("en,de-DE,fr_FR,EN").unwrap().unwrap(),
            vec!["en", "de", "fr"]
        );
    }

    #[test]
    fn a_malformed_locale_list_is_rejected() {
        for bad in ["", "en,,de", "english", "e", "12"] {
            assert!(parse_locales(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// The rewriter against a synthetic package with the real layout: a header, a
    /// name-sorted TOC, a name pool, and 16-byte-aligned items.
    #[test]
    fn a_trim_keeps_supplemental_data_and_the_named_languages() {
        let names = [
            "icudt78l/de.res",
            "icudt78l/en.res",
            "icudt78l/fr.res",
            "icudt78l/root.res",
            "icudt78l/supplementalData.res",
            "icudt78l/zz_ZZ.res",
        ];
        let mut package = build_package(&names);
        let original = package.len();
        // Padded exactly the way a real binary embeds it, so the rewrite has room.
        package.resize(original * 2, 0xAB);

        let report = trim(&mut package, &["en".to_string()]).unwrap();

        // `zz_ZZ` is last by offset and is therefore outside the rewrite by design.
        assert_eq!(report.total, names.len());
        assert_eq!(
            kept_names(&package),
            [
                "icudt78l/en.res",
                "icudt78l/root.res",
                "icudt78l/supplementalData.res"
            ]
        );
        assert!(report.kept < report.total, "the package must shrink");
    }

    #[test]
    fn a_trim_matches_a_language_across_its_regional_variants() {
        let names = [
            "icudt78l/en.res",
            "icudt78l/root.res",
            "icudt78l/zh.res",
            "icudt78l/zh_Hans_CN.res",
            "icudt78l/zz_ZZ.res",
        ];
        let mut package = build_package(&names);
        package.resize(package.len() * 2, 0);
        trim(&mut package, &["zh".to_string()]).unwrap();
        assert_eq!(
            kept_names(&package),
            [
                "icudt78l/root.res",
                "icudt78l/zh.res",
                "icudt78l/zh_Hans_CN.res"
            ]
        );
    }

    #[test]
    fn a_binary_with_no_icu_package_is_refused() {
        let mut not_node = vec![0u8; 4096];
        assert!(trim(&mut not_node, &["en".to_string()]).is_err());
    }

    const HEADER_LEN: usize = 32;

    /// A minimal but structurally faithful common-data package.
    fn build_package(names: &[&str]) -> Vec<u8> {
        let mut header = vec![0u8; HEADER_LEN];
        header[0..2].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        header[2] = MAGIC.0;
        header[3] = MAGIC.1;
        header[12..16].copy_from_slice(COMMON_DATA_FORMAT);

        let mut cursor = 4 + 8 * names.len();
        let name_offsets: Vec<usize> = names
            .iter()
            .map(|n| {
                let at = cursor;
                cursor += n.len() + 1;
                at
            })
            .collect();
        let data_start = cursor.div_ceil(ITEM_ALIGN) * ITEM_ALIGN;

        let mut body = Vec::new();
        body.extend_from_slice(&(names.len() as u32).to_le_bytes());
        for (i, _) in names.iter().enumerate() {
            body.extend_from_slice(&(name_offsets[i] as u32).to_le_bytes());
            body.extend_from_slice(&((data_start + i * ITEM_ALIGN) as u32).to_le_bytes());
        }
        for n in names {
            body.extend_from_slice(n.as_bytes());
            body.push(0);
        }
        body.resize(data_start, 0);
        for i in 0..names.len() {
            body.extend_from_slice(&[i as u8 + 1; ITEM_ALIGN]);
        }
        header.extend_from_slice(&body);
        header
    }

    fn kept_names(package: &[u8]) -> Vec<String> {
        let header = find_package(package).unwrap();
        let toc = header + u16_at(package, header);
        let count = u32_at(package, toc);
        (0..count)
            .map(|i| {
                let name_at = toc + u32_at(package, toc + 4 + 8 * i);
                let end = package[name_at..]
                    .iter()
                    .position(|&c| c == 0)
                    .map(|p| name_at + p)
                    .unwrap();
                String::from_utf8_lossy(&package[name_at..end]).into_owned()
            })
            .collect()
    }
}
