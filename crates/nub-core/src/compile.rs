//! The `nub compile` payload container — the single source of truth for the
//! byte layout embedded in a compiled executable, shared by the compile-time
//! writer (`nub-cli`) and the runtime reader (`nub-launcher`).
//!
//! The container is one opaque blob written into the target executable at
//! compile time and read back from the launcher's own image at runtime. Where it
//! lands is per-format (Mach-O `__SUI,__nubc` section / ELF `.note.sui` note /
//! PE `RT_RCDATA` resource — see [`ContainerFormat`]), but the blob itself is
//! byte-identical across formats. It carries: a small JSON manifest (shape,
//! entry name, Node version/triple, the embedded Node's content hash), the
//! bundled app files (entry + Rolldown chunks), and — for the default `embed`
//! shape — the zstd-compressed Node binary. `smol` shape carries no Node blob.
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

/// The payload's name in every container format — a Mach-O section under
/// libsui's fixed `__SUI` segment (names are capped at 16 bytes), the name field
/// of an ELF `SUI` note, and a PE `RT_RCDATA` resource (libsui upper-cases it on
/// both write and read, so the asymmetry is invisible here). Read back at runtime
/// via `libsui::find_section`, whose implementation is selected by the launcher's
/// own `target_os`.
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
    /// The concrete Node version this binary targets. Embed: the EXACT embedded
    /// version. Smol: the acceptance FLOOR the launcher enforces (`discovered >=
    /// node_version`) and the version it provisions when nothing is found.
    pub node_version: String,
    /// Smol only: the original Node requirement (`>=20`, `^22`, `24`, `lts`) the
    /// artifact was compiled against, recorded for provenance. `None` when the
    /// target was a bare exact version (embed always None).
    #[serde(default)]
    pub node_range: Option<String>,
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
    /// The text the launcher shows while a FIRST RUN extracts the embedded Node
    /// or provisions one — the only thing between the user and a multi-second
    /// silent startup. Baked at compile time because the launcher has no app
    /// name of its own. `None` — the default, when `--install-message` is
    /// omitted — means stay quiet. Shown on a TTY only, both shapes; the
    /// launcher chooses between its box and one plain line from the terminal.
    #[serde(default)]
    pub install_message: Option<String>,
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

    // Cumulative offsets via checked_add: a corrupted section can carry garbage
    // lengths that would wrap under the launcher's panic=abort/no-overflow-checks
    // release profile, sneak past a `len < end` guard, and then panic-abort on the
    // slice. Overflow → clean Err instead.
    let corrupt = || anyhow::anyhow!("compiled payload corrupted (bad length)");
    let manifest_end = HEADER_LEN.checked_add(manifest_len).ok_or_else(corrupt)?;
    let app_end = manifest_end.checked_add(app_len).ok_or_else(corrupt)?;
    let node_end = app_end.checked_add(node_len).ok_or_else(corrupt)?;
    if bytes.len() < node_end {
        bail!("compiled payload truncated (body)");
    }

    // `.get(..)` rather than direct indexing: even with the checks above, never let
    // a bad range panic — a corrupted payload must error, not abort the process.
    let manifest: Manifest =
        serde_json::from_slice(bytes.get(HEADER_LEN..manifest_end).ok_or_else(corrupt)?)
            .context("parsing the compiled payload manifest")?;

    let app_region = bytes.get(manifest_end..app_end).ok_or_else(corrupt)?;
    let app_files = decode_app_region(app_region)?;
    let node_blob = bytes.get(app_end..node_end).ok_or_else(corrupt)?;

    Ok(PayloadView {
        manifest,
        app_files,
        node_blob,
    })
}

fn decode_app_region(region: &[u8]) -> Result<Vec<(String, &[u8])>> {
    // Every advance is checked_add + `.get(..)`: a corrupted region must Err, never
    // panic (debug overflow-check) or wrap-then-mis-slice. `end = p.checked_add(n)`
    // → None on overflow → clean error.
    let corrupt = || anyhow::anyhow!("compiled payload corrupted (app region)");
    let mut p = 0usize;
    let take = |len: usize, p: &mut usize| -> Result<&[u8]> {
        let end = p.checked_add(len).ok_or_else(corrupt)?;
        let slice = region.get(*p..end).ok_or_else(corrupt)?;
        *p = end;
        Ok(slice)
    };
    let count = u32::from_le_bytes(take(4, &mut p)?.try_into().unwrap());
    // No `with_capacity(count)`: a corrupt count would pre-allocate hugely. Each
    // iteration is bounded by the region, so a bad count errors out quickly.
    let mut files = Vec::new();
    for _ in 0..count {
        let name_len = u16::from_le_bytes(take(2, &mut p)?.try_into().unwrap()) as usize;
        let name = std::str::from_utf8(take(name_len, &mut p)?)
            .context("app file name is not utf-8")?
            .to_string();
        let data_len = u64::from_le_bytes(take(8, &mut p)?.try_into().unwrap()) as usize;
        let data = take(data_len, &mut p)?;
        files.push((name, data));
    }
    Ok(files)
}

