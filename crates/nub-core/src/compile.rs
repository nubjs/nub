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
//! app files (entry + Rolldown chunks + verbatim `--include`d assets), and — for the default `embed`
//! shape — the zstd-compressed Node binary. `smol` shape carries no Node blob.
//!
//! Design points:
//! - **Decode borrows, never copies the Node blob.** [`decode`] returns slices
//!   into the section bytes, so a warm run (Node already cached) never copies the
//!   ~26 MB Node payload — it only reads the manifest + the tiny app files.
//! - **No compression/hashing here.** The Node blob arrives already zstd-19'd
//!   from the writer; the launcher decompresses it. Content hashes are computed
//!   at compile time and carried verbatim in the manifest as cache keys. The
//!   launcher trusts the payload mapped from its own executable; publisher
//!   signing, when applied, is what protects distribution integrity. These
//!   hashes identify extracted cache content rather than authenticating it.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The payload's name in every container format — a Mach-O section under
/// libsui's fixed `__SUI` segment (names are capped at 16 bytes), the name field
/// of an ELF `SUI` note, and a PE `RT_RCDATA` resource (libsui upper-cases it on
/// both write and read, so the asymmetry is invisible here). Read back at runtime
/// via `libsui::find_section`, whose implementation is selected by the launcher's
/// own `target_os`.
pub const SECTION_NAME: &str = "__nubc";

/// The fixed private file name for compile-time bootstrap code at the payload
/// root. This is an internal compiler/launcher ABI: it is never manifest data
/// and must not vary with the entry's layout.
pub const COMPILE_BOOTSTRAP_NAME: &str = "__nub_compile_bootstrap.cjs";

const MAGIC: &[u8; 4] = b"NUBC";
const FORMAT_VERSION_V1: u8 = 1;
const FORMAT_VERSION: u8 = 2;
const HEADER_LEN_V1: usize = 4 + 1 + 3 + 4 + 8 + 8; // magic, ver, pad, manifest_len, app_len, node_len
const HEADER_LEN_V2: usize = HEADER_LEN_V1 + 8; // + license_len

