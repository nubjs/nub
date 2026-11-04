//! The projects' own lifecycle hooks — the root's and each workspace
//! member's — and which of them each PM command runs, in pnpm's order
//! (pnpm's `PROJECT_LIFECYCLE_STAGES`, `PROJECT_INSTALL_STAGES` and the
//! uninstall stages, `exec/lifecycle`). Offline: the only dependencies are
//! local directories.

use std::path::{Path, PathBuf};
use std::process::Command;

const HOOKS: [&str; 10] = [
    "pnpm:devPreinstall",
    "preinstall",
    "install",
    "postinstall",
    "preprepare",
    "prepare",
    "postprepare",
    "preuninstall",
    "uninstall",
    "postuninstall",
];

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

fn tmpdir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nub-lifecycle-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A project whose every hook appends `<name>:<hook>` to `$LC_LOG`, with
/// `+dep` when `node_modules/dep` is linked at that moment.
fn project(dir: &Path, name: &str, dependencies: serde_json::Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("log.js"),
        "const fs = require('fs');\n\
         const linked = fs.existsSync('node_modules/dep') ? '+dep' : '';\n\
         fs.appendFileSync(process.env.LC_LOG, \
         `${process.env.npm_package_name}:${process.env.npm_lifecycle_event}${linked}\\n`);\n",
    )
    .unwrap();
    let scripts: serde_json::Map<String, serde_json::Value> = HOOKS
        .iter()
        .map(|hook| (hook.to_string(), "node log.js".into()))
        .collect();
    let manifest = serde_json::json!({
        "name": name,
        "version": "1.0.0",
        "private": true,
        "dependencies": dependencies,
        "scripts": scripts,
    });
    std::fs::write(
        dir.join("package.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

/// A dependency-free local package for `file:` specs to point at.
fn local_dep(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"dep","version":"1.0.0"}"#,
    )
    .unwrap();
}

