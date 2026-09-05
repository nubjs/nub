//! Filesystem layout planning for native-addon package islands.
//!
//! The bundler plugin records cheap seed metadata while Rolldown builds its graph.
//! This module does the expensive work only for seed tokens that survived into an
//! emitted chunk: target validation, explicit dependency-edge traversal, and
//! regular-file materialization.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::{TargetArch, TargetOs, TargetPlatform};
use serde_json::Value;
use sha2::{Digest, Sha256};

const ISLAND_DIR: &str = "__nub_native";

#[derive(Debug, Clone)]
pub struct Seed {
    pub token: String,
    pub source: PathBuf,
    owner: Package,
    island: Island,
    addon_rel: PathBuf,
}

/// Where the island's filesystem boundary comes from, which decides both what is
/// copied and what the payload paths look like.
///
/// Only an INSTALLED package has a boundary worth preserving. Treating the
/// nearest `package.json` as the island root regardless was a silent secret leak:
/// for `require("./build/Release/addon.node")` that manifest is the user's whole
/// application, so `.git`, `.env`, `.npmrc`, and every source file were copied
/// into the distributed binary — no diagnostic, no size bound, and the checkout
/// directory's name hashed into the payload digest.
#[derive(Debug, Clone)]
enum Island {
    /// The owner lives under `node_modules` or a `.yarn` tree. Payload paths are
    /// relative to the directory holding that tree, which keeps the install
    /// geometry a loader walks to reach its companion shared libraries.
    Installed { anchor: PathBuf },
    /// The owner is not installed — a project-local addon, or a workspace sibling
    /// whose symlink canonicalization resolved away. The island is exactly the
    /// addon's own directory: enough for a co-built shared library beside the
    /// `.node` file, and nothing else.
    Colocated,
}

#[derive(Debug, Clone)]
struct Package {
    /// The directory whose subtree is copied. The manifest's own directory for an
    /// installed package; for [`Island::Colocated`] it is the addon's directory,
    /// which sits BELOW the manifest that `meta` came from.
    source_root: PathBuf,
    /// `source_root` relative to the island root, and the payload prefix for
    /// everything copied from it. Empty for a colocated island.
    logical_root: PathBuf,
    meta: PackageMeta,
}

#[derive(Debug, Clone)]
struct PackageMeta {
    name: String,
    version: Option<String>,
    os: Vec<String>,
    cpu: Vec<String>,
    libc: Vec<String>,
    dependencies: BTreeMap<String, EdgeKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    Required,
    Optional,
    Peer,
}

#[derive(Debug)]
pub struct IslandFile {
    pub name: String,
    pub bytes: Vec<u8>,
    /// The source file's Unix executable bit. Native packages ship real
    /// executables beside their addon (`esbuild`'s Go binary, a vendored helper),
    /// and an island is a verbatim copy of a package directory, so the mode has to
    /// travel with the bytes or the artifact spawns them with EACCES.
    pub executable: bool,
}

/// One collected file, before its payload name is known.
#[derive(Debug, PartialEq, Eq)]
struct Body {
    bytes: Vec<u8>,
    executable: bool,
}

#[derive(Debug)]
pub struct PlannedIsland {
    pub token: String,
    pub digest: String,
    pub files: Vec<IslandFile>,
    pub dropped: Vec<DroppedEdge>,
    pub summary: String,
}

/// An optional dependency of a native island that is not installed, so nothing
/// from it is in the payload.
///
/// `@img/sharp-<platform>` declares its libvips companion exactly this way, and a
/// missing one used to be invisible: the island holds the `.node` alone, the
/// object-header check passes, the compile succeeds, and the artifact dies at
/// `dlopen` on the user's machine. The build says so instead.
#[derive(Debug)]
pub struct DroppedEdge {
    /// The package that declared the edge.
    pub owner: String,
    pub name: String,
}