/// Bit 31 of a V2 logical file's `data_index` marks the file executable.
///
/// The index addresses physical data records, each of which costs at least an
/// 8-byte length header, so a payload cannot approach 2^31 of them — the bit is
/// free, and spending it leaves the per-file record's shape, the header, and the
/// format version untouched. A payload with no executable file is therefore
/// byte-identical to what the pre-flag encoder produced. V1 has no such bit, so
/// a V1 payload decodes as all-non-executable, exactly as it did before.
const EXEC_FLAG: u32 = 1 << 31;
const DATA_INDEX_MASK: u32 = EXEC_FLAG - 1;

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
    /// The entry file's path within the extracted app dir, `/`-separated. Flat
    /// (`main.js`) unless `--include` anchored the app dir above the entry's own
    /// directory, which nests it (`src/main.js`).
    pub entry: String,
    /// The concrete Node version this binary targets. Embed: the EXACT embedded
    /// version. Smol: the acceptance floor for a non-range target, or the oldest
    /// version satisfying [`Self::smol_version_range`].
    pub node_version: String,
    /// What `smol` DOWNLOADS when discovery finds nothing: the newest release
    /// satisfying the compiled pin that can also RUN this payload, resolved at
    /// compile time.
    ///
    /// Deliberately NOT an acceptance bound — discovery uses the explicit exact,
    /// range, or floor policy stored beside it. This exists because
    /// provisioning the floor means `--target 26` fetches 26.0.0, the OLDEST
    /// acceptable release and several stale on the day it is built, when a bare
    /// major plainly asks for the newest in that line. Resolved here rather than
    /// in the launcher to keep version lookup out of a component that is
    /// deliberately minimal.
    ///
    /// The capability qualifier is load-bearing, not decorative. The newest
    /// satisfying release is not always one that can run the payload, so when
    /// [`Self::requires_augmentation`] is set and that release lacks the API, this
    /// field is left EMPTY and the launcher provisions the floor — which the build
    /// gate has already proven capable. Recording it anyway produced a binary that
    /// built clean and then refused its own download on the user's machine.
    ///
    /// Empty for embed, where `node_version` is already exact, and in legacy
    /// manifests, where the launcher falls back to the floor.
    #[serde(default)]
    pub provision_version: String,
    /// Whether `smol` requires a discovered Node to match `node_version` exactly
    /// rather than accepting the legacy floor. Missing in legacy manifests means
    /// floor mode.
    #[serde(default)]
    pub smol_exact_target: bool,
    /// The normalized semver range a `smol` launcher enforces when selecting an
    /// installed Node. Empty for exact, major/minor, alias, and embed targets,
    /// and in legacy manifests whose launcher retains floor behavior.
    ///
    /// The compiler derives this from its parsed target rather than carrying raw
    /// user text. The launcher parses it through that same semver implementation
    /// before relaxing the cache for an external Node, so malformed payload data
    /// fails closed.
    #[serde(default)]
    pub smol_version_range: String,
    /// Whether the payload installs a `module.registerHooks` shim, which a
    /// discovered Node must therefore provide. Set for a `--smol` build carrying
    /// `--external` or `--allow-dynamic-import`.
    ///
    /// The build-time gate cannot stand in for this. It sees the pin's FLOOR, and
    /// a floor in 22.15.0..23.0.0 admits the 23.0–23.4 band, which sorts above the
    /// floor but predates `registerHooks` on the 23.x line — so `--target ">=22.15"`
    /// passed the build and the artifact died at launch on `registerHooks is not a
    /// function`. Recording the REQUIREMENT lets the launcher apply it to the
    /// candidate it actually found, which closes the class rather than the shapes
    /// the floor happens to catch.
    ///
    /// Absent in legacy manifests, where `false` reproduces their behavior exactly
    /// — not because they carry no shim (a legacy `--smol --external` payload
    /// carries one; that IS the bug) but because no launcher that read them ever
    /// applied this check. `false` is what they were already doing.
    #[serde(default)]
    pub requires_augmentation: bool,
    /// The target triple this binary was compiled for (e.g. `darwin-arm64`).
    pub triple: String,
    /// Content hash (hex) of the DECOMPRESSED embedded Node — the cache key for
    /// the extracted binary, and what lets the launcher dedup against an already
    /// cached official Node. Empty for `smol`.
    #[serde(default)]
    pub node_sha256: String,
    /// BLAKE3 (hex) of the DECOMPRESSED embedded Node, retained as format headroom
    /// for a future verification or migration policy. The current launcher's warm
    /// path checks [`Self::node_size`] instead and does not read this field.
    /// Empty for `smol`.
    #[serde(default)]
    pub node_blake3: String,
    /// Byte length of the DECOMPRESSED embedded Node — the WARM-START check, in
    /// place of re-hashing the whole file.
    ///
    /// The hash was never establishing IDENTITY. The extraction lives at
    /// `compile-node/<version>-<short_key(node_sha256)>`, so the content digest is
    /// already in the PATH — Deno closed the equivalent PR unmerged with exactly
    /// that reasoning, and there the proposed check was a free string compare where
    /// nub was paying O(107 MB) on every launch. All the re-hash added was detecting
    /// a change SINCE extraction, and against a same-uid attacker it adds nothing:
    /// the extraction dir is already owner-only and non-group-writable, while that
    /// same uid can simply rewrite the artifact binary itself.
    ///
    /// What a size check DOES catch is the failure actually observed in the field —
    /// an OS or antivirus sweep truncating or clearing cached files, which is what
    /// drove `vercel/pkg` to ship and then revert an always-copy warm path. macOS
    /// `~/Library/Caches` is purgeable, so nub is exposed to the same class.
    ///
    /// Zero for `smol`, and zero in a payload written before this field existed —
    /// the launcher falls back to the digest then, so old artifacts keep working.
    #[serde(default)]
    pub node_size: u64,
    /// Whether each app file's bytes are individually zstd-compressed.
    ///
    /// Per FILE, not per region: this module is a pure container (see the header —
    /// "No compression/hashing here"), and nub-core's `zstd` is an optional dep, so
    /// the writer compresses and the reader decompresses exactly as they already do
    /// for the Node blob. Measured cost of that choice: 71.6% saved vs 77.4% for a
    /// single whole-region stream over 50 small files — 6 points, bought back by
    /// lazy per-file decode and no new unconditional dependency.
    ///
    /// `app_sha256` stays over the UNCOMPRESSED bytes: it is the extraction cache
    /// key, and keying it on compressed output would move it with the zstd version.
    #[serde(default)]
    pub app_compressed: bool,
    /// Content hash (hex) of the app payload region — the app-extraction cache key.
    #[serde(default)]
    pub app_sha256: String,
    /// Whether the bundle was minified (informational; affects nothing at runtime).
    #[serde(default)]
    pub minify: bool,
    /// The text the launcher shows while a FIRST RUN extracts the embedded Node
    /// or provisions one — the only thing between the user and a multi-second
    /// silent startup. Baked at compile time because the launcher has no app
    /// name of its own. `None` means stay quiet, which `nub compile` never
    /// emits: an omitted `--install-message` takes the default text instead.
    /// Shown on a TTY only, both shapes; the launcher chooses between its box
    /// and one plain line from the terminal.
    #[serde(default)]
    pub install_message: Option<String>,
    /// Node CLI flags baked in by `--node-flag`, prepended to the launcher's own
    /// injected flags so a later user flag wins where Node takes the last
    /// occurrence.
    ///
    /// The equivalent of Bun's `compile.execArgv`. Some libraries only work
    /// behind an experimental flag, and there is otherwise no way for the
    /// publisher of a compiled binary to supply one — `NODE_OPTIONS` is the
    /// user's channel, not theirs, and Node rejects several flags there anyway.
    /// Not validated against the target Node at build time: a foreign target's
    /// Node is not present to ask, so a flag it rejects fails loudly at startup.
    #[serde(default)]
    pub node_flags: Vec<String>,
    /// Whether every module this artifact can execute was already parsed by the
    /// bundler — no `--external` packages, no retained computed `import()`, and
    /// no verbatim payload file at all (an `--include`, an emitted asset copy, a
    /// native island), so nothing the artifact ships resolves through the real
    /// module loader at runtime. Presence, not extension: the CJS loader parses
    /// an exact-path `require()` of any unknown extension as JS.
    ///
    /// What it buys: the launcher skips the feature matrix's `UnflagArgv` rows
    /// (today `--js-defer-import-eval`). Those flags exist to enable in-progress
    /// JS syntax in files Node parses at runtime, which `nub run` must assume
    /// anywhere in the graph — but a sealed bundle has no such files, and the
    /// bundler already lowered the graph it emitted (Rolldown evaluates
    /// `import defer` eagerly, so the syntax never survives into the payload).
    /// Measured on Node 26.7: the V8 flag alone costs ~6 ms of warm start,
    /// because a non-default V8 flag invalidates the snapshot fast path.
    ///
    /// Residual channels a sealed graph still has — `createRequire()` on a
    /// computed path — behave exactly as they do under bare `node app.js`,
    /// which carries none of these flags either, so skipping them cannot make
    /// the artifact diverge from the plain-Node baseline.
    ///
    /// Absent in legacy manifests; `false` preserves their launcher behavior
    /// (always inject) exactly.
    #[serde(default)]
    pub sealed_module_graph: bool,
    /// Whether `--hide-console` built this artifact, which the launcher needs to
    /// know at RUN time and cannot infer from the image it is running as.
    ///
    /// The subsystem flip alone does not hide anything here. Bun and Deno ARE the
    /// process they hide, so a GUI subsystem is their whole fix; nub's launcher
    /// SPAWNS Node, and Windows allocates a console for a console-subsystem child
    /// whose parent has none — so the flash simply moves from the launcher to
    /// Node. Closing that means passing `CREATE_NO_WINDOW` on the spawn, and the
    /// launcher has no other way to tell a hidden build from an ordinary one:
    /// nothing readable at run time distinguishes them, and reading its own PE
    /// header back would be both slower and a layering inversion.
    ///
    /// Absent in legacy manifests, where `false` is exactly what they were doing.
    #[serde(default)]
    pub hide_console: bool,
}

