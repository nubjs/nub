//! Node's single-executable-application blob, written by nub rather than by
//! `node --build-sea`.
//!
//! Writing it here removes the only host requirement a SEA would otherwise
//! impose: `node --experimental-sea-config` embeds no version header, so its
//! output is only valid for the Node that produced it — a cross-compile would
//! need a host Node of the target's exact version before it could emit a single
//! byte. nub already resolves and downloads the target's Node; serializing the
//! blob itself means it never has to run one.
//!
//! The format is `SeaSerializer::Write` in Node's `src/node_sea.cc`, which is a
//! flat little-endian struct with no padding and no alignment — every length is
//! a target `size_t` (8 bytes; all eight of nub's targets are 64-bit) and every
//! string is a raw byte run with no terminator:
//!
//! ```text
//! u32  magic = 0x0143da20
//! u32  flags
//! u8   execArgvExtension
//! u8   mainFormat            // only from Node 25.7 — see FORMAT_BYTE_FLOOR
//! u64 + bytes                 code path
//! u64 + bytes                 main source
//! u64 + bytes                 code cache      (only when kUseCodeCache)
//! u64 count, then per asset: u64 + key, u64 + contents   (only when kIncludeAssets)
//! u64 count, then per arg:   u64 + arg                   (only when kIncludeExecArgv)
//! ```
//!
//! The two count-prefixed sections are written on the FLAG, not on emptiness, so
//! a flag set without a section (or the reverse) desynchronizes the reader for
//! everything after it. [`Blob::serialize`] derives both flags from the data for
//! that reason.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::{ContainerFormat, TargetPlatform};

use super::bundle;
use nub_core::node::version::NodeVersion;

/// `kMagic` in `src/node_sea.h`. Unchanged since SEAs shipped.
const MAGIC: u32 = 0x0143_da20;

const FLAG_DISABLE_EXPERIMENTAL_SEA_WARNING: u32 = 1 << 0;
#[allow(dead_code)] // Snapshots are refused; the bit is here to document the layout.
const FLAG_USE_SNAPSHOT: u32 = 1 << 1;
const FLAG_USE_CODE_CACHE: u32 = 1 << 2;
const FLAG_INCLUDE_ASSETS: u32 = 1 << 3;
const FLAG_INCLUDE_EXEC_ARGV: u32 = 1 << 4;

/// The first Node whose blob header carries a `mainFormat` byte, from
/// `sea: support ESM entry point in SEA` (`2d874dfb8e0`, v25.7.0; the same change
/// reached 26.x as `af0d6d42481`, and 26.0.0 sorts above 25.7.0 so one bound
/// covers both). Below it the header is nine bytes, not ten, and writing the
/// extra byte shifts every length that follows.
const FORMAT_BYTE_FLOOR: NodeVersion = NodeVersion::new(25, 7, 0);

/// `SeaExecArgvExtension`. nub always writes [`ExecArgvExtension::Env`], the
/// value Node itself defaults to: an artifact's own flags come from the blob, and
/// `NODE_OPTIONS` stays the user's channel exactly as it is for the launcher
/// shape. `Cli` is the `--node-options=` form, which would claim an argument out
/// of the application's own argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecArgvExtension {
    #[allow(dead_code)]
    None = 0,
    Env = 1,
    #[allow(dead_code)]
    Cli = 2,
}

/// `ModuleFormat`. nub's main is always CommonJS — an ESM main inside a SEA can
/// import builtins and nothing else, which is not enough to reach the payload's
/// chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainFormat {
    CommonJs = 0,
    #[allow(dead_code)]
    Module = 1,
}

/// Everything a blob carries. Assets are a `Vec` rather than a map so the emitted
/// bytes follow the caller's order and a rebuild of an unchanged payload is
/// byte-identical — Node reads them into an `unordered_map` and does not care.
pub struct Blob<'a> {
    pub disable_experimental_warning: bool,
    pub exec_argv_extension: ExecArgvExtension,
    pub main_format: MainFormat,
    /// Reported as the main's path. Never opened: Node uses it for stack frames
    /// and for `module.filename`.
    pub code_path: &'a str,
    pub main: &'a [u8],
    /// A V8 code cache for the main. Native builds only — V8 rejects a cache
    /// built for another platform, and Node then warns and falls back to source.
    pub code_cache: Option<&'a [u8]>,
    pub assets: Vec<(String, Vec<u8>)>,
    pub exec_argv: Vec<String>,
}

impl Blob<'_> {
    /// Serialize for `node_version`'s reader.
    ///
    /// The version is load-bearing rather than informational: the header gained a
    /// byte at 25.7, and a blob written with the wrong header length is not
    /// rejected — it deserializes into garbage lengths and aborts inside V8.
    pub fn serialize(&self, node_version: &NodeVersion) -> Result<Vec<u8>> {
        if self.main_format == MainFormat::Module && *node_version < FORMAT_BYTE_FLOOR {
            bail!(
                "Node {node_version} has no SEA module-format field, so its main must be CommonJS"
            );
        }

        let mut flags = 0u32;
        if self.disable_experimental_warning {
            flags |= FLAG_DISABLE_EXPERIMENTAL_SEA_WARNING;
        }
        if self.code_cache.is_some() {
            flags |= FLAG_USE_CODE_CACHE;
        }
        if !self.assets.is_empty() {
            flags |= FLAG_INCLUDE_ASSETS;
        }
        if !self.exec_argv.is_empty() {
            flags |= FLAG_INCLUDE_EXEC_ARGV;
        }

        let mut out = Vec::with_capacity(self.main.len() + 4096);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.push(self.exec_argv_extension as u8);
        if *node_version >= FORMAT_BYTE_FLOOR {
            out.push(self.main_format as u8);
        }

        write_bytes(&mut out, self.code_path.as_bytes());
        write_bytes(&mut out, self.main);
        if let Some(cache) = self.code_cache {
            write_bytes(&mut out, cache);
        }
        if !self.assets.is_empty() {
            write_len(&mut out, self.assets.len());
            for (key, contents) in &self.assets {
                write_bytes(&mut out, key.as_bytes());
                write_bytes(&mut out, contents);
            }
        }
        if !self.exec_argv.is_empty() {
            write_len(&mut out, self.exec_argv.len());
            for arg in &self.exec_argv {
                write_bytes(&mut out, arg.as_bytes());
            }
        }
        Ok(out)
    }
}