impl Seed {
    /// Locate the nearest package boundary and compute the wrapper path without
    /// walking package contents or dependencies. That defers all island copying
    /// until emitted chunks prove this seed survived tree-shaking.
    pub fn discover(path: &Path) -> Result<Self> {
        let source = fs::canonicalize(path)
            .with_context(|| format!("resolving native addon {}", path.display()))?;
        if !fs::metadata(&source)
            .with_context(|| format!("reading native addon metadata {}", source.display()))?
            .is_file()
        {
            bail!("native addon is not a regular file: {}", source.display());
        }
        let owner_root = nearest_package_root(&source)?;
        let meta = read_package(&owner_root)?;
        let (island, source_root, logical_root, addon_rel) = match install_anchor(&owner_root) {
            Some(anchor) => {
                let logical_root = owner_root
                    .strip_prefix(&anchor)
                    .map(Path::to_path_buf)
                    .map_err(|_| {
                        anyhow!(
                            "native package {} is outside its installation root {}",
                            owner_root.display(),
                            anchor.display()
                        )
                    })?;
                let addon_rel = source
                    .strip_prefix(&owner_root)
                    .map(Path::to_path_buf)
                    .map_err(|_| {
                        anyhow!(
                            "native addon {} escapes its owning package {}",
                            source.display(),
                            owner_root.display()
                        )
                    })?;
                (
                    Island::Installed { anchor },
                    owner_root,
                    logical_root,
                    addon_rel,
                )
            }
            None => {
                let (Some(directory), Some(file)) = (source.parent(), source.file_name()) else {
                    bail!(
                        "native addon {} has no containing directory",
                        source.display()
                    );
                };
                (
                    Island::Colocated,
                    directory.to_path_buf(),
                    PathBuf::new(),
                    PathBuf::from(file),
                )
            }
        };
        let token = hex_sha256(source.to_string_lossy().as_bytes());
        Ok(Self {
            token,
            source,
            owner: Package {
                source_root,
                logical_root,
                meta,
            },
            island,
            addon_rel,
        })
    }

    pub fn wrapper_path(&self) -> Result<String> {
        let rel = self.owner.logical_root.join(&self.addon_rel);
        Ok(format!(
            "{ISLAND_DIR}/{}/{}",
            self.token,
            portable_path(&rel)?
        ))
    }

    pub fn plan(&self, target: &TargetPlatform) -> Result<PlannedIsland> {
        ensure_metadata_matches(&self.owner.meta, target, &self.owner.source_root)?;

        let mut dropped = Vec::new();
        let mut logical = BTreeMap::<String, Body>::new();
        match &self.island {
            Island::Installed { anchor } => {
                let packages = self.reached_packages(anchor, target, &mut dropped)?;
                for package in packages.values() {
                    collect_package_files(package, Walk::Package, &mut logical)?;
                }
            }
            // No dependency traversal. The manifest above a colocated addon is the
            // application's, so its `dependencies` are the app's own: following
            // them would drag the installed tree into the binary, and any hoisted
            // one resolves above the island root and would hard-fail the build.
            Island::Colocated => {
                collect_package_files(&self.owner, Walk::ProjectDirectory, &mut logical)?;
            }
        }

        let addon_name = portable_path(&self.owner.logical_root.join(&self.addon_rel))?;
        if !logical.contains_key(&addon_name) {
            bail!(
                "native addon {} was not materialized from its owning package",
                self.source.display()
            );
        }

        let digest = digest_files(&logical);
        let count = logical.len();
        let total_bytes: usize = logical.values().map(|body| body.bytes.len()).sum();
        let files = logical
            .into_iter()
            .map(|(name, body)| IslandFile {
                name: format!("{ISLAND_DIR}/{digest}/{name}"),
                bytes: body.bytes,
                executable: body.executable,
            })
            .collect();
        let version = self
            .owner
            .meta
            .version
            .as_deref()
            .map(|v| format!("@{v}"))
            .unwrap_or_default();
        let addon = self
            .source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("native addon");
        let name = &self.owner.meta.name;
        Ok(PlannedIsland {
            token: self.token.clone(),
            digest,
            files,
            dropped,
            // Size is in the report because an island is copied whole and follows
            // the INSTALL graph, so a package that also pulls a build-time
            // toolchain can multiply the artifact with nothing else to show it.
            summary: format!(
                "{name}{version} ({addon}, {count} file(s), {})",
                human_bytes(total_bytes)
            ),
        })
    }