/// One logical file in the payload. `B` is `Vec<u8>` on the writing side and
/// `&[u8]` on the reading side, which borrows out of the mapped image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFile<B> {
    pub name: String,
    pub bytes: B,
    /// Whether the launcher restores an executable bit when it extracts this
    /// file. Set from the source file's Unix mode (`mode & 0o111 != 0`) for
    /// content embedded verbatim — `--include`d assets and native-package
    /// islands — and false for everything the compiler generates.
    ///
    /// Windows has no such mode, so a Windows BUILD host always records false;
    /// see [`AppFile::from_source_mode`].
    pub executable: bool,
}

impl<B> AppFile<B> {
    /// A generated payload file: compiler output, never executable.
    pub fn plain(name: impl Into<String>, bytes: B) -> Self {
        Self {
            name: name.into(),
            bytes,
            executable: false,
        }
    }

    /// A payload file embedded verbatim from disk, carrying the source file's
    /// executable bit.
    ///
    /// `mode` is a Unix mode, or `None` on a host that has none. Windows expresses
    /// executability through ACLs and a filename extension rather than a mode bit,
    /// so a Windows build host has nothing to read and records false — sniffing
    /// content or guessing from an extension would make the same source tree
    /// produce different payloads on different build machines. The cost is
    /// confined to cross-compiling a Unix target FROM Windows, where an embedded
    /// helper arrives non-executable; native Windows targets are unaffected
    /// because the launcher has no bit to restore there either.
    pub fn from_source_mode(name: impl Into<String>, bytes: B, mode: Option<u32>) -> Self {
        Self {
            name: name.into(),
            bytes,
            executable: mode.is_some_and(|mode| mode & 0o111 != 0),
        }
    }
}

/// A borrowed view over a decoded payload — app files and the Node blob are
/// slices into the caller's section bytes, so no large copy happens on decode.
pub struct PayloadView<'a> {
    pub manifest: Manifest,
    /// Every bundled app file, in write order. The entry is located by NAME
    /// (`Manifest::entry`), never by position — `--external` appends a generated
    /// wrapper that becomes the entry, so it is written last.
    pub app_files: Vec<AppFile<&'a [u8]>>,
    /// The zstd-compressed Node binary (`embed` shape), or empty (`smol`).
    pub node_blob: &'a [u8],
    /// The zstd-19-compressed aggregate root `LICENSE` from the exact official
    /// Node distribution embedded above. Empty for `smol` and V1 payloads.
    pub node_license_blob: &'a [u8],
}

impl PayloadView<'_> {
    /// Whether extracting this payload materializes something the application can
    /// spawn. Decides whether the app cache is data storage or an execution
    /// surface — a `smol` artifact running a Node it found elsewhere may keep its
    /// payload on a `noexec` volume only while that stays true.
    pub fn has_executable_file(&self) -> bool {
        self.app_files.iter().any(|file| file.executable)
    }
}

/// Whose path rules a payload name has to survive.
///
/// The machine that BUILDS an artifact and the machine that RUNS it are different
/// platforms under `--platform`, and `std::path` only ever parses the rules of the
/// platform it was compiled for. `a\..\..\x` is one ordinary filename on Unix and
/// a traversal on Windows, so a Unix host cross-compiling `win32-*` must judge
/// names by the TARGET's rules or it bakes a name its own launcher will refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameRules {
    Unix,
    Windows,
}

impl NameRules {
    /// The rules of the platform this binary runs on — what the launcher uses,
    /// since a launcher only ever executes on the target it was built for.
    pub const HOST: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };
}

/// Whether a payload file name is safe to join under the extraction dir on the
/// HOST. See [`is_safe_relative_name_for`]; this is the launcher's spelling.
pub fn is_safe_relative_name(name: &str) -> bool {
    is_safe_relative_name_for(NameRules::HOST, name)
}

