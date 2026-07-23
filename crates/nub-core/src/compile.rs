//! The `nub compile` payload container — the single source of truth for the
//! byte layout embedded in a compiled executable, shared by the compile-time
//! writer (`nub-cli`) and the runtime reader (`nub-launcher`).
//!
//! The container is one opaque blob written into a Mach-O `__SUI,__nubc` section
//! (libsui's segment) at compile time and read back from the launcher's own
//! image at runtime. It carries: a small JSON manifest (shape, entry name, Node
//! version/triple, the embedded Node's content hash), the bundled app files
//! (entry + Rolldown chunks), and — for the default `embed` shape — the
//! zstd-compressed Node binary. `smol` shape carries no Node blob.
//!
//! Design points:
//! - **Decode borrows, never copies the Node blob.** [`decode`] returns slices
//!   into the section bytes, so a warm run (Node already cached) never copies the
//!   ~26 MB Node payload — it only reads the manifest + the tiny app files.
//! - **No compression/hashing here.** The Node blob arrives already zstd-19'd
//!   from the writer; the launcher decompresses it. Content hashes are computed
//!   at compile time and carried verbatim in the manifest as cache keys — the
//!   whole payload is inside the code-signed binary, so those hashes are
//!   integrity-protected without re-verification at load.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The Mach-O section name (≤16 bytes) libsui writes the payload into, under its
/// fixed `__SUI` segment. Read back at runtime via `libsui::find_section`.
pub const SECTION_NAME: &str = "__nubc";

const MAGIC: &[u8; 4] = b"NUBC";
const FORMAT_VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 3 + 4 + 8 + 8; // magic, ver, pad, manifest_len, app_len, node_len

/// The two artifact shapes. `Embed` carries a compressed Node; `Smol` discovers
/// or provisions one at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    /// Default: embed a stripped, zstd-19 Node, decompressed once to the cache.
    Embed,
    /// No embedded Node: discover an existing one, else provision at first run.
    Smol,
}

/// The compile-time metadata carried alongside the payload bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub shape: Shape,
    /// The entry file's name within the extracted app dir (e.g. `main.js`).
    pub entry: String,
    /// The concrete Node version this binary targets (down-level + embed/provision).
    pub node_version: String,
    /// The target triple this binary was compiled for (e.g. `darwin-arm64`).
    pub triple: String,
    /// Content hash (hex) of the DECOMPRESSED embedded Node — the cache key for
    /// the extracted binary, and what lets the launcher dedup against an already
    /// cached official Node. Empty for `smol`.
    #[serde(default)]
    pub node_sha256: String,
    /// Content hash (hex) of the app payload region — the app-extraction cache key.
    #[serde(default)]
    pub app_sha256: String,
    /// Whether the bundle was minified (informational; affects nothing at runtime).
    #[serde(default)]
    pub minify: bool,
}

/// A borrowed view over a decoded payload — app files and the Node blob are
/// slices into the caller's section bytes, so no large copy happens on decode.
pub struct PayloadView<'a> {
    pub manifest: Manifest,
    /// `(name, bytes)` for every bundled app file, in write order (entry first).
    pub app_files: Vec<(String, &'a [u8])>,
    /// The zstd-compressed Node binary (`embed` shape), or empty (`smol`).
    pub node_blob: &'a [u8],
}

/// Encode a payload into the container blob written to the Mach-O section.
///
/// `node_blob` is the already-zstd-compressed Node binary (empty for `smol`).
pub fn encode(manifest: &Manifest, app_files: &[(String, Vec<u8>)], node_blob: &[u8]) -> Vec<u8> {
    let manifest_bytes = serde_json::to_vec(manifest).expect("manifest serializes");

    // App region: [file_count u32][per file: name_len u16, name, data_len u64, data].
    let mut app_region = Vec::new();
    app_region.extend_from_slice(&(app_files.len() as u32).to_le_bytes());
    for (name, data) in app_files {
        app_region.extend_from_slice(&(name.len() as u16).to_le_bytes());
        app_region.extend_from_slice(name.as_bytes());
        app_region.extend_from_slice(&(data.len() as u64).to_le_bytes());
        app_region.extend_from_slice(data);
    }

    let mut out =
        Vec::with_capacity(HEADER_LEN + manifest_bytes.len() + app_region.len() + node_blob.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&[0u8; 3]);
    out.extend_from_slice(&(manifest_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&(app_region.len() as u64).to_le_bytes());
    out.extend_from_slice(&(node_blob.len() as u64).to_le_bytes());
    out.extend_from_slice(&manifest_bytes);
    out.extend_from_slice(&app_region);
    out.extend_from_slice(node_blob);
    out
}

/// Decode a container blob, borrowing the app-file and Node-blob bytes from
/// `bytes` (which the launcher holds for the process lifetime — the mapped image).
pub fn decode(bytes: &[u8]) -> Result<PayloadView<'_>> {
    if bytes.len() < HEADER_LEN {
        bail!("compiled payload truncated (header)");
    }
    if &bytes[0..4] != MAGIC {
        bail!("compiled payload has a bad magic");
    }
    if bytes[4] != FORMAT_VERSION {
        bail!(
            "compiled payload format version {} is unsupported",
            bytes[4]
        );
    }
    let manifest_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let app_len = u64::from_le_bytes(bytes[12..20].try_into().unwrap()) as usize;
    let node_len = u64::from_le_bytes(bytes[20..28].try_into().unwrap()) as usize;

    let mut off = HEADER_LEN;
    let manifest_end = off + manifest_len;
    let app_end = manifest_end + app_len;
    let node_end = app_end + node_len;
    if bytes.len() < node_end {
        bail!("compiled payload truncated (body)");
    }

    let manifest: Manifest = serde_json::from_slice(&bytes[off..manifest_end])
        .context("parsing the compiled payload manifest")?;
    off = manifest_end;

    let app_region = &bytes[off..app_end];
    let app_files = decode_app_region(app_region)?;
    let node_blob = &bytes[app_end..node_end];

    Ok(PayloadView {
        manifest,
        app_files,
        node_blob,
    })
}

