//! microbe — install one npm package and its dependency tree into a directory.
//!
//! ```no_run
//! let installed = microbe::Microbe::new()?.install("esbuild@^0.25", std::path::Path::new("/tmp/x"))?;
//! println!("{} {} {:?}", installed.name, installed.version, installed.bins);
//! # Ok::<(), microbe::Error>(())
//! ```
//!
//! Two phases. **Plan**: a breadth-first walk over `dependencies` and platform-matching
//! `optionalDependencies`, fetching each level's packuments in parallel and deciding every
//! package's directory deterministically — flat under `<dir>/node_modules/`, with a version
//! conflict nested under its dependent, exactly as Node's resolver expects. **Materialize**:
//! every planned tarball downloaded, verified and extracted in parallel. The split is what
//! makes the install latency-bound on the slowest single fetch rather than on their sum.
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
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
const ABBREVIATED: &str = "application/vnd.npm.install-v1+json";
/// Matches npm's and pnpm's default network concurrency.
const DEFAULT_CONCURRENCY: usize = 16;

pub struct Microbe {
    transport: Box<dyn Transport>,
    registry: String,
    concurrency: usize,
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
            concurrency: DEFAULT_CONCURRENCY,
            packuments: Mutex::new(HashMap::new()),
        }
    }

    pub fn registry(mut self, url: &str) -> Self {
        self.registry = url.trim_end_matches('/').to_string();
        self
    }

    /// Simultaneous registry requests, for both packuments and tarballs.
    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    /// `spec` is `name`, `name@tag`, `name@version` or `name@range` (`@scope/name@^1` works).
    /// `dir` is created if needed; packages go under `dir/node_modules/`.
    pub fn install(&self, spec: &str, dir: &Path) -> Result<Installed, Error> {
        let (name, range) = split_spec(spec);
        std::fs::create_dir_all(dir)?;
        let root = dir.canonicalize()?;
        let plan = self.plan(&root, name, range)?;
        let extracted = self.materialize(&plan)?;
        let head = &plan.packages[0];
        let mut bins = BTreeMap::new();
        for (cmd, rel) in head.manifest.bin.entries(name) {
            let path = head.dir.join(rel);
            make_executable(&path)?;
            bins.insert(cmd, path);
        }
        Ok(Installed {
            name: name.to_string(),
            version: head.version.clone(),
            dir: head.dir.clone(),
            bins,
            packages: extracted,
            skipped_install_scripts: plan
                .packages
                .iter()
                .filter(|p| p.manifest.has_install_script)
                .map(|p| format!("{}@{}", p.name, p.version))
                .collect(),
        })
    }

    /// Phase one. Breadth-first so that placement is deterministic: whichever version of a
    /// name is reached first from the root takes the flat slot, and later conflicting
    /// versions nest under their dependents. Each level's packuments are fetched together
    /// before any of that level is placed.
    fn plan(&self, root: &Path, name: &str, range: &str) -> Result<Plan, Error> {
        let mut plan = Plan::default();
        let mut level: VecDeque<Want> = VecDeque::from([Want {
            parent: root.to_path_buf(),
            name: name.to_string(),
            range: range.to_string(),
            optional: false,
        }]);
        while !level.is_empty() {
            self.prefetch(level.iter().map(|w| w.name.as_str()));
            let mut next = VecDeque::new();
            for want in level.drain(..) {
                if let Some(planned) = self.place(root, &mut plan, &want)? {
                    next.extend(planned.wants());
                }
            }
            level = next;
        }
        if plan.packages.is_empty() {
            // The root was already present at a satisfying version; plan it anyway so the
            // caller gets its manifest and bins.
            let (version, dir) = plan
                .satisfied(root, root, name, range)?
                .expect("root either planned or found");
            let manifest = self.manifest_for(name, &version)?;
            plan.packages
                .push(Planned::existing(name, version, dir, manifest));
        }
        Ok(plan)
    }

    /// Decide where one wanted package goes, or that nothing needs doing. Returns the new
    /// entry when a fetch is needed so the caller can enqueue its dependencies.
    fn place(&self, root: &Path, plan: &mut Plan, want: &Want) -> Result<Option<Planned>, Error> {
        let Want {
            parent,
            name,
            range,
            optional,
        } = want;
        if plan.satisfied(root, parent, name, range)?.is_some() {
            return Ok(None);
        }
        let picked = self.with_packument(name, |p| {
            registry::pick(p, name, range).map(|(v, m)| (v.to_string(), m.clone()))
        });
        let (version, manifest) = match picked {
            Ok(Ok(vm)) => vm,
            // An optional dependency may be unpublished, or fail to resolve; npm proceeds.
            Err(_) | Ok(Err(_)) if *optional => return Ok(None),
            Err(e) | Ok(Err(e)) => return Err(e),
        };
        if *optional && !registry::platform_allowed(&manifest) {
            return Ok(None);
        }
        let flat = root.join("node_modules").join(name);
        let dir = if plan.placed.contains_key(&flat) || installed_version(&flat)?.is_some() {
            parent.join("node_modules").join(name)
        } else {
            flat
        };
        plan.placed.insert(dir.clone(), version.clone());
        let planned = Planned {
            name: name.clone(),
            version,
            dir,
            manifest,
            optional: *optional,
            fetch: true,
        };
        plan.packages.push(planned.clone());
        Ok(Some(planned))
    }

    /// Phase two: every planned tarball, `concurrency` at a time. A failed optional package
    /// is dropped and its directory removed; any other failure aborts the install.
    fn materialize(&self, plan: &Plan) -> Result<usize, Error> {
        let todo: Vec<&Planned> = plan.packages.iter().filter(|p| p.fetch).collect();
        let next = AtomicUsize::new(0);
        let extracted = AtomicUsize::new(0);
        let failures: Mutex<Vec<Error>> = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..self.concurrency.min(todo.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(p) = todo.get(i) else { break };
                        match self.fetch_one(p) {
                            Ok(()) => {
                                extracted.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) if p.optional => {
                                let _ = std::fs::remove_dir_all(&p.dir);
                            }
                            Err(e) => {
                                if let Ok(mut f) = failures.lock() {
                                    f.push(e);
                                }
                            }
                        }
                    }
                });
            }
        });
        let mut failures = failures.into_inner().unwrap_or_default();
        if !failures.is_empty() {
            return Err(failures.remove(0));
        }
        Ok(extracted.into_inner())
    }

    fn fetch_one(&self, p: &Planned) -> Result<(), Error> {
        let tgz = self
            .transport
            .get(&p.manifest.dist.tarball, "application/octet-stream")?;
        extract::verify(&tgz, &p.manifest.dist, &p.name, &p.version)?;
        extract::extract(&tgz, &p.dir)
    }

    /// Fetch every packument in `names` that is not cached yet, `concurrency` at a time.
    /// Failures are not cached: the later serial lookup refetches and reports them.
    fn prefetch<'a>(&self, names: impl Iterator<Item = &'a str>) {
        let missing: Vec<&str> = {
            let Ok(cache) = self.packuments.lock() else {
                return;
            };
            let mut seen = std::collections::HashSet::new();
            names
                .filter(|n| !cache.contains_key(*n) && seen.insert(*n))
                .collect()
        };
        if missing.len() < 2 {
            return;
        }
        let next = AtomicUsize::new(0);
        let fetched: Mutex<Vec<(String, Packument)>> = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..self.concurrency.min(missing.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(name) = missing.get(i) else { break };
                        if let Ok(p) = self.fetch_packument(name)
                            && let Ok(mut f) = fetched.lock()
                        {
                            f.push((name.to_string(), p));
                        }
                    }
                });
            }
        });
        if let Ok(mut cache) = self.packuments.lock() {
            for (name, p) in fetched.into_inner().unwrap_or_default() {
                cache.entry(name).or_insert(p);
            }
        }
    }

    fn fetch_packument(&self, name: &str) -> Result<Packument, Error> {
        let url = format!("{}/{}", self.registry, name.replace('/', "%2f"));
        let body = self.transport.get(&url, ABBREVIATED)?;
        registry::parse(name, &body)
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
            let p = self.fetch_packument(name)?;
            cache.insert(name.to_string(), p);
        }
        Ok(f(&cache[name]))
    }
}

