use std::path::PathBuf;

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("file error: {0}")]
    Xx(String),
    #[error("failed to link {0} -> {1}: {2}")]
    #[diagnostic(code(ERR_AUBE_LINK_FAILED))]
    Link(PathBuf, PathBuf, String),
    #[error("failed to apply patch for {0}: {1}")]
    #[diagnostic(code(ERR_AUBE_PATCH_FAILED))]
    Patch(String, String),
    #[error(
        "internal: missing package index for {0} — caller skipped `load_index` but the package wasn't already materialized"
    )]
    #[diagnostic(code(ERR_AUBE_MISSING_PACKAGE_INDEX))]
    MissingPackageIndex(String),
    #[error("refusing to materialize unsafe index key: {0:?}")]
    #[diagnostic(code(ERR_AUBE_UNSAFE_INDEX_KEY))]
    UnsafeIndexKey(String),
    #[error("refusing to create node_modules entry for unsafe package name: {0:?}")]
    #[diagnostic(code(ERR_AUBE_UNSAFE_PACKAGE_NAME))]
    UnsafePackageName(String),
    #[error(
        "cached package index references a missing CAS shard at {store_path} (file: {rel_path:?}). The store and its index cache are out of sync — rerun the install to re-fetch the tarball."
    )]
    #[diagnostic(code(ERR_AUBE_MISSING_STORE_FILE))]
    MissingStoreFile {
        store_path: PathBuf,
        rel_path: String,
    },
}
