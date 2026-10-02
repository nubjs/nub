use std::collections::BTreeMap;

/// Re-key a canonical-indexed indices map to match the peer-contextualized
/// dep_paths in `graph`. Each contextualized entry points at the same
/// underlying files as its canonical name@version. Move each index into
/// one placement; only additional peer contexts need a copy.
pub(super) fn remap_indices_to_contextualized(
    mut canonical_indices: BTreeMap<String, aube_store::PackageIndex>,
    graph: &aube_lockfile::LockfileGraph,
) -> BTreeMap<String, aube_store::PackageIndex> {
    let mut placements: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (dep_path, pkg) in &graph.packages {
        let canonical_key = pkg.spec_key();
        // The peer-context pass appends a `(peer@ver)` suffix (or a
        // parenthesized `(<short-hash>)` when it exceeds the cap) onto a
        // package's canonical dep_path. Source-backed deps (git /
        // remote tarball / file) are streamed from the resolver — and
        // therefore keyed in `canonical_indices` — under their
        // *source-coordinate* dep_path (`name@git+<short>`), not their
        // semver `spec_key()`. So once such a dep picks up a peer
        // suffix, neither the contextualized `dep_path` (carries the
        // suffix) nor `spec_key()` (semver, not the git coordinate)
        // matches the streamed key, and the index would be silently
        // dropped — later tripping `ERR_AUBE_MISSING_PACKAGE_INDEX` in
        // the linker's global-virtual-store pass. Stripping the suffix
        // recovers the exact canonical coordinate the index was stored
        // under (the peer-context pass builds the key as
        // `{canonical_base}{suffix}`, so this is its precise inverse).
        let canonical_dep_path = strip_peer_context_suffix(dep_path);
        if let Some((key, _)) = canonical_indices
            .get_key_value(dep_path)
            .or_else(|| canonical_indices.get_key_value(canonical_dep_path))
            .or_else(|| canonical_indices.get_key_value(&canonical_key))
        {
            placements.entry(key.clone()).or_default().push(dep_path);
        }
    }
    // Resolve all keys before removing indexes: an exact key can also be
    // another placement's canonical fallback.
    let mut out = BTreeMap::new();
    for (key, dep_paths) in placements {
        let index = canonical_indices.remove(&key).expect("resolved index");
        let mut dep_paths = dep_paths.into_iter();
        let last = dep_paths.next_back().expect("at least one placement");
        for dep_path in dep_paths {
            out.insert(dep_path.to_owned(), index.clone());
        }
        out.insert(last.to_owned(), index);
    }
    out
}

/// Strip the peer-context suffix from a `dep_path`, recovering the
/// canonical dep_path the resolver streamed it under (and that
/// `canonical_indices` is keyed by). The peer-context pass in
/// `aube-resolver` appends either a parenthesized `(peer@ver)…` tail
/// or, when the suffix body exceeds the length cap, a single
/// parenthesized short hash `(<short-hash>)` (pnpm's
/// `createPeerDepGraphHash`). Both forms begin at the first `(`, so
/// cutting there is the exact inverse and recovers the canonical
/// coordinate. A `dep_path` with no suffix is returned unchanged — a
/// bare `_<hex>` tail belongs to a `git+`/`url+`/`file+` source
/// coordinate and is never a peer marker, so it is preserved.
pub(super) fn strip_peer_context_suffix(dep_path: &str) -> &str {
    dep_path.split('(').next().unwrap_or(dep_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{LockedPackage, LockfileGraph};
    use aube_store::{PackageIndex, StoredFile};

    fn index(hash: &str) -> PackageIndex {
        [(
            "package.json".into(),
            StoredFile {
                hex_hash: hash.into(),
                store_path: "/store/file".into(),
                executable: false,
                size: Some(2),
            },
        )]
        .into_iter()
        .collect()
    }

    #[test]
    fn moves_single_placement_and_clones_additional_peers() {
        let original = index("original");
        let hash_ptr = original["package.json"].hex_hash.as_ptr();
        let mut graph = LockfileGraph::default();
        for path in ["foo@1.0.0(peer@1)", "foo@1.0.0(peer@2)"] {
            graph.packages.insert(
                path.into(),
                LockedPackage {
                    name: "foo".into(),
                    version: "1.0.0".into(),
                    dep_path: path.into(),
                    ..Default::default()
                },
            );
        }
        let mut output =
            remap_indices_to_contextualized([("foo@1.0.0".into(), original)].into(), &graph);
        assert_eq!(output.len(), 2);
        assert_eq!(
            output["foo@1.0.0(peer@2)"]["package.json"]
                .hex_hash
                .as_ptr(),
            hash_ptr
        );
        output.get_mut("foo@1.0.0(peer@1)").unwrap().clear();
        assert_eq!(
            output["foo@1.0.0(peer@2)"]["package.json"].hex_hash,
            "original"
        );
    }

    #[test]
    fn resolves_exact_and_fallback_keys_before_moving() {
        let mut graph = LockfileGraph::default();
        for path in [
            "foo@1.0.0",
            "foo@1.0.0(peer@1)",
            "foo@1.0.0(peer@2)",
            "alias",
        ] {
            graph.packages.insert(
                path.into(),
                LockedPackage {
                    name: "foo".into(),
                    version: "1.0.0".into(),
                    dep_path: path.into(),
                    ..Default::default()
                },
            );
        }
        let canonical = index("canonical");
        let exact = index("exact");
        let output = remap_indices_to_contextualized(
            [
                ("foo@1.0.0".into(), canonical),
                ("foo@1.0.0(peer@1)".into(), exact),
                ("unused@1.0.0".into(), index("unused")),
            ]
            .into(),
            &graph,
        );
        assert_eq!(output.len(), 4);
        assert_eq!(
            output["foo@1.0.0(peer@1)"]["package.json"].hex_hash,
            "exact"
        );
        for key in ["foo@1.0.0", "foo@1.0.0(peer@2)", "alias"] {
            assert_eq!(output[key]["package.json"].hex_hash, "canonical");
        }
    }
}