/// Whether a payload file name is safe to join under the extraction dir under
/// `rules`: every component a plain name — no root/prefix, no `..`, no `.`, no
/// leading/trailing/doubled separator. Nested `a/b.js` is allowed; anything that
/// could escape, name a device, or collide after the platform normalizes it is not.
///
/// Lives here because BOTH sides of the container must agree. The launcher
/// enforces it while extracting (a corrupted or hostile section must not write
/// outside the cache), and `nub compile` checks it while building — since
/// `--include` derives payload names from user-supplied paths, a name the
/// launcher would reject has to fail the BUILD rather than ship an executable
/// that aborts on the user's machine.
///
/// Parsed by hand rather than through `std::path` precisely because `std::path`
/// is fixed to the host at compile time; see [`NameRules`].
pub fn is_safe_relative_name_for(rules: NameRules, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let separators: &[char] = match rules {
        NameRules::Unix => &['/'],
        // Win32 resolves `\` and `/` identically, so a name is split on both
        // before any component is judged — this is the whole cross-compile hole.
        NameRules::Windows => &['/', '\\'],
    };
    for part in name.split(separators) {
        // Empty = a leading, trailing, or doubled separator: an absolute path, a
        // UNC prefix, or a spelling the two platforms disagree about.
        if part.is_empty() || part == "." || part == ".." || part.contains('\0') {
            return false;
        }
        if rules == NameRules::Windows && !is_safe_windows_component(part) {
            return false;
        }
    }
    true
}

/// The Win32-only half: names that are not traversals but still do not denote the
/// file the payload says they do.
fn is_safe_windows_component(part: &str) -> bool {
    // Win32 strips trailing dots and spaces, so `main.js. ` and `main.js` open the
    // SAME file. Two payload entries differing only there pass the build's
    // distinct-name check and then overwrite each other at extraction — including
    // an `--include`d file landing on top of a compiled chunk.
    if part.ends_with('.') || part.ends_with(' ') {
        return false;
    }
    // `:` opens a drive or an NTFS alternate data stream; the rest are outright
    // illegal in a Win32 name, so the launcher's write fails on the user's machine
    // with nothing pointing back at the build that caused it.
    if part.contains(|c: char| {
        matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 0x20
    }) {
        return false;
    }
    // A reserved DOS device resolves to the device at ANY directory depth and
    // through any extension — writing `CON.txt` writes to the console.
    !is_dos_device(part.split('.').next().unwrap_or(part))
}