    /// Every package the owner's declared production edges reach, keyed by
    /// (real directory, payload path) so a pnpm binding and its store target are
    /// both materialized.
    fn reached_packages(
        &self,
        anchor: &Path,
        target: &TargetPlatform,
        dropped: &mut Vec<DroppedEdge>,
    ) -> Result<BTreeMap<(PathBuf, PathBuf), Package>> {
        let mut queue = VecDeque::from([self.owner.clone()]);
        let mut packages = BTreeMap::<(PathBuf, PathBuf), Package>::new();
        while let Some(package) = queue.pop_front() {
            let key = (package.source_root.clone(), package.logical_root.clone());
            if packages.contains_key(&key) {
                continue;
            }
            for (name, kind) in &package.meta.dependencies {
                let Some((binding, source_root, meta)) =
                    resolve_edge(&package.source_root, name, *kind)?
                else {
                    match kind {
                        EdgeKind::Required => bail!(
                            "native package {} requires {name}, but no installed binding was found\n{}",
                            package.meta.name,
                            architecture_advice()
                        ),
                        EdgeKind::Optional => dropped.push(DroppedEdge {
                            owner: package.meta.name.clone(),
                            name: name.clone(),
                        }),
                        // A peer is the CONSUMER's to provide, and the consumer's
                        // copy is what the bundler already resolved into a chunk.
                        // Only an optional edge names something the island itself
                        // was meant to carry.
                        EdgeKind::Peer => {}
                    }
                    continue;
                };
                if !metadata_matches(&meta, target) {
                    // A platform-pinned optional package for a foreign target is
                    // what a multi-platform install looks like, not an omission,
                    // so it is skipped without a report — otherwise every compile
                    // against a cross-platform tree would warn.
                    if *kind == EdgeKind::Optional {
                        continue;
                    }
                    ensure_metadata_matches(&meta, target, &source_root)?;
                }
                let logical_root = binding.strip_prefix(anchor).map(Path::to_path_buf)
                    .or_else(|_| source_root.strip_prefix(anchor).map(Path::to_path_buf))
                    .map_err(|_| anyhow!(
                        "native dependency {name} resolved outside the package installation root {}: {}\n{}",
                        anchor.display(), binding.display(), architecture_advice()
                    ))?;
                queue.push_back(Package {
                    source_root: source_root.clone(),
                    logical_root,
                    meta: meta.clone(),
                });
                // pnpm's binding is a symlink into its content-addressed store.
                // Preserve both the logical binding and the real store geometry,
                // but as regular files; Payload V2 deduplicates their bytes.
                if let Ok(real_logical) = source_root.strip_prefix(anchor)
                    && real_logical != binding.strip_prefix(anchor).unwrap_or(real_logical)
                {
                    let real_logical = real_logical.to_path_buf();
                    queue.push_back(Package {
                        source_root,
                        logical_root: real_logical,
                        meta,
                    });
                }
            }
            packages.insert(key, package);
        }
        Ok(packages)
    }
}

fn nearest_package_root(source: &Path) -> Result<PathBuf> {
    let mut current = source.parent();
    while let Some(dir) = current {
        if dir.join("package.json").is_file() {
            return fs::canonicalize(dir)
                .with_context(|| format!("resolving native package {}", dir.display()));
        }
        current = dir.parent();
    }
    bail!(
        "native addon {} has no owning package.json in any parent directory",
        source.display()
    )
}

/// The directory containing the install tree the owning package lives in.
///
/// `None` when the package is not installed at all, which is the whole basis of
/// [`Island::Colocated`]. There is deliberately no fallback: anchoring an
/// uninstalled package at its own parent made the island root a directory of the
/// user's project.
fn install_anchor(owner: &Path) -> Option<PathBuf> {
    let components: Vec<_> = owner.components().collect();
    let boundary = |tree: &str| {
        components
            .iter()
            .position(|part| matches!(part, Component::Normal(name) if *name == tree))
    };
    // `.yarn` first: an unplugged package sits at `.yarn/unplugged/<slot>/
    // node_modules/<pkg>`, and the outer tree is the one to anchor at.
    let index = boundary(".yarn").or_else(|| boundary("node_modules"))?;
    Some(
        components[..index]
            .iter()
            .map(|component| component.as_os_str())
            .collect(),
    )
}

fn read_package(root: &Path) -> Result<PackageMeta> {
    let manifest = root.join("package.json");
    let bytes = fs::read(&manifest)
        .with_context(|| format!("reading native package metadata {}", manifest.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing native package metadata {}", manifest.display()))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{} is not a JSON object", manifest.display()))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("native package {} has no string name", manifest.display()))?
        .to_string();
    let mut dependencies = BTreeMap::new();
    add_edges(
        &mut dependencies,
        object.get("peerDependencies"),
        EdgeKind::Peer,
    );
    add_edges(
        &mut dependencies,
        object.get("dependencies"),
        EdgeKind::Required,
    );
    add_edges(
        &mut dependencies,
        object.get("optionalDependencies"),
        EdgeKind::Optional,
    );
    if let Some(optional_peers) = object
        .get("peerDependenciesMeta")
        .and_then(Value::as_object)
    {
        for (name, meta) in optional_peers {
            if meta.get("optional").and_then(Value::as_bool) == Some(true)
                && dependencies.get(name) == Some(&EdgeKind::Peer)
            {
                dependencies.insert(name.clone(), EdgeKind::Optional);
            }
        }
    }
    Ok(PackageMeta {
        name,
        version: object
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        os: string_list(object.get("os")),
        cpu: string_list(object.get("cpu")),
        libc: string_list(object.get("libc")),
        dependencies,
    })
}

