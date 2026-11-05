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
    /// Resolved `./register` files, in registration order.
    pub registers: Vec<Register>,
    /// The directory holding the `node_modules` a mapper package was found in — where
    /// the chainer goes when nothing else asked for one, so a run started outside the
    /// project never creates `node_modules/` in the CWD.
    pub chain_root: Option<PathBuf>,
    /// Mapped extensions nothing will load at runtime, for the runtime's error.
    pub unloaded: Vec<UnloadedContentMapper>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Register {
    pub path: String,
    /// The length of the entry's longest extension.
    pub specificity: usize,
}

/// A mapped extension whose package contributes no runtime, read by the runtime so
/// an import that Node then refuses says why.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnloadedContentMapper {
    pub extension: String,
    pub package: String,
    pub tsconfig: String,
    /// `"missing"` when the package does not resolve, `"no-register"` when it ships
    /// no `./register` export, `"pnp"` under Yarn Plug'n'Play, where this discovery
    /// does not reach.
    pub reason: String,
}

impl Discovered {
    /// Registration order is by specificity: `module.registerHooks` runs the hook
    /// registered LAST first, and TypeScript routes a file to the mapper with the
    /// longest matching extension, so entries register in ascending order of their
    /// longest extension and `.schema.jsonc` answers before `.jsonc`. Ties keep
    /// declaration order. One package listed twice registers once, at its most
    /// specific slot.
    fn add(&mut self, path: String, specificity: usize) {
        match self.registers.iter_mut().find(|r| r.path == path) {
            Some(existing) => existing.specificity = existing.specificity.max(specificity),
            None => self.registers.push(Register { path, specificity }),
        }
        self.registers.sort_by_key(|r| r.specificity);
    }

    pub(crate) fn merge(&mut self, other: Discovered) {
        for register in other.registers {
            self.add(register.path, register.specificity);
        }
        self.chain_root = self.chain_root.take().or(other.chain_root);
        for entry in other.unloaded {
            if !self.unloaded.contains(&entry) {
                self.unloaded.push(entry);
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
        let specificity = mapper.extensions.iter().map(String::len).max().unwrap_or(0);
        let reason = match package_dir(declared_dir, &mapper.package) {
            None if nub_core::pnp::detect(declared_dir).is_some() => "pnp",
            None => "missing",
            Some((dir, root)) => match register_of(&resolver, &dir) {
                None => "no-register",
                Some(register) => {
                    out.add(register, specificity);
                    out.chain_root = out.chain_root.take().or(root);
                    continue;
                }
            },
        };
        out.unloaded
            .extend(mapper.extensions.iter().map(|ext| UnloadedContentMapper {
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
    resolver
        .resolve(dir, &format!("{name}/register"))
        .ok()
        .map(|resolution| resolution.full_path().to_string_lossy().into_owned())
}
