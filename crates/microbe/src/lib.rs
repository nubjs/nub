//! microbe — install one npm package and its dependency tree into a directory.
//!
//! ```no_run
//! let installed = microbe::Microbe::new()?.install("esbuild@^0.25", std::path::Path::new("/tmp/x"))?;
//! println!("{} {} {:?}", installed.name, installed.version, installed.bins);
//! # Ok::<(), microbe::Error>(())
//! ```
//!
//! What it does: abbreviated packument fetch, semver selection, tarball download with
//! integrity verification, extraction, and recursion over `dependencies` and platform-
//! matching `optionalDependencies`. Packages land flat under `<dir>/node_modules/`, and a
//! version conflict nests the loser under its dependent, exactly as Node's resolver expects.
//!
//! What it deliberately does not do: run lifecycle scripts (reported instead, see
//! [`Installed::skipped_install_scripts`]), honour `peerDependencies`, write a lockfile, keep
//! a cache or store, or reconcile with an existing `node_modules`. Those are the parts of a
//! package manager that take up the space.

mod error;
mod extract;
pub mod registry;
pub mod transport;

pub use error::Error;
pub use transport::Transport;

use registry::{Manifest, Packument};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
const ABBREVIATED: &str = "application/vnd.npm.install-v1+json";

pub struct Microbe {
    transport: Box<dyn Transport>,
    registry: String,
    packuments: Mutex<HashMap<String, Packument>>,
}

/// What an install produced. `bins` maps each command the root package declares to the
/// absolute path of its script, with the executable bit set.
#[derive(Debug)]
pub struct Installed {
    pub name: String,
    pub version: String,
    /// `<dir>/node_modules/<name>`.
    pub dir: PathBuf,
    pub bins: BTreeMap<String, PathBuf>,
    /// Packages extracted, the root included.
    pub packages: usize,
    /// `name@version` of every package whose install script was NOT run.
    pub skipped_install_scripts: Vec<String>,
}

impl Microbe {
    /// Uses the first transport the host provides; see [`transport::detect`].
    pub fn new() -> Result<Self, Error> {
        Ok(Self::from_boxed(transport::detect()?))
    }

    pub fn with_transport(transport: impl Transport + 'static) -> Self {
        Self::from_boxed(Box::new(transport))
    }

    fn from_boxed(transport: Box<dyn Transport>) -> Self {
        Microbe {
            transport,
            registry: DEFAULT_REGISTRY.to_string(),
            packuments: Mutex::new(HashMap::new()),
        }
    }

    pub fn registry(mut self, url: &str) -> Self {
        self.registry = url.trim_end_matches('/').to_string();
        self
    }

    /// `spec` is `name`, `name@tag`, `name@version` or `name@range` (`@scope/name@^1` works).
    /// `dir` is created if needed; packages go under `dir/node_modules/`.
    pub fn install(&self, spec: &str, dir: &Path) -> Result<Installed, Error> {
        let (name, range) = split_spec(spec);
        std::fs::create_dir_all(dir)?;
        let root = dir.canonicalize()?;
        let mut state = State::default();
        let (version, manifest, pkg_dir) = self.place(&root, &root, name, range, &mut state)?;
        let mut bins = BTreeMap::new();
        for (cmd, rel) in manifest.bin.entries(name) {
            let path = pkg_dir.join(rel);
            make_executable(&path)?;
            bins.insert(cmd, path);
        }
        Ok(Installed {
            name: name.to_string(),
            version,
            dir: pkg_dir,
            bins,
            packages: state.packages,
            skipped_install_scripts: state.skipped_install_scripts,
        })
    }