fn add_edges(out: &mut BTreeMap<String, EdgeKind>, value: Option<&Value>, kind: EdgeKind) {
    if let Some(map) = value.and_then(Value::as_object) {
        for name in map.keys() {
            out.insert(name.clone(), kind);
        }
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) => vec![value.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn metadata_matches(package: &PackageMeta, target: &TargetPlatform) -> bool {
    let os = match target.os {
        TargetOs::Darwin => "darwin",
        TargetOs::Linux => "linux",
        TargetOs::Win32 => "win32",
    };
    let cpu = match target.arch {
        TargetArch::X64 => "x64",
        TargetArch::Arm64 => "arm64",
    };
    let libc = (target.os == TargetOs::Linux).then_some(if target.musl { "musl" } else { "glibc" });
    axis_matches(&package.os, Some(os))
        && axis_matches(&package.cpu, Some(cpu))
        && axis_matches(&package.libc, libc)
}

fn axis_matches(values: &[String], target: Option<&str>) -> bool {
    if values.is_empty() {
        return true;
    }
    let mut has_positive = false;
    let mut positive_match = false;
    for value in values {
        if let Some(blocked) = value.strip_prefix('!') {
            if target == Some(blocked) {
                return false;
            }
        } else {
            has_positive = true;
            positive_match |= value == "any" || target == Some(value.as_str());
        }
    }
    !has_positive || positive_match
}

fn ensure_metadata_matches(
    package: &PackageMeta,
    target: &TargetPlatform,
    root: &Path,
) -> Result<()> {
    if metadata_matches(package, target) {
        return Ok(());
    }
    bail!(
        "native package {} is not compatible with target {}: {}\n{}",
        package.name,
        target.triple(),
        root.display(),
        architecture_advice()
    )
}

/// Names only Nub's own `--os` / `--cpu` / `--libc` install flags, for the same
/// reason [`super::native`]'s `check_target` does: the persistent
/// `supportedArchitectures` setting is read only from an incumbent pnpm or
/// yarn's config, so it is advice a nub, npm or bun project cannot follow.
fn architecture_advice() -> &'static str {
    "\x20\x20Install a compatible optional package before compiling.\n\
     \n\x20\x20Select it with nub install --os <os> --cpu <cpu> --libc <libc>, then compile again.\n\
     \x20\x20If no prebuilt exists for the target, install on the target platform itself; a\n\
     \x20\x20container is the usual way."
}

/// Resolve a declared edge to an installed, readable package.
///
/// `None` means "not installed for this island's purposes". For an OPTIONAL edge
/// that also covers a directory that is not a readable package: a stray entry
/// under an optional dependency's name is not something to fail a whole compile
/// over, and the caller reports the omission either way.
fn resolve_edge(
    package_root: &Path,
    name: &str,
    kind: EdgeKind,
) -> Result<Option<(PathBuf, PathBuf, PackageMeta)>> {
    let Some((binding, source_root)) = resolve_dependency(package_root, name, kind)? else {
        return Ok(None);
    };
    match read_package(&source_root) {
        Ok(meta) => Ok(Some((binding, source_root, meta))),
        Err(_) if kind == EdgeKind::Optional => Ok(None),
        Err(error) => Err(error),
    }
}

fn resolve_dependency(
    package_root: &Path,
    name: &str,
    kind: EdgeKind,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let parts = package_name_parts(name)?;
    let mut current = Some(package_root);
    while let Some(dir) = current {
        let mut candidate = dir.join("node_modules");
        for part in &parts {
            candidate.push(part);
        }
        if fs::symlink_metadata(&candidate).is_ok() {
            let source = fs::canonicalize(&candidate).with_context(|| {
                format!(
                    "resolving native dependency binding {}",
                    candidate.display()
                )
            })?;
            if !source.join("package.json").is_file() {
                if kind == EdgeKind::Optional {
                    return Ok(None);
                }
                bail!(
                    "native dependency binding {} has no package.json",
                    candidate.display()
                );
            }
            return Ok(Some((candidate, source)));
        }
        current = dir.parent();
    }
    Ok(None)
}

fn package_name_parts(name: &str) -> Result<Vec<&str>> {
    let parts: Vec<_> = name.split('/').collect();
    let valid = match parts.as_slice() {
        [plain] => !plain.is_empty() && *plain != "." && *plain != "..",
        [scope, plain] => scope.starts_with('@') && scope.len() > 1 && !plain.is_empty(),
        _ => false,
    };
    if !valid
        || parts
            .iter()
            .any(|part| *part == "." || *part == ".." || part.contains('\\'))
    {
        bail!("native package metadata contains an unsafe dependency name {name:?}");
    }
    Ok(parts)
}

/// What a directory walk may sweep up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// An installed package directory: published content, taken whole. Its dot
    /// entries are part of the package, so they are kept.
    Package,
    /// A directory inside the user's own project, where a dot entry is the user's
    /// state and not the addon's. `.git`, `.env`, and `.npmrc` are skipped because
    /// this walk can begin AT a project root — a `.node` file sitting beside a
    /// `package.json` — and the result is a distributed artifact.
    ProjectDirectory,
}