// ---- compile targets ----------------------------------------------------------
//
// A compile target is chosen at compile time and decides three things the rest of
// the pipeline branches on: which container format the payload is injected into,
// which Node dist build is embedded, and which prebuilt launcher template is used.
// The runtime half needs none of this — the launcher's `find_section` is picked by
// its OWN `target_os` — so this model is compile-time-only and lives here purely
// because the container format and the manifest's `triple` are the same concern.

/// The executable container format of a compile target. One compile-time
/// injection leg per variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerFormat {
    /// macOS. Payload = a `__SUI,__nubc` section; the artifact is ad-hoc signed
    /// (Gatekeeper rejects an invalid signature, and arm64 requires one at all).
    MachO,
    /// Linux. Payload = a `SUI` note in an appended `PT_NOTE`; never signed.
    Elf,
    /// Windows. Payload = an `RT_RCDATA` resource; no Authenticode by default.
    Pe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    Darwin,
    Linux,
    Win32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArch {
    X64,
    Arm64,
}

/// A `nub compile` target platform, in nub's own `<os>-<arch>[-musl]` vocabulary
/// — the same tokens the per-platform npm packages use, so `--platform linux-x64`
/// and `@nubjs/nub-linux-x64` (which will carry that triple's launcher template)
/// name the same thing. Deliberately NOT a Rust target triple: the user-facing
/// vocabulary is Node's/npm's, not rustc's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetPlatform {
    pub os: TargetOs,
    pub arch: TargetArch,
    /// Linux-musl. The official Node dist publishes no musl build, so this routes
    /// the embedded Node to unofficial-builds and selects the musl launcher.
    pub musl: bool,
}

/// Every triple `nub compile` accepts, in help/error order.
pub const SUPPORTED_TRIPLES: &[&str] = &[
    "darwin-arm64",
    "darwin-x64",
    "linux-arm64",
    "linux-arm64-musl",
    "linux-x64",
    "linux-x64-musl",
    "win32-arm64",
    "win32-x64",
];

impl TargetPlatform {
    /// The host, or `None` when nub is running on a platform `nub compile` has no
    /// target vocabulary for (nub ships per-platform binaries, so in practice this
    /// is only reachable from a locally-built nub on an exotic host).
    pub fn host() -> Option<Self> {
        let os = match std::env::consts::OS {
            "macos" => TargetOs::Darwin,
            "linux" => TargetOs::Linux,
            "windows" => TargetOs::Win32,
            _ => return None,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => TargetArch::X64,
            "aarch64" => TargetArch::Arm64,
            _ => return None,
        };
        let musl = os == TargetOs::Linux && crate::version_management::host_is_musl();
        Some(Self { os, arch, musl })
    }

    /// Parse a `--platform` value. `None` for anything outside
    /// [`SUPPORTED_TRIPLES`]; the caller renders the list in the error.
    pub fn parse(token: &str) -> Option<Self> {
        let (base, musl) = match token.strip_suffix("-musl") {
            Some(base) => (base, true),
            None => (token, false),
        };
        let (os, arch) = base.split_once('-')?;
        let os = match os {
            "darwin" => TargetOs::Darwin,
            "linux" => TargetOs::Linux,
            "win32" => TargetOs::Win32,
            _ => return None,
        };
        let arch = match arch {
            "x64" => TargetArch::X64,
            "arm64" => TargetArch::Arm64,
            _ => return None,
        };
        // musl is a Linux-only libc; `darwin-x64-musl` must not round-trip.
        if musl && os != TargetOs::Linux {
            return None;
        }
        Some(Self { os, arch, musl })
    }

    /// The canonical triple string — round-trips [`parse`](Self::parse) and is
    /// what the manifest records.
    pub fn triple(&self) -> String {
        let os = match self.os {
            TargetOs::Darwin => "darwin",
            TargetOs::Linux => "linux",
            TargetOs::Win32 => "win32",
        };
        let arch = match self.arch {
            TargetArch::X64 => "x64",
            TargetArch::Arm64 => "arm64",
        };
        if self.musl {
            format!("{os}-{arch}-musl")
        } else {
            format!("{os}-{arch}")
        }
    }

