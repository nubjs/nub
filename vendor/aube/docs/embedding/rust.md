# Embedding in Rust

## Migrating to aube 2

`aube_resolver::ResolutionMode` now includes `LowestDirect` and is
`#[non_exhaustive]`. Downstream matches must include a wildcard arm so future
resolution modes can be added without another source-breaking enum change.

Add aube without its binary-only default features:

```toml
[dependencies]
aube = { version = "2", default-features = false }
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

## Node runtime

By default aube resolves its own Node for lifecycle scripts. A host that
manages Node itself describes how Node should be invoked with an
`EmbedderRuntime`.

A host that merely selects a toolchain (a version manager) uses `selector` —
the bin directory goes on `PATH` after project-local `.bin` directories, and
its `node` is both `NODE` and `npm_node_execpath`:

```rust
use aube::embed::EmbedderRuntime;

let runtime = EmbedderRuntime::selector("/opt/mytool/node-24.4.1/bin");
```

A host that *wraps* Node — an instrumenting runtime, a transpiling loader, a
sandbox — uses `wrapper`. The shim is `NODE` and leads `PATH` so a script's
`node` / `$NODE` stays wrapped even when a dependency provides a `node` bin,
while `real_node` is exported as
`npm_node_execpath` so `node-gyp` builds against the real binary. Use
`env_append` to add a `NODE_OPTIONS` preload without dropping the user's value
(`env_set` overwrites):

```rust
let runtime = EmbedderRuntime::wrapper("/opt/mytool/shim/node")
    .real_node("/opt/mytool/node-24.4.1/bin/node")
    .internal_node("/opt/mytool/node-24.4.1/bin/node") // aube's own spawns skip the wrapper
    .version("24.4.1") // supplied, not probed
    .env_append("NODE_OPTIONS", "--import /opt/mytool/preload.mjs");
```

Use `.without_path()` when the host should set `NODE` and use the wrapper for
direct aube spawns but intentionally leave bare `node` resolution to the
inherited `PATH`. Calling `.path_dir(...)` later restores an explicit PATH
entry; the last path builder call wins.

`internal_node` splits off the node aube's *internal* machinery spawns —
pnpmfile hooks, the security scanner, version probes — so those hot paths run
on the real binary while user-facing spawns (scripts, `NODE`, `aube node`)
stay wrapped. It defaults to the wrapper program.

Apply it per call, or register it once so every spawn path — install lifecycle
scripts, `dlx`, `exec`, `run`, `node` — is covered:

```rust
// Per call.
let mut options = InstallOptions::new(&project_dir);
options.runtime = Some(runtime.clone());
embed::install(options).await?;

// Or once at startup (first-write-wins; a per-call `runtime` still wins for
// that install).
embed::set_embedder_runtime(runtime);
```

Every field is optional and unset reproduces standalone aube's behavior. The
runtime is honored regardless of the host's `runtime_switching` flag — an
explicit invocation is an override, not a resolution.

## Run scripts, binaries, and Node

With a runtime registered, a host can drive the run-class commands in process.
Each resolves its project from the directory passed in — never the process
working directory — and returns the child's exit code:

```rust
embed::run(&project_dir, "build", vec![], None).await?;          // package script
embed::exec(&project_dir, "eslint", vec![".".into()], None).await?; // node_modules/.bin
embed::dlx(&project_dir, vec!["cowsay".into(), "hi".into()], vec![], None).await?;
embed::node(&project_dir, vec!["--eval".into(), "console.log(1)".into()], None).await?;
```

The trailing parameter is an optional per-call `EmbedderRuntime`. `None` uses
the process-wide registration; pass `Some(runtime)` when the runtime varies
per invocation (e.g. a host that provisions a fresh shim directory per
command), since `set_embedder_runtime` is set-once. Unlike the CLI,
`embed::node` supervises a child rather than replacing the host process.

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
