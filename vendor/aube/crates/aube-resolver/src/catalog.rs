use crate::semver_util::version_satisfies;
use crate::{CatalogDetails, Error, Resolver};
use std::collections::BTreeMap;

impl Resolver {
    /// Resolve a `catalog:[<name>]` specifier to its pinned range. Returns
    /// `None` when `spec` isn't a catalog reference, or
    /// `Some((catalog_name, real_range))` when it is. The catalog name is
    /// normalized — the bare `catalog:` form maps to the `default` catalog.
    /// Errors on an unknown catalog or missing entry.
    ///
    /// Shared between the pre-override catalog rewrite (directly-declared
    /// `catalog:` deps) and the override handler (`"overrides":
    /// {"pkg": "catalog:"}`), so both paths stay in lockstep.
    pub(crate) fn resolve_catalog_spec(
        &self,
        task_name: &str,
        spec: &str,
    ) -> Result<Option<(String, String)>, Error> {
        let Some(catalog_name) = spec.strip_prefix("catalog:").map(|n| {
            if n.is_empty() {
                "default".to_string()
            } else {
                n.to_string()
            }
        }) else {
            return Ok(None);
        };
        match self.catalogs.get(&catalog_name) {
            Some(catalog) => match catalog.get(task_name) {
                Some(real_range) => {
                    // Catch `catalog:` pointing at another `catalog:`
                    // value. Before this guard, the rewrite ran once
                    // then the outer loop treated `catalog:other` as
                    // a literal semver range. User got a confusing
                    // "range does not satisfy" from the registry
                    // instead of "catalog is not a level of
                    // indirection". pnpm disallows the same. No
                    // cycle detection needed beyond depth-one since
                    // we refuse the chain outright.
                    if aube_util::pkg::is_catalog_spec(real_range) {
                        // Preserve the chain explanation in the catalog
                        // field so the top-level `#[error]` template still
                        // tells the user *why* the entry "doesn't resolve",
                        // and set `chained_value` so the help formatter
                        // skips the suggestion path (which would otherwise
                        // match the user's own input back at them since
                        // the entry exists, its value is just invalid).
                        return Err(Error::UnknownCatalogEntry(Box::new(CatalogDetails {
                            name: task_name.to_string(),
                            spec: spec.to_string(),
                            catalog: format!(
                                "{catalog_name} (value {real_range} is itself a catalog: \
                                 reference, catalogs cannot chain)"
                            ),
                            available: Vec::new(),
                            chained_value: Some(real_range.clone()),
                        })));
                    }
                    Ok(Some((catalog_name, real_range.clone())))
                }
                None => Err(Error::UnknownCatalogEntry(Box::new(CatalogDetails {
                    name: task_name.to_string(),
                    spec: spec.to_string(),
                    catalog: catalog_name,
                    available: catalog.keys().cloned().collect(),
                    chained_value: None,
                }))),
            },
            None => Err(Error::UnknownCatalog(Box::new(CatalogDetails {
                name: task_name.to_string(),
                spec: spec.to_string(),
                catalog: catalog_name,
                available: self.catalogs.keys().cloned().collect(),
                chained_value: None,
            }))),
        }
    }
}

/// Materialize the BFS-accumulated `catalog_picks` (raw specifier per
/// (catalog, package)) into the final `LockfileGraph.catalogs` shape,
/// which carries both the specifier and the locked version. Version
/// comes from `resolved_versions` — prefer the entry that satisfies the
/// catalog range, fall back to any locked version when an override
/// pushed the only resolution out of range, and fall back to the raw
/// spec string as a last resort when nothing locked at all.
pub(crate) fn materialize_catalog_picks(
    catalog_picks: BTreeMap<String, BTreeMap<String, String>>,
    resolved_versions: &crate::FxHashMap<String, Vec<String>>,
) -> BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>> {
    let mut resolved_catalogs: BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>> =
        BTreeMap::new();
    for (cat_name, entries) in catalog_picks {
        let mut out: BTreeMap<String, aube_lockfile::CatalogEntry> = BTreeMap::new();
        for (pkg, spec) in entries {
            let resolved_for_pkg = resolved_versions.get(&pkg);
            let version = resolved_for_pkg
                .and_then(|vs| vs.iter().find(|v| version_satisfies(v, &spec)).cloned())
                .or_else(|| resolved_for_pkg.and_then(|vs| vs.first().cloned()))
                .unwrap_or_else(|| spec.clone());
            out.insert(
                pkg,
                aube_lockfile::CatalogEntry {
                    specifier: spec,
                    version,
                },
            );
        }
        if !out.is_empty() {
            resolved_catalogs.insert(cat_name, out);
        }
    }
    resolved_catalogs
}