/// One dependency edge waiting to be placed.
struct Want {
    parent: PathBuf,
    name: String,
    range: String,
    optional: bool,
}

#[derive(Clone)]
struct Planned {
    name: String,
    version: String,
    dir: PathBuf,
    manifest: Manifest,
    optional: bool,
    /// False for a package already on disk at a satisfying version.
    fetch: bool,
}

impl Planned {
    fn existing(name: &str, version: String, dir: PathBuf, manifest: Manifest) -> Self {
        Planned {
            name: name.to_string(),
            version,
            dir,
            manifest,
            optional: false,
            fetch: false,
        }
    }

    /// The edges this package adds to the next level. A name listed under
    /// `optionalDependencies` is optional even when it also appears under `dependencies`,
    /// because `npm publish` mirrors it there; a bundled name ships inside the tarball.
    fn wants(&self) -> Vec<Want> {
        let m = &self.manifest;
        let bundled = |n: &str| m.bundle_dependencies.contains(n);
        let want = |n: &String, r: &String, optional: bool| Want {
            parent: self.dir.clone(),
            name: n.clone(),
            range: r.clone(),
            optional,
        };
        m.dependencies
            .iter()
            .filter(|(n, _)| !m.optional_dependencies.contains_key(*n) && !bundled(n))
            .map(|(n, r)| want(n, r, false))
            .chain(
                m.optional_dependencies
                    .iter()
                    .filter(|(n, _)| !bundled(n))
                    .map(|(n, r)| want(n, r, true)),
            )
            .collect()
    }
}

#[derive(Default)]
struct Plan {
    /// In placement order; the first entry is the requested package.
    packages: Vec<Planned>,
    /// Directory → version, for every package this plan will produce.
    placed: HashMap<PathBuf, String>,
}

impl Plan {
    /// Walk from the dependent's directory up to the install root looking for `name` at a
    /// version satisfying `range`, in the plan first and then on disk — Node's own resolution
    /// order, so whatever is found here is what `require` will find too.
    fn satisfied(
        &self,
        root: &Path,
        parent: &Path,
        name: &str,
        range: &str,
    ) -> Result<Option<(String, PathBuf)>, Error> {
        let mut dir = Some(parent);
        while let Some(d) = dir {
            let candidate = d.join("node_modules").join(name);
            let version = match self.placed.get(&candidate) {
                Some(v) => Some(v.clone()),
                None => installed_version(&candidate)?,
            };
            if let Some(v) = version
                && registry::satisfies(&v, range)
            {
                return Ok(Some((v, candidate)));
            }
            if d == root {
                break;
            }
            dir = d.parent();
        }
        Ok(None)
    }
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