struct Fixture {
    dir: PathBuf,
    log: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = tmpdir(tag);
        Fixture {
            dir: root.join("proj"),
            log: root.join("lc.log"),
            data: root.join("xdg-data"),
            cache: root.join("xdg-cache"),
        }
    }

    /// Run `nub <args>` in `dir` and return the hooks it ran, in order.
    fn hooks(&self, args: &[&str]) -> Vec<String> {
        self.hooks_in(".", args)
    }

    /// `nub <args>` in the subdirectory `sub`, with the fixture's environment.
    fn command(&self, sub: &str, args: &[&str]) -> Command {
        let mut command = Command::new(nub_binary());
        command
            .args(args)
            .current_dir(self.dir.join(sub))
            .env("LC_LOG", &self.log)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CACHE_HOME", &self.cache)
            .env_remove("NODE_COMPAT")
            .env_remove("CI");
        command
    }

    /// [`Self::hooks`] from the subdirectory `sub`.
    fn hooks_in(&self, sub: &str, args: &[&str]) -> Vec<String> {
        std::fs::write(&self.log, "").unwrap();
        let out = self
            .command(sub, args)
            .output()
            .expect("failed to spawn nub");
        assert!(
            out.status.success(),
            "nub {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn names(hooks: &[&str]) -> Vec<String> {
    hooks.iter().map(|hook| hook.to_string()).collect()
}

/// A workspace of `root` and members `a` and `b`, with no dependencies.
fn workspace(fx: &Fixture) {
    project(&fx.dir, "root", serde_json::json!({}));
    std::fs::write(
        fx.dir.join("pnpm-workspace.yaml"),
        "packages:\n  - packages/*\n",
    )
    .unwrap();
}

#[test]
fn install_runs_every_project_stage_after_linking_in_dependency_order() {
    let fx = Fixture::new("ws");
    workspace(&fx);
    // `a` depends on `b`, so `b`'s stages run first, against the
    // directories' sort order.
    project(
        &fx.dir.join("packages/a"),
        "a",
        serde_json::json!({ "b": "workspace:*" }),
    );
    project(&fx.dir.join("packages/b"), "b", serde_json::json!({}));
    let stages = |name: &str| {
        [
            "preinstall",
            "install",
            "postinstall",
            "preprepare",
            "prepare",
            "postprepare",
        ]
        .map(|stage| format!("{name}:{stage}"))
    };
    let mut expected = vec!["root:pnpm:devPreinstall".to_string()];
    expected.extend(stages("root"));
    expected.extend(stages("b"));
    expected.extend(stages("a"));
    assert_eq!(fx.hooks(&["install"]), expected);
}

#[test]
fn install_without_dev_dependencies_runs_only_the_install_stages() {
    let fx = Fixture::new("prod");
    project(&fx.dir, "root", serde_json::json!({}));
    assert_eq!(
        fx.hooks(&["install", "--prod"]),
        names(&["root:preinstall", "root:install", "root:postinstall"])
    );
}

#[test]
fn remove_runs_uninstall_stages_before_and_after_unlinking() {
    let fx = Fixture::new("remove");
    project(&fx.dir, "root", serde_json::json!({ "dep": "file:./dep" }));
    local_dep(&fx.dir.join("dep"));
    fx.hooks(&["install"]);
    assert_eq!(
        fx.hooks(&["remove", "dep"]),
        names(&[
            "root:pnpm:devPreinstall+dep",
            "root:preuninstall+dep",
            "root:uninstall+dep",
            "root:postuninstall",
        ])
    );
}

#[test]
fn update_of_everything_runs_the_project_stages_and_a_named_update_does_not() {
    let fx = Fixture::new("update");
    project(&fx.dir, "root", serde_json::json!({ "dep": "file:./dep" }));
    local_dep(&fx.dir.join("dep"));
    fx.hooks(&["install"]);
    assert_eq!(
        fx.hooks(&["update"]),
        names(&[
            "root:pnpm:devPreinstall+dep",
            "root:preinstall+dep",
            "root:install+dep",
            "root:postinstall+dep",
            "root:preprepare+dep",
            "root:prepare+dep",
            "root:postprepare+dep",
        ])
    );
    assert_eq!(
        fx.hooks(&["update", "dep"]),
        names(&["root:pnpm:devPreinstall+dep"])
    );
}

#[test]
fn update_stops_at_a_failing_root_preinstall_before_the_lockfile_changes() {
    let fx = Fixture::new("update-guard");
    project(&fx.dir, "root", serde_json::json!({ "dep": "file:./dep" }));
    local_dep(&fx.dir.join("dep"));
    fx.hooks(&["install"]);
    // A dependency the lockfile does not have yet, so the update has a
    // change to write, and a guard that fails while FAIL is set.
    let manifest_path = fx.dir.join("package.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["dependencies"]["dep2"] = "file:./dep2".into();
    manifest["scripts"]["preinstall"] = "node -e \"process.exit(process.env.FAIL ? 7 : 0)\"".into();
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    std::fs::create_dir_all(fx.dir.join("dep2")).unwrap();
    std::fs::write(
        fx.dir.join("dep2/package.json"),
        r#"{"name":"dep2","version":"1.0.0"}"#,
    )
    .unwrap();
    let lockfile_names_dep2 = || {
        std::fs::read_dir(&fx.dir).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name.ends_with(".lock") || name.ends_with("-lock.yaml"))
                && std::fs::read_to_string(&path).unwrap().contains("dep2")
        })
    };
    let out = fx
        .command(".", &["update"])
        .env("FAIL", "1")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "the failing guard fails the update"
    );
    assert!(!lockfile_names_dep2(), "the lockfile is untouched");
    fx.hooks(&["update"]);
    assert!(
        lockfile_names_dep2(),
        "without the failure the update writes it"
    );
}

#[test]
fn workspace_add_runs_the_install_stages_of_the_root_and_the_edited_project() {
    let fx = Fixture::new("add");
    workspace(&fx);
    // The root listed among the workspace packages, as `.`.
    std::fs::write(
        fx.dir.join("pnpm-workspace.yaml"),
        "packages:\n  - .\n  - packages/*\n",
    )
    .unwrap();
    project(&fx.dir.join("packages/a"), "a", serde_json::json!({}));
    project(&fx.dir.join("packages/b"), "b", serde_json::json!({}));
    local_dep(&fx.dir.join("dep"));
    fx.hooks(&["install"]);
    // The root, which pnpm pushes into every workspace run, runs its
    // `preinstall` before anything is linked; `b` was not edited.
    assert_eq!(
        fx.hooks_in("packages/a", &["add", "file:../../dep"]),
        names(&[
            "root:pnpm:devPreinstall",
            "root:preinstall",
            "root:install",
            "root:postinstall",
            "a:preinstall+dep",
            "a:install+dep",
            "a:postinstall+dep",
        ])
    );
    assert_eq!(
        fx.hooks(&["add", "-w", "file:./dep"]),
        names(&[
            "root:pnpm:devPreinstall",
            "root:preinstall",
            "root:install+dep",
            "root:postinstall+dep",
        ])
    );
    let dep_spec = format!("file:{}", fx.dir.join("dep").display());
    // Every member edited: with the root they cover the workspace, so pnpm
    // runs the root's hooks alone.
    assert_eq!(
        fx.hooks(&["add", "--filter", "a", "--filter", "b", &dep_spec]),
        names(&[
            "root:pnpm:devPreinstall+dep",
            "root:preinstall+dep",
            "root:install+dep",
            "root:postinstall+dep",
        ])
    );
}