fn is_dos_device(stem: &str) -> bool {
    // Win32's device matcher strips trailing spaces off the segment before
    // comparing, so `con .txt` is the console as much as `CON.txt` is — and the
    // whole-component trailing-space rule above cannot see a space that sits
    // before the extension.
    let upper = stem.trim_end_matches(' ').to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    // Microsoft's reserved list in full, superscripts included. Erring wide costs
    // nothing — no real project names a file `COM0` — while erring narrow ships a
    // binary that writes to a device.
    let Some(rest) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    matches!(
        rest,
        "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

/// Encode a payload into the container blob written to the Mach-O section.
///
/// `node_blob` is the already-zstd-compressed Node binary (empty for `smol`).
///
/// This compatibility wrapper produces V2 but carries no license. New compiled
/// artifacts must use [`encode_with_license`]; keeping this spelling lets
/// independent container-scanner fixtures continue to build V2 payloads without
/// pretending they are redistributable Node artifacts.
pub fn encode(manifest: &Manifest, app_files: &[AppFile<Vec<u8>>], node_blob: &[u8]) -> Vec<u8> {
    encode_with_license(manifest, app_files, node_blob, &[])
}

/// Encode a V2 payload with the already-zstd-19-compressed official Node
/// `LICENSE`. V2 stores each physical app byte sequence once: later logical
/// names with identical bytes reference the first record, while the logical
/// file order (and consequently `app_sha256`) stays unchanged.
pub fn encode_with_license(
    manifest: &Manifest,
    app_files: &[AppFile<Vec<u8>>],
    node_blob: &[u8],
    node_license_blob: &[u8],
) -> Vec<u8> {
    let manifest_bytes = serde_json::to_vec(manifest).expect("manifest serializes");

    // V2 app region:
    // [logical file_count u32][physical data_count u32]
    // [per logical file: name_len u16, name, data_index u32 (bit 31 = executable)]
    // [per physical data: data_len u64, data]
    // The input bodies outlive encoding, so keys borrow them rather than cloning
    // every distinct asset just to find aliases. Slice hashing/equality is by
    // bytes, and first occurrence still decides each record's index. The
    // executable bit rides the LOGICAL record, so two names sharing one body may
    // still differ in it.
    let mut data_indices: HashMap<&[u8], u32> = HashMap::new();
    let mut files = Vec::with_capacity(app_files.len());
    let mut data_records: Vec<&[u8]> = Vec::new();
    for file in app_files {
        let data = file.bytes.as_slice();
        let index = match data_indices.get(data) {
            Some(&index) => index,
            None => {
                let index = u32::try_from(data_records.len())
                    .ok()
                    .filter(|index| *index < EXEC_FLAG)
                    .expect("too many app data records");
                data_indices.insert(data, index);
                data_records.push(data);
                index
            }
        };
        let field = if file.executable {
            index | EXEC_FLAG
        } else {
            index
        };
        files.push((&file.name, field));
    }

    let mut app_region = Vec::new();
    app_region.extend_from_slice(
        &u32::try_from(files.len())
            .expect("too many app files")
            .to_le_bytes(),
    );
    app_region.extend_from_slice(
        &u32::try_from(data_records.len())
            .expect("too many app data records")
            .to_le_bytes(),
    );
    for (name, field) in files {
        app_region.extend_from_slice(
            &u16::try_from(name.len())
                .expect("app file name exceeds payload format limit")
                .to_le_bytes(),
        );
        app_region.extend_from_slice(name.as_bytes());
        app_region.extend_from_slice(&field.to_le_bytes());
    }
    for data in data_records {
        app_region.extend_from_slice(
            &u64::try_from(data.len())
                .expect("app file length exceeds payload format limit")
                .to_le_bytes(),
        );
        app_region.extend_from_slice(data);
    }

    let mut out = Vec::with_capacity(
        HEADER_LEN_V2
            + manifest_bytes.len()
            + app_region.len()
            + node_blob.len()
            + node_license_blob.len(),
    );
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&[0u8; 3]);
    out.extend_from_slice(
        &u32::try_from(manifest_bytes.len())
            .expect("manifest exceeds payload format limit")
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u64::try_from(app_region.len())
            .expect("app region exceeds payload format limit")
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u64::try_from(node_blob.len())
            .expect("Node blob exceeds payload format limit")
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u64::try_from(node_license_blob.len())
            .expect("license blob exceeds payload format limit")
            .to_le_bytes(),
    );
    out.extend_from_slice(&manifest_bytes);
    out.extend_from_slice(&app_region);
    out.extend_from_slice(node_blob);
    out.extend_from_slice(node_license_blob);
    out
}

/// Decode a container blob, borrowing the app-file and Node-blob bytes from
/// `bytes` (which the launcher holds for the process lifetime — the mapped image).
pub fn decode(bytes: &[u8]) -> Result<PayloadView<'_>> {
    if bytes.len() < HEADER_LEN_V1 {
        bail!("compiled payload truncated (header)");
    }
    if &bytes[0..4] != MAGIC {
        bail!("compiled payload has a bad magic");
    }
    let version = bytes[4];
    if version != FORMAT_VERSION_V1 && version != FORMAT_VERSION {
        bail!(
            "compiled payload format version {} is unsupported",
            bytes[4]
        );
    }
    let corrupt = || anyhow::anyhow!("compiled payload corrupted (bad length)");
    let header_len = match version {
        FORMAT_VERSION_V1 => HEADER_LEN_V1,
        FORMAT_VERSION => HEADER_LEN_V2,
        _ => unreachable!("version was checked above"),
    };
    if bytes.len() < header_len {
        bail!("compiled payload truncated (header)");
    }
    let manifest_len = usize::try_from(u32::from_le_bytes(bytes[8..12].try_into().unwrap()))
        .map_err(|_| corrupt())?;
    let app_len = usize::try_from(u64::from_le_bytes(bytes[12..20].try_into().unwrap()))
        .map_err(|_| corrupt())?;
    let node_len = usize::try_from(u64::from_le_bytes(bytes[20..28].try_into().unwrap()))
        .map_err(|_| corrupt())?;
    let license_len = if version == FORMAT_VERSION {
        usize::try_from(u64::from_le_bytes(bytes[28..36].try_into().unwrap()))
            .map_err(|_| corrupt())?
    } else {
        0
    };

    // Cumulative offsets via checked_add: a corrupted section can carry garbage
    // lengths that would wrap under the launcher's panic=abort/no-overflow-checks
    // release profile, sneak past a `len < end` guard, and then panic-abort on the
    // slice. Overflow → clean Err instead.
    let manifest_end = header_len.checked_add(manifest_len).ok_or_else(corrupt)?;
    let app_end = manifest_end.checked_add(app_len).ok_or_else(corrupt)?;
    let node_end = app_end.checked_add(node_len).ok_or_else(corrupt)?;
    let license_end = node_end.checked_add(license_len).ok_or_else(corrupt)?;
    if bytes.len() < license_end {
        bail!("compiled payload truncated (body)");
    }

    // `.get(..)` rather than direct indexing: even with the checks above, never let
    // a bad range panic — a corrupted payload must error, not abort the process.
    let manifest: Manifest =
        serde_json::from_slice(bytes.get(header_len..manifest_end).ok_or_else(corrupt)?)
            .context("parsing the compiled payload manifest")?;

    let app_region = bytes.get(manifest_end..app_end).ok_or_else(corrupt)?;
    let app_files = match version {
        FORMAT_VERSION_V1 => decode_app_region_v1(app_region)?,
        FORMAT_VERSION => decode_app_region_v2(app_region)?,
        _ => unreachable!("version was checked above"),
    };
    let node_blob = bytes.get(app_end..node_end).ok_or_else(corrupt)?;
    let node_license_blob = bytes.get(node_end..license_end).ok_or_else(corrupt)?;

    Ok(PayloadView {
        manifest,
        app_files,
        node_blob,
        node_license_blob,
    })
}

fn decode_app_region_v1(region: &[u8]) -> Result<Vec<AppFile<&[u8]>>> {
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
        let data_len = usize::try_from(u64::from_le_bytes(take(8, &mut p)?.try_into().unwrap()))
            .map_err(|_| corrupt())?;
        let data = take(data_len, &mut p)?;
        // V1 predates per-file metadata: nothing it carries can be executable.
        files.push(AppFile::plain(name, data));
    }
    Ok(files)
}

