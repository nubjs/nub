# Embedding in Rust

Add aube without its binary-only default features:

```toml
[dependencies]
aube = { version = "1", default-features = false }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

The embedding application owns its Tokio runtime and global allocator. aube
does not install an allocator when used as a library.

## Initialize the host

Define a process-lifetime host profile and register it before starting any aube
operation:

```rust
use aube::embed::{self, Host};

static HOST: Host = Host {
    name: "mytool",
    display_name: "My Tool",
    vendor: None,
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("mytool/", env!("CARGO_PKG_VERSION")),
    self_names: &["mytool"],
    compatible_names: &["pnpm"],
    lockfile_basename: "mytool-lock.yaml",
    workspace_yaml: None,
    manifest_namespace: "mytool",
    env_prefix: None,
    config_env_prefix: None,
    cache_namespace: "mytool",
    data_namespace: "mytool",
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
};

embed::initialize(
    &HOST,
    vec![("nodeLinker".to_owned(), "hoisted".to_owned())],
);
```

Initialization is process-global and first-write-wins. Setting defaults have
the lowest precedence, so users can still override them through normal aube
configuration sources. `Host` fields, by contrast, are decisions of the
embedding application and are not user-configurable.

## Install a project

Always select the project directory explicitly. `InstallControl::silent()` is
appropriate when the host does not want aube to write directly to the
terminal.

```rust
use aube::embed::{self, InstallControl, InstallOptions};

let mut options = InstallOptions::new(&project_dir);
options.ignore_scripts = true;
options.control = InstallControl::silent();

embed::install(options).await?;
```

Lifecycle scripts remain subject to aube's build policy. Set `ignore_scripts`
when the host requires scripts to be disabled regardless of project policy.

## Add packages

`add` holds the workspace project lock across both the `package.json` mutation
and installation:

```rust
use aube::embed::{self, AddToProjectOptions, InstallControl};

let packages = vec!["typescript@latest".to_owned()];
embed::add(
    &project_dir,
    &packages,
    AddToProjectOptions {
        save_dev: true,
        save_exact: true,
        ignore_scripts: true,
        control: InstallControl::silent(),
        ..Default::default()
    },
).await?;
```

Set `save_dev`, `save_optional`, or `save_peer` to select the manifest section.
Combining `save_peer` with `save_dev` writes both sections, matching the CLI.
Both install option types also expose `osv_transitive_check` when a host wants
to force a live transitive OSV check for an otherwise unchanged lockfile.
Offline mode always disables that live request. Set
`dangerously_allow_all_builds` to bypass the lifecycle build allowlist for the
invocation; `ignore_scripts` still disables scripts entirely.

## Progress and cancellation

Implement `InstallReporter` and pass it through `InstallControl::events`.
`report` must be non-blocking; enqueue the event when crossing into another
runtime or thread.

```rust
use aube::embed::{InstallControl, InstallEvent, InstallReporter};
use std::sync::Arc;

struct Reporter;

impl InstallReporter for Reporter {
    fn report(&self, event: InstallEvent) {
        // Enqueue the event for the host.
        let _ = event;
    }
}

let control = InstallControl::events(Arc::new(Reporter));
let cancellation_handle = control.clone();
options.control = control;

// Call this from the host's abort handler.
cancellation_handle.cancel();
```

Each invocation has independent event and cancellation state. Installs for
unrelated projects can run concurrently; operations within the same workspace
wait on its project lock. Cancelling `add` restores the project manifest and
lockfile snapshots before returning the cancellation error. Other installation
failures preserve the manifest change so the host can retry `install` without
repeating the add operation.

## Errors

Embedding operations return `miette::Result`. Extract the stable code without
parsing the rendered message:

```rust
if let Err(error) = embed::install(options).await {
    let code = embed::error_code(&error);
    let message = error.to_string();
    let diagnostic = format!("{error:?}");
}
```

Code identifiers and meanings are stable once published. Human-readable
messages and rendered diagnostics may evolve.