fn collect_package_files(
    package: &Package,
    walk: Walk,
    out: &mut BTreeMap<String, Body>,
) -> Result<()> {
    let canonical_root = fs::canonicalize(&package.source_root)?;
    let mut stack = BTreeSet::new();
    collect_dir(
        &canonical_root,
        &canonical_root,
        &package.logical_root,
        walk,
        &mut stack,
        out,
    )
}

fn collect_dir(
    package_root: &Path,
    source_dir: &Path,
    logical_dir: &Path,
    walk: Walk,
    stack: &mut BTreeSet<PathBuf>,
    out: &mut BTreeMap<String, Body>,
) -> Result<()> {
    let canonical_dir = fs::canonicalize(source_dir).with_context(|| {
        format!(
            "resolving native package directory {}",
            source_dir.display()
        )
    })?;
    if !canonical_dir.starts_with(package_root) {
        bail!(
            "native package symlink escapes its package root: {}",
            source_dir.display()
        );
    }
    if !stack.insert(canonical_dir.clone()) {
        bail!(
            "native package contains a directory symlink cycle at {}",
            source_dir.display()
        );
    }
    let mut entries = fs::read_dir(source_dir)
        .with_context(|| format!("reading native package directory {}", source_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name == "node_modules"
            || (walk == Walk::ProjectDirectory && name.as_encoded_bytes().starts_with(b"."))
        {
            continue;
        }
        let source = entry.path();
        let logical = logical_dir.join(&name);
        collect_entry(package_root, &source, &logical, walk, stack, out)?;
    }
    stack.remove(&canonical_dir);
    Ok(())
}

fn collect_entry(
    package_root: &Path,
    source: &Path,
    logical: &Path,
    walk: Walk,
    stack: &mut BTreeSet<PathBuf>,
    out: &mut BTreeMap<String, Body>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("reading native package entry {}", source.display()))?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        let target = fs::canonicalize(source)
            .with_context(|| format!("resolving native package symlink {}", source.display()))?;
        if !target.starts_with(package_root) {
            bail!(
                "native package symlink escapes its package root: {}",
                source.display()
            );
        }
        let target_meta = fs::metadata(&target).with_context(|| {
            format!("reading native package symlink target {}", target.display())
        })?;
        if target_meta.is_dir() {
            return collect_dir(package_root, &target, logical, walk, stack, out);
        }
        if !target_meta.is_file() {
            bail!(
                "native package symlink does not target a regular file: {}",
                source.display()
            );
        }
        return insert_file(
            logical,
            fs::read(&target).with_context(|| format!("reading {}", target.display()))?,
            source_mode(&target_meta),
            out,
        );
    }
    if kind.is_dir() {
        return collect_dir(package_root, source, logical, walk, stack, out);
    }
    if kind.is_file() {
        return insert_file(
            logical,
            fs::read(source).with_context(|| format!("reading {}", source.display()))?,
            source_mode(&metadata),
            out,
        );
    }
    bail!(
        "native package entry is not a regular file or directory (devices, FIFOs, and sockets cannot be embedded): {}",
        source.display()
    )
}

