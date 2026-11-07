//! tsconfig `contentMappers` (TypeScript 7.1) as the registry of runtime plugins.
//!
//! A content mapper teaches `tsc` a file type; TypeScript never emits code for those
//! files. The ecosystem's runtime half is the package's `./register` export, the
//! module users already pass as `node --import <pkg>/register`. Nub preloads that
//! export for every package the governing tsconfig lists, through the same chainer
//! as `nub.jsonc` `preload` ([`crate::cli`]'s `prepare_preload_chain`), and never
//! runs the mapper process itself. An entry's `options` reach its register as JSON in
//! [`OPTIONS_ENV`], because Node has no channel for handing options to a preload. The
//! name is neutral and plain `node --import` can supply it by hand, so a register
//! written for it runs the same anywhere.

use std::path::{Path, PathBuf};

pub(crate) const OPTIONS_ENV: &str = "CONTENT_MAPPER_OPTIONS";

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
    /// The entry's `options` as JSON text, spelled by [`encode_options`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
}

/// JSON text for the runtime snapshot with its `\` and `"` spelled out. A `script-shell`
/// pointing at busybox-w32 runs without the bundled shell's `-X`, so its `ash` rewrites
/// `\` to `/` in every variable it imports: any JSON escape in a value would reach the
/// descendant corrupted, and an escaped quote would break the snapshot's own parse.
fn encode_options(options: &serde_json::Value) -> String {
    options
        .to_string()
        .replace('%', "%25")
        .replace('\\', "%5C")
        .replace('"', "%22")
}

fn decode_options(encoded: &str) -> String {
    encoded
        .replace("%22", "\"")
        .replace("%5C", "\\")
        .replace("%25", "%")
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
    fn add(&mut self, path: String, extensions: &[String]) -> &mut Register {
        let index = match self.registers.iter().position(|r| r.path == path) {
            Some(index) => index,
            None => {
                self.registers.push(Register {
                    path,
                    extensions: Vec::new(),
                    options: None,
                });
                self.registers.len() - 1
            }
        };
        let register = &mut self.registers[index];
        for extension in extensions {
            if !register.extensions.contains(extension) {
                register.extensions.push(extension.clone());
            }
        }
        register
    }

    /// Each register with options, by its real path, so an entry the user also lists
    /// under `preload` still gets them in the slot they gave it.
    pub(crate) fn options_by_file(&self) -> std::collections::HashMap<PathBuf, String> {
        self.registers
            .iter()
            .filter_map(|register| {
                let options = decode_options(register.options.as_deref()?);
                Some((std::fs::canonicalize(&register.path).ok()?, options))
            })
            .collect()
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
                .position(|r| {
                    !remaining
                        .iter()
                        .any(|other| !std::ptr::eq(*other, *r) && more_general(other, r))
                })
                .unwrap_or(0);
            ordered.push(remaining.remove(next).path.clone());
        }
        ordered
    }

    /// `other` is what a descendant's own entry lists. Its config is the nearer one, so
    /// a package it lists takes that entry's options, none included.
    pub(crate) fn merge(&mut self, other: Discovered) {
        for register in other.registers {
            self.add(register.path, &register.extensions).options = register.options;
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
                    // A config's own entry comes before its references', and wins even
                    // when it has no options.
                    let first = !out.registers.iter().any(|known| known.path == register);
                    let known = out.add(register, &mapper.extensions);
                    if first {
                        known.options = mapper.options.as_ref().map(encode_options);
                    }
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
    // One spelling per file, or a descendant merges a second register for it: two URLs,
    // so it evaluates twice. Hence the real path, whatever the anchor's spelling
    // (`RUNNER~1`), and forward slashes on Windows: under a `script-shell` pointing at
    // busybox-w32 the snapshot passes through an `ash` that rewrites `\` to `/` in every
    // variable it imports, and JSON's `\\` comes out as `//`, which still parses. Windows
    // only, because a backslash is a legal filename character on POSIX.
    let real = std::fs::canonicalize(register.full_path()).ok()?;
    let path = crate::cli::strip_windows_verbatim_prefix(&real.to_string_lossy());
    Some(if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path
    })
}
