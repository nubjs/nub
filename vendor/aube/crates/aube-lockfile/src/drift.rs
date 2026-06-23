use crate::{
    DepType, DirectDep, LocalSource, LockfileGraph, LockfileKind, dep_type_label, override_match,
};
use std::collections::{BTreeMap, BTreeSet};

impl LockfileGraph {
    /// Compare this lockfile's root importer against a single manifest.
    ///
    /// Mirrors pnpm's `prefer-frozen-lockfile` check: a lockfile is "fresh" iff
    /// every direct dep specifier in `package.json` exactly matches the specifier
    /// recorded in the lockfile (string compare, not semver). Used to decide
    /// whether to skip resolution and trust the lockfile (`Fresh`) or fall back
    /// to a full re-resolve (`Stale { reason }`).
    ///
    /// For workspace projects, use [`check_drift_workspace`] instead — this
    /// method only inspects the root importer.
    ///
    /// `workspace_overrides` is the `overrides:` block from
    /// `pnpm-workspace.yaml` (pnpm v10 moved overrides there). Pass an
    /// empty map when the project has no workspace-yaml overrides. Keys
    /// are merged on top of `manifest.overrides_map()` before the drift
    /// comparison, matching the resolver's effective-override set —
    /// otherwise a lockfile written with a workspace override
    /// immediately looks stale on the next `--frozen-lockfile` run.
    ///
    /// `workspace_ignored_optional` is the same idea for
    /// `pnpm-workspace.yaml`'s `ignoredOptionalDependencies` block:
    /// the resolver unions it with the manifest's list, so the drift
    /// check has to see the same union or a freshly-written lockfile
    /// immediately reads as stale.
    ///
    /// `workspace_catalogs` is the `catalog:` / `catalogs:` block from
    /// `pnpm-workspace.yaml`. pnpm resolves `catalog:` references in
    /// override values against this map before writing the lockfile
    /// and before comparing on re-install, so both sides of the drift
    /// check have to see the catalog-resolved form — otherwise a
    /// `"lodash": "catalog:"` override reads as stale against a
    /// lockfile that recorded the resolved `"lodash": "4.17.21"`.
    ///
    /// Lockfile formats that don't record specifiers (npm, yarn, bun) always
    /// return `Fresh` since we have no way to detect drift without re-resolving.
    ///
    /// [`check_drift_workspace`]: Self::check_drift_workspace
    pub fn check_drift(
        &self,
        manifest: &aube_manifest::PackageJson,
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> DriftStatus {
        self.check_drift_with_options(
            manifest,
            workspace_overrides,
            workspace_ignored_optional,
            workspace_catalogs,
            true,
        )
    }

    pub fn check_drift_for_kind(
        &self,
        manifest: &aube_manifest::PackageJson,
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
        kind: LockfileKind,
    ) -> DriftStatus {
        self.check_drift_with_options(
            manifest,
            workspace_overrides,
            workspace_ignored_optional,
            workspace_catalogs,
            kind_records_resolution_metadata(kind),
        )
    }

    /// Workspace-aware drift check.
    ///
    /// Each entry in `manifests` is `(importer_path, manifest)` — for example
    /// `(".", root_manifest), ("packages/app", app_manifest), ...`. Every
    /// importer is checked against its own manifest; the first stale importer
    /// determines the result.
    ///
    /// See [`check_drift`] for the `workspace_overrides` contract.
    ///
    /// [`check_drift`]: Self::check_drift
    pub fn check_drift_workspace(
        &self,
        manifests: &[(String, aube_manifest::PackageJson)],
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
        is_workspace_install: bool,
    ) -> DriftStatus {
        self.check_drift_workspace_with_options(
            manifests,
            workspace_overrides,
            workspace_ignored_optional,
            workspace_catalogs,
            is_workspace_install,
            true,
        )
    }

    pub fn check_drift_workspace_for_kind(
        &self,
        manifests: &[(String, aube_manifest::PackageJson)],
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
        is_workspace_install: bool,
        kind: LockfileKind,
    ) -> DriftStatus {
        self.check_drift_workspace_with_options(
            manifests,
            workspace_overrides,
            workspace_ignored_optional,
            workspace_catalogs,
            is_workspace_install,
            kind_records_resolution_metadata(kind),
        )
    }

    fn check_drift_with_options(
        &self,
        manifest: &aube_manifest::PackageJson,
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
        check_resolution_metadata: bool,
    ) -> DriftStatus {
        let mut effective = resolve_catalog_refs_in_overrides(
            &merge_manifest_and_workspace_overrides(manifest, workspace_overrides),
            workspace_catalogs,
        );
        manifest.resolve_override_refs(&mut effective);
        if check_resolution_metadata
            && let Some(reason) = self.resolution_metadata_drift_reason(
                manifest,
                workspace_overrides,
                workspace_ignored_optional,
                workspace_catalogs,
            )
        {
            return DriftStatus::Stale { reason };
        }
        self.check_drift_for_importer(".", manifest, &effective)
    }

    fn check_drift_workspace_with_options(
        &self,
        manifests: &[(String, aube_manifest::PackageJson)],
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
        is_workspace_install: bool,
        check_resolution_metadata: bool,
    ) -> DriftStatus {
        // Override drift is checked once at the workspace level, against
        // the root manifest. Workspace-package manifests may declare
        // their own `overrides` blocks but pnpm only honors the root's,
        // so we mirror that here.
        let effective_overrides = match manifests.iter().find(|(p, _)| p == ".") {
            Some((_, root_manifest)) => {
                let mut effective = resolve_catalog_refs_in_overrides(
                    &merge_manifest_and_workspace_overrides(root_manifest, workspace_overrides),
                    workspace_catalogs,
                );
                root_manifest.resolve_override_refs(&mut effective);
                if check_resolution_metadata
                    && let Some(reason) = self.resolution_metadata_drift_reason(
                        root_manifest,
                        workspace_overrides,
                        workspace_ignored_optional,
                        workspace_catalogs,
                    )
                {
                    return DriftStatus::Stale { reason };
                }
                effective
            }
            None => BTreeMap::new(),
        };
        let workspace_link_names: std::collections::HashSet<&str> = manifests
            .iter()
            .filter(|(path, _)| path != ".")
            .filter_map(|(_, manifest)| manifest.name.as_deref())
            .collect();
        for (importer_path, manifest) in manifests {
            match self.check_drift_for_importer_with_workspace_links(
                importer_path,
                manifest,
                &effective_overrides,
                &workspace_link_names,
            ) {
                DriftStatus::Fresh => continue,
                stale => return stale,
            }
        }
        // Stale-importer pass: in a workspace install, lockfile
        // importer entries for workspace projects that no longer
        // exist on disk must invalidate the lockfile. Without this
        // guard, the warm-path short-circuit and drift check both
        // report fresh and the next install carries the orphan
        // importer/snapshot pair forward in the shared lockfile
        // until a user explicitly runs `--no-frozen-lockfile`.
        //
        // Gated on the caller-supplied `is_workspace_install` flag
        // (true when `pnpm-workspace.yaml` exists or `package.json`
        // declares `workspaces`) — the manifests array can collapse
        // to `[(".", root)]` even in a workspace install when the
        // last sub-package is removed, so a manifest-shape check
        // would miss the all-packages-gone case. The flag is also
        // what tells us we're not in the npm `package-lock.json`
        // path, where the parser synthesizes importer entries for
        // every `file:` link and a manifest-shape gate would
        // false-positive on legitimate single-package installs.
        if is_workspace_install {
            let current_importers: std::collections::HashSet<&str> =
                manifests.iter().map(|(p, _)| p.as_str()).collect();
            for importer_path in self.importers.keys() {
                if !current_importers.contains(importer_path.as_str()) {
                    return DriftStatus::Stale {
                        reason: format!(
                            "workspace importer {importer_path} is in the lockfile but not in the workspace"
                        ),
                    };
                }
            }
        }
        DriftStatus::Fresh
    }

    fn resolution_metadata_drift_reason(
        &self,
        manifest: &aube_manifest::PackageJson,
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Option<String> {
        let mut effective = resolve_catalog_refs_in_overrides(
            &merge_manifest_and_workspace_overrides(manifest, workspace_overrides),
            workspace_catalogs,
        );
        // Resolve `$pkg` sibling references the same way the install
        // path does (`settings.rs` calls `resolve_override_refs` before
        // storing the override into the lockfile). Without this the
        // drift check compares the manifest literal `$pkg` against the
        // lockfile's resolved range and reports false drift.
        manifest.resolve_override_refs(&mut effective);
        let locked = resolve_catalog_refs_in_overrides(&self.overrides, workspace_catalogs);
        overrides_drift_reason(&locked, &effective)
            .or_else(|| {
                let mut effective_ignored = manifest.pnpm_ignored_optional_dependencies();
                effective_ignored.extend(workspace_ignored_optional.iter().cloned());
                ignored_optional_drift_reason(
                    &self.ignored_optional_dependencies,
                    &effective_ignored,
                )
            })
            .or_else(|| runtime_drift_reason(&self.runtimes, manifest))
    }

    /// Compare this lockfile's recorded patch config against the
    /// manifest/workspace-declared one. `effective_paths` is the
    /// merged `patchedDependencies` map (bun top-level +
    /// `pnpm.patchedDependencies` + workspace yaml, selector → rel
    /// path); `effective_hashes` is the sha256 hex of each patch
    /// file's *current* contents. Mirrors pnpm's config-mismatch rule
    /// (`ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`): a declared patch the
    /// lockfile doesn't record, a moved patch path, or an edited
    /// patch file (hash mismatch) all mean the lockfile is stale.
    ///
    /// Skipped entirely for lockfile formats that have no
    /// patched-dependency construct (npm, yarn) — same rule as
    /// [`kind_records_resolution_metadata`]: a `package-lock.json`
    /// can never record the block, so comparing against it would
    /// re-resolve (or frozen-fail) on every install. Hash comparison
    /// is also skipped for lockfile entries that never recorded a
    /// hash (bun.lock, pnpm v8's bare-path form) — those formats
    /// carry no hash to compare against.
    pub fn check_patched_dependencies_drift(
        &self,
        kind: LockfileKind,
        effective_paths: &BTreeMap<String, String>,
        effective_hashes: &BTreeMap<String, String>,
    ) -> DriftStatus {
        if !matches!(
            kind,
            LockfileKind::Aube | LockfileKind::Pnpm | LockfileKind::Bun
        ) {
            return DriftStatus::Fresh;
        }
        // pnpm records the patch's per-file *hash* as the lockfile value
        // (no path), so drift is a pure hash-against-hash comparison —
        // exactly pnpm's own `getOutdatedLockfileSetting` rule, which
        // diffs `lockfile.patchedDependencies` (selector → hash) against
        // the freshly computed `calcPatchHashes`. aube's own lock.yaml
        // shares pnpm's `{ hash, path }` block and the same reader (which
        // keeps only the hash, leaving the path map empty), so its drift
        // is hash-against-hash too. The path-against-path comparison below
        // is only meaningful for bun, whose reader stores a real path.
        if matches!(kind, LockfileKind::Pnpm | LockfileKind::Aube) {
            return self.check_patched_dependency_hashes_drift(effective_hashes);
        }
        // Both directions matter, exactly like pnpm: a lockfile entry
        // whose selector the project no longer declares is as stale as
        // a declared patch the lockfile doesn't record (`patch-remove`
        // relies on this firing to drop the entry on the next write).
        for selector in self.patched_dependencies.keys() {
            if !effective_paths.contains_key(selector) {
                return DriftStatus::Stale {
                    reason: format!(
                        "patchedDependencies.{selector}: recorded in the lockfile but no longer declared in the project"
                    ),
                };
            }
        }
        for (selector, path) in effective_paths {
            match self.patched_dependencies.get(selector) {
                None => {
                    return DriftStatus::Stale {
                        reason: format!(
                            "patchedDependencies.{selector}: declared in the project but missing from the lockfile"
                        ),
                    };
                }
                Some(locked_path) if locked_path != path => {
                    return DriftStatus::Stale {
                        reason: format!(
                            "patchedDependencies.{selector}: project says {path}, lockfile says {locked_path}"
                        ),
                    };
                }
                Some(_) => {}
            }
            if let (Some(effective_hash), Some(locked_hash)) = (
                effective_hashes.get(selector),
                self.patched_dependency_hashes.get(selector),
            ) && effective_hash != locked_hash
            {
                return DriftStatus::Stale {
                    reason: format!(
                        "patchedDependencies.{selector}: patch file contents changed (hash mismatch)"
                    ),
                };
            }
        }
        DriftStatus::Fresh
    }

    /// Hash-only patched-dependency drift, for pnpm lockfiles whose
    /// `patchedDependencies` value is the patch's per-file hash. Fires in
    /// both directions: a selector the lockfile records but the project
    /// no longer declares, a declared selector the lockfile is missing,
    /// or a hash mismatch (the patch file's contents changed). Mirrors
    /// pnpm's `getOutdatedLockfileSetting` `patchedDependencies` check.
    fn check_patched_dependency_hashes_drift(
        &self,
        effective_hashes: &BTreeMap<String, String>,
    ) -> DriftStatus {
        for selector in self.patched_dependency_hashes.keys() {
            if !effective_hashes.contains_key(selector) {
                return DriftStatus::Stale {
                    reason: format!(
                        "patchedDependencies.{selector}: recorded in the lockfile but no longer declared in the project"
                    ),
                };
            }
        }
        for (selector, effective_hash) in effective_hashes {
            match self.patched_dependency_hashes.get(selector) {
                None => {
                    return DriftStatus::Stale {
                        reason: format!(
                            "patchedDependencies.{selector}: declared in the project but missing from the lockfile"
                        ),
                    };
                }
                Some(locked_hash) if locked_hash != effective_hash => {
                    return DriftStatus::Stale {
                        reason: format!(
                            "patchedDependencies.{selector}: patch file contents changed (hash mismatch)"
                        ),
                    };
                }
                Some(_) => {}
            }
        }
        DriftStatus::Fresh
    }

    /// Compare this lockfile's catalog snapshot against the current
    /// `pnpm-workspace.yaml` catalogs.
    ///
    /// pnpm only writes catalog entries that at least one importer
    /// references — unused entries are absent from the lockfile. So
    /// "missing from lockfile" doesn't mean "added by the user", it
    /// means "declared but unreferenced", which is not drift. The
    /// transition from unused → used is caught by the importer-level
    /// drift check, since a fresh `catalog:` reference shows up as a
    /// new dep in some `package.json`.
    ///
    /// We fire on two cases only:
    /// - the spec changed for an entry the lockfile already records
    ///   (the entry is in use, and re-resolution must rerun);
    /// - the workspace removed an entry that the lockfile records
    ///   (the importer using `catalog:` now points at nothing).
    ///
    /// Resolved versions are deliberately not part of the comparison —
    /// the version is an *output* of resolution, so a stale lockfile
    /// version is what re-resolution is supposed to fix. Drift only
    /// fires on user intent (the specifier).
    pub fn check_catalogs_drift(
        &self,
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> DriftStatus {
        for (cat_name, cat) in workspace_catalogs {
            let Some(locked) = self.catalogs.get(cat_name) else {
                continue;
            };
            for (pkg, spec) in cat {
                if let Some(entry) = locked.get(pkg)
                    && entry.specifier != *spec
                {
                    return DriftStatus::Stale {
                        reason: format!(
                            "catalogs.{cat_name}.{pkg}: workspace says {spec}, lockfile says {}",
                            entry.specifier
                        ),
                    };
                }
            }
        }
        for (cat_name, cat) in &self.catalogs {
            let workspace_cat = workspace_catalogs.get(cat_name);
            for pkg in cat.keys() {
                if workspace_cat.map(|c| c.contains_key(pkg)) != Some(true) {
                    return DriftStatus::Stale {
                        reason: format!("catalogs.{cat_name}: workspace removed {pkg}"),
                    };
                }
            }
        }
        DriftStatus::Fresh
    }

    /// Compare a single importer's `DirectDep` list against the corresponding
    /// `package.json`. Used by both [`check_drift`] and [`check_drift_workspace`].
    ///
    /// [`check_drift`]: Self::check_drift
    /// [`check_drift_workspace`]: Self::check_drift_workspace
    fn check_drift_for_importer(
        &self,
        importer_path: &str,
        manifest: &aube_manifest::PackageJson,
        effective_overrides: &BTreeMap<String, String>,
    ) -> DriftStatus {
        self.check_drift_for_importer_with_workspace_links(
            importer_path,
            manifest,
            effective_overrides,
            &std::collections::HashSet::new(),
        )
    }

    fn check_drift_for_importer_with_workspace_links(
        &self,
        importer_path: &str,
        manifest: &aube_manifest::PackageJson,
        effective_overrides: &BTreeMap<String, String>,
        workspace_link_names: &std::collections::HashSet<&str>,
    ) -> DriftStatus {
        let label = if importer_path == "." {
            String::new()
        } else {
            format!("{importer_path}: ")
        };

        let importer_deps: &[DirectDep] = self
            .importers
            .get(importer_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        // Skip the check entirely if no DirectDep has a specifier (non-pnpm format).
        if importer_deps.iter().all(|d| d.specifier.is_none()) {
            return DriftStatus::Fresh;
        }
        let lockfile_specs: BTreeMap<&str, &str> = importer_deps
            .iter()
            .filter_map(|d| d.specifier.as_deref().map(|s| (d.name.as_str(), s)))
            .collect();

        let override_rules = override_match::compile(effective_overrides);

        // Optionals the previous resolve recorded as intentionally
        // skipped on this importer's platform — keyed by name, value
        // is the specifier captured at that time. Distinct from
        // `ignored_optional_dependencies`, which is the user's static
        // ignore list; this map captures *runtime* platform skips.
        let skipped_optionals: BTreeMap<&str, &str> = self
            .skipped_optional_dependencies
            .get(importer_path)
            .map(|m| m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect())
            .unwrap_or_default();

        // Iterate prod / dev / optional with a flag so the
        // skipped-optional exemption only applies to deps that came
        // from `optional_dependencies`. Without the flag, moving a
        // previously-skipped optional into `dependencies` with the same
        // specifier would silently report Fresh and the dep would
        // never install as a required dep.
        //
        // Optionals named in `ignored_optional_dependencies` are
        // dropped from the manifest-side scan: the resolver never
        // enqueues them, so the lockfile importer never has them
        // either, and the loop would otherwise report drift on every
        // install. (Their *spec* is still verified separately by the
        // round-tripped `ignored_optional_dependencies` block below.)
        let ignored = &self.ignored_optional_dependencies;
        let mut manifest_deps: Vec<(&String, &String, bool)> = manifest
            .dependencies
            .iter()
            .map(|(k, v)| (k, v, false))
            .chain(manifest.dev_dependencies.iter().map(|(k, v)| (k, v, false)))
            .chain(
                manifest
                    .optional_dependencies
                    .iter()
                    .filter(|(name, _)| !ignored.contains(name.as_str()))
                    .map(|(k, v)| (k, v, true)),
            )
            .collect();
        if self.settings.auto_install_peers {
            manifest_deps.extend(
                manifest
                    .non_optional_peer_dependencies()
                    .filter(|(name, _)| {
                        !manifest.dependencies.contains_key(name.as_str())
                            && !manifest.dev_dependencies.contains_key(name.as_str())
                            && !manifest.optional_dependencies.contains_key(name.as_str())
                    })
                    .map(|(k, v)| (k, v, false)),
            );
        }

        for (name, spec, is_optional) in manifest_deps {
            match lockfile_specs.get(name.as_str()) {
                None => {
                    // A *missing* optional dep is only "fresh" if the
                    // previous resolve recorded it as intentionally
                    // skipped (platform mismatch or
                    // `pnpm.ignoredOptionalDependencies`) AND the
                    // recorded specifier still matches what's in the
                    // manifest. A genuinely *new* optional that the
                    // resolver has never seen is real drift — without
                    // that branch, adding `fsevents` to a fresh manifest
                    // would silently never get installed.
                    if is_optional && let Some(locked_spec) = skipped_optionals.get(name.as_str()) {
                        if *locked_spec == spec {
                            continue;
                        }
                        return DriftStatus::Stale {
                            reason: format!(
                                "{label}{name}: manifest says {spec}, lockfile (skipped) says {locked_spec}"
                            ),
                        };
                    }
                    return DriftStatus::Stale {
                        reason: format!("{label}manifest adds {name}@{spec}"),
                    };
                }
                Some(locked_spec) if *locked_spec != spec => {
                    // pnpm rewrites the importer specifier to the
                    // override-applied value when an override fires on
                    // a direct dep, so a pnpm-generated lockfile shows
                    // `specifier: ">=3.0.5"` even though `package.json`
                    // still reads `^3.0.4`. Accept that as fresh when
                    // an override for this name (bare or version-keyed)
                    // resolves to the lockfile's recorded spec —
                    // otherwise any pnpm-written lockfile with
                    // overrides reads stale on every frozen install.
                    if let Some(override_spec) =
                        override_match::apply(&override_rules, name.as_str(), spec)
                        && override_spec == *locked_spec
                    {
                        continue;
                    }
                    // A pnpmfile `readPackage` hook can rewrite an
                    // importer's own dep spec into a local source — the
                    // canonical case is wiring a monorepo package to a
                    // sibling's build output (`"@scope/api": "*"` →
                    // `link:../api/dist`). pnpm records the *rewritten*
                    // spec in the lockfile importer (so does aube), then
                    // re-runs the hook on every install to compare. The
                    // drift fast path deliberately does not re-run the
                    // hook, so a raw `manifest says *, lockfile says
                    // link:...` comparison would read stale forever and
                    // re-resolve (or hard-fail `--frozen-lockfile`) on
                    // every install. Trust the lockfile's hook-derived
                    // local spec when (a) the lockfile was produced with
                    // a pnpmfile that exports hooks (`pnpmfileChecksum`
                    // is recorded) and (b) the manifest spec is a plain
                    // non-local range the hook turned into a
                    // link/file/portal. A pnpmfile edit changes the
                    // recorded checksum (busting the warm path
                    // separately), so this cannot mask a stale link once
                    // the hook itself changes; and gating on a non-local
                    // manifest spec keeps a user-authored `link:` that
                    // was later repointed at the registry detectable.
                    if self.pnpmfile_checksum.is_some()
                        && is_local_source_spec(locked_spec)
                        && !is_local_source_spec(spec)
                    {
                        continue;
                    }
                    return DriftStatus::Stale {
                        reason: format!(
                            "{label}{name}: manifest says {spec}, lockfile says {locked_spec}"
                        ),
                    };
                }
                Some(_) => {}
            }
        }

        // Detect dep-type drift: a name kept in the manifest but moved
        // between sections (e.g. `dependencies` → `devDependencies`)
        // keeps the same specifier, so the spec-only checks above
        // report Fresh and the warm path short-circuits without
        // rewriting the lockfile. The resolver's priority is
        // `dependencies` > `devDependencies` > `optionalDependencies`,
        // matching `seed_direct_deps` in aube-resolver.
        let mut manifest_dep_types: BTreeMap<&str, DepType> = BTreeMap::new();
        for name in manifest.dependencies.keys() {
            manifest_dep_types.insert(name.as_str(), DepType::Production);
        }
        for name in manifest.dev_dependencies.keys() {
            manifest_dep_types
                .entry(name.as_str())
                .or_insert(DepType::Dev);
        }
        for name in manifest.optional_dependencies.keys() {
            if ignored.contains(name.as_str()) {
                continue;
            }
            manifest_dep_types
                .entry(name.as_str())
                .or_insert(DepType::Optional);
        }
        if self.settings.auto_install_peers {
            // Both required AND optional peers can be auto-installed by
            // pnpm `auto-install-peers=true` and recorded under the
            // importer's lockfile `dependencies`. An optional peer that
            // resolves in scope (e.g. `typescript` declared
            // `peerDependenciesMeta.optional` by `packages/vue`, present
            // as a root devDep) lands there too — so classify both as
            // Production for the dep-type drift check, or a valid pnpm 11
            // lockfile reads stale on the optional-peer row.
            for name in manifest.peer_dependencies.keys() {
                if manifest.dependencies.contains_key(name)
                    || manifest.dev_dependencies.contains_key(name)
                    || manifest.optional_dependencies.contains_key(name)
                {
                    continue;
                }
                manifest_dep_types
                    .entry(name.as_str())
                    .or_insert(DepType::Production);
            }
        }
        for dep in importer_deps {
            let Some(expected) = manifest_dep_types.get(dep.name.as_str()) else {
                continue;
            };
            if *expected != dep.dep_type {
                return DriftStatus::Stale {
                    reason: format!(
                        "{label}{}: manifest section is {}, lockfile section is {}",
                        dep.name,
                        dep_type_label(*expected),
                        dep_type_label(dep.dep_type),
                    ),
                };
            }
        }

        // Anything in the lockfile but missing from the manifest is stale
        // — UNLESS it was auto-hoisted as a peer by the resolver. pnpm-style
        // `auto-install-peers=true` puts peers into the importer's
        // `dependencies` without the user having written them in
        // `package.json`, so we have to recognize those as derived state
        // rather than user intent.
        //
        // Critically, we identify an auto-hoisted entry by matching its
        // *recorded specifier* against peer ranges declared in the graph,
        // not just by name. A name-only check would silently exempt a
        // user-pinned `react` that the user later removed (if any package
        // anywhere in the graph peer-declares react, the name match would
        // fire and we'd report Fresh forever — defeating the drift check).
        //
        // The rule: a lockfile entry whose (name, specifier) pair exactly
        // matches some package's declared (peer_name, peer_range) is
        // auto-hoisted. If the user had pinned react with a different
        // specifier string and then removed it, the (name, specifier)
        // pair no longer matches any peer range, and drift correctly
        // fires so the resolver re-runs and rewrites the lockfile.
        let mut manifest_names: std::collections::HashSet<&str> = manifest
            .dependencies
            .keys()
            .chain(manifest.dev_dependencies.keys())
            .chain(
                manifest
                    .optional_dependencies
                    .keys()
                    .filter(|name| !ignored.contains(name.as_str())),
            )
            .map(|s| s.as_str())
            .collect();
        if self.settings.auto_install_peers {
            // Exempt the importer's OWN declared peers — required and
            // optional alike — from the "manifest removed" check. Under
            // `auto-install-peers=true` pnpm auto-installs an importer's
            // optional peer that resolves in scope (e.g. `typescript`
            // declared `peerDependenciesMeta.optional` by `packages/vue`)
            // and records it in the importer's lockfile `dependencies`;
            // it is derived state, not a removed manifest dep. This is a
            // pure by-NAME exemption keyed on the importer's OWN manifest
            // — it short-circuits before the (name, spec) auto-hoisted
            // gate below, and is safe precisely because pnpm RE-installs
            // an own optional peer that still resolves in scope on the
            // next install, so the lockfile row stays valid. A dep shared
            // with some OTHER package's peer declaration is unaffected
            // (it isn't in THIS importer's peer set) and stays gated by
            // the (name, spec) match below.
            manifest_names.extend(
                manifest
                    .peer_dependencies
                    .keys()
                    .filter(|name| {
                        !manifest.dependencies.contains_key(name.as_str())
                            && !manifest.dev_dependencies.contains_key(name.as_str())
                            && !manifest.optional_dependencies.contains_key(name.as_str())
                    })
                    .map(|name| name.as_str()),
            );
        }
        let auto_hoisted_peer_specs: std::collections::HashSet<(&str, &str)> = self
            .packages
            .values()
            .flat_map(|p| {
                p.peer_dependencies
                    .iter()
                    .map(|(name, range)| (name.as_str(), range.as_str()))
            })
            .collect();
        for (locked_name, locked_spec) in &lockfile_specs {
            if manifest_names.contains(locked_name) {
                continue;
            }
            if auto_hoisted_peer_specs.contains(&(*locked_name, *locked_spec)) {
                continue;
            }
            let workspace_link = importer_path == "."
                && workspace_link_names.contains(locked_name)
                && importer_deps
                    .iter()
                    .find(|dep| dep.name == *locked_name)
                    .and_then(|dep| self.packages.get(&dep.dep_path))
                    .is_some_and(|pkg| matches!(pkg.local_source, Some(LocalSource::Link(_))));
            if workspace_link {
                continue;
            }
            return DriftStatus::Stale {
                reason: format!("{label}manifest removed {locked_name}"),
            };
        }

        DriftStatus::Fresh
    }
}

/// Merge `pnpm-workspace.yaml` overrides on top of the manifest's
/// `overrides_map()`. Workspace entries win on key conflict, matching
/// pnpm v10's behavior where the workspace yaml is the canonical
/// home for overrides. Callers pass this into `overrides_drift_reason`
/// so the drift check sees the same effective map the resolver used.
fn merge_manifest_and_workspace_overrides(
    manifest: &aube_manifest::PackageJson,
    workspace_overrides: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = manifest.overrides_map();
    for (k, v) in workspace_overrides {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Rewrite `catalog:` / `catalog:<name>` override values to the catalog's
/// resolved range. pnpm writes resolved override values into the lockfile
/// and compares against the resolved form on re-install, so both sides
/// of the drift check have to see the catalog-substituted map — otherwise
/// a `"lodash": "catalog:"` workspace-yaml override reads as stale against
/// a lockfile that recorded `"lodash": "4.17.21"`. Unresolvable references
/// (missing catalog or missing entry) pass through untouched; the caller
/// would have errored at resolve time if this ever reached a real install,
/// so a drift-mismatch here is fine.
fn resolve_catalog_refs_in_overrides(
    overrides: &BTreeMap<String, String>,
    workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    overrides
        .iter()
        .map(|(k, v)| {
            let resolved = v
                .strip_prefix("catalog:")
                .map(|tail| if tail.is_empty() { "default" } else { tail })
                .and_then(|cat_name| workspace_catalogs.get(cat_name))
                .and_then(|cat| cat.get(override_key_package_name(k)))
                .cloned()
                .unwrap_or_else(|| v.clone());
            (k.clone(), resolved)
        })
        .collect()
}

/// Extract the package name from an override selector key so the catalog
/// can be looked up by pkg name. Handles bare (`lodash`), scoped
/// (`@babel/core`), ranged (`lodash@<5`), ancestor-chained
/// (`parent>lodash`), and combinations. Unparseable keys return the
/// input unchanged; the catalog lookup will then miss and leave the
/// value as-is.
fn override_key_package_name(key: &str) -> &str {
    let last = key.rsplit('>').next().unwrap_or(key);
    if let Some(after_scope) = last.strip_prefix('@') {
        match after_scope.find('@') {
            Some(idx) => &last[..idx + 1],
            None => last,
        }
    } else {
        match last.find('@') {
            Some(idx) => &last[..idx],
            None => last,
        }
    }
}

/// Compare two override maps and return a human-readable reason
/// describing the first difference, or `None` if they're identical.
/// Drift messages cite the offending key by name so users can act on
/// them — `(lockfile: N entries, manifest: M entries)` is useless
/// when N == M but a value changed.
fn overrides_drift_reason(
    lockfile: &BTreeMap<String, String>,
    manifest: &BTreeMap<String, String>,
) -> Option<String> {
    for (k, v) in manifest {
        match lockfile.get(k) {
            None => return Some(format!("overrides: manifest adds {k}@{v}")),
            Some(locked) if locked != v => {
                return Some(format!("overrides: {k} changed ({locked} → {v})"));
            }
            Some(_) => {}
        }
    }
    for k in lockfile.keys() {
        if !manifest.contains_key(k) {
            return Some(format!("overrides: manifest removes {k}"));
        }
    }
    None
}

/// Compare two `ignoredOptionalDependencies` sets and return a drift
/// reason string for the first difference, or `None` if identical.
fn ignored_optional_drift_reason(
    lockfile: &BTreeSet<String>,
    manifest: &BTreeSet<String>,
) -> Option<String> {
    for name in manifest {
        if !lockfile.contains(name) {
            return Some(format!("ignoredOptionalDependencies: manifest adds {name}"));
        }
    }
    for name in lockfile {
        if !manifest.contains(name) {
            return Some(format!(
                "ignoredOptionalDependencies: manifest removes {name}"
            ));
        }
    }
    None
}

/// Compare recorded runtime pins against the manifest's
/// `devEngines.runtime` declarations.
///
/// Only an *existing* pin can drift here: the requested range changed,
/// or the manifest dropped the devEngines entry the pin came from. The
/// inverse case — devEngines present but no pin recorded yet — is
/// deliberately not drift, because formats that can't record runtime
/// pins (npm/yarn/bun) would read as permanently stale. The install
/// driver adds the missing pin on formats that support it.
fn runtime_drift_reason(
    runtimes: &BTreeMap<String, crate::RuntimePin>,
    manifest: &aube_manifest::PackageJson,
) -> Option<String> {
    for (name, pin) in runtimes {
        let entry = manifest
            .dev_engines
            .as_ref()
            .and_then(|d| d.runtime.iter().find(|r| r.name == *name));
        match entry {
            None => {
                return Some(format!(
                    "devEngines.runtime: manifest no longer pins {name} (lockfile records {})",
                    pin.version
                ));
            }
            // An entry that names the runtime but declares no
            // `version` carries no concrete range — resolution treats
            // it as "no requirement", so it can't contradict the pin.
            // Flagging it would hard-fail frozen installs over a
            // field that changes nothing.
            Some(entry) => match entry.version.as_deref() {
                None => {}
                Some(range) if range != pin.specifier => {
                    return Some(format!(
                        "devEngines.runtime: {name} changed ({} → {range})",
                        pin.specifier
                    ));
                }
                Some(_) => {}
            },
        }
    }
    None
}

/// Result of comparing a lockfile against a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftStatus {
    /// The lockfile is in sync with the manifest. Safe to use without re-resolving.
    Fresh,
    /// The lockfile is out of date. The reason describes the first mismatch found.
    Stale { reason: String },
}

fn kind_records_resolution_metadata(kind: LockfileKind) -> bool {
    matches!(
        kind,
        LockfileKind::Aube | LockfileKind::Pnpm | LockfileKind::Bun
    )
}

/// True for importer specifiers that point at a local on-disk source
/// (`link:` / `file:` / `portal:` / `exec:`) rather than a registry range
/// or workspace alias — the same set `LocalSource::parse` recognizes. The
/// drift check uses this to recognize pnpmfile-`readPackage`-rewritten
/// importer deps, where the hook turns a plain range into a local link
/// (e.g. `"*"` → `link:../pkg/dist`).
fn is_local_source_spec(spec: &str) -> bool {
    spec.starts_with("link:")
        || spec.starts_with("file:")
        || spec.starts_with("portal:")
        || spec.starts_with("exec:")
}

#[cfg(test)]
mod drift_tests {
    use super::*;
    use crate::{CatalogEntry, LockedPackage, LockfileSettings};
    use aube_manifest::PackageJson;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn make_manifest(deps: &[(&str, &str)]) -> PackageJson {
        let mut m = PackageJson {
            name: Some("test".into()),
            version: Some("1.0.0".into()),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            update_config: None,
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
            dev_engines: None,
            workspaces: None,
            bundled_dependencies: None,
            extra: BTreeMap::new(),
        };
        for (name, spec) in deps {
            m.dependencies.insert((*name).into(), (*spec).into());
        }
        m
    }

    fn make_graph(deps: &[(&str, &str, &str)]) -> LockfileGraph {
        // (name, specifier, dep_path)
        let direct: Vec<DirectDep> = deps
            .iter()
            .map(|(name, spec, dep_path)| DirectDep {
                name: (*name).into(),
                dep_path: (*dep_path).into(),
                dep_type: DepType::Production,
                specifier: Some((*spec).into()),
            })
            .collect();
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), direct);
        LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn stale_when_dep_moves_between_sections() {
        // Discussion #602: moving a dep between `dependencies` and
        // `devDependencies` keeps the same specifier, so the spec-only
        // checks reported Fresh and the warm path short-circuited
        // without rewriting the lockfile.
        let mut manifest = make_manifest(&[]);
        manifest
            .dev_dependencies
            .insert("msw".into(), "catalog:".into());
        let mut graph = make_graph(&[("msw", "catalog:", "msw@2.14.4")]);
        graph
            .importers
            .get_mut(".")
            .unwrap()
            .iter_mut()
            .for_each(|d| d.dep_type = DepType::Production);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => {
                assert!(reason.contains("msw"), "reason: {reason}");
                assert!(reason.contains("devDependencies"), "reason: {reason}");
            }
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn fresh_when_specifiers_match() {
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_specifier_changes() {
        let manifest = make_manifest(&[("lodash", "^4.18.0")]);
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("lodash")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn stale_when_manifest_adds_dep() {
        let manifest = make_manifest(&[("lodash", "^4.17.0"), ("express", "^4.18.0")]);
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("express")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn stale_when_manifest_removes_dep() {
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let graph = make_graph(&[
            ("lodash", "^4.17.0", "lodash@4.17.21"),
            ("express", "^4.18.0", "express@4.18.0"),
        ]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("express")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn fresh_when_pnpmfile_hook_rewrites_dep_to_link() {
        // A pnpmfile `readPackage` hook rewrites `"@scope/api": "*"` to
        // `link:../api/dist` (wiring a sibling's build output). pnpm and
        // aube both record the *rewritten* spec in the importer, so the
        // raw manifest (`*`) never matches the lockfile (`link:...`). With
        // a `pnpmfileChecksum` recorded — i.e. the lockfile was produced
        // by a hook-exporting pnpmfile — trust the local spec instead of
        // re-resolving on every install. Mirrors pnpm, which re-runs the
        // hook and reports "Already up to date".
        let manifest = make_manifest(&[("@scope/api", "*")]);
        let mut graph = make_graph(&[(
            "@scope/api",
            "link:../api/dist",
            "@scope/api@link:../api/dist",
        )]);
        graph.pnpmfile_checksum = Some("sha256-deadbeef".into());
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_link_importer_spec_has_no_pnpmfile_checksum() {
        // Without a recorded pnpmfileChecksum there's no hook to attribute
        // the link to, so a `link:` importer spec the manifest doesn't
        // contain is genuine drift (e.g. the user hand-edited the lockfile
        // or repointed a dep) and must re-resolve.
        let manifest = make_manifest(&[("@scope/api", "*")]);
        let graph = make_graph(&[(
            "@scope/api",
            "link:../api/dist",
            "@scope/api@link:../api/dist",
        )]);
        assert!(matches!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Stale { .. }
        ));
    }

    #[test]
    fn stale_when_manifest_link_repointed_even_with_pnpmfile_checksum() {
        // The hook exemption only covers a *non-local* manifest range the
        // hook turned into a link. A user-authored `link:` that changed
        // target stays a local spec on the manifest side, so the gate
        // (`!is_local_source_spec(spec)`) keeps it detectable and forces a
        // re-resolve rather than silently trusting a stale link.
        let manifest = make_manifest(&[("@scope/api", "link:../api/old")]);
        let mut graph = make_graph(&[(
            "@scope/api",
            "link:../api/new",
            "@scope/api@link:../api/new",
        )]);
        graph.pnpmfile_checksum = Some("sha256-deadbeef".into());
        assert!(matches!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Stale { .. }
        ));
    }

    #[test]
    fn fresh_when_importer_peer_dependency_is_recorded_as_dependency() {
        let mut manifest = make_manifest(&[]);
        manifest
            .peer_dependencies
            .insert("zod".into(), "^3.22.0".into());
        let graph = make_graph(&[("zod", "^3.22.0", "zod@3.22.0")]);

        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_importer_peer_dependency_row_exists_with_auto_install_peers_false() {
        let mut manifest = make_manifest(&[]);
        manifest
            .peer_dependencies
            .insert("zod".into(), "^3.22.0".into());
        let mut graph = make_graph(&[("zod", "^3.22.0", "zod@3.22.0")]);
        graph.settings.auto_install_peers = false;

        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("zod")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    // pnpm 11 with `auto-install-peers=true` auto-installs an OPTIONAL
    // importer peer that resolves in scope and records it under the
    // importer's lockfile `dependencies` (real-world: `typescript`,
    // declared `peerDependenciesMeta.optional` by vuejs/core's
    // `packages/vue`, present as a root devDep, lands in the importer
    // deps with version 5.6.3). That is a VALID lockfile — the drift
    // check must read it Fresh, not "manifest removed".
    #[test]
    fn fresh_when_optional_importer_peer_dependency_is_recorded_as_dependency() {
        let mut manifest = make_manifest(&[]);
        manifest
            .peer_dependencies
            .insert("zod".into(), "^3.22.0".into());
        manifest.extra.insert(
            "peerDependenciesMeta".into(),
            serde_json::json!({"zod": {"optional": true}}),
        );
        let graph = make_graph(&[("zod", "^3.22.0", "zod@3.22.0")]);

        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    // The importer's OWN optional peer that resolves in scope is
    // re-auto-installed by pnpm on every install, so a lockfile row for
    // it is valid derived state even after the user removes any direct
    // pin of the same name — drift must stay Fresh. (The shared-name
    // protection that DOES fire is for a peer declared by some OTHER
    // package, covered by `stale_when_user_removes_pinned_dep_that_shares_name_with_a_peer`.)
    #[test]
    fn fresh_when_user_removes_dep_sharing_name_with_own_optional_peer() {
        // Manifest declares `zod` ONLY as an optional peer (the direct
        // dependency the user once had is gone). Lockfile still records
        // `zod` in the importer deps — pnpm re-auto-installs it.
        let mut manifest = make_manifest(&[]);
        manifest
            .peer_dependencies
            .insert("zod".into(), "^3.22.0".into());
        manifest.extra.insert(
            "peerDependenciesMeta".into(),
            serde_json::json!({"zod": {"optional": true}}),
        );
        let graph = make_graph(&[("zod", "^3.22.0", "zod@3.22.0")]);

        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    // With `auto-install-peers=false` no peer (optional or required) is
    // auto-installed, so an optional-peer row in the lockfile that the
    // manifest doesn't otherwise declare is genuinely extraneous → stale.
    #[test]
    fn stale_when_optional_importer_peer_recorded_with_auto_install_peers_false() {
        let mut manifest = make_manifest(&[]);
        manifest
            .peer_dependencies
            .insert("zod".into(), "^3.22.0".into());
        manifest.extra.insert(
            "peerDependenciesMeta".into(),
            serde_json::json!({"zod": {"optional": true}}),
        );
        let mut graph = make_graph(&[("zod", "^3.22.0", "zod@3.22.0")]);
        graph.settings.auto_install_peers = false;

        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("zod")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    // Regression guard for #42: the drift check must recognize
    // auto-hoisted peers as derived state, not as "manifest removed X".
    // Without this, every project that has any peer dep would trigger
    // a full re-resolve on every install, defeating lockfile caching.
    #[test]
    fn fresh_when_lockfile_has_auto_hoisted_peer() {
        let manifest = make_manifest(&[("use-sync-external-store", "1.2.0")]);
        let mut graph = make_graph(&[
            (
                "use-sync-external-store",
                "1.2.0",
                "use-sync-external-store@1.2.0",
            ),
            // Hoisted peer — in the lockfile importers but not in the
            // user's package.json.
            ("react", "^16.8.0 || ^17.0.0 || ^18.0.0", "react@18.3.1"),
        ]);
        // The declaring package must list react as a peer for the
        // drift check to recognize the hoist. We add that here.
        let mut declaring_pkg = LockedPackage {
            name: "use-sync-external-store".into(),
            version: "1.2.0".into(),
            dep_path: "use-sync-external-store@1.2.0".into(),
            ..Default::default()
        };
        declaring_pkg
            .peer_dependencies
            .insert("react".into(), "^16.8.0 || ^17.0.0 || ^18.0.0".into());
        graph
            .packages
            .insert("use-sync-external-store@1.2.0".into(), declaring_pkg);

        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    // Regression: when a user explicitly pinned a dep that also happens
    // to share its name with a peer declaration elsewhere in the graph,
    // removing that pin from package.json must still be flagged as
    // stale — otherwise the old pinned version gets locked forever.
    // The check must key on (name, specifier), not name alone.
    #[test]
    fn stale_when_user_removes_pinned_dep_that_shares_name_with_a_peer() {
        // Manifest after the user removed react entirely. Only
        // use-sync-external-store remains.
        let manifest = make_manifest(&[("use-sync-external-store", "1.2.0")]);

        // Lockfile still has the user's old `react: 17.0.2` pin alongside
        // use-sync-external-store. Pre-removal state.
        let mut graph = make_graph(&[
            (
                "use-sync-external-store",
                "1.2.0",
                "use-sync-external-store@1.2.0",
            ),
            ("react", "17.0.2", "react@17.0.2"),
        ]);
        // Add the peer declaration on the consumer package. This is
        // the case that previously defeated the name-only check:
        // react's specifier "17.0.2" doesn't match the declared peer
        // range, so the hoist recognizer must reject it.
        let mut consumer = LockedPackage {
            name: "use-sync-external-store".into(),
            version: "1.2.0".into(),
            dep_path: "use-sync-external-store@1.2.0".into(),
            ..Default::default()
        };
        consumer
            .peer_dependencies
            .insert("react".into(), "^16.8.0 || ^17.0.0 || ^18.0.0".into());
        graph
            .packages
            .insert("use-sync-external-store@1.2.0".into(), consumer);

        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("react")),
            DriftStatus::Fresh => panic!(
                "drift check should flag a removed user-pinned dep as stale, \
                 even when its name matches a peer declaration"
            ),
        }
    }

    // But if the lockfile has a user-removed dep that ISN'T declared as a
    // peer anywhere, we still need to flag it as stale.
    #[test]
    fn stale_when_lockfile_has_removed_non_peer_dep() {
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let graph = make_graph(&[
            ("lodash", "^4.17.0", "lodash@4.17.21"),
            ("chalk", "^5.0.0", "chalk@5.0.0"),
        ]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("chalk")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn workspace_drift_allows_root_links_for_workspace_packages() {
        let root_manifest = make_manifest(&[]);
        let mut app_manifest = make_manifest(&[]);
        app_manifest.name = Some("@scope/app".to_string());

        let link = LocalSource::Link(PathBuf::from("packages/app"));
        let dep_path = link.dep_path("@scope/app");
        let mut graph = make_graph(&[("@scope/app", "*", &dep_path)]);
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: "@scope/app".to_string(),
                version: "1.0.0".to_string(),
                dep_path,
                local_source: Some(link),
                ..Default::default()
            },
        );

        assert_eq!(
            graph.check_drift_workspace(
                &[
                    (".".to_string(), root_manifest),
                    ("packages/app".to_string(), app_manifest),
                ],
                &BTreeMap::new(),
                &[],
                &BTreeMap::new(),
                true,
            ),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn fresh_when_no_specifiers_recorded() {
        // Non-pnpm formats (npm/yarn/bun) don't store specifiers, so we can't
        // detect drift — we treat them as fresh and let the resolver decide.
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let graph = LockfileGraph {
            importers: {
                let mut m = BTreeMap::new();
                m.insert(
                    ".".to_string(),
                    vec![DirectDep {
                        name: "lodash".into(),
                        dep_path: "lodash@4.17.21".into(),
                        dep_type: DepType::Production,
                        specifier: None,
                    }],
                );
                m
            },
            packages: BTreeMap::new(),
            ..Default::default()
        };
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_manifest_adds_override() {
        // Lockfile recorded no overrides; manifest now has one. Drift
        // must fire so the next install re-runs the resolver and bakes
        // the override into the graph.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"lodash": "4.17.21"}));
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("overrides")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn fresh_when_npm_lockfile_cannot_record_overrides() {
        // package-lock.json has no top-level override snapshot. Treating
        // that absence as drift makes aube re-resolve and rewrite npm's
        // lockfile graph even when the override is unrelated to the
        // existing packages.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"left-pad": "1.3.0"}));
        let graph = LockfileGraph {
            importers: {
                let mut m = BTreeMap::new();
                m.insert(
                    ".".to_string(),
                    vec![DirectDep {
                        name: "lodash".into(),
                        dep_path: "lodash@4.17.21".into(),
                        dep_type: DepType::Production,
                        specifier: None,
                    }],
                );
                m
            },
            packages: BTreeMap::new(),
            ..Default::default()
        };
        assert_eq!(
            graph.check_drift_for_kind(
                &manifest,
                &BTreeMap::new(),
                &[],
                &BTreeMap::new(),
                LockfileKind::Npm,
            ),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_bun_lockfile_can_record_overrides() {
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"left-pad": "1.3.0"}));
        let graph = LockfileGraph {
            importers: {
                let mut m = BTreeMap::new();
                m.insert(
                    ".".to_string(),
                    vec![DirectDep {
                        name: "lodash".into(),
                        dep_path: "lodash@4.17.21".into(),
                        dep_type: DepType::Production,
                        specifier: None,
                    }],
                );
                m
            },
            packages: BTreeMap::new(),
            ..Default::default()
        };
        match graph.check_drift_for_kind(
            &manifest,
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            LockfileKind::Bun,
        ) {
            DriftStatus::Stale { reason } => assert!(reason.contains("overrides")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn stale_drift_message_names_changed_override_key() {
        // Both sides have one entry, but the value differs. The reason
        // should name the key — the previous "lockfile: 1 entries,
        // manifest: 1 entries" message looked like nothing changed.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"lodash": "5.0.0"}));
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => {
                assert!(reason.contains("lodash"), "expected key in: {reason}");
                assert!(
                    reason.contains("4.17.21"),
                    "expected old value in: {reason}"
                );
                assert!(reason.contains("5.0.0"), "expected new value in: {reason}");
            }
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn stale_when_manifest_removes_override() {
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => {
                assert!(reason.contains("removes"));
                assert!(reason.contains("lodash"));
            }
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn fresh_when_overrides_match() {
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"lodash": "4.17.21"}));
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn fresh_when_workspace_yaml_overrides_match_lockfile() {
        // pnpm v10 moved `overrides` to pnpm-workspace.yaml. When the
        // resolver wrote them into `self.overrides`, the drift check
        // must see the same map — otherwise the second install run
        // rejects the lockfile as stale with "manifest removes ..."
        // (reported in discussion #174).
        let manifest = make_manifest(&[("semver", "^7.5.0")]);
        let mut graph = make_graph(&[("semver", "^7.5.0", "semver@7.7.1")]);
        graph.overrides.insert("semver".into(), "7.7.1".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("semver".into(), "7.7.1".into());
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn workspace_yaml_overrides_win_over_package_json() {
        // When both pnpm-workspace.yaml and package.json declare an
        // override for the same key, the workspace yaml wins — pnpm
        // v10's precedence. The drift check must apply the merged
        // effective map.
        let mut manifest = make_manifest(&[("semver", "^7.5.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"semver": "7.0.0"}));
        let mut graph = make_graph(&[("semver", "^7.5.0", "semver@7.7.1")]);
        graph.overrides.insert("semver".into(), "7.7.1".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("semver".into(), "7.7.1".into());
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_override_catalog_ref_matches_lockfile_resolved() {
        // pnpm-workspace.yaml: `overrides: { lodash: "catalog:" }` with
        // `catalog: { lodash: 4.17.21 }`. pnpm writes the lockfile with
        // the resolved override value (`lodash: 4.17.21`), so a frozen
        // install comparing the raw `catalog:` string against the
        // resolved form would always read stale (discussion #174).
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("lodash".into(), "catalog:".into());
        let mut catalogs = BTreeMap::new();
        let mut default_cat = BTreeMap::new();
        default_cat.insert("lodash".into(), "4.17.21".into());
        catalogs.insert("default".into(), default_cat);
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &catalogs),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_override_named_catalog_ref_matches_lockfile_resolved() {
        // Named catalog variant: `overrides: { lodash: "catalog:evens" }`
        // resolves against `catalogs.evens.lodash`.
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("lodash".into(), "catalog:evens".into());
        let mut catalogs = BTreeMap::new();
        let mut evens = BTreeMap::new();
        evens.insert("lodash".into(), "4.17.21".into());
        catalogs.insert("evens".into(), evens);
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &catalogs),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn stale_when_override_catalog_ref_diverges_from_lockfile() {
        // If the catalog moves to a new version, the resolved override
        // no longer matches the lockfile — drift must fire, not silently
        // accept.
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("lodash".into(), "catalog:".into());
        let mut catalogs = BTreeMap::new();
        let mut default_cat = BTreeMap::new();
        default_cat.insert("lodash".into(), "4.17.22".into());
        catalogs.insert("default".into(), default_cat);
        match graph.check_drift(&manifest, &ws_overrides, &[], &catalogs) {
            DriftStatus::Stale { reason } => assert!(reason.contains("lodash")),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn fresh_when_override_dollar_ref_matches_lockfile_resolved() {
        // pnpm's `$pkg` sibling-reference syntax: `overrides: { foo: "$zod" }`
        // resolves to the root's declared `zod` range and pnpm writes the
        // *resolved* value into the lockfile. A frozen install comparing the
        // raw `$zod` literal against the resolved `^4.3.5` would read stale
        // (issue #16). Drift must resolve the `$`-ref the same way the
        // install path does.
        let mut manifest = make_manifest(&[("zod", "^4.3.5")]);
        manifest.extra.insert(
            "overrides".into(),
            serde_json::json!({ "some-dep": "$zod" }),
        );
        let mut graph = make_graph(&[("zod", "^4.3.5", "zod@4.3.5")]);
        graph.overrides.insert("some-dep".into(), "^4.3.5".into());
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn stale_when_override_dollar_ref_target_version_changes() {
        // The `$`-ref target moved (`zod` bumped to `^4.4.0`) but the
        // lockfile still records the old resolved override (`^4.3.5`).
        // Resolving the ref surfaces the divergence as drift.
        let mut manifest = make_manifest(&[("zod", "^4.4.0")]);
        manifest.extra.insert(
            "overrides".into(),
            serde_json::json!({ "some-dep": "$zod" }),
        );
        let mut graph = make_graph(&[("zod", "^4.4.0", "zod@4.4.0")]);
        graph.overrides.insert("some-dep".into(), "^4.3.5".into());
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => {
                assert!(reason.contains("some-dep"), "reason: {reason}")
            }
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn fresh_when_pnpm_wrote_override_rewritten_importer_spec() {
        // pnpm rewrites the importer `specifier:` to the post-override
        // value when a bare-name override applies, so a pnpm-generated
        // lockfile records `specifier: 4.17.21` even though
        // `package.json` still reads `^4.17.0`. Without override-aware
        // drift, every frozen install against a pnpm lockfile with
        // overrides reads stale (discussion #174).
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "lodash".into(),
                dep_path: "lodash@4.17.21".into(),
                dep_type: DepType::Production,
                specifier: Some("4.17.21".into()),
            }],
        );
        let mut graph = LockfileGraph {
            importers,
            ..Default::default()
        };
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("lodash".into(), "4.17.21".into());
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_version_keyed_override_rewrites_importer_spec() {
        // Discussion #352: an override keyed by name+range
        // (`plist@<3.0.5` → `>=3.0.5`) rewrites the importer specifier
        // the same way bare-name overrides do. The drift check has to
        // parse the key and compare-by-rule, not by raw map lookup,
        // otherwise pnpm-written lockfiles read stale on every frozen
        // install when version-conditional overrides are in play.
        let manifest = make_manifest(&[("plist", "^3.0.4")]);
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "plist".into(),
                dep_path: "plist@3.0.6".into(),
                dep_type: DepType::Production,
                specifier: Some(">=3.0.5".into()),
            }],
        );
        let mut graph = LockfileGraph {
            importers,
            ..Default::default()
        };
        graph
            .overrides
            .insert("plist@<3.0.5".into(), ">=3.0.5".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("plist@<3.0.5".into(), ">=3.0.5".into());
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_workspace_yaml_ignored_optional_matches_lockfile() {
        // Same drift-shaped bug as overrides: the resolver unions
        // `ignoredOptionalDependencies` from package.json and
        // pnpm-workspace.yaml, so the lockfile's
        // `ignored_optional_dependencies` carries the union, and the
        // drift check has to see the same union or the next
        // `--frozen-lockfile` run fails with "manifest removes".
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph
            .ignored_optional_dependencies
            .insert("fsevents".to_string());
        let ws_ignored = vec!["fsevents".to_string()];
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &ws_ignored, &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_optional_dep_was_recorded_as_skipped() {
        // Regression: a platform-skipped optional dep would otherwise
        // loop forever as "manifest adds X". When the previous
        // resolve recorded it under skipped_optional_dependencies with
        // a matching specifier, drift must report Fresh.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .optional_dependencies
            .insert("fsevents".into(), "^2.3.0".into());
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        let mut inner = BTreeMap::new();
        inner.insert("fsevents".to_string(), "^2.3.0".to_string());
        graph
            .skipped_optional_dependencies
            .insert(".".to_string(), inner);
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_new_optional_dep_was_never_seen() {
        // Cursor Bugbot regression: a brand-new optional dep that the
        // previous resolve never saw must trigger drift, otherwise it
        // would silently never get installed. Distinct from a
        // platform-skipped optional, which has an entry in
        // `skipped_optional_dependencies`.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .optional_dependencies
            .insert("fsevents".into(), "^2.3.0".into());
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("fsevents"), "{reason}"),
            DriftStatus::Fresh => panic!("expected Stale on new optional dep"),
        }
    }

    #[test]
    fn stale_when_skipped_optional_dep_specifier_changes() {
        // The user bumped the range on a previously-skipped optional;
        // the recorded specifier no longer matches the manifest, so we
        // need to re-resolve.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .optional_dependencies
            .insert("fsevents".into(), "^2.4.0".into());
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        let mut inner = BTreeMap::new();
        inner.insert("fsevents".to_string(), "^2.3.0".to_string());
        graph
            .skipped_optional_dependencies
            .insert(".".to_string(), inner);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("fsevents"), "{reason}"),
            DriftStatus::Fresh => panic!("expected Stale on skipped optional spec change"),
        }
    }

    #[test]
    fn stale_when_skipped_optional_is_promoted_to_required() {
        // Cursor Bugbot regression: if the user moves a previously-
        // skipped optional into `dependencies` (same specifier), the
        // skipped-list exemption must NOT fire — the dep is now
        // required and the lockfile genuinely doesn't include it.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0"), ("fsevents", "^2.3.0")]);
        // Note: fsevents lives in `dependencies`, not
        // `optional_dependencies`, even though the lockfile recorded
        // it under skipped optionals from a previous resolve.
        manifest.optional_dependencies.clear();
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        let mut inner = BTreeMap::new();
        inner.insert("fsevents".to_string(), "^2.3.0".to_string());
        graph
            .skipped_optional_dependencies
            .insert(".".to_string(), inner);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("fsevents"), "{reason}"),
            DriftStatus::Fresh => {
                panic!("expected Stale: skipped-optional exemption must not apply to required deps")
            }
        }
    }

    #[test]
    fn stale_when_optional_dep_specifier_changes_in_lockfile() {
        // Spec changes on optionals that *are* present must still
        // drift, so the resolver re-runs when the user bumps a range.
        let mut manifest = make_manifest(&[]);
        manifest
            .optional_dependencies
            .insert("fsevents".into(), "^2.4.0".into());
        let mut graph = make_graph(&[]);
        graph.importers.get_mut(".").unwrap().push(DirectDep {
            name: "fsevents".into(),
            dep_path: "fsevents@2.3.0".into(),
            dep_type: DepType::Optional,
            specifier: Some("^2.3.0".into()),
        });
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("fsevents"), "{reason}"),
            DriftStatus::Fresh => panic!("expected Stale on optional spec change"),
        }
    }

    #[test]
    fn fresh_for_empty_manifest_and_lockfile() {
        let manifest = make_manifest(&[]);
        let graph = make_graph(&[]);
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn workspace_drift_detects_change_in_non_root_importer() {
        // Build a graph with two importers: root and packages/app.
        let root_dep = DirectDep {
            name: "lodash".into(),
            dep_path: "lodash@4.17.21".into(),
            dep_type: DepType::Production,
            specifier: Some("^4.17.0".into()),
        };
        let app_dep = DirectDep {
            name: "express".into(),
            dep_path: "express@4.18.0".into(),
            dep_type: DepType::Production,
            specifier: Some("^4.18.0".into()),
        };
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![root_dep]);
        importers.insert("packages/app".to_string(), vec![app_dep]);
        let graph = LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            ..Default::default()
        };

        let root_manifest = make_manifest(&[("lodash", "^4.17.0")]);
        // App manifest changed express to ^5.0.0 — should be detected as stale.
        let app_manifest = make_manifest(&[("express", "^5.0.0")]);

        let workspace_manifests = vec![
            (".".to_string(), root_manifest.clone()),
            ("packages/app".to_string(), app_manifest),
        ];
        match graph.check_drift_workspace(
            &workspace_manifests,
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
            true,
        ) {
            DriftStatus::Stale { reason } => {
                assert!(reason.contains("packages/app"));
                assert!(reason.contains("express"));
            }
            DriftStatus::Fresh => panic!("expected Stale"),
        }

        // Single-importer check_drift on root only would say Fresh.
        assert_eq!(
            graph.check_drift(&root_manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn filter_deps_prunes_dev_only_subtree() {
        // Graph: prod-root (foo) + dev-root (jest) with transitive chains.
        // After filtering out Dev, jest + its transitives should be pruned,
        // foo + its transitives should remain.
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".into(),
                    dep_path: "foo@1.0.0".into(),
                    dep_type: DepType::Production,
                    specifier: Some("^1.0.0".into()),
                },
                DirectDep {
                    name: "jest".into(),
                    dep_path: "jest@29.0.0".into(),
                    dep_type: DepType::Dev,
                    specifier: Some("^29.0.0".into()),
                },
            ],
        );

        let mut packages = BTreeMap::new();
        let mut foo_deps = BTreeMap::new();
        foo_deps.insert("bar".to_string(), "2.0.0".to_string());
        packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".into(),
                version: "1.0.0".into(),
                integrity: None,
                dependencies: foo_deps,
                dep_path: "foo@1.0.0".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "bar@2.0.0".to_string(),
            LockedPackage {
                name: "bar".into(),
                version: "2.0.0".into(),
                integrity: None,
                dependencies: BTreeMap::new(),
                dep_path: "bar@2.0.0".into(),
                ..Default::default()
            },
        );
        let mut jest_deps = BTreeMap::new();
        jest_deps.insert("jest-core".to_string(), "29.0.0".to_string());
        packages.insert(
            "jest@29.0.0".to_string(),
            LockedPackage {
                name: "jest".into(),
                version: "29.0.0".into(),
                integrity: None,
                dependencies: jest_deps,
                dep_path: "jest@29.0.0".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "jest-core@29.0.0".to_string(),
            LockedPackage {
                name: "jest-core".into(),
                version: "29.0.0".into(),
                integrity: None,
                dependencies: BTreeMap::new(),
                dep_path: "jest-core@29.0.0".into(),
                ..Default::default()
            },
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let prod = graph.filter_deps(|d| d.dep_type != DepType::Dev);

        // Direct deps: only foo, jest dropped
        let roots = prod.root_deps();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "foo");

        // Reachable packages: foo + bar (transitive), NOT jest or jest-core
        assert!(prod.packages.contains_key("foo@1.0.0"));
        assert!(prod.packages.contains_key("bar@2.0.0"));
        assert!(!prod.packages.contains_key("jest@29.0.0"));
        assert!(!prod.packages.contains_key("jest-core@29.0.0"));
    }

    // Regression for #50 feedback: `filter_deps` is a structural
    // operation and must preserve the source graph's `settings:`
    // metadata. A filtered graph that's handed to the lockfile writer
    // (as `aube prune` does today) would otherwise reset
    // `autoInstallPeers` to its default and silently flip the user's
    // choice on the next install.
    #[test]
    fn filter_deps_preserves_lockfile_settings() {
        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages: BTreeMap::new(),
            settings: LockfileSettings {
                auto_install_peers: false,
                exclude_links_from_lockfile: true,
                lockfile_include_tarball_url: false,
            },
            ..Default::default()
        };
        let filtered = graph.filter_deps(|_| true);
        assert!(!filtered.settings.auto_install_peers);
        assert!(filtered.settings.exclude_links_from_lockfile);
    }

    #[test]
    fn filter_deps_keeps_shared_transitive_reachable_via_prod() {
        // Graph: prod foo → shared, dev jest → shared
        // Filtering out Dev should still keep `shared` because foo → shared
        // keeps it reachable.
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".into(),
                    dep_path: "foo@1.0.0".into(),
                    dep_type: DepType::Production,
                    specifier: Some("^1.0.0".into()),
                },
                DirectDep {
                    name: "jest".into(),
                    dep_path: "jest@29.0.0".into(),
                    dep_type: DepType::Dev,
                    specifier: Some("^29.0.0".into()),
                },
            ],
        );

        let mut packages = BTreeMap::new();
        for (name, ver, deps) in [
            ("foo", "1.0.0", vec![("shared", "1.0.0")]),
            ("jest", "29.0.0", vec![("shared", "1.0.0")]),
            ("shared", "1.0.0", vec![]),
        ] {
            let mut dep_map = BTreeMap::new();
            for (n, v) in deps {
                dep_map.insert(n.to_string(), v.to_string());
            }
            packages.insert(
                format!("{name}@{ver}"),
                LockedPackage {
                    name: name.into(),
                    version: ver.into(),
                    integrity: None,
                    dependencies: dep_map,
                    dep_path: format!("{name}@{ver}"),
                    ..Default::default()
                },
            );
        }

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let prod = graph.filter_deps(|d| d.dep_type != DepType::Dev);

        assert!(prod.packages.contains_key("foo@1.0.0"));
        assert!(prod.packages.contains_key("shared@1.0.0"));
        assert!(!prod.packages.contains_key("jest@29.0.0"));
    }

    #[test]
    fn subset_to_importer_returns_none_for_missing_importer() {
        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages: BTreeMap::new(),
            ..Default::default()
        };
        assert!(graph.subset_to_importer("packages/lib", |_| true).is_none());
    }

    #[test]
    fn subset_to_importer_keeps_only_requested_importer_transitive_closure() {
        // Workspace graph with two importers that own independent
        // subtrees: packages/lib pulls is-odd → is-number, packages/app
        // pulls express. Subsetting to packages/lib must yield a graph
        // rooted at `.` containing only is-odd + is-number, with
        // express pruned. Matches what `aube deploy --filter @test/lib`
        // should write into the target.
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![]);
        importers.insert(
            "packages/lib".to_string(),
            vec![DirectDep {
                name: "is-odd".into(),
                dep_path: "is-odd@3.0.1".into(),
                dep_type: DepType::Production,
                specifier: Some("^3.0.1".into()),
            }],
        );
        importers.insert(
            "packages/app".to_string(),
            vec![DirectDep {
                name: "express".into(),
                dep_path: "express@4.18.0".into(),
                dep_type: DepType::Production,
                specifier: Some("^4.18.0".into()),
            }],
        );

        let mut packages = BTreeMap::new();
        let mut is_odd_deps = BTreeMap::new();
        is_odd_deps.insert("is-number".to_string(), "6.0.0".to_string());
        packages.insert(
            "is-odd@3.0.1".to_string(),
            LockedPackage {
                name: "is-odd".into(),
                version: "3.0.1".into(),
                dependencies: is_odd_deps,
                dep_path: "is-odd@3.0.1".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "is-number@6.0.0".to_string(),
            LockedPackage {
                name: "is-number".into(),
                version: "6.0.0".into(),
                dep_path: "is-number@6.0.0".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "express@4.18.0".to_string(),
            LockedPackage {
                name: "express".into(),
                version: "4.18.0".into(),
                dep_path: "express@4.18.0".into(),
                ..Default::default()
            },
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let subset = graph
            .subset_to_importer("packages/lib", |_| true)
            .expect("packages/lib importer present");

        assert_eq!(subset.importers.len(), 1);
        let roots = subset.root_deps();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "is-odd");

        assert!(subset.packages.contains_key("is-odd@3.0.1"));
        assert!(subset.packages.contains_key("is-number@6.0.0"));
        assert!(!subset.packages.contains_key("express@4.18.0"));
    }

    #[test]
    fn subset_to_importer_honors_keep_predicate_for_prod_deploys() {
        // packages/lib has both prod (is-odd) and dev (jest) deps.
        // `aube deploy --prod` should pass `|d| d.dep_type != Dev` as
        // the keep filter; the resulting subset retains only is-odd
        // so drift against the target's dev-stripped manifest stays
        // clean.
        let mut importers = BTreeMap::new();
        importers.insert(
            "packages/lib".to_string(),
            vec![
                DirectDep {
                    name: "is-odd".into(),
                    dep_path: "is-odd@3.0.1".into(),
                    dep_type: DepType::Production,
                    specifier: Some("^3.0.1".into()),
                },
                DirectDep {
                    name: "jest".into(),
                    dep_path: "jest@29.0.0".into(),
                    dep_type: DepType::Dev,
                    specifier: Some("^29.0.0".into()),
                },
            ],
        );
        let mut packages = BTreeMap::new();
        packages.insert(
            "is-odd@3.0.1".to_string(),
            LockedPackage {
                name: "is-odd".into(),
                version: "3.0.1".into(),
                dep_path: "is-odd@3.0.1".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "jest@29.0.0".to_string(),
            LockedPackage {
                name: "jest".into(),
                version: "29.0.0".into(),
                dep_path: "jest@29.0.0".into(),
                ..Default::default()
            },
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let prod = graph
            .subset_to_importer("packages/lib", |d| d.dep_type != DepType::Dev)
            .expect("importer present");
        let roots = prod.root_deps();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "is-odd");
        assert!(prod.packages.contains_key("is-odd@3.0.1"));
        assert!(!prod.packages.contains_key("jest@29.0.0"));
    }

    #[test]
    fn subset_to_importer_preserves_graph_settings() {
        // Structural pruning, not a resolution-mode reset: a deploy
        // into a target that uses the source workspace's settings
        // header (autoInstallPeers / lockfileIncludeTarballUrl)
        // should write them through unchanged so a frozen install in
        // the target sees the same resolution-mode state.
        let mut importers = BTreeMap::new();
        importers.insert("packages/lib".to_string(), vec![]);
        let graph = LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            settings: LockfileSettings {
                auto_install_peers: false,
                exclude_links_from_lockfile: true,
                lockfile_include_tarball_url: true,
            },
            ..Default::default()
        };
        let subset = graph.subset_to_importer("packages/lib", |_| true).unwrap();
        assert!(!subset.settings.auto_install_peers);
        assert!(subset.settings.exclude_links_from_lockfile);
        assert!(subset.settings.lockfile_include_tarball_url);
    }

    #[test]
    fn subset_to_importer_rekeys_skipped_optionals_to_root() {
        // `skipped_optional_dependencies` is per-importer. After
        // subsetting, only the retained importer's entry should
        // survive — rekeyed to `.` so a frozen install in the target
        // (which has exactly one importer) doesn't see ghost entries.
        let mut importers = BTreeMap::new();
        importers.insert("packages/lib".to_string(), vec![]);
        importers.insert("packages/app".to_string(), vec![]);
        let mut skipped = BTreeMap::new();
        let mut lib_skip = BTreeMap::new();
        lib_skip.insert("fsevents".to_string(), "^2".to_string());
        skipped.insert("packages/lib".to_string(), lib_skip);
        let mut app_skip = BTreeMap::new();
        app_skip.insert("ghost".to_string(), "*".to_string());
        skipped.insert("packages/app".to_string(), app_skip);
        let graph = LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            skipped_optional_dependencies: skipped,
            ..Default::default()
        };
        let subset = graph.subset_to_importer("packages/lib", |_| true).unwrap();
        assert_eq!(subset.skipped_optional_dependencies.len(), 1);
        let root = subset.skipped_optional_dependencies.get(".").unwrap();
        assert!(root.contains_key("fsevents"));
        assert!(!root.contains_key("ghost"));
    }

    #[test]
    fn workspace_drift_fresh_when_all_importers_match() {
        let root_dep = DirectDep {
            name: "lodash".into(),
            dep_path: "lodash@4.17.21".into(),
            dep_type: DepType::Production,
            specifier: Some("^4.17.0".into()),
        };
        let app_dep = DirectDep {
            name: "express".into(),
            dep_path: "express@4.18.0".into(),
            dep_type: DepType::Production,
            specifier: Some("^4.18.0".into()),
        };
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![root_dep]);
        importers.insert("packages/app".to_string(), vec![app_dep]);
        let graph = LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            ..Default::default()
        };

        let workspace_manifests = vec![
            (".".to_string(), make_manifest(&[("lodash", "^4.17.0")])),
            (
                "packages/app".to_string(),
                make_manifest(&[("express", "^4.18.0")]),
            ),
        ];
        assert_eq!(
            graph.check_drift_workspace(
                &workspace_manifests,
                &BTreeMap::new(),
                &[],
                &BTreeMap::new(),
                true,
            ),
            DriftStatus::Fresh
        );
    }

    #[allow(clippy::type_complexity)]
    fn mk_catalogs(
        entries: &[(&str, &[(&str, &str, &str)])],
    ) -> BTreeMap<String, BTreeMap<String, CatalogEntry>> {
        let mut out: BTreeMap<String, BTreeMap<String, CatalogEntry>> = BTreeMap::new();
        for (cat, pkgs) in entries {
            let mut inner = BTreeMap::new();
            for (pkg, spec, ver) in *pkgs {
                inner.insert(
                    (*pkg).to_string(),
                    CatalogEntry {
                        specifier: (*spec).to_string(),
                        version: (*ver).to_string(),
                    },
                );
            }
            out.insert((*cat).to_string(), inner);
        }
        out
    }

    fn mk_workspace_catalogs(
        entries: &[(&str, &[(&str, &str)])],
    ) -> BTreeMap<String, BTreeMap<String, String>> {
        entries
            .iter()
            .map(|(cat, pkgs)| {
                (
                    (*cat).to_string(),
                    pkgs.iter()
                        .map(|(p, s)| ((*p).to_string(), (*s).to_string()))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn catalog_drift_fresh_when_specifiers_match() {
        let graph = LockfileGraph {
            catalogs: mk_catalogs(&[("default", &[("react", "^18.0.0", "18.2.0")])]),
            ..Default::default()
        };
        let ws = mk_workspace_catalogs(&[("default", &[("react", "^18.0.0")])]);
        assert_eq!(graph.check_catalogs_drift(&ws), DriftStatus::Fresh);
    }

    #[test]
    fn catalog_drift_stale_on_changed_specifier() {
        let graph = LockfileGraph {
            catalogs: mk_catalogs(&[("default", &[("react", "^18.0.0", "18.2.0")])]),
            ..Default::default()
        };
        let ws = mk_workspace_catalogs(&[("default", &[("react", "^19.0.0")])]);
        match graph.check_catalogs_drift(&ws) {
            DriftStatus::Stale { reason } => assert!(reason.contains("react")),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn catalog_drift_fresh_when_workspace_adds_unused_entry() {
        // pnpm only writes referenced entries — an unreferenced
        // workspace entry is not drift. The "newly used" transition
        // is caught by the importer-level drift check.
        let graph = LockfileGraph::default();
        let ws = mk_workspace_catalogs(&[("default", &[("react", "^18")])]);
        assert_eq!(graph.check_catalogs_drift(&ws), DriftStatus::Fresh);
    }

    #[test]
    fn catalog_drift_stale_on_removed_workspace_entry() {
        let graph = LockfileGraph {
            catalogs: mk_catalogs(&[("default", &[("react", "^18", "18.2.0")])]),
            ..Default::default()
        };
        let ws = mk_workspace_catalogs(&[]);
        assert!(matches!(
            graph.check_catalogs_drift(&ws),
            DriftStatus::Stale { .. }
        ));
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // --- patched-dependency drift (issue #15) ---
    //
    // A pnpm lockfile records the patch's per-file *hash* as the
    // `patchedDependencies` value (no path), so drift for pnpm is a pure
    // hash-against-hash comparison. The path map is empty for a pnpm
    // lockfile; comparing it against the manifest-derived paths used to
    // report every declared patch as "missing from the lockfile".

    #[test]
    fn pnpm_patched_dep_fresh_when_hash_matches() {
        let graph = LockfileGraph {
            patched_dependency_hashes: map(&[("ms@2.1.3", "abc123")]),
            ..Default::default()
        };
        // The project declares the patch (path + freshly computed hash).
        let effective_paths = map(&[("ms@2.1.3", "patches/ms@2.1.3.patch")]);
        let effective_hashes = map(&[("ms@2.1.3", "abc123")]);
        assert_eq!(
            graph.check_patched_dependencies_drift(
                LockfileKind::Pnpm,
                &effective_paths,
                &effective_hashes
            ),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn pnpm_patched_dep_stale_when_hash_differs() {
        let graph = LockfileGraph {
            patched_dependency_hashes: map(&[("ms@2.1.3", "abc123")]),
            ..Default::default()
        };
        let effective_paths = map(&[("ms@2.1.3", "patches/ms@2.1.3.patch")]);
        let effective_hashes = map(&[("ms@2.1.3", "def456")]);
        match graph.check_patched_dependencies_drift(
            LockfileKind::Pnpm,
            &effective_paths,
            &effective_hashes,
        ) {
            DriftStatus::Stale { reason } => assert!(reason.contains("ms@2.1.3"), "{reason}"),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn pnpm_patched_dep_stale_when_declared_but_lockfile_missing() {
        let graph = LockfileGraph::default();
        let effective_paths = map(&[("ms@2.1.3", "patches/ms@2.1.3.patch")]);
        let effective_hashes = map(&[("ms@2.1.3", "abc123")]);
        assert!(matches!(
            graph.check_patched_dependencies_drift(
                LockfileKind::Pnpm,
                &effective_paths,
                &effective_hashes
            ),
            DriftStatus::Stale { .. }
        ));
    }

    #[test]
    fn aube_patched_dep_fresh_when_hash_matches_with_empty_path_map() {
        // aube's own lock.yaml shares pnpm's `{ hash, path }` block and is
        // read by the same parser, which keeps only the hash and leaves
        // `patched_dependencies` (the path map) empty. A frozen install
        // declaring the patch with a matching hash must read as Fresh, not
        // a false "declared in the project but missing from the lockfile"
        // (which is what the old path-against-empty-map comparison produced
        // for the Aube kind after a `nub pm use nub` conversion).
        let graph = LockfileGraph {
            patched_dependency_hashes: map(&[("ms@2.1.3", "abc123")]),
            ..Default::default()
        };
        let effective_paths = map(&[("ms@2.1.3", "patches/ms@2.1.3.patch")]);
        let effective_hashes = map(&[("ms@2.1.3", "abc123")]);
        assert_eq!(
            graph.check_patched_dependencies_drift(
                LockfileKind::Aube,
                &effective_paths,
                &effective_hashes
            ),
            DriftStatus::Fresh,
        );
        // And a changed hash is still Stale under the Aube kind.
        let stale = LockfileGraph {
            patched_dependency_hashes: map(&[("ms@2.1.3", "abc123")]),
            ..Default::default()
        };
        match stale.check_patched_dependencies_drift(
            LockfileKind::Aube,
            &effective_paths,
            &map(&[("ms@2.1.3", "def456")]),
        ) {
            DriftStatus::Stale { reason } => assert!(reason.contains("ms@2.1.3"), "{reason}"),
            other => panic!("expected stale on changed hash, got {other:?}"),
        }
    }

    #[test]
    fn bun_patched_dep_compares_path_not_hash() {
        // The kind-branch keeps bun on the PATH interpretation: bun's
        // lockfile carries a real patch path in `patched_dependencies`,
        // so a matching path is Fresh and a moved path is Stale —
        // independent of any recorded hash.
        let graph = LockfileGraph {
            patched_dependencies: map(&[("ms@2.1.3", "patches/ms@2.1.3.patch")]),
            ..Default::default()
        };
        let effective_hashes = map(&[("ms@2.1.3", "abc123")]);
        assert_eq!(
            graph.check_patched_dependencies_drift(
                LockfileKind::Bun,
                &map(&[("ms@2.1.3", "patches/ms@2.1.3.patch")]),
                &effective_hashes
            ),
            DriftStatus::Fresh,
        );
        match graph.check_patched_dependencies_drift(
            LockfileKind::Bun,
            &map(&[("ms@2.1.3", "patches/moved.patch")]),
            &effective_hashes,
        ) {
            DriftStatus::Stale { reason } => assert!(reason.contains("ms@2.1.3"), "{reason}"),
            other => panic!("expected stale on moved path, got {other:?}"),
        }
    }
}