/// A `size_t` length, little-endian. Every one of nub's eight targets is 64-bit,
/// which is what makes this width a constant rather than a per-target decision.
fn write_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u64).to_le_bytes());
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

/// Node's Mach-O segment. `postject_find_resource` is called with
/// `macho_segment_name = "NODE_SEA"` and the resource name `NODE_SEA_BLOB`,
/// which postject prefixes with `__` because it does not already start with one
/// (`deps/postject/postject-api.h`). Both names are compiled into the Node
/// binary doing the lookup, so neither is nub's to choose.
const MACHO_SEGMENT: &str = "NODE_SEA";
pub(super) const MACHO_SECTION: &str = "__NODE_SEA_BLOB";

/// The ELF note name and the PE resource name — the same string, matched
/// directly by `postject_find_resource` on both.
pub(super) const RESOURCE_NAME: &str = "NODE_SEA_BLOB";

/// What Node's own builder calls the ELF section wrapping the note. Cosmetic to
/// the runtime, which reads the `PT_NOTE` program header, but it is what a
/// reader running `readelf -n` on a nub artifact expects to see.
const ELF_NOTE_SECTION: &str = ".note.node.sea";

/// The postject "fuse": a sentinel string every Node binary carries exactly
/// once, ending in `:0`. `postject_has_resource()` reads the byte after the
/// colon and returns true only when it is `1`, so an unflipped fuse means Node
/// never even looks for the blob.
const FUSE_PREFIX: &[u8] = b"NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2:";