fn decode_app_region_v2(region: &[u8]) -> Result<Vec<AppFile<&[u8]>>> {
    let corrupt = || anyhow::anyhow!("compiled payload corrupted (app region)");
    let mut p = 0usize;
    let take = |len: usize, p: &mut usize| -> Result<&[u8]> {
        let end = p.checked_add(len).ok_or_else(corrupt)?;
        let slice = region.get(*p..end).ok_or_else(corrupt)?;
        *p = end;
        Ok(slice)
    };
    let file_count = u32::from_le_bytes(take(4, &mut p)?.try_into().unwrap());
    let data_count = u32::from_le_bytes(take(4, &mut p)?.try_into().unwrap());
    let mut names = Vec::new();
    for _ in 0..file_count {
        let name_len = u16::from_le_bytes(take(2, &mut p)?.try_into().unwrap()) as usize;
        let name = std::str::from_utf8(take(name_len, &mut p)?)
            .context("app file name is not utf-8")?
            .to_string();
        let field = u32::from_le_bytes(take(4, &mut p)?.try_into().unwrap());
        names.push((name, field & DATA_INDEX_MASK, field & EXEC_FLAG != 0));
    }
    let mut records = Vec::new();
    for _ in 0..data_count {
        let data_len = usize::try_from(u64::from_le_bytes(take(8, &mut p)?.try_into().unwrap()))
            .map_err(|_| corrupt())?;
        records.push(take(data_len, &mut p)?);
    }
    if p != region.len() {
        bail!("compiled payload corrupted (app region)");
    }
    names
        .into_iter()
        .map(|(name, index, executable)| {
            let data = records.get(index as usize).copied().ok_or_else(corrupt)?;
            Ok(AppFile {
                name,
                bytes: data,
                executable,
            })
        })
        .collect()
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

    /// The path rules a payload name must survive once this target extracts it.
    pub fn name_rules(&self) -> NameRules {
        match self.os {
            TargetOs::Win32 => NameRules::Windows,
            _ => NameRules::Unix,
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

    fn test_manifest(shape: Shape) -> Manifest {
        Manifest {
            shape,
            entry: "main.js".into(),
            node_version: "24.10.0".into(),
            provision_version: String::new(),
            smol_exact_target: false,
            smol_version_range: String::new(),
            requires_augmentation: false,
            triple: "darwin-arm64".into(),
            node_sha256: "abc123".into(),
            node_blake3: String::new(),
            node_size: 0,
            app_compressed: false,
            app_sha256: "def456".into(),
            minify: false,
            install_message: None,
            node_flags: Vec::new(),
            sealed_module_graph: false,
            hide_console: false,
        }
    }

    #[test]
    fn compile_bootstrap_name_is_a_fixed_root_filename() {
        assert_eq!(COMPILE_BOOTSTRAP_NAME, "__nub_compile_bootstrap.cjs");
        assert!(!COMPILE_BOOTSTRAP_NAME.contains(['/', '\\']));
    }

    #[test]
    fn roundtrips_embed_shape() {
        let manifest = Manifest {
            shape: Shape::Embed,
            entry: "main.js".into(),
            node_version: "24.10.0".into(),
            provision_version: String::new(),
            smol_exact_target: false,
            smol_version_range: String::new(),
            requires_augmentation: false,
            triple: "darwin-arm64".into(),
            node_sha256: "abc123".into(),
            node_blake3: String::new(),
            node_size: 0,
            app_compressed: false,
            app_sha256: "def456".into(),
            minify: true,
            install_message: Some("Setting up app".into()),
            // Non-empty on purpose: an empty vec round-trips through almost any
            // encoding bug, so it would fix the build without covering the field.
            node_flags: vec!["--max-old-space-size=256".into(), "--no-warnings".into()],
            sealed_module_graph: false,
            // True on purpose: `false` is this field's serde default, so a
            // manifest carrying it would round-trip through an encoding bug that
            // dropped the field entirely.
            hide_console: true,
        };
        let app = vec![
            AppFile::plain("main.js", b"import './c.js'\n".to_vec()),
            AppFile::plain("c.js", b"export const x=1\n".to_vec()),
        ];
        let node = vec![9u8; 4096];
        let license = b"Node.js license".to_vec();
        let blob = encode_with_license(&manifest, &app, &node, &license);

        let view = decode(&blob).unwrap();
        assert_eq!(view.manifest.shape, Shape::Embed);
        assert_eq!(view.manifest.entry, "main.js");
        assert_eq!(
            view.manifest.node_flags,
            ["--max-old-space-size=256", "--no-warnings"],
            "flags baked into the binary must survive the round trip in order"
        );
        assert!(
            view.manifest.hide_console,
            "the hidden-console flag decides whether the launcher passes              CREATE_NO_WINDOW, so losing it in the payload un-hides the artifact"
        );
        assert_eq!(view.app_files.len(), 2);
        assert_eq!(view.app_files[0].name, "main.js");
        assert_eq!(view.app_files[0].bytes, b"import './c.js'\n");
        assert_eq!(view.app_files[1].name, "c.js");
        assert_eq!(view.node_blob, &node[..]);
        assert_eq!(view.node_license_blob, &license[..]);
    }

    #[test]
    fn roundtrips_smol_shape_no_node() {
        let manifest = Manifest {
            shape: Shape::Smol,
            entry: "main.js".into(),
            node_version: "24.10.0".into(),
            provision_version: String::new(),
            smol_exact_target: false,
            smol_version_range: ">=24 <25".into(),
            requires_augmentation: false,
            triple: "darwin-arm64".into(),
            node_sha256: String::new(),
            node_blake3: String::new(),
            node_size: 0,
            app_compressed: false,
            app_sha256: "aa".into(),
            minify: false,
            install_message: None,
            // Empty here, so the pair covers both ends of the field.
            node_flags: Vec::new(),
            sealed_module_graph: false,
            hide_console: false,
        };
        let app = vec![AppFile::plain("main.js", b"console.log(1)".to_vec())];
        let blob = encode_with_license(&manifest, &app, &[], &[]);
        let view = decode(&blob).unwrap();
        assert_eq!(view.manifest.shape, Shape::Smol);
        assert!(!view.manifest.smol_exact_target);
        assert_eq!(view.manifest.smol_version_range, ">=24 <25");
        assert!(view.node_blob.is_empty());
        assert!(view.node_license_blob.is_empty());
        assert_eq!(view.app_files[0].bytes, b"console.log(1)");
    }

    #[test]
    fn rejects_bad_magic() {
        let bad = vec![0u8; HEADER_LEN_V1 + 4];
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn rejects_overflowing_lengths_without_panicking() {
        // A corrupted section with a garbage length must Err cleanly, never
        // panic (the launcher's release profile is panic=abort with overflow
        // checks off, so an unchecked `+` would abort the process).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(FORMAT_VERSION_V1);
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
    fn decodes_handcrafted_v1_with_an_empty_license() {
        let mut manifest = serde_json::to_value(test_manifest(Shape::Embed)).unwrap();
        manifest
            .as_object_mut()
            .unwrap()
            .remove("smol_exact_target");
        manifest
            .as_object_mut()
            .unwrap()
            .remove("smol_version_range");
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut app = Vec::new();
        app.extend_from_slice(&1u32.to_le_bytes());
        app.extend_from_slice(&7u16.to_le_bytes());
        app.extend_from_slice(b"main.js");
        app.extend_from_slice(&3u64.to_le_bytes());
        app.extend_from_slice(b"app");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(FORMAT_VERSION_V1);
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(app.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&manifest);
        bytes.extend_from_slice(&app);
        bytes.extend_from_slice(b"nz");

        let view = decode(&bytes).expect("V1 payload decodes");
        assert!(
            !view.manifest.smol_exact_target,
            "a legacy manifest without the policy bit remains floor mode"
        );
        assert!(
            view.manifest.smol_version_range.is_empty(),
            "a legacy manifest without a range retains floor mode"
        );
        assert_eq!(view.app_files, vec![AppFile::plain("main.js", &b"app"[..])]);
        assert_eq!(view.node_blob, b"nz");
        assert!(view.node_license_blob.is_empty());
    }

    #[test]
    fn v2_aliases_deduplicate_physical_data_but_preserve_logical_files() {
        let app = vec![
            AppFile::plain("hash-a.js", b"same".to_vec()),
            AppFile::plain("layout/a.js", b"same".to_vec()),
            AppFile::plain("other.js", b"other".to_vec()),
        ];
        let blob = encode(&test_manifest(Shape::Smol), &app, &[]);
        let manifest_len = u32::from_le_bytes(blob[8..12].try_into().unwrap()) as usize;
        let app_start = HEADER_LEN_V2 + manifest_len;
        assert_eq!(
            u32::from_le_bytes(blob[app_start..app_start + 4].try_into().unwrap()),
            3
        );
        assert_eq!(
            u32::from_le_bytes(blob[app_start + 4..app_start + 8].try_into().unwrap()),
            2
        );

        let view = decode(&blob).expect("V2 payload decodes");
        assert_eq!(view.app_files.len(), 3);
        assert_eq!(view.app_files[0], AppFile::plain("hash-a.js", &b"same"[..]));
        assert_eq!(
            view.app_files[1],
            AppFile::plain("layout/a.js", &b"same"[..])
        );
        assert_eq!(view.app_files[2], AppFile::plain("other.js", &b"other"[..]));
    }

    #[test]
    fn the_executable_bit_rides_the_existing_per_file_index_field() {
        let plain = vec![
            AppFile::plain("main.js", b"app".to_vec()),
            AppFile::plain("bin/helper", b"#!/bin/sh\n".to_vec()),
        ];
        let mut marked = plain.clone();
        marked[1].executable = true;

        let without = encode(&test_manifest(Shape::Smol), &plain, &[]);
        let with = encode(&test_manifest(Shape::Smol), &marked, &[]);
        assert_eq!(
            without.len(),
            with.len(),
            "marking a file executable must not change the container's size"
        );
        assert_eq!(
            without.iter().zip(&with).filter(|(a, b)| a != b).count(),
            1,
            "the flag must live in the existing per-file index field, not a new one"
        );

        let view = decode(&with).expect("a payload carrying the flag decodes");
        assert_eq!(view.app_files[0].name, "main.js");
        assert!(
            !view.app_files[0].executable,
            "an unmarked file must stay non-executable"
        );
        assert_eq!(view.app_files[1].name, "bin/helper");
        assert!(view.app_files[1].executable);
        assert_eq!(view.app_files[1].bytes, b"#!/bin/sh\n");
    }

    #[test]
    fn aliased_bodies_carry_independent_executable_bits() {
        let app = vec![
            AppFile::plain("tools/copy.sh", b"#!/bin/sh\n".to_vec()),
            AppFile {
                name: "bin/run.sh".to_owned(),
                bytes: b"#!/bin/sh\n".to_vec(),
                executable: true,
            },
        ];
        let blob = encode(&test_manifest(Shape::Smol), &app, &[]);
        let manifest_len = u32::from_le_bytes(blob[8..12].try_into().unwrap()) as usize;
        let app_start = HEADER_LEN_V2 + manifest_len;
        assert_eq!(
            u32::from_le_bytes(blob[app_start + 4..app_start + 8].try_into().unwrap()),
            1,
            "identical bodies must still collapse to one physical record"
        );

        let view = decode(&blob).expect("V2 payload decodes");
        assert!(!view.app_files[0].executable);
        assert!(view.app_files[1].executable);
        assert_eq!(view.app_files[0].bytes, view.app_files[1].bytes);
    }

    #[test]
    fn the_source_mode_decides_executability_and_a_modeless_host_records_none() {
        let exec = |mode| AppFile::from_source_mode("f", &b""[..], mode).executable;
        assert!(exec(Some(0o755)));
        assert!(exec(Some(0o700)));
        assert!(exec(Some(0o100)), "owner-only execute still counts");
        assert!(!exec(Some(0o644)));
        // A Windows build host has no mode to read, and sniffing content or an
        // extension would make the same source tree build differently per host.
        assert!(!exec(None));
    }

    #[test]
    fn rejects_truncated_overflowing_and_bad_alias_v2_payloads() {
        let app = vec![AppFile::plain("main.js", b"app".to_vec())];
        let blob = encode_with_license(&test_manifest(Shape::Embed), &app, b"node", b"license");
        let mut truncated = blob.clone();
        truncated.pop();
        assert!(decode(&truncated).is_err(), "truncated V2 must not decode");

        let mut overflowing = blob.clone();
        overflowing[28..36].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(
            decode(&overflowing).is_err(),
            "overflowing V2 must not decode"
        );

        let mut bad_alias = encode(&test_manifest(Shape::Smol), &app, &[]);
        let manifest_len = u32::from_le_bytes(bad_alias[8..12].try_into().unwrap()) as usize;
        let app_start = HEADER_LEN_V2 + manifest_len;
        let alias_offset = app_start + 8 + 2 + "main.js".len();
        bad_alias[alias_offset..alias_offset + 4].copy_from_slice(&1u32.to_le_bytes());
        assert!(
            decode(&bad_alias).is_err(),
            "out-of-range V2 alias must not decode"
        );
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

    /// The gate runs on the BUILD machine, so both target families must be
    /// judgeable from either host — these cases all run on every CI platform.
    #[test]
    fn a_payload_name_is_judged_by_the_targets_path_rules_not_the_hosts() {
        use NameRules::{Unix, Windows};

        for rules in [Unix, Windows] {
            for ok in ["main.js", "chunk-abc.js", "src/nested/chunk.js", "a.b.c"] {
                assert!(is_safe_relative_name_for(rules, ok), "{rules:?} {ok:?}");
            }
            for bad in [
                "",
                ".",
                "..",
                "./main.js",
                "../evil",
                "a/../../evil",
                "/etc/passwd",
                "a//b",
                "a/",
                "a/\0b",
            ] {
                assert!(!is_safe_relative_name_for(rules, bad), "{rules:?} {bad:?}");
            }
        }

        // The cross-compile hole: ordinary Unix filenames, traversals on Windows.
        for backslash in ["a\\..\\..\\x", "..\\evil", "C:\\Windows\\x", "\\etc\\x"] {
            assert!(
                is_safe_relative_name_for(Unix, backslash),
                "{backslash:?} is a legal single-component Unix filename"
            );
            assert!(
                !is_safe_relative_name_for(Windows, backslash),
                "{backslash:?} escapes under Win32 path rules"
            );
        }

        // Win32-only hazards that are not traversals: a name Win32 normalizes
        // onto another payload entry, an illegal character, a drive/ADS colon,
        // and a reserved device at any depth or extension.
        for win_only in [
            "main.js. ",
            "main.js.",
            "main.js ",
            "a<b",
            "a|b",
            "C:x",
            "con",
            "assets/CON.txt",
            "com1",
            "COM0",
            "LPT9.dat",
            // Win32 strips the trailing space off the segment before matching
            // the device, so the space does not make this an ordinary file.
            "con .txt",
            "aux/data.json",
        ] {
            assert!(
                is_safe_relative_name_for(Unix, win_only),
                "{win_only:?} is an ordinary name on a Unix target"
            );
            assert!(
                !is_safe_relative_name_for(Windows, win_only),
                "{win_only:?} must not be embedded for a Windows target"
            );
        }
        // Not devices: a longer name merely starts with the prefix.
        for ok in [
            "COM10",
            "console.js",
            "nulls.json",
            "PROGRA~1",
            "..a",
            "a..b",
        ] {
            assert!(is_safe_relative_name_for(Windows, ok), "{ok:?}");
        }
    }

    #[test]
    fn the_target_platform_selects_the_rules() {
        let rules = |t: &str| TargetPlatform::parse(t).unwrap().name_rules();
        assert_eq!(rules("win32-x64"), NameRules::Windows);
        assert_eq!(rules("win32-arm64"), NameRules::Windows);
        assert_eq!(rules("linux-x64-musl"), NameRules::Unix);
        assert_eq!(rules("darwin-arm64"), NameRules::Unix);
    }
}
