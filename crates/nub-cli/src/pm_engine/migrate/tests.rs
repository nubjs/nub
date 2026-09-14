use super::migration_hint;
use super::pending_migration;
use std::path::PathBuf;

fn root(tag: &str, files: &[&str]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nub-migrate-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is past the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("creating the fixture root");
    for file in files {
        std::fs::write(dir.join(file), "").expect("writing a fixture lockfile");
    }
    dir
}

fn name_of(path: Option<PathBuf>) -> Option<String> {
    path.map(|p| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    })
}

/// Each foreign lockfile on its own is a migration waiting to happen, so an
/// install in that project has something to say.
#[test]
fn a_foreign_lockfile_alone_is_a_pending_migration() {
    for file in [
        "yarn.lock",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "bun.lock",
        "bun.lockb",
    ] {
        let dir = root("foreign", &[file]);
        assert_eq!(name_of(pending_migration(&dir)).as_deref(), Some(file));
    }
}

/// A lockfile of nub's own answers the question the migration would, so
/// nothing is pending — including under the legacy name and under pnpm's,
/// which is the same bytes. Without this the hint would print on every
/// install of a project that has already migrated but kept the old file
/// around.
#[test]
fn a_lockfile_of_nubs_own_settles_it() {
    for own in ["nub.lock", "lock.yaml", "pnpm-lock.yaml"] {
        let dir = root("own", &[own, "package-lock.json"]);
        assert_eq!(
            pending_migration(&dir),
            None,
            "{own} left a migration pending"
        );
    }
}

/// A project with no lockfile at all has nothing to migrate.
#[test]
fn no_lockfile_is_no_migration() {
    assert_eq!(pending_migration(&root("empty", &[])), None);
}

/// The hint names the file it did not read and the command that reads it.
/// It is one line: an install that succeeded must not end in a paragraph.
#[test]
fn the_hint_names_the_file_and_the_command() {
    let hint = migration_hint(std::path::Path::new("/p/yarn.lock"));
    assert!(hint.contains("yarn.lock"), "{hint}");
    assert!(hint.contains("nub pm migrate"), "{hint}");
    assert_eq!(hint.lines().count(), 1, "{hint}");
}

/// Lockfiles from two package managers are two answers to what the project
/// resolves to, so the migration refuses rather than pick one and remove it.
#[test]
fn lockfiles_from_two_package_managers_are_refused_untouched() {
    let dir = root("two-families", &["yarn.lock", "bun.lock"]);
    std::fs::write(dir.join("package.json"), "{\"name\":\"app\"}\n").expect("writing the manifest");

    let error =
        super::run_pm_migrate(&dir).expect_err("two package managers' lockfiles must refuse");

    let message = error.to_string();
    assert!(
        message.contains("multiple lockfiles found (yarn.lock, bun.lock)"),
        "the refusal must name both files: {message}"
    );
    assert!(
        dir.join("yarn.lock").is_file() && dir.join("bun.lock").is_file(),
        "a refusal must remove neither lockfile"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
