//! Yarn Plug'n'Play detection.
use std::path::{Path, PathBuf};

/// A detected Yarn PnP context: the nearest `.pnp.cjs` walking up from cwd.
pub struct PnpContext {
    pub pnp_cjs: PathBuf,
}

/// Walk up from `cwd` to the nearest `.pnp.cjs`. `None` if not a PnP tree.
pub fn detect(cwd: &Path) -> Option<PnpContext> {
    let mut dir = cwd.to_path_buf();
    loop {
        let candidate = dir.join(".pnp.cjs");
        if candidate.is_file() {
            return Some(PnpContext { pnp_cjs: candidate });
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::detect;

    #[test]
    fn detect_finds_pnp_cjs_at_an_ancestor_and_none_without_one() {
        let tmp = std::env::temp_dir().join(format!("nub-pnp-detect-{}", std::process::id()));
        let nested = tmp.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();

        // No `.pnp.cjs` anywhere up the chain (within the tempdir) → None.
        // (Walk could in theory hit a real `.pnp.cjs` above the tempdir; the
        // system temp dir is not a PnP tree, so this stays None in practice.)
        assert!(detect(&nested).is_none());

        // Place `.pnp.cjs` at an ancestor; a nested cwd resolves up to it.
        let pnp = tmp.join("a").join(".pnp.cjs");
        std::fs::write(&pnp, "// pnp").unwrap();
        let found = detect(&nested).expect("should find ancestor .pnp.cjs");
        assert_eq!(found.pnp_cjs, pnp);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