fn insert_file(
    logical: &Path,
    bytes: Vec<u8>,
    mode: Option<u32>,
    out: &mut BTreeMap<String, Body>,
) -> Result<()> {
    let name = portable_path(logical)?;
    let body = Body {
        bytes,
        executable: mode.is_some_and(|mode| mode & 0o111 != 0),
    };
    match out.get(&name) {
        Some(existing) if existing == &body => Ok(()),
        Some(_) => bail!("native package island path {name:?} identifies different bytes"),
        None => {
            out.insert(name, body);
            Ok(())
        }
    }
}

fn portable_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| {
                anyhow!("native package path is not valid UTF-8: {}", path.display())
            })?),
            _ => bail!(
                "native package path is not safely relative: {}",
                path.display()
            ),
        }
    }
    if parts.is_empty() {
        bail!("native package path is empty");
    }
    Ok(parts.join("/"))
}

/// The island's content address. The executable bit is part of what gets
/// materialized, so it is hashed alongside the bytes — two islands that differ
/// only there must not land on one payload path.
fn digest_files(files: &BTreeMap<String, Body>) -> String {
    let mut hash = Sha256::new();
    for (name, body) in files {
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((body.bytes.len() as u64).to_le_bytes());
        hash.update(&body.bytes);
        hash.update([u8::from(body.executable)]);
    }
    hex::encode(hash.finalize())
}