fn decode_app_region(region: &[u8]) -> Result<Vec<(String, &[u8])>> {
    let mut p = 0usize;
    let read_u32 = |region: &[u8], p: &mut usize| -> Result<u32> {
        let end = *p + 4;
        let v = region
            .get(*p..end)
            .context("app region truncated (u32)")?
            .try_into()
            .unwrap();
        *p = end;
        Ok(u32::from_le_bytes(v))
    };
    let count = read_u32(region, &mut p)?;
    let mut files = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name_len = {
            let end = p + 2;
            let v: [u8; 2] = region
                .get(p..end)
                .context("app region truncated (name_len)")?
                .try_into()
                .unwrap();
            p = end;
            u16::from_le_bytes(v) as usize
        };
        let name = {
            let end = p + name_len;
            let s = std::str::from_utf8(region.get(p..end).context("app region truncated (name)")?)
                .context("app file name is not utf-8")?
                .to_string();
            p = end;
            s
        };
        let data_len = {
            let end = p + 8;
            let v: [u8; 8] = region
                .get(p..end)
                .context("app region truncated (data_len)")?
                .try_into()
                .unwrap();
            p = end;
            u64::from_le_bytes(v) as usize
        };
        let data = {
            let end = p + data_len;
            let d = region.get(p..end).context("app region truncated (data)")?;
            p = end;
            d
        };
        files.push((name, data));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_embed_shape() {
        let manifest = Manifest {
            shape: Shape::Embed,
            entry: "main.js".into(),
            node_version: "24.10.0".into(),
            triple: "darwin-arm64".into(),
            node_sha256: "abc123".into(),
            app_sha256: "def456".into(),
            minify: true,
        };
        let app = vec![
            ("main.js".to_string(), b"import './c.js'\n".to_vec()),
            ("c.js".to_string(), b"export const x=1\n".to_vec()),
        ];
        let node = vec![9u8; 4096];
        let blob = encode(&manifest, &app, &node);

        let view = decode(&blob).unwrap();
        assert_eq!(view.manifest.shape, Shape::Embed);
        assert_eq!(view.manifest.entry, "main.js");
        assert_eq!(view.app_files.len(), 2);
        assert_eq!(view.app_files[0].0, "main.js");
        assert_eq!(view.app_files[0].1, b"import './c.js'\n");
        assert_eq!(view.app_files[1].0, "c.js");
        assert_eq!(view.node_blob, &node[..]);
    }

    #[test]
    fn roundtrips_smol_shape_no_node() {
        let manifest = Manifest {
            shape: Shape::Smol,
            entry: "main.js".into(),
            node_version: "24.10.0".into(),
            triple: "darwin-arm64".into(),
            node_sha256: String::new(),
            app_sha256: "aa".into(),
            minify: false,
        };
        let app = vec![("main.js".to_string(), b"console.log(1)".to_vec())];
        let blob = encode(&manifest, &app, &[]);
        let view = decode(&blob).unwrap();
        assert_eq!(view.manifest.shape, Shape::Smol);
        assert!(view.node_blob.is_empty());
        assert_eq!(view.app_files[0].1, b"console.log(1)");
    }

    #[test]
    fn rejects_bad_magic() {
        let bad = vec![0u8; HEADER_LEN + 4];
        assert!(decode(&bad).is_err());
    }
}
