//! Transpile-cache key derivation, factored out of `nub-native` so the
//! invalidation contract is unit-testable (see this crate's Cargo.toml for why a
//! test can't live in the cdylib `nub-native`).
//!
//! The key preimage is NUL-separated and order-fixed (no trailing separator):
//!   `nub_version \0 schema \0 build_id \0 source \0 ext \0 tsconfig_hash \0 pkg_type`
//! → blake3 → 64-hex lowercase → the cache FILENAME. Every component is hashed
//! IN, so a change to ANY of them (a nub release, a CACHE_SCHEMA bump, a rebuild
//! at a new build-id) yields a disjoint filename — old on-disk entries are
//! silently ignored (a miss), never mis-read. `nub-native` calls `cache_key` with
//! its `NUB_VERSION` / `CACHE_SCHEMA` / `BUILD_ID` consts.

/// blake3 of the NUL-separated key preimage → 64-hex lowercase.
///
/// `nub_version` / `schema` / `build_id` are the cross-build invalidation
/// components (compile-time consts on the production path); `source` / `ext` /
/// `tsconfig_hash` / `pkg_type` are the per-file inputs.
#[allow(clippy::too_many_arguments)]
pub fn cache_key(
    nub_version: &str,
    schema: &str,
    build_id: &str,
    source: &str,
    ext: &str,
    tsconfig_hash: &str,
    pkg_type: &str,
) -> String {
    let mut h = blake3::Hasher::new();
    h.update(nub_version.as_bytes());
    h.update(b"\0");
    h.update(schema.as_bytes());
    h.update(b"\0");
    h.update(build_id.as_bytes());
    h.update(b"\0");
    h.update(source.as_bytes());
    h.update(b"\0");
    h.update(ext.as_bytes());
    h.update(b"\0");
    h.update(tsconfig_hash.as_bytes());
    h.update(b"\0");
    h.update(pkg_type.as_bytes());
    h.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::cache_key;

    // Fixed per-file inputs so each test varies exactly one cross-build component.
    const SRC: &str = "export const x: number = 1;";
    const EXT: &str = "ts";
    const TSCONFIG: &str = "tsconfig-hash";
    const PKG: &str = "module";

    fn key(version: &str, schema: &str, build_id: &str) -> String {
        cache_key(version, schema, build_id, SRC, EXT, TSCONFIG, PKG)
    }

    /// A rebuilt binary (new build-id) must not serve a prior build's entries:
    /// folding the build-id into the key is the whole point of the compile-time
    /// build-id stamp, so a changed build-id over identical source/config has to
    /// produce a different filename.
    #[test]
    fn cache_key_changes_when_build_id_changes() {
        assert_ne!(
            key("0.0.1", "5", "abc1234"),
            key("0.0.1", "5", "def5678"),
            "a different build-id must yield a different cache key"
        );
    }

    /// The schema is hashed into the key, so a schema bump (e.g. the 4→5 move)
    /// makes the two eras' filenames disjoint — a "5" build can never read a
    /// "4"-era entry, it simply misses.
    #[test]
    fn cache_key_namespaced_by_schema() {
        assert_ne!(
            key("0.0.1", "4", "abc1234"),
            key("0.0.1", "5", "abc1234"),
            "a different schema must yield a different cache key"
        );
    }

    /// At a fixed clean commit the build-id is reproducible, so a release's
    /// rebuilds reuse the cache — identical inputs must always map to one key.
    #[test]
    fn cache_key_is_stable_for_identical_inputs() {
        assert_eq!(key("0.0.1", "5", "abc1234"), key("0.0.1", "5", "abc1234"));
    }
}