/// Flip the fuse in `image`, in place.
///
/// Done BEFORE the container write, and therefore before signing, which is not
/// an ordering preference: a Mach-O signature covers these bytes, so flipping
/// afterwards invalidates it and the kernel kills the process with SIGKILL
/// rather than reporting anything.
fn flip_fuse(image: &mut [u8]) -> Result<()> {
    let mut found = None;
    let mut from = 0usize;
    while let Some(at) = find(&image[from..], FUSE_PREFIX) {
        let start = from + at;
        if found.is_some() {
            bail!(
                "this Node binary carries the single-executable fuse more than once, so nub \
                 cannot tell which one it must flip"
            );
        }
        found = Some(start);
        from = start + FUSE_PREFIX.len();
    }
    let Some(start) = found else {
        bail!(
            "this Node binary carries no single-executable fuse.\n\
             \x20\x20It was built with --disable-single-executable-application, so it cannot \
             host a compiled artifact."
        )
    };
    let value = start + FUSE_PREFIX.len();
    match image.get(value) {
        Some(b'0') => {
            image[value] = b'1';
            Ok(())
        }
        Some(b'1') => bail!("this Node binary already hosts a single-executable blob"),
        _ => bail!("the single-executable fuse in this Node binary is truncated"),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The ELF note postject reads: `n_name` is the resource name itself and the
/// descriptor is the blob with nothing in front of it.
///
/// libsui's own note wraps the descriptor in a length-prefixed section name,
/// which is the right shape for the launcher's reader and the wrong one here —
/// `postject_find_resource` returns `note + sizeof(Nhdr) + roundup(n_namesz, 4)`
/// and reports `n_descsz` as the length, so any prefix would land inside the
/// blob and shift every offset in it.
fn elf_note(blob: &[u8]) -> Vec<u8> {
    // `n_namesz` counts the terminating NUL; both it and the descriptor are
    // padded to a 4-byte boundary, which is the note layout postject walks.
    let name = RESOURCE_NAME.as_bytes();
    let namesz = name.len() + 1;
    let mut note = Vec::with_capacity(12 + namesz.next_multiple_of(4) + blob.len() + 4);
    note.extend_from_slice(&(namesz as u32).to_le_bytes());
    note.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    // `n_type` is 0, matching what Node's LIEF builder writes (`kNoteType`).
    note.extend_from_slice(&0u32.to_le_bytes());
    note.extend_from_slice(name);
    note.push(0);
    note.resize(note.len().next_multiple_of(4), 0);
    note.extend_from_slice(blob);
    note.resize(note.len().next_multiple_of(4), 0);
    note
}

/// Write `blob` into a copy of the target's `node` binary and publish the result
/// at `out` as a single-executable application.
///
/// This is [`super::inject::inject`]'s counterpart for the SEA shape, and it is
/// deliberately a sibling rather than a parameter of it: the two write different
/// names into different containers, and only this one flips a fuse. What they do
/// share is the rule that every decision dispatches on the TARGET.
///
/// The Mach-O leg keeps the input's `LC_CODE_SIGNATURE`, which is the one thing
/// that makes darwin-x64 work at all. The template here is a RELEASE-SIGNED Node
/// rather than nub's own unsigned launcher, and stripping that signature first —
/// which is what libsui's x86_64 path does by default — makes the later `codesign`
/// fail with `main executable failed strict validation` and puts the section
/// somewhere the runtime cannot find it. Measured both ways on Node 26.7.0; the
/// stripped one is caught by [`verify_artifact`] rather than shipped.
///
/// This is also the whole of why `node --build-sea` cannot produce a working
/// darwin-x64 SEA and nub can. LIEF, which that tool and postject use, grows
/// `__TEXT` by a page and shifts every later segment, including the three
/// `__thread_*` sections — and dyld computes its thread-local span across those in
/// load-command order, so the shifted binary blows a 4 GB sanity check and dies at
/// exec before any code runs. Stock Intel `node` has 208 bytes of header slack and
/// a one-section `LC_SEGMENT_64` needs 152, so libsui's writer fits into what is
/// already there and moves only `__LINKEDIT`.
pub fn inject(
    target: &TargetPlatform,
    node_image: &[u8],
    blob: &[u8],
    icon: Option<&[u8]>,
    version_info: Option<&[u8]>,
    hide_console: bool,
    out: &Path,
) -> Result<()> {
    let mut image = node_image.to_vec();
    flip_fuse(&mut image)?;

    let mut file = fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    match target.format() {
        ContainerFormat::MachO => libsui::Macho::from_keeping_signature(image)
            .map_err(|e| anyhow!("parsing the target's Node as Mach-O: {e:?}"))?
            .write_section_in_segment(MACHO_SEGMENT, MACHO_SECTION, blob.to_vec())
            .map_err(|e| anyhow!("injecting the single-executable blob: {e:?}"))?
            .build_and_sign(&mut file)
            .map_err(|e| anyhow!("building + ad-hoc signing the executable: {e:?}")),
        ContainerFormat::Elf => libsui::Elf::new(&image)
            .append_note(&elf_note(blob), ELF_NOTE_SECTION, &mut file)
            .map_err(|e| anyhow!("appending the single-executable note: {e:?}")),
        ContainerFormat::Pe => {
            // Same builder-chain ordering as the launcher leg: libsui rebuilds
            // the resource directory from scratch, so an icon or version
            // resource set anywhere but inside this chain is discarded.
            //
            // Which is why the manifest is carried across HERE, and why the SEA
            // shape needs it where the launcher shape did not: this image is
            // Node's own binary, and Node's `wmain` refuses to start when
            // `IsWindows10OrGreater` says no — which is what Windows tells any
            // image declaring no `<supportedOS>` GUID. Dropping the manifest
            // produced an artifact that printed "Node.js is only supported on
            // Windows 10, Windows Server 2016, or higher" and exited 216 on both
            // Windows targets.
            let manifest = super::inject::find_manifest_resource(&image)
                .context("reading the target Node's application manifest")?;
            let mut pe = libsui::PortableExecutable::from(&image)
                .map_err(|e| anyhow!("parsing the target's Node as PE: {e:?}"))?;
            if let Some((manifest, language)) = manifest {
                pe = pe.set_manifest(manifest.to_vec(), language);
            }
            if let Some(icon) = icon {
                pe = pe
                    .set_icon(icon)
                    .map_err(|e| anyhow!("setting the executable icon: {e:?}"))?;
            }
            if let Some(version_info) = version_info {
                pe = pe.set_version_info(version_info.to_vec());
            }
            // Built into memory rather than straight into the file, because the
            // subsystem flip has to land on the FINISHED image — the same ordering
            // the launcher leg documents.
            let mut image = Vec::with_capacity(node_image.len() + blob.len());
            pe.write_resource(RESOURCE_NAME, blob.to_vec())
                .map_err(|e| anyhow!("injecting the single-executable resource: {e:?}"))?
                .build(&mut image)
                .map_err(|e| anyhow!("building the executable: {e:?}"))?;
            if hide_console {
                super::inject::set_pe_subsystem_gui(&mut image)
                    .context("hiding the console window (--hide-console)")?;
            }
            file.write_all(&image)
                .with_context(|| format!("writing {}", out.display()))
        }
    }
}

/// Whether this Node reads `execArgv` out of the SEA blob.
///
/// The gate that decides whether an artifact can be a SEA at all. nub's flag set
/// is version-banded and injected ahead of the entry — `--experimental-vm-modules`,
/// `--disable-warning=ExperimentalWarning`, the whole unflag band — and V8 flags
/// have to be applied before the isolate exists, so there is no way to add them
/// from inside the main. Without this field an artifact would start with none of
/// them and diverge from `nub <file>` on the first program that uses one.
///
/// The bands are backports, not a single floor. `sea: support execArgv in sea
/// config` landed three times — `48bfbd3dca5` (22.20.0), `c6e3d5d98de` (24.7.0)
/// and `3fc70198e0a` (25.0.0) — and 23.x reached end of life before any of them,
/// so that whole line is out. Below 22.20 there is no blob field to write.
pub fn supports_blob_exec_argv(version: &NodeVersion) -> bool {
    const V22_20: NodeVersion = NodeVersion::new(22, 20, 0);
    const V23: NodeVersion = NodeVersion::new(23, 0, 0);
    const V24_7: NodeVersion = NodeVersion::new(24, 7, 0);
    *version >= V24_7 || (*version >= V22_20 && *version < V23)
}

/// A qualifying payload, split into what the blob carries.
pub struct Payload {
    /// The blob's `main` — bootstrap plus loader, run as CommonJS.
    pub main: Vec<u8>,
    /// The chunks and their CommonJS siblings, by payload name, stored RAW.
    ///
    /// Not compressed, unlike the launcher shape's app region. The blob is mapped
    /// from the executable and the loader hands each asset to Node as the
    /// `ArrayBuffer` `getRawAsset` returns, so a compressed asset would have to be
    /// decoded in JavaScript into a string first — which is the copy the whole
    /// loader is built to avoid, for 1.0 ms on a 60 KB chunk. The bytes are a
    /// fraction of the ~110 MB Node they sit next to either way.
    pub assets: Vec<(String, Vec<u8>)>,
}

/// The root manifest `assemble_app` synthesizes for the extracted tree. Dropped
/// here for the inline shape's reason: it exists so a walk-up finds a package
/// boundary on disk, and a SEA has no disk to walk.
const ROOT_MANIFEST_NAME: &str = "package.json";

/// Split a qualifying payload into a blob main and its assets.
///
/// The chunks travel VERBATIM — no `import.meta.url` prefix, no cross-chunk
/// specifier substitution, no eval-global cleanup. All three exist in the inline
/// shape to work around a `data:` URL having no identity and no base; a chunk
/// served from `module.registerHooks` at `file:///N:/$nub/<name>` has both, so
/// Node sets `import.meta.url` from the resolved URL and resolves each relative
/// specifier against it exactly as it does in the extracted tree. That is why the
/// SEA and the extracted shape ship byte-identical chunks.
pub fn payload(
    files: &[nub_core::compile::AppFile<Vec<u8>>],
    entry: &str,
    app_sha: &str,
    neutralize_localstorage: bool,
) -> Result<Payload> {
    let mut main = None;
    let mut assets = Vec::with_capacity(files.len());
    for file in files {
        if file.name == nub_core::compile::COMPILE_BOOTSTRAP_NAME {
            main = Some(file.bytes.clone());
        } else if file.name != ROOT_MANIFEST_NAME {
            assets.push((file.name.clone(), file.bytes.clone()));
        }
    }
    let Some(bootstrap) = main else {
        bail!("the compiled payload carries no bootstrap, so it cannot become a single executable")
    };
    let names: Vec<String> = assets.iter().map(|(name, _)| name.clone()).collect();
    if !names.iter().any(|name| name == entry) {
        bail!("the compiled entry {entry:?} is not among the emitted chunks");
    }
    Ok(Payload {
        main: main_source(&bootstrap, entry, &names, app_sha, neutralize_localstorage)?,
        assets,
    })
}

/// The blob's `main`: the compile bootstrap with its preload argument neutralized,
/// followed by the SEA loader.
///
/// Not wrapped in an IIFE the way the inline shape wraps the same pair. That
/// wrapper exists because `-e` evaluates its script in a context where a top-level
/// function declaration becomes a GLOBAL, and the bootstrap has one. A SEA main
/// runs through `embedderRunCjs`, which compiles it as an ordinary CommonJS
/// wrapper — `exports`, `require`, `module`, `__filename` and `__dirname` arrive as
/// PARAMETERS, not globals — so the declaration stays module-scoped exactly as it
/// does under `--require` in the extracted shape. Same reason the inline shape's
/// `EVAL_GLOBAL_CLEANUP` has no counterpart here: nothing was published to clean up.
fn main_source(
    bootstrap: &[u8],
    entry: &str,
    files: &[String],
    app_sha: &str,
    neutralize_localstorage: bool,
) -> Result<Vec<u8>> {
    let bootstrap = std::str::from_utf8(bootstrap).context("the compile bootstrap is not utf-8")?;
    let bootstrap = neutralize_preload_arg(bootstrap)?;

    let loader = bundle::compile_runtime_file("compile-sea-loader.cjs")?;
    let loader = String::from_utf8(loader).context("the compile SEA loader is not utf-8")?;
    // Payload names are validated against the target's path rules long before this,
    // so none can carry a quote or a backslash — but each is substituted into a
    // JavaScript literal, so all of them are escaped rather than trusted.
    let loader = loader
        .replace(
            "\"__NUB_SEA_ENTRY__\"",
            &serde_json::to_string(entry).expect("a payload name serializes"),
        )
        .replace(
            "__NUB_SEA_FILES__",
            &serde_json::to_string(files).expect("payload names serialize"),
        )
        .replace(
            "\"__NUB_SEA_COMPILE_CACHE__\"",
            &serde_json::to_string(&compile_cache_key(app_sha)).expect("a cache key serializes"),
        )
        .replace(
            "__NUB_SEA_NEUTRALIZE_LOCALSTORAGE__",
            if neutralize_localstorage {
                "true"
            } else {
                "false"
            },
        );

    let mut out = bootstrap.into_bytes();
    out.push(b'\n');
    out.extend_from_slice(loader.as_bytes());
    Ok(out)
}

/// The compile cache's per-artifact subdirectory, resolved against nub's cache
/// root by the loader. Matches what the launcher passes as `NODE_COMPILE_CACHE`
/// for the extracted shape, so the two containers do not each grow their own tree.
fn compile_cache_key(app_sha: &str) -> String {
    format!("compile-v8/{}", &app_sha[..16.min(app_sha.len())])
}

/// Drop the bootstrap's `--require` argument.
///
/// It names `__filename`, which under `--require` is the preload's own path and is
/// exactly right; inside a SEA there is no file, and `__filename` is the blob's
/// `code_path`. Every consumer already treats a missing value as "do not prepend a
/// preload", which is the truth here — the inline shape reaches the same state
/// through Node's `[eval]` sentinel.
fn neutralize_preload_arg(bootstrap: &str) -> Result<String> {
    const NEEDLE: &str =
        "const requireArg = __filename === \"[eval]\" ? undefined : `--require=${__filename}`;";
    const REPLACEMENT: &str = "const requireArg = undefined;";
    if !bootstrap.contains(NEEDLE) {
        bail!(
            "the compile bootstrap no longer declares its preload argument the way the \
             single-executable main expects, so nub cannot neutralize it"
        );
    }
    Ok(bootstrap.replacen(NEEDLE, REPLACEMENT, 1))
}

/// The payload name the Node LICENSE rides under.
///
/// Never loaded — the module hooks do not serve it and nothing imports it. It is
/// in the blob because the artifact redistributes a Node binary and has to carry
/// that binary's copyright notice, which is what the launcher shape's separate
/// license region does. `--nub-internal licenses` prints it back out.
pub(super) const LICENSE_ASSET: &str = "__nub_node_license";

/// Everything the blob is built from.
pub struct Inputs<'a> {
    pub app_files: &'a [nub_core::compile::AppFile<Vec<u8>>],
    pub entry: &'a str,
    pub app_sha: &'a str,
    pub node_license: &'a [u8],
    /// The EXACT Node this artifact embeds. Decides the blob's header layout and
    /// the flag band below, both of which are version-shaped.
    pub node_version: &'a NodeVersion,
    /// The publisher's own `--node-options`, verbatim from the manifest.
    pub node_flags: &'a [String],
}

/// Serialize the blob for `inputs`.
pub fn build_blob(inputs: &Inputs<'_>) -> Result<Vec<u8>> {
    let payload = payload(
        inputs.app_files,
        inputs.entry,
        inputs.app_sha,
        nub_core::node::flags::should_inject_experimental_webstorage(
            inputs.node_version,
            &[],
            None,
        ) && nub_core::node::flags::should_neutralize_experimental_webstorage_localstorage(
            inputs.node_version,
            &[],
            None,
        ),
    )?;
    let mut assets = payload.assets;
    assets.push((LICENSE_ASSET.to_string(), inputs.node_license.to_vec()));

    Blob {
        // Node's "this is experimental" line is for someone who built a SEA; a
        // compiled nub artifact is a product, and its user did not ask Node for
        // anything.
        disable_experimental_warning: true,
        exec_argv_extension: ExecArgvExtension::Env,
        main_format: MainFormat::CommonJs,
        // Reported as the main's `__filename` and in its stack frames. The same
        // virtual root the chunks live under, so a frame from the loader and a
        // frame from the app read as one tree.
        code_path: "/N:/$nub/__nub_sea_main.cjs",
        main: &payload.main,
        // Never set. The blob's code cache covers the generated main ALONE — the
        // chunks are assets and get nothing from it, measured at -0.7 ms — and V8
        // rejects a cache built for another platform, so it would also make every
        // cross-compile emit a `Code cache data rejected.` warning at start. Node's
        // on-disk compile cache is what covers the chunks, and the loader enables
        // it.
        code_cache: None,
        assets,
        exec_argv: exec_argv(inputs.node_version, inputs.node_flags),
    }
    .serialize(inputs.node_version)
}

/// The flags Node splices into argv out of the blob — nub's injected set, then the
/// publisher's own.
///
/// This is the launcher's runtime computation, done at build time, and it can be:
/// an embedding artifact's Node is EXACT and provisioned by nub, which is the
/// `NodeOrigin::Managed` case where the launcher already skips its
/// `allowedNodeEnvironmentFlags` probe and takes the version band alone. The two
/// argv-only families it also computes are empty here by construction — both are
/// withheld for a sealed module graph, and a payload that is not sealed does not
/// become a SEA.
///
/// ONE input is not available at build time, and it is a deliberate divergence:
/// the launcher passes the user's runtime `NODE_OPTIONS` through
/// `compute_inject_flags`, which SUBTRACTS any flag the user negated there, so nub
/// never emits a positive competing with a user's disable. A blob is written once,
/// so an artifact's Node flags are fixed at build time exactly as its Node version
/// and its bundle are. `NODE_OPTIONS` still reaches the process — the blob's
/// `execArgvExtension` is `env`, Node's own default — but a `--no-…` in it no
/// longer removes nub's positive, because argv beats the environment.
fn exec_argv(version: &NodeVersion, node_flags: &[String]) -> Vec<String> {
    use nub_core::node::flags;

    let mut argv: Vec<String> =
        flags::compute_inject_flags(version.clone(), &[], None, false, None)
            .into_iter()
            .map(str::to_string)
            .collect();
    // Computed outside `compute_inject_flags` for the same reason the launcher
    // computes it separately: it honours an explicit user polarity, which that
    // helper's static set would bypass.
    if flags::should_inject_experimental_webstorage(version, &[], None) {
        argv.push("--experimental-webstorage".to_string());
    }
    // AFTER nub's own, because Node takes the last occurrence of a repeated flag —
    // so a publisher who asked for one gets it even where nub injects the opposite.
    // Exact duplicates only: a flag that merely shares a NAME with an injected one
    // (`--max-old-space-size=…`) must still be emitted, because coming last is how
    // it wins.
    for flag in node_flags {
        if !argv.iter().any(|injected| injected == flag) {
            argv.push(flag.clone());
        }
    }
    argv
}

/// Prove the produced artifact is a single-executable application.
///
/// The counterpart to [`super::verify_artifact`], and it checks the same two
/// things that one does for a cross-compiled target: the payload is where the
/// runtime will look for it, and it is the payload that was written. Both are read
/// back off the FILE, through the same structures Node's own `postject_find_resource`
/// walks — so a scan that finds the blob is evidence the loader will, not a re-read
/// of what the writer intended.
///
/// The fuse is checked too, and it is the failure that would be hardest to
/// diagnose without it: a blob present with an unflipped fuse produces a binary
/// that runs as a plain `node` REPL, because `postject_has_resource()` returns
/// false and Node never looks.
pub fn verify_artifact(
    path: &Path,
    target: &TargetPlatform,
    version_info: Option<&[u8]>,
    hide_console: bool,
) -> Result<()> {
    let image = fs::read(path).with_context(|| format!("reading back {}", path.display()))?;

    let fuse = find(&image, FUSE_PREFIX)
        .and_then(|at| image.get(at + FUSE_PREFIX.len()).copied())
        .context("the written artifact carries no single-executable fuse")?;
    if fuse != b'1' {
        bail!(
            "the written artifact's single-executable fuse is not set, so its Node would ignore \
             the blob and start a REPL"
        );
    }

    let blob = super::inject::find_sea_blob(target.format(), &image)
        .context("scanning the written artifact for its single-executable blob")?
        .context("the written artifact carries no single-executable blob")?;
    let magic = blob
        .get(0..4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if magic != Some(MAGIC) {
        bail!(
            "the written artifact's single-executable blob does not start with Node's magic number"
        );
    }

    // The manifest, which is this container's own Windows requirement rather than
    // part of the dressing: the launcher shape hands Node's binary to the target
    // untouched, and only here is that binary the thing being rewritten. Without
    // it Windows reports 6.2 to `IsWindows10OrGreater` and Node's `wmain` exits
    // 216 before any nub code runs. Every shipping Node carries one — an image
    // without it could not start on Windows 10 either — so its absence is a
    // failure rather than a case to tolerate.
    if target.format() == ContainerFormat::Pe
        && super::inject::find_manifest_resource(&image)
            .context("scanning the written artifact for its application manifest")?
            .is_none()
    {
        bail!(
            "the written artifact carries no application manifest, so Windows would refuse to \
             start it"
        );
    }

    // The Windows-only half, and the reason it is worth reading back rather than
    // trusting the write: a Windows artifact is routinely cross-compiled, so it is
    // never executed on the build host, and both of these fail SILENTLY on the
    // target — an un-hidden console, or an Explorer Details tab showing nothing.
    // Shared with the launcher path, which carries the full account of how the
    // resource directory's ascending-id rule loses the icon along with them.
    super::verify_windows_dressing(&image, version_info, hide_console)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32, patch: u32) -> NodeVersion {
        NodeVersion::new(major, minor, patch)
    }

    /// Byte-for-byte against a blob `node --experimental-sea-config` produced on
    /// Node 26.7.0, which is the only check that proves the layout rather than
    /// re-reading nub's own idea of it. Asset order is the caller's here and the
    /// reference blob's `unordered_map` iteration order there, so the fixture
    /// lists them in the order that binary emitted.
    #[test]
    fn matches_a_blob_node_itself_wrote() {
        let main = b"console.log(\"hi\", process.execArgv.join(\",\"));\n";
        let blob = Blob {
            disable_experimental_warning: true,
            exec_argv_extension: ExecArgvExtension::Cli,
            main_format: MainFormat::CommonJs,
            code_path: "main.cjs",
            main,
            code_cache: None,
            assets: vec![
                ("b.dat".into(), b"asset two".to_vec()),
                ("a.txt".into(), b"asset-one-contents".to_vec()),
            ],
            exec_argv: vec!["--no-warnings".into(), "--max-old-space-size=4096".into()],
        };
        let bytes = blob.serialize(&v(26, 7, 0)).unwrap();
        let expected: &[u8] = &[
            0x20, 0xda, 0x43, 0x01, // magic
            0x19, 0x00, 0x00, 0x00, // warning-off | assets | exec argv
            0x02, // execArgvExtension: cli
            0x00, // mainFormat: commonjs
            0x08, 0, 0, 0, 0, 0, 0, 0, b'm', b'a', b'i', b'n', b'.', b'c', b'j', b's', 0x2f, 0, 0,
            0, 0, 0, 0, 0, // main, 47 bytes
        ];
        assert_eq!(
            &bytes[..expected.len()],
            expected,
            "header/code-path/main-length prefix diverged from Node's own blob"
        );
        assert_eq!(&bytes[expected.len()..expected.len() + main.len()], main);

        let tail = &bytes[expected.len() + main.len()..];
        let mut want = Vec::new();
        want.extend_from_slice(&2u64.to_le_bytes());
        for (k, v) in [("b.dat", "asset two"), ("a.txt", "asset-one-contents")] {
            want.extend_from_slice(&(k.len() as u64).to_le_bytes());
            want.extend_from_slice(k.as_bytes());
            want.extend_from_slice(&(v.len() as u64).to_le_bytes());
            want.extend_from_slice(v.as_bytes());
        }
        want.extend_from_slice(&2u64.to_le_bytes());
        for a in ["--no-warnings", "--max-old-space-size=4096"] {
            want.extend_from_slice(&(a.len() as u64).to_le_bytes());
            want.extend_from_slice(a.as_bytes());
        }
        assert_eq!(tail, &want[..], "asset/execArgv section diverged");
        assert_eq!(bytes.len(), 220, "reference blob.bin is 220 bytes");
    }

    /// The header is one byte shorter below 25.7, and nothing in the blob says so
    /// — a reader on an older Node would take the format byte as the first byte of
    /// the code path's length.
    #[test]
    fn omits_the_format_byte_below_its_floor() {
        let blob = Blob {
            disable_experimental_warning: false,
            exec_argv_extension: ExecArgvExtension::Env,
            main_format: MainFormat::CommonJs,
            code_path: "m",
            main: b"1",
            code_cache: None,
            assets: Vec::new(),
            exec_argv: Vec::new(),
        };
        assert_eq!(blob.serialize(&v(25, 6, 1)).unwrap()[8..9], [1]);
        assert_eq!(
            blob.serialize(&v(25, 6, 1)).unwrap()[9..17],
            1u64.to_le_bytes()
        );
        assert_eq!(blob.serialize(&v(25, 7, 0)).unwrap()[9..10], [0]);
        assert_eq!(
            blob.serialize(&v(25, 7, 0)).unwrap().len(),
            blob.serialize(&v(25, 6, 1)).unwrap().len() + 1
        );
    }

    /// A count-prefixed section is read on its FLAG, so writing one without the
    /// other desynchronizes everything after it.
    #[test]
    fn sets_a_section_flag_exactly_when_it_writes_that_section() {
        let bare = Blob {
            disable_experimental_warning: false,
            exec_argv_extension: ExecArgvExtension::Env,
            main_format: MainFormat::CommonJs,
            code_path: "m",
            main: b"1",
            code_cache: None,
            assets: Vec::new(),
            exec_argv: Vec::new(),
        };
        let flags = |b: &Blob| {
            u32::from_le_bytes(b.serialize(&v(26, 7, 0)).unwrap()[4..8].try_into().unwrap())
        };
        assert_eq!(flags(&bare), 0);

        let with_assets = Blob {
            assets: vec![("a".into(), b"x".to_vec())],
            ..bare_clone(&bare)
        };
        assert_eq!(flags(&with_assets), FLAG_INCLUDE_ASSETS);

        let with_argv = Blob {
            exec_argv: vec!["--x".into()],
            ..bare_clone(&bare)
        };
        assert_eq!(flags(&with_argv), FLAG_INCLUDE_EXEC_ARGV);

        let with_cache = Blob {
            code_cache: Some(b"cache"),
            ..bare_clone(&bare)
        };
        assert_eq!(flags(&with_cache), FLAG_USE_CODE_CACHE);

        let quiet = Blob {
            disable_experimental_warning: true,
            ..bare_clone(&bare)
        };
        assert_eq!(flags(&quiet), FLAG_DISABLE_EXPERIMENTAL_SEA_WARNING);
    }

    /// The three bands are backports, and the hole between them is the part a
    /// single floor would get wrong: 23.x reached end of life before any of the
    /// three commits landed, so the whole line has no `execArgv` field.
    #[test]
    fn gates_exec_argv_on_the_backport_bands_rather_than_one_floor() {
        for (major, minor, patch, want) in [
            (18, 19, 0, false),
            (22, 19, 0, false),
            (22, 20, 0, true),
            (22, 23, 2, true),
            // The hole. 23.11 is the newest 23.x ever published.
            (23, 0, 0, false),
            (23, 11, 0, false),
            (24, 0, 0, false),
            (24, 6, 9, false),
            (24, 7, 0, true),
            (25, 0, 0, true),
            (26, 7, 0, true),
        ] {
            assert_eq!(
                supports_blob_exec_argv(&v(major, minor, patch)),
                want,
                "Node {major}.{minor}.{patch}"
            );
        }
    }

    /// The fuse decides whether Node looks for the blob AT ALL, so every way it
    /// can be wrong has to be a build failure rather than a binary that silently
    /// starts a REPL.
    #[test]
    fn flips_the_fuse_and_refuses_every_other_state() {
        let with = |tail: &str| {
            let mut image = b"....".to_vec();
            image.extend_from_slice(FUSE_PREFIX);
            image.extend_from_slice(tail.as_bytes());
            image
        };

        let mut ok = with("0 trailing");
        flip_fuse(&mut ok).unwrap();
        assert!(
            String::from_utf8_lossy(&ok).contains("df1996b2:1 trailing"),
            "the byte after the colon is the fuse, and only it changes"
        );

        assert!(
            flip_fuse(&mut with("1")).is_err(),
            "an already-set fuse means this Node already hosts a blob"
        );
        assert!(
            flip_fuse(&mut b"a Node built with SEA support disabled".to_vec()).is_err(),
            "no fuse at all must fail the build, not produce a REPL"
        );
        assert!(flip_fuse(&mut with("")).is_err(), "a truncated fuse");

        let mut twice = with("0");
        twice.extend_from_slice(FUSE_PREFIX);
        twice.extend_from_slice(b"0");
        assert!(
            flip_fuse(&mut twice).is_err(),
            "two fuses means nub cannot tell which one the runtime reads"
        );
    }

    /// postject takes the descriptor as the blob with nothing in front of it, and
    /// matches the note's own `n_name`. libsui's note does neither, which is why
    /// this layout is built here rather than reused.
    #[test]
    fn builds_the_note_postject_reads() {
        let note = elf_note(b"BLOB");
        assert_eq!(
            u32::from_le_bytes(note[0..4].try_into().unwrap()),
            14,
            "namesz counts the NUL"
        );
        assert_eq!(
            u32::from_le_bytes(note[4..8].try_into().unwrap()),
            4,
            "descsz is the blob alone"
        );
        assert_eq!(
            u32::from_le_bytes(note[8..12].try_into().unwrap()),
            0,
            "type matches Node's builder"
        );
        assert_eq!(&note[12..25], RESOURCE_NAME.as_bytes());
        assert_eq!(note[25], 0);
        // namesz 14 rounds to 16, so the descriptor starts at 12 + 16.
        assert_eq!(&note[28..32], b"BLOB");
        assert_eq!(note.len() % 4, 0, "the note is padded to a 4-byte boundary");
    }

    /// A drift guard, and the only thing standing between a bootstrap edit and an
    /// artifact that tries to `--require` a file no SEA has.
    #[test]
    fn refuses_a_bootstrap_it_cannot_neutralize() {
        let real = "const requireArg = __filename === \"[eval]\" ? undefined : `--require=${__filename}`;\nrest";
        assert_eq!(
            neutralize_preload_arg(real).unwrap(),
            "const requireArg = undefined;\nrest"
        );
        assert!(neutralize_preload_arg("const requireArg = something_else;").is_err());
    }

    /// The publisher's `--node-options` come LAST, because Node takes the last
    /// occurrence of a repeated flag — but an exact duplicate is noise in
    /// `process.execArgv`, and one that merely shares a NAME has to be emitted.
    #[test]
    fn puts_the_publishers_flags_after_nubs_own() {
        let argv = exec_argv(
            &v(26, 7, 0),
            &[
                "--experimental-vm-modules".into(),
                "--max-old-space-size=4096".into(),
            ],
        );
        assert!(argv.contains(&"--experimental-vm-modules".to_string()));
        assert_eq!(
            argv.iter()
                .filter(|f| *f == "--experimental-vm-modules")
                .count(),
            1,
            "an exact duplicate of an injected flag is dropped"
        );
        assert_eq!(
            argv.last().map(String::as_str),
            Some("--max-old-space-size=4096"),
            "a publisher flag comes last so it wins"
        );
    }

    /// The split the whole container rests on: the bootstrap becomes the main, the
    /// chunks become assets, and the extracted tree's `package.json` is dropped
    /// because nothing walks up a directory that does not exist.
    #[test]
    fn splits_a_payload_into_a_main_and_its_assets() {
        let file = |name: &str, bytes: &[u8]| nub_core::compile::AppFile {
            name: name.to_string(),
            plain_size: Some(bytes.len() as u64),
            bytes: bytes.to_vec(),
            executable: false,
        };
        let bootstrap = "const requireArg = __filename === \"[eval]\" ? undefined : `--require=${__filename}`;\n";
        let files = vec![
            file(
                nub_core::compile::COMPILE_BOOTSTRAP_NAME,
                bootstrap.as_bytes(),
            ),
            file("app.mjs", b"export default 1;"),
            file("package.json", b"{\"type\":\"module\"}"),
        ];

        let split = payload(&files, "app.mjs", "0123456789abcdef0123", false).unwrap();
        assert_eq!(
            split
                .assets
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["app.mjs"],
            "the bootstrap is the main and package.json is dropped"
        );
        let main = String::from_utf8(split.main).unwrap();
        assert!(main.contains("const requireArg = undefined;"));
        assert!(main.contains("\"app.mjs\""), "the loader names the entry");
        assert!(
            main.contains("compile-v8/0123456789abcdef"),
            "and the artifact's compile-cache key"
        );
        // Named one by one rather than as a `__NUB_SEA_` prefix scan, because the
        // loader's own header comment says the word. A survivor is not a cosmetic
        // problem: `__NUB_SEA_FILES__` left in place is an undefined identifier and
        // the artifact throws before it reaches the app.
        for placeholder in [
            "\"__NUB_SEA_ENTRY__\"",
            "__NUB_SEA_FILES__",
            "\"__NUB_SEA_COMPILE_CACHE__\"",
            "__NUB_SEA_NEUTRALIZE_LOCALSTORAGE__",
        ] {
            assert!(
                !main.contains(placeholder),
                "{placeholder} survived substitution"
            );
        }

        assert!(
            payload(&files, "missing.mjs", "0123456789abcdef0123", false).is_err(),
            "an entry that is not among the chunks is a build failure"
        );
    }

    /// The loader reads this name out of the blob to answer release CI's license
    /// gate. Two spellings of one string, in two languages; this is what keeps
    /// them one string.
    #[test]
    fn the_loader_and_the_writer_agree_on_the_license_asset_name() {
        let loader = super::bundle::compile_runtime_file("compile-sea-loader.cjs")
            .expect("the SEA loader ships in the runtime directory");
        let loader = String::from_utf8(loader).unwrap();
        assert!(
            loader.contains(&format!("getRawAsset(\"{LICENSE_ASSET}\")")),
            "the loader must read the asset the blob writer names"
        );
    }

    fn bare_clone<'a>(b: &Blob<'a>) -> Blob<'a> {
        Blob {
            disable_experimental_warning: b.disable_experimental_warning,
            exec_argv_extension: b.exec_argv_extension,
            main_format: b.main_format,
            code_path: b.code_path,
            main: b.main,
            code_cache: b.code_cache,
            assets: b.assets.clone(),
            exec_argv: b.exec_argv.clone(),
        }
    }
}