    /// Install `name@range` for a dependent living at `parent`, then its own dependencies.
    /// Returns the version, manifest, and directory used — whether newly extracted or an
    /// already-present package that satisfies the range.
    fn place(
        &self,
        root: &Path,
        parent: &Path,
        name: &str,
        range: &str,
        state: &mut State,
    ) -> Result<(String, Manifest, PathBuf), Error> {
        if let Some((version, dir)) = find_satisfying(root, parent, name, range)? {
            let manifest = self.manifest_for(name, &version)?;
            return Ok((version, manifest, dir));
        }
        let (version, manifest) = self.with_packument(name, |p| {
            registry::pick(p, name, range).map(|(v, m)| (v.to_string(), m.clone()))
        })??;
        let flat = root.join("node_modules").join(name);
        let dir = if flat.exists() {
            parent.join("node_modules").join(name)
        } else {
            flat
        };
        let tgz = self
            .transport
            .get(&manifest.dist.tarball, "application/octet-stream")?;
        extract::verify(&tgz, &manifest.dist, name, &version)?;
        extract::extract(&tgz, &dir)?;
        state.packages += 1;
        if manifest.has_install_script {
            state
                .skipped_install_scripts
                .push(format!("{name}@{version}"));
        }
        for (dep, dep_range) in &manifest.dependencies {
            self.place(root, &dir, dep, dep_range, state)?;
        }
        for (dep, dep_range) in &manifest.optional_dependencies {
            // An optional dependency may legitimately be absent for this platform, or fail
            // to install; npm proceeds without it either way.
            if let Ok(true) = self.optional_allowed(dep, dep_range) {
                let _ = self.place(root, &dir, dep, dep_range, state);
            }
        }
        Ok((version, manifest, dir))
    }

    fn optional_allowed(&self, name: &str, range: &str) -> Result<bool, Error> {
        self.with_packument(name, |p| {
            registry::pick(p, name, range).map(|(_, m)| registry::platform_allowed(m))
        })?
    }

    fn manifest_for(&self, name: &str, version: &str) -> Result<Manifest, Error> {
        self.with_packument(name, |p| {
            p.versions
                .get(version)
                .cloned()
                .ok_or_else(|| Error::NoVersion {
                    name: name.to_string(),
                    spec: version.to_string(),
                })
        })?
    }

    /// Run `f` over the abbreviated packument for `name`, fetching it on first use. One
    /// registry round-trip per package name per install, however many dependents share it.
    fn with_packument<R>(&self, name: &str, f: impl FnOnce(&Packument) -> R) -> Result<R, Error> {
        let mut cache = self
            .packuments
            .lock()
            .map_err(|_| Error::Transport("packument cache poisoned".into()))?;
        if !cache.contains_key(name) {
            let url = format!("{}/{}", self.registry, name.replace('/', "%2f"));
            let body = self.transport.get(&url, ABBREVIATED)?;
            cache.insert(name.to_string(), registry::parse(name, &body)?);
        }
        Ok(f(&cache[name]))
    }
}

#[derive(Default)]
struct State {
    packages: usize,
    skipped_install_scripts: Vec<String>,
}

/// Walk from the dependent's directory up to the install root looking for an installed
/// `name` that satisfies `range` — Node's own resolution order, so whatever is found here is
/// what `require` will find too.
fn find_satisfying(
    root: &Path,
    parent: &Path,
    name: &str,
    range: &str,
) -> Result<Option<(String, PathBuf)>, Error> {
    let mut dir = Some(parent);
    while let Some(d) = dir {
        let candidate = d.join("node_modules").join(name);
        if let Some(version) = installed_version(&candidate)?
            && registry::satisfies(&version, range)
        {
            return Ok(Some((version, candidate)));
        }
        if d == root {
            break;
        }
        dir = d.parent();
    }
    Ok(None)
}

fn installed_version(pkg_dir: &Path) -> Result<Option<String>, Error> {
    let manifest = pkg_dir.join("package.json");
    if !manifest.is_file() {
        return Ok(None);
    }
    #[derive(serde::Deserialize)]
    struct V {
        version: String,
    }
    let v: V = serde_json::from_slice(&std::fs::read(&manifest)?).map_err(|e| Error::Registry {
        name: pkg_dir.display().to_string(),
        detail: e.to_string(),
    })?;
    Ok(Some(v.version))
}

/// `@scope/name@^1` → (`@scope/name`, `^1`); a bare name has an empty range (→ `latest`).
fn split_spec(spec: &str) -> (&str, &str) {
    match spec.rfind('@') {
        Some(i) if i > 0 => (&spec[..i], &spec[i + 1..]),
        _ => (spec, ""),
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_spec;

    #[test]
    fn spec_splits_on_the_last_at_sign_only() {
        assert_eq!(split_spec("chalk"), ("chalk", ""));
        assert_eq!(split_spec("chalk@5"), ("chalk", "5"));
        assert_eq!(split_spec("@scope/name"), ("@scope/name", ""));
        assert_eq!(split_spec("@scope/name@^1.2"), ("@scope/name", "^1.2"));
    }
}
