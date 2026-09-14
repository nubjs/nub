//! Which package manager's rules a directory declares — the per-directory half
//! of a project's identity.
//!
//! Every project is pnpm's or nub's. pnpm's markers are a `pnpm-lock.yaml`, a
//! `pnpm-workspace.yaml`, or a pnpm pin; everything else, a fresh directory
//! included, is nub's. npm, yarn and bun confer no identity of their own.
//!
//! The rule lives here, not beside the CLI's identity walk, because two readers
//! have to reach the SAME answer: the CLI's walk, which picks the profile an
//! install runs under, and this crate's workspace detection, which decides
//! whether a `pnpm-workspace.yaml` names the members `run -r` and `--filter`
//! see. They once held different rules — workspace detection still demanded a
//! `pnpm-lock.yaml` after the install accepted the yaml alone — so a pnpm
//! workspace that had not been installed yet ran as pnpm's and reported no
//! members.

use std::path::Path;

/// The rules a project runs under. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectIdentity {
    /// The project declares pnpm. Behave exactly like pnpm.
    Pnpm,
    /// Everything else, including a fresh project.
    Nub,
}

/// The identity `dir` declares on its own, or `None` when it declares
/// nothing and a search should keep walking up.
///
/// pnpm's markers are checked before `nub.lock`: a directory holding both a
/// `pnpm-lock.yaml` and a `nub.lock` is a project mid-migration, and until
/// the migration finishes the incumbent is still pnpm.
pub fn identity_of_dir(dir: &Path) -> Option<ProjectIdentity> {
    let declared = declared_package_manager(dir);
    // A project that names NUB as its owner is nub's, whatever pnpm-named
    // file is lying beside it. That file is an artifact — `pm use nub`
    // leaves one behind, and so does adding the declaration by hand — and
    // nub already answers it with a warning naming the file unread and the
    // two ways to resolve it. Reading the artifact instead refused the
    // project outright, in the engine's own words: `This project is
    // configured to use nub. pnpm cannot provide nub.`
    //
    // Only that way round. A declaration naming anything ELSE keeps the
    // files ahead of it: npm, yarn and bun confer no identity at all, so a
    // foreign name is no statement of ownership, while a real
    // `pnpm-lock.yaml` beside it still is.
    if declared.as_deref() == Some("nub") {
        return Some(ProjectIdentity::Nub);
    }
    if dir.join("pnpm-lock.yaml").exists() || dir.join("pnpm-workspace.yaml").exists() {
        return Some(ProjectIdentity::Pnpm);
    }
    match declared {
        Some(name) if name == "pnpm" => Some(ProjectIdentity::Pnpm),
        Some(_) => Some(ProjectIdentity::Nub),
        None => dir
            .join("nub.lock")
            .exists()
            .then_some(ProjectIdentity::Nub),
    }
}

/// The package manager `dir`'s own manifest names, without its version.
///
/// Read through the crate's one pin reader, so `packageManager` and every form
/// of `devEngines.packageManager` parse exactly as they do for provisioning.
/// Best-effort: an absent, unreadable or malformed manifest declares nothing,
/// and the command that needs the manifest reports the real error later. A name
/// nub does not recognise still counts as a declaration — the project named an
/// owner that is not pnpm.
fn declared_package_manager(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(crate::strip_utf8_bom(&text)).ok()?;
    super::resolve::raw_pin_name_version(&manifest).map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `devEngines.packageManager` may be an array, and its LAST named entry is
    /// the declaration. Each ordering is the other's control: a reader that took
    /// the first entry, or ignored the array, fails one of the two.
    #[test]
    fn the_last_named_dev_engines_entry_is_the_declaration() {
        for (entries, expected) in [
            (r#"[{"name":"pnpm"},{"name":"nub"}]"#, ProjectIdentity::Nub),
            (r#"[{"name":"nub"},{"name":"pnpm"}]"#, ProjectIdentity::Pnpm),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "nub-identity-dev-engines-{}-{}",
                std::process::id(),
                expected == ProjectIdentity::Nub
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("package.json"),
                format!(r#"{{"name":"fx","devEngines":{{"packageManager":{entries}}}}}"#),
            )
            .unwrap();
            // A pnpm-named file beside it, so only the declaration can say nub.
            std::fs::write(dir.join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
            assert_eq!(
                identity_of_dir(&dir),
                Some(expected),
                "devEngines.packageManager = {entries}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
