//! tsconfig `contentMappers` (TypeScript 7.1) as the registry of runtime plugins.
//!
//! A content mapper teaches `tsc` a file type; TypeScript never emits code for those
//! files. The ecosystem's runtime half is the package's `./register` export, the
//! module users already pass as `node --import <pkg>/register`. Nub preloads that
//! export for every package the governing tsconfig lists, through the same chainer
//! as `nub.jsonc` `preload` ([`crate::cli`]'s `prepare_preload_chain`), and never
//! runs the mapper process itself. Options stay the register's business: it reads
//! its own entry from the tsconfig, as it must under plain Node anyway.

use std::path::{Path, PathBuf};

/// The mapper registers a run loads. Crosses to children in the runtime snapshot: a
/// descendant keeps what it inherited and adds what its own entry's tsconfig lists.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Discovered {
    /// Resolved `./register` files, in declaration order. See
    /// [`Discovered::registration_order`].
    pub registers: Vec<Register>,
    /// The directory holding the `node_modules` a mapper package was found in — where
    /// the chainer goes when nothing else asked for one, so a run started outside the
    /// project never creates `node_modules/` in the CWD.
    pub chain_root: Option<PathBuf>,
    /// Every mapped extension and what became of its package, for the runtime's error.
    pub extensions: Vec<MappedExtension>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Register {
    pub path: String,
    #[serde(default)]
    pub extensions: Vec<String>,
}

/// One mapped extension, read by the runtime so an import Node refuses says why. It is
/// decided here rather than recorded by the chainer because the async tier's hook runs
/// on another thread, which no main-thread state reaches.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MappedExtension {
    pub extension: String,
    pub package: String,
    pub tsconfig: String,
    /// `"registered"` when its `./register` is in the chain, so an unknown-extension
    /// error means that register did not answer: skipped for needing a newer Node, or
    /// loaded without claiming this extension. `"missing"` when the package does not
    /// resolve, `"no-register"` when it ships no `./register` export, `"pnp"` under
    /// Yarn Plug'n'Play, where this discovery does not reach.
    pub reason: String,
}

impl Discovered {
    /// One package listed twice registers once, with the union of its extensions.
    fn add(&mut self, path: String, extensions: &[String]) {
        let index = match self.registers.iter().position(|r| r.path == path) {
            Some(index) => index,
            None => {
                self.registers.push(Register {
                    path,
                    extensions: Vec::new(),
                });
                self.registers.len() - 1
            }
        };
        let known = &mut self.registers[index].extensions;
        for extension in extensions {
            if !known.contains(extension) {
                known.push(extension.clone());
            }
        }
    }

    /// `module.registerHooks` runs the hook registered LAST first, and TypeScript routes
    /// a file to the mapper with the longest matching extension. Only suffix relations
    /// can disagree between two mappers, so a package holding a proper suffix of another's
    /// extension registers first, in a stable topological sort that leaves unrelated
    /// packages in declaration order. A true cycle (A `.b` + `.x.c`, B `.c` + `.y.b`)
    /// cannot be expressed by ordering whole modules and falls back to declaration order.
    pub(crate) fn registration_order(&self) -> Vec<String> {
        let more_general = |a: &Register, b: &Register| {
            a.extensions.iter().any(|x| {
                b.extensions
                    .iter()
                    .any(|y| y.len() > x.len() && y.ends_with(x))
            })
        };
        let mut remaining: Vec<&Register> = self.registers.iter().collect();
        let mut ordered = Vec::with_capacity(remaining.len());
        while !remaining.is_empty() {
            let next = remaining
                .iter()
                .position(|r| !remaining.iter().any(|other| more_general(other, r)))
                .unwrap_or(0);
            ordered.push(remaining.remove(next).path.clone());
        }
        ordered
    }

    pub(crate) fn merge(&mut self, other: Discovered) {
        for register in other.registers {
            self.add(register.path, &register.extensions);
        }
        self.chain_root = self.chain_root.take().or(other.chain_root);
        for entry in other.extensions {
            if !self.extensions.contains(&entry) {
                self.extensions.push(entry);
            }
        }
    }
}

/// Discover the registers for the tsconfig governing `anchor`.
pub(crate) fn discover(anchor: &str, explicit: Option<&str>) -> Discovered {
    let mut out = Discovered::default();
    let mappers = nub_tsconfig::content_mappers(anchor, explicit);
    if mappers.is_empty() {
        return out;
    }
    let resolver = crate::cli::bare_preload_resolver(true);
    for mapper in mappers {
        let declared_dir = Path::new(&mapper.declared_in)
            .parent()
            .unwrap_or(Path::new("."));
        let reason = match package_dir(declared_dir, &mapper.package) {
            None if nub_core::pnp::detect(declared_dir).is_some() => "pnp",
            None => "missing",
            Some((dir, root)) => match register_of(&resolver, &dir) {
                None => "no-register",
                Some(register) => {
                    out.add(register, &mapper.extensions);
                    out.chain_root = out.chain_root.take().or(root);
                    "registered"
                }
            },
        };
        out.extensions
            .extend(mapper.extensions.iter().map(|ext| MappedExtension {
                extension: ext.clone(),
                package: mapper.package.clone(),
                tsconfig: mapper.declared_in.clone(),
                reason: reason.to_string(),
            }));
    }
    out
}

/// TypeScript's `ResolvePackageDirectory` for a mapper `package`: a directory path
/// relative to the declaring config, or a name found by walking `node_modules` up from
/// it. Returns the package directory and, for a name, the directory whose
/// `node_modules` it sat in.
fn package_dir(from: &Path, package: &str) -> Option<(PathBuf, Option<PathBuf>)> {
    let as_path = Path::new(package);
    if package.starts_with('.') || as_path.is_absolute() {
        let dir = from.join(as_path);
        return dir.join("package.json").is_file().then_some((dir, None));
    }
    from.ancestors().find_map(|ancestor| {
        let dir = ancestor.join("node_modules").join(package);
        dir.join("package.json")
            .is_file()
            .then(|| (dir, Some(ancestor.to_path_buf())))
    })
}

/// The file a package's `./register` export names, resolved by self-reference from
/// inside the package with the conditions `--import` uses. Only an `exports` entry
/// counts: a package without an `exports` map has made no `./register` promise, and a
/// stray `register.js` in it is not one.
fn register_of(resolver: &oxc_resolver::Resolver, dir: &Path) -> Option<String> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("package.json")).ok()?).ok()?;
    manifest.get("exports")?;
    let name = manifest.get("name")?.as_str()?;
    let register = resolver.resolve(dir, &format!("{name}/register")).ok()?;
    // One spelling per file, whatever the anchor. A `nub run` anchored at a Windows 8.3
    // path (`RUNNER~1`) and a descendant anchored at its entry's real path would
    // otherwise see two registers in one file: two URLs, so it evaluates twice.
    let real = std::fs::canonicalize(register.full_path()).ok()?;
    Some(crate::cli::strip_windows_verbatim_prefix(
        &real.to_string_lossy(),
    ))
}