    pub fn format(&self) -> ContainerFormat {
        match self.os {
            TargetOs::Darwin => ContainerFormat::MachO,
            TargetOs::Linux => ContainerFormat::Elf,
            TargetOs::Win32 => ContainerFormat::Pe,
        }
    }

    /// The executable suffix for this target (`.exe` on Windows).
    pub fn exe_suffix(&self) -> &'static str {
        match self.os {
            TargetOs::Win32 => ".exe",
            _ => "",
        }
    }

    /// Whether this target is the host — the gate on everything that can only be
    /// done natively: running the produced binary's self-probe, shelling out to
    /// `codesign`, verifying a stripped Node by executing it.
    pub fn is_host(&self) -> bool {
        Self::host().is_some_and(|h| h == *self)
    }
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
            node_range: None,
            triple: "darwin-arm64".into(),
            node_sha256: "abc123".into(),
            app_sha256: "def456".into(),
            minify: true,
            install_message: Some("Setting up app".into()),
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
            node_range: Some(">=20".into()),
            triple: "darwin-arm64".into(),
            node_sha256: String::new(),
            app_sha256: "aa".into(),
            minify: false,
            install_message: None,
        };
        let app = vec![("main.js".to_string(), b"console.log(1)".to_vec())];
        let blob = encode(&manifest, &app, &[]);
        let view = decode(&blob).unwrap();
        assert_eq!(view.manifest.shape, Shape::Smol);
        assert!(view.node_blob.is_empty());
        assert_eq!(view.manifest.node_range.as_deref(), Some(">=20"));
        assert_eq!(view.app_files[0].1, b"console.log(1)");
    }

    #[test]
    fn rejects_bad_magic() {
        let bad = vec![0u8; HEADER_LEN + 4];
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn rejects_overflowing_lengths_without_panicking() {
        // A corrupted section with a garbage length must Err cleanly, never
        // panic (the launcher's release profile is panic=abort with overflow
        // checks off, so an unchecked `+` would abort the process).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(FORMAT_VERSION);
        bytes.extend_from_slice(&[0u8; 3]);
        bytes.extend_from_slice(&0u32.to_le_bytes()); // manifest_len = 0
        bytes.extend_from_slice(&0u64.to_le_bytes()); // app_len = 0
        bytes.extend_from_slice(&u64::MAX.to_le_bytes()); // node_len = MAX → overflow
        // (PayloadView isn't Debug — it holds the raw section slices — so match
        // rather than unwrap_err.)
        match decode(&bytes) {
            Err(e) => assert!(
                format!("{e:#}").contains("corrupted"),
                "expected a clean corruption error, got: {e:#}"
            ),
            Ok(_) => panic!("an overflowing length must not decode successfully"),
        }
    }

    #[test]
    fn every_supported_triple_parses_and_round_trips() {
        for token in SUPPORTED_TRIPLES {
            let p = TargetPlatform::parse(token)
                .unwrap_or_else(|| panic!("{token} is listed as supported but does not parse"));
            assert_eq!(&p.triple(), token);
        }
    }

    #[test]
    fn triples_map_to_their_container_format() {
        let fmt = |t: &str| TargetPlatform::parse(t).unwrap().format();
        assert_eq!(fmt("darwin-arm64"), ContainerFormat::MachO);
        assert_eq!(fmt("linux-x64-musl"), ContainerFormat::Elf);
        assert_eq!(fmt("win32-x64"), ContainerFormat::Pe);
        assert_eq!(
            TargetPlatform::parse("win32-x64").unwrap().exe_suffix(),
            ".exe"
        );
        assert_eq!(TargetPlatform::parse("linux-x64").unwrap().exe_suffix(), "");
    }

    #[test]
    fn rejects_triples_outside_the_supported_set() {
        // musl is Linux-only; the rest are typos or platforms nub has no launcher for.
        for bad in [
            "darwin-x64-musl",
            "win32-arm64-musl",
            "linux-x86",
            "freebsd-x64",
            "darwin",
            "",
            "linux-x64-gnu",
        ] {
            assert!(
                TargetPlatform::parse(bad).is_none(),
                "{bad:?} must not parse as a compile target"
            );
        }
    }

    #[test]
    fn the_host_is_a_supported_triple_and_is_its_own_target() {
        let host = TargetPlatform::host().expect("a dev/CI host is always a supported platform");
        assert!(SUPPORTED_TRIPLES.contains(&host.triple().as_str()));
        assert!(host.is_host());
    }
}