/// The source file's Unix mode, or `None` on a host that has none. Windows
/// expresses executability through ACLs and the filename, so there is nothing to
/// read — see `nub_core::compile::AppFile::from_source_mode` for why guessing is
/// worse than recording false.
#[cfg(unix)]
fn source_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    Some(metadata.permissions().mode())
}
#[cfg(not(unix))]
fn source_mode(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Island size for the build report, in the decimal units the rest of `compile`
/// prints.
fn human_bytes(bytes: usize) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{:.0} KB", bytes as f64 / 1_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture(tag: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "nub-native-layout-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn package(root: &Path, manifest: &str, files: &[(&str, &[u8])]) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("package.json"), manifest).unwrap();
        for (name, bytes) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }

    #[test]
    fn target_axes_include_libc_and_negation() {
        let package = PackageMeta {
            name: "fixture".into(),
            version: None,
            os: vec!["linux".into(), "!darwin".into()],
            cpu: vec!["x64".into()],
            libc: vec!["musl".into()],
            dependencies: BTreeMap::new(),
        };
        assert!(metadata_matches(
            &package,
            &TargetPlatform::parse("linux-x64-musl").unwrap()
        ));
        assert!(!metadata_matches(
            &package,
            &TargetPlatform::parse("linux-x64").unwrap()
        ));
    }

    #[test]
    fn island_digest_is_order_independent_and_content_sensitive() {
        let body = |bytes: Vec<u8>| Body {
            bytes,
            executable: false,
        };
        let a = BTreeMap::from([("b".into(), body(vec![2])), ("a".into(), body(vec![1]))]);
        let b = BTreeMap::from([("a".into(), body(vec![1])), ("b".into(), body(vec![2]))]);
        assert_eq!(digest_files(&a), digest_files(&b));
        let changed = BTreeMap::from([("a".into(), body(vec![9])), ("b".into(), body(vec![2]))]);
        assert_ne!(digest_files(&a), digest_files(&changed));
        // The mode is materialized alongside the bytes, so it addresses the island too.
        let executable = BTreeMap::from([
            (
                "a".into(),
                Body {
                    bytes: vec![1],
                    executable: true,
                },
            ),
            ("b".into(), body(vec![2])),
        ]);
        assert_ne!(digest_files(&a), digest_files(&executable));
    }

    #[test]
    fn planner_follows_only_declared_dependency_edges_and_keeps_geometry() {
        let root = fixture("closure");
        let modules = root.join("node_modules");
        let addon = modules.join("addon");
        package(
            &addon,
            r#"{"name":"addon","version":"1.2.3","dependencies":{"companion":"1"}}"#,
            &[("build/Release/addon.node", b"native")],
        );
        package(
            &modules.join("companion"),
            r#"{"name":"companion","version":"1.0.0"}"#,
            &[("lib/companion.so", b"shared")],
        );
        package(
            &modules.join("unreached"),
            r#"{"name":"unreached","version":"1.0.0"}"#,
            &[("should-not-copy", b"no")],
        );

        let seed = Seed::discover(&addon.join("build/Release/addon.node")).unwrap();
        let plan = seed
            .plan(&TargetPlatform::parse("linux-x64").unwrap())
            .unwrap();
        let names: BTreeSet<_> = plan.files.iter().map(|file| file.name.as_str()).collect();
        assert!(
            names
                .iter()
                .any(|name| name.ends_with("node_modules/addon/build/Release/addon.node"))
        );
        assert!(
            names
                .iter()
                .any(|name| name.ends_with("node_modules/companion/lib/companion.so"))
        );
        assert!(!names.iter().any(|name| name.contains("unreached")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn yarn_unplugged_owner_geometry_is_retained_in_the_wrapper_path() {
        let root = fixture("yarn-unplugged");
        let addon = root.join(".yarn/unplugged/addon/node_modules/addon");
        package(
            &addon,
            r#"{"name":"addon","version":"1.0.0"}"#,
            &[("build/addon.node", b"native")],
        );
        let wrapper = Seed::discover(&addon.join("build/addon.node"))
            .unwrap()
            .wrapper_path()
            .unwrap();
        assert!(wrapper.contains("/.yarn/unplugged/addon/node_modules/addon/build/addon.node"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_project_local_addon_embeds_only_its_own_directory() {
        let root = fixture("project-local");
        // `absent` is deliberately not installed: the manifest above a colocated
        // addon is the application's, so its edges must never be traversed.
        package(
            &root,
            r#"{"name":"app","version":"1.0.0","dependencies":{"absent":"1"}}"#,
            &[
                (".env", b"SECRET=1"),
                (".git/config", b"[core]"),
                ("src/app.js", b"console.log(1)"),
                ("native/addon.node", b"native"),
                ("native/libcompanion.so", b"shared"),
                ("native/.env", b"SECRET=2"),
            ],
        );

        let plan = Seed::discover(&root.join("native/addon.node"))
            .unwrap()
            .plan(&TargetPlatform::parse("linux-x64").unwrap())
            .unwrap();
        let prefix = format!("{ISLAND_DIR}/{}/", plan.digest);
        let names: BTreeSet<_> = plan
            .files
            .iter()
            .map(|file| {
                file.name
                    .strip_prefix(prefix.as_str())
                    .unwrap_or(&file.name)
            })
            .collect();
        assert_eq!(
            names,
            BTreeSet::from(["addon.node", "libcompanion.so"]),
            "the island is the addon's directory, never the user's project tree"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_project_local_island_digest_ignores_the_checkout_directory() {
        let roots: Vec<_> = ["checkout-a", "checkout-b"]
            .into_iter()
            .map(|tag| {
                let root = fixture(tag);
                package(
                    &root,
                    r#"{"name":"app","version":"1.0.0"}"#,
                    &[("native/addon.node", b"native")],
                );
                root
            })
            .collect();

        let digests: BTreeSet<_> = roots
            .iter()
            .map(|root| {
                Seed::discover(&root.join("native/addon.node"))
                    .unwrap()
                    .plan(&TargetPlatform::parse("linux-x64").unwrap())
                    .unwrap()
                    .digest
            })
            .collect();
        assert_eq!(
            digests.len(),
            1,
            "a payload digest must not carry the build host's directory names"
        );
        for root in roots {
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn an_uninstalled_optional_dependency_is_reported_and_a_foreign_one_is_not() {
        let root = fixture("optional-edges");
        let modules = root.join("node_modules");
        package(
            &modules.join("addon"),
            r#"{"name":"addon","version":"1.0.0","optionalDependencies":{"absent":"1","foreign":"1"}}"#,
            &[("addon.node", b"native")],
        );
        package(
            &modules.join("foreign"),
            r#"{"name":"foreign","version":"1.0.0","os":["win32"]}"#,
            &[("lib/foreign.dll", b"windows")],
        );

        let plan = Seed::discover(&modules.join("addon/addon.node"))
            .unwrap()
            .plan(&TargetPlatform::parse("linux-x64").unwrap())
            .unwrap();
        assert_eq!(
            plan.dropped
                .iter()
                .map(|edge| edge.name.as_str())
                .collect::<Vec<_>>(),
            ["absent"],
            "an uninstalled optional dependency is the silent dlopen failure, so it is reported"
        );
        assert!(
            !plan.files.iter().any(|file| file.name.contains("foreign")),
            "an optional package pinned to another platform is skipped without a report"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_malformed_optional_dependency_is_treated_as_absent() {
        for (tag, manifest) in [("no-manifest", None), ("unparseable", Some("{ not json"))] {
            let root = fixture(tag);
            let modules = root.join("node_modules");
            package(
                &modules.join("addon"),
                r#"{"name":"addon","version":"1.0.0","optionalDependencies":{"companion":"1"}}"#,
                &[("addon.node", b"native")],
            );
            let companion = modules.join("companion");
            fs::create_dir_all(&companion).unwrap();
            if let Some(manifest) = manifest {
                fs::write(companion.join("package.json"), manifest).unwrap();
            }

            let plan = Seed::discover(&modules.join("addon/addon.node"))
                .unwrap()
                .plan(&TargetPlatform::parse("linux-x64").unwrap())
                .unwrap_or_else(|err| {
                    panic!("a stray {tag} directory must not fail the compile: {err:#}")
                });
            assert_eq!(
                plan.dropped
                    .iter()
                    .map(|edge| edge.name.as_str())
                    .collect::<Vec<_>>(),
                ["companion"],
                "it is reported on the same channel as one that is simply absent ({tag})"
            );
            let _ = fs::remove_dir_all(root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn package_symlinks_are_materialized_only_when_they_stay_inside() {
        use std::os::unix::fs::symlink;

        let root = fixture("symlink-escape");
        let addon = root.join("node_modules/addon");
        package(
            &addon,
            r#"{"name":"addon","version":"1.0.0"}"#,
            &[("addon.node", b"native"), ("inside.dat", b"inside")],
        );
        symlink("inside.dat", addon.join("alias.dat")).unwrap();
        fs::write(root.join("outside.dat"), b"outside").unwrap();
        symlink(root.join("outside.dat"), addon.join("escape.dat")).unwrap();

        let err = Seed::discover(&addon.join("addon.node"))
            .unwrap()
            .plan(&TargetPlatform::parse("linux-x64").unwrap())
            .unwrap_err();
        assert!(format!("{err:#}").contains("escapes its package root"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn pnpm_binding_and_store_geometry_are_both_regular_island_entries() {
        use std::os::unix::fs::symlink;

        let root = fixture("pnpm-geometry");
        let store = root.join("node_modules/.pnpm");
        let addon_slot = store.join("addon@1.0.0/node_modules");
        let addon = addon_slot.join("addon");
        let companion = store.join("companion@1.0.0/node_modules/companion");
        package(
            &addon,
            r#"{"name":"addon","version":"1.0.0","dependencies":{"companion":"1"}}"#,
            &[("addon.node", b"native")],
        );
        package(
            &companion,
            r#"{"name":"companion","version":"1.0.0"}"#,
            &[("lib/libcompanion.so", b"shared")],
        );
        symlink(&companion, addon_slot.join("companion")).unwrap();

        let plan = Seed::discover(&addon.join("addon.node"))
            .unwrap()
            .plan(&TargetPlatform::parse("linux-x64").unwrap())
            .unwrap();
        let names: BTreeSet<_> = plan.files.iter().map(|file| file.name.as_str()).collect();
        assert!(names.iter().any(|name| {
            name.ends_with(".pnpm/addon@1.0.0/node_modules/companion/lib/libcompanion.so")
        }));
        assert!(names.iter().any(|name| {
            name.ends_with(".pnpm/companion@1.0.0/node_modules/companion/lib/libcompanion.so")
        }));
        let _ = fs::remove_dir_all(root);
    }

    /// The release pre-publish gate compiles a loose `.node` sitting beside its
    /// entry, so the manifest above it is the application's own. That manifest is
    /// REQUIRED — the gate staged its fixture without one and failed every
    /// platform leg on "no owning package.json in any parent directory".
    #[test]
    fn a_loose_addon_needs_an_application_manifest_above_it() {
        let root = fixture("colocated-app");
        package(
            &root,
            r#"{"name":"app","version":"0.0.0","private":true,"type":"module"}"#,
            &[("addon.node", b"\0")],
        );
        let seed = Seed::discover(&root.join("addon.node")).unwrap();
        assert!(
            matches!(seed.island, Island::Colocated),
            "an app manifest outside any install tree is the colocated island, got {:?}",
            seed.island
        );

        // Without it the addon has no owner at all, which is the gate's shape.
        let bare = fixture("colocated-bare");
        fs::write(bare.join("addon.node"), b"\0").unwrap();
        let err = Seed::discover(&bare.join("addon.node"))
            .expect_err("a loose addon with no manifest above it must not resolve")
            .to_string();
        assert!(
            err.contains("no owning package.json"),
            "the error must name the missing manifest, got: {err}"
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(bare);
    }
}
