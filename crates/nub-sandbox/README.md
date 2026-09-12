# Sandbox engine

The engine compiles filesystem, network and environment permissions for native child processes. Its raw Rust interface accepts configuration and paths supplied by an embedder; these examples do not define a `nub.jsonc` field or a command-line policy format.

## Filesystem permissions

```json
{
  "fs": {
    "./": "rw",
    "$tooldirs": "rw",
    "$tmp": "rw"
  },
  "net": false
}
```

Directory grants cover descendants. Overlapping positive grants combine: a read-only grant does not remove an existing write grant. Filesystem deny entries such as `!~/.ssh` are rejected; Nub does not translate them into sibling grants.

The filesystem boolean `false` grants no authored paths; `true` requests unrestricted filesystem access. Backend runtime essentials still apply to a confined command. Prefer explicit paths for an allowlist:

```json
{"fs": {"./": "r", "./output": "rw", "$tmp": "rw"}, "net": false}
```

| Convenience | Meaning | Example |
| --- | --- | --- |
| `$home`, `~` | Home root supplied in `CompileCtx`. | `"$home/.config/tool": "r"` |
| `$cache` | One standard OS cache root, supplied in `CompileCtx`. | `"$cache/tool": "rw"` |
| `$tmp` | Managed private storage retained for the resource's lifetime. | `"$tmp": "rw"` |
| `$tooldirs` | Conventional tool directories, environment relocations and bounded process/coordination capabilities. | `"$tooldirs": "rw"` |

The standard cache roots are `XDG_CACHE_HOME` or `~/.cache` on Linux, `~/Library/Caches` on macOS, and `LOCALAPPDATA` on Windows. The cache convenience does not include every tool's storage location. A dot-directory outside that root needs its own grant unless it is a member of the tool-directory set:

```json
{
  "fs": {
    "./": "rw",
    "$cache": "rw",
    "$home/.custom-tool": "rw",
    "$tmp": "rw"
  }
}
```

The array form `{"fs":["./","$tooldirs","$tmp"]}` means read-write. The object form makes access explicit. Private temp accepts `"rw"` or `true`; `false` requests no temp access. It rejects read-only access and suffixes such as `$tmp/work`. The tool-directory set also takes no suffix.

Broad home reads can be combined with writes limited to the project and private temporary storage:

```json
{"fs":{"$home":"r","./":"rw","$tmp":"rw"}}
```

A home grant is literal: it includes SSH keys, package-manager credentials and other readable home files. Changing `$home` to `"rw"` also permits writes throughout home. Neither home grants nor tool-directory grants promise secret-free contents.

### Unix directory listing

The tool-directory bundle permits listing the project's non-root ancestors and the conventional temporary directory. Bun 1.3 needs these listings for installed-bin execution and cache cleanup. These are directory-node grants, not sibling file grants: they add no file-content access, creation or deletion rights. Linux's listing right also permits listing descendant directories; macOS applies it to the named nodes.

Older Bun versions hardcode `/tmp` or `/private/tmp` for `bunx` downloads, even with private `TMPDIR`. Listing does not permit creating or deleting those shared cache entries. The bundle does not silently grant writable host temp; those operations require explicit grants or a runtime version that honors a private cache location.

### Explicit Linux process metadata

The tool-directory bundle includes read-only process metadata on Linux. A policy without `$tooldirs` can select individual capabilities:

```json
{
  "fs": {
    "./": "rw",
    "$tmp": "rw",
    "/proc/self/maps": "r",
    "/proc/self/stat": "r",
    "/proc/self/cmdline": "r",
    "/proc/self/statm": "r",
    "/proc/self/status": "r",
    "/proc/self/task": "r",
    "/proc/self/task/*/stat": "r",
    "/proc/self/task/*/status": "r"
  },
  "net": false
}
```

These permissions follow the requesting process, including child processes and threads. They never refer to the process compiling the policy.

- The first five paths expose that process's memory map, statistics, command line, memory sizes and status.
- The task directory permits thread enumeration. The two task patterns permit only numeric thread IDs belonging to that process, and only the selected `stat` or `status` files.
- All eight permissions are read-only, including when selected through `$tooldirs:rw`.
- Environment files, memory contents, file-descriptor directories and other processes' metadata remain excluded. Command-line access exposes the requesting process's arguments, not its owner's arguments.

This option requires Linux 5.14 or newer with seccomp user notifications and atomic file-descriptor injection. Unsupported hosts refuse acquisition. macOS and Windows reject these Linux-specific permissions. Policies without these grants do not add read-open notifications; opt-in policies route read opens through the supervisor before ordinary paths continue under Landlock.

The notification path has a measurable cost. In the [Linux release control](https://github.com/nubjs/nub/actions/runs/34522013136), 2,000 small-file opens took a median 62.2 ms with the bundle versus 13.9 ms on the preceding engine's directory-only bundle. An empty command took 2.58 ms versus 2.28 ms. Exact-path policies without metadata notifications remained near their preceding-engine timings. These are microbenchmarks, not package-install timings.

## Operating-system support

The engine probes the facilities required by each policy at acquisition.

| Platform | Required facilities | Runtime coverage |
| --- | --- | --- |
| Linux | Filesystem confinement uses [Landlock](https://docs.kernel.org/userspace-api/landlock.html). Available restrictions depend on its ABI, not just its presence. Per-host networking additionally uses seccomp user notifications, pidfd access, and `SECCOMP_IOCTL_NOTIF_ADDFD`. Self-process metadata uses atomic `SECCOMP_ADDFD_FLAG_SEND` injection. | Ubuntu 24.04 x86-64, kernel `6.17.0-1022-azure`, runs the filesystem, network and package-install contracts. An older Linux floor is not established. |
| macOS | Seatbelt through the system `sandbox-exec` interface. | macOS 14.8.9 arm64 passed the macOS readiness contract. |
| Windows | AppContainer and extended process startup, including [`PROC_THREAD_ATTRIBUTE_JOB_LIST`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute#proc_thread_attribute_job_list). | Windows Server 2022 x64 and Windows 11 arm64 run the documented compatibility sequences. Windows Server 2019 x64, build 17763, has standard-user coverage for ordinary filesystem and policy parsing operations only. |

Runtime coverage does not set a general distribution, architecture, or OS-version floor. It also does not set the loader or runtime-library floor for a distributed Nub binary. That floor requires inspection and execution of the exact artifact on each claimed baseline.

Linux capability depends on the running kernel configuration and Landlock ABI. macOS owner-loss cleanup reaps descendants that remain in the command's process group; a descendant that deliberately leaves the group can survive while remaining Seatbelt-confined. Windows limitations for AppContainer ACLs, devices, IPC, and runtime adapters are documented in [the compatibility matrix](COMPATIBILITY.md). A full-disk Windows catalog grant explicitly omits AppContainer. The API has no detached-command operation.

## Tool directories

The set includes package caches, stores, global installations and user-level tool state. Its members cover Nub, npm, pnpm, Yarn, Bun, pip, uv, Cargo/rustup, Go, Gradle, Maven, NuGet, Composer and Git.

| Additional tool requirement | Scope |
| --- | --- |
| Linux process introspection | The eight read-only [self-metadata capabilities](#explicit-linux-process-metadata), for Node, Bun and .NET. No other-process or environment-file access. |
| Unix .NET coordination | `/tmp/.dotnet`, used by named mutexes even with private temp storage. A writable bundle creates this directory at acquisition; a read-only bundle does not. This is shared tool state, not session-owned data, and session cleanup leaves it in place. |

The [member table and environment mapping](src/compiler/builtin_sets.rs) are the implementation reference. The bundle is opt-in; a policy with only explicit paths gains neither capability automatically.

The [versioned compatibility matrix](COMPATIBILITY.md) records tested commands and backend restrictions. A directory member is not a promise that every command works under every backend.

Examples of conventional roots:

| Tool | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Nub | `~/.cache/nub`, `~/.config/nub`, `~/.local/share/nub/store` | Same; also `$cache/nub/pm` for embedder-supplied cache anchors | `~/.cache/nub`, `~/.config/nub`, `~/AppData/Local/nub`, `~/AppData/Roaming/nub` |
| npm | `~/.npm` | `~/.npm` | `~/AppData/Local/npm-cache`, `~/AppData/Roaming/npm` |
| pnpm | `~/.local/share/pnpm`, `~/.cache/pnpm`, `~/.config/pnpm`, `~/.local/state/pnpm` | `~/Library/pnpm`, `~/Library/Caches/pnpm`, `~/Library/Preferences/pnpm`, `~/.local/state/pnpm` | `~/AppData/Local/pnpm`, `~/AppData/Local/pnpm-cache`, `~/AppData/Local/pnpm-state`; legacy `~/.pnpm`, `~/.pnpm-cache`, `~/.pnpm-state`, `~/.config/pnpm` |
| Bun | `~/.bun/install` | `~/.bun/install` | `~/.bun/install` |
| Yarn | `~/.cache/yarn`, `~/.config/yarn`, `~/.yarn` | `~/Library/Caches/Yarn`, `~/.config/yarn`, `~/.yarn` | `~/AppData/Local/Yarn`, `~/AppData/Roaming/Yarn`, `~/.yarn` |
| pip | `~/.cache/pip`, `~/.config/pip`, `~/.pip`, `~/.local/lib` | `~/Library/Caches/pip`, `~/Library/Application Support/pip`, `~/.config/pip`, `~/.pip`, `~/Library/Python` | `~/AppData/Local/pip`, `~/AppData/Roaming/pip`, `~/pip`, `~/AppData/Roaming/Python` |
| uv | `~/.cache/uv`, `~/.local/share/uv`, `~/.config/uv`, `~/.local/bin` | The Linux roots, plus legacy `~/Library/Caches/uv` and `~/Library/Application Support/uv` | `~/AppData/Local/uv`, `~/AppData/Roaming/uv`, `~/.local/bin` |
| Cargo/rustup | `~/.cargo`, `~/.rustup` | `~/.cargo`, `~/.rustup` | `~/.cargo`, `~/.rustup` |
| Go | `~/go`, `$cache/go-build`, `~/.config/go` | `~/go`, `~/Library/Caches/go-build`, `~/Library/Application Support/go` | `~/go`, `~/AppData/Local/go-build`, `~/AppData/Roaming/go` |
| Gradle/Maven | `~/.gradle`, `~/.m2`; file `~/.mavenrc` | Same | `~/.gradle`, `~/.m2`; files `~/mavenrc.cmd`, `~/mavenrc_pre.cmd`, `~/mavenrc_post.cmd` and legacy pre/post `.bat` files |
| .NET/NuGet | `~/.dotnet`, `~/.nuget`, `~/.local/share/NuGet` | The Linux roots | `~/.dotnet`, `~/.nuget`, `~/AppData/Local/NuGet`, `~/AppData/Roaming/NuGet` |
| Composer | `~/.cache/composer`, `~/.composer`, `~/.config/composer` | `~/.composer`, `~/Library/Caches/composer`, `~/Library/Application Support/Composer` | `~/AppData/Local/Composer`, `~/AppData/Roaming/Composer` |
| Git | `~/.config/git`, `~/.cache/git`, `~/.git-credential-cache`; files `~/.gitconfig`, `~/.gitconfig.lock`, `~/.git-credentials` | Same | Same |

Documented environment locations are expanded from the compilation snapshot, including `NPM_CONFIG_CACHE`, `PNPM_HOME`, `YARN_CACHE_FOLDER`, `BUN_INSTALL`, `UV_CACHE_DIR`, `CARGO_HOME`, `GOPATH`, `GRADLE_USER_HOME`, `NUGET_PACKAGES` and `COMPOSER_HOME`. Resolved policy environment values override that snapshot; an approved `vars` command substitution runs once, and its result determines both the child's value and its tool-directory grant. Windows environment names are case-insensitive. Granting a directory does not automatically inherit its environment variable.

The set also includes tool-specific children of supplied XDG and Windows app-data roots. Expansion itself does not execute tools, inspect PATH, parse their configuration files or scan the disk.

```json
{
  "fs": ["./", "$tooldirs", "$tmp"],
  "vars": { "UV_CACHE_DIR": "$(cache-location)" }
}
```

This approved-scope example explicitly invokes `cache-location`. Its output becomes the child's `UV_CACHE_DIR` and a read/write grant. Dependency-controlled configuration cannot request environment command substitution. A filesystem-root result is rejected rather than granting the whole disk.

These values add grants alongside the conventional roots. They do not change the access mode selected for `$tooldirs`, and expanding a variable does not itself pass that variable to the command.

| Tool | Additional path variables |
| --- | --- |
| PM storage | `NPM_CONFIG_CACHE_DIR`, `NPM_CONFIG_STORE_DIR`, `NPM_CONFIG_VIRTUAL_STORE_DIR`, `NPM_CONFIG_GLOBAL_VIRTUAL_STORE_DIR`; their lowercase spellings also work. |
| npm | `NPM_CONFIG_CACHE`, `NPM_CONFIG_PREFIX`, `NPM_CONFIG_USERCONFIG`, `NPM_CONFIG_GLOBALCONFIG`; their lowercase spellings also work. |
| pnpm | `PNPM_HOME`, `PNPM_CONFIG_STORE_DIR`, `PNPM_CONFIG_CACHE_DIR`, `PNPM_CONFIG_STATE_DIR`, `PNPM_CONFIG_CONFIG_DIR` |
| Yarn | `YARN_CACHE_FOLDER`, `YARN_GLOBAL_FOLDER` |
| Bun | `BUN_INSTALL`, `BUN_INSTALL_CACHE_DIR`, `BUN_INSTALL_GLOBAL_DIR`, `BUN_INSTALL_BIN` |
| pip/Python | `PIP_CACHE_DIR`, `PIP_CONFIG_FILE`, `PYTHONUSERBASE` |
| uv | `UV_CACHE_DIR`, `UV_TOOL_DIR`, `UV_TOOL_BIN_DIR`, `UV_PYTHON_INSTALL_DIR`, `UV_PYTHON_BIN_DIR`, `UV_INSTALL_DIR`, `UV_PROJECT_ENVIRONMENT` |
| Cargo/rustup | `CARGO_HOME`, `RUSTUP_HOME`, `CARGO_TARGET_DIR` |
| Go | `GOPATH`, `GOMODCACHE`, `GOCACHE`, `GOBIN`, `GOTMPDIR`, `GOENV` (except `off`) |
| Gradle | `GRADLE_USER_HOME` |
| .NET/NuGet | `NUGET_PACKAGES`, `NUGET_HTTP_CACHE_PATH`, `NUGET_SCRATCH`, `NUGET_PLUGINS_CACHE_PATH`, `DOTNET_CLI_HOME`, `DOTNET_BUNDLE_EXTRACT_BASE_DIR` |
| Composer | `COMPOSER_HOME`, `COMPOSER_CACHE_DIR`, `COMPOSER_VENDOR_DIR`, `COMPOSER_BIN_DIR` |
| Git | `GIT_DIR`, `GIT_COMMON_DIR`, `GIT_CONFIG_GLOBAL`, `GIT_TEMPLATE_DIR`, `GIT_EXEC_PATH` |

Empty values are ignored. Go's `GOPATH` is split into separate roots using the OS path-list separator. Standard XDG and Windows app-data variables add the tool-specific children in the member table, not the entire app-data directory. For Nub these are `XDG_CACHE_HOME/nub`, `XDG_DATA_HOME/nub/store` and `XDG_CONFIG_HOME/nub`, plus the Windows app-data `nub` children. An override that resolves to a filesystem root is rejected.

For example, a supplied `UV_CACHE_DIR=/work/uv-cache` adds that location to `$tooldirs`. A relocation found only in a tool's configuration file needs an explicit grant instead:

```json
{"fs":{"./":"rw","$tooldirs":"rw","/work/custom-store":"rw","$tmp":"rw"}}
```

The uv defaults use XDG locations on both Linux and macOS. Its executable fallback follows `XDG_DATA_HOME/../bin`; `UV_INSTALL_DIR` and `UV_PROJECT_ENVIRONMENT` add their configured locations too. [uv storage reference](https://docs.astral.sh/uv/reference/storage/).

Operational limits:

- Linux Landlock and Windows ACL grants need existing objects. Missing speculative set members are skipped, not created. Initialize the cache root before acquiring a sandbox, or place it under an already writable directory. macOS path rules can admit later creation.
- Linux sessions retain handles to the granted filesystem objects. Renaming a granted directory preserves access to that directory; replacing its old pathname does not grant the replacement. A speculative path absent at acquisition stays ungranted until a new session is acquired. Files created beneath a retained writable directory remain accessible.
- An exact writable file is not a writable parent directory. Git's default global-config update creates an adjacent `.gitconfig.lock` and renames it; granting the existing `.gitconfig` alone is insufficient on inode-based backends. A dedicated, writable Git config directory supports that protocol without granting all of home.
- Removing or replacing a grant root may require write access to its parent. This affects commands such as `cargo clean` and cache deletion. Put disposable output below a writable directory, or explicitly grant its parent; the engine does not synthesize sibling exceptions.
- Linked Git worktrees and relocated common directories may sit outside the project. Supply `GIT_DIR`/`GIT_COMMON_DIR` or explicit grants for those locations.
- Filesystem access does not provide network access, an interpreter's installation files, macOS Keychain access or Windows Credential Manager access. Embedders supply those capabilities separately.

## Network and environment permissions

Network filtering is independent of filesystem access:

```json
{
  "fs": {"./": "rw", "$tooldirs": "rw", "$tmp": "rw"},
  "net": ["registry.npmjs.org", "*.example.com", "!admin.example.com"],
  "vars": {"PATH": true, "HOME": true, "CI?": true},
  "secrets": {"API_TOKEN?": true}
}
```

Network entries accept host patterns and CIDRs. Unlike filesystem grants, network rules retain ordered allow/deny matching. A host grant is not an HTTP-method restriction and does not prevent uploads to that host. The boolean `false` denies egress; `true` disables Nub's network filtering. Windows AppContainer capabilities still constrain networking even without a Nub host filter.

The proxy checks the CONNECT/SOCKS destination and the visible TLS server name (SNI). Without TLS termination, it cannot inspect encrypted HTTP host headers or an Encrypted ClientHello's inner name. An allowed service can relay traffic elsewhere. Host filtering restricts connections; it is not a guarantee about every application-level destination or the data sent to an allowed host.

### Network rules

A network rule is a literal host, a CIDR, `*`, or `*.suffix`. The suffix wildcard excludes its apex, and the last matching entry wins.

```jsonc
{
  "net": [
    "registry.npmjs.org",
    "198.51.100.0/24",
    "*.packages.example.com",
    "!admin.packages.example.com",
    "<private>"
  ]
}
```

The `<private>` token, also spelled `<local>`, permits RFC 1918 and IPv6 ULA addresses. A bare `*` does not permit those ranges. `$trusted` and `$downloads` are array-only built-in host sets. A fine-grained allow starts the proxy; `net: true` and `net: false` do not. The object form accepts per-host booleans, but `proxy` is compiler-derived rather than a policy key.

The proxy also checks resolved IP addresses after the hostname rule. These destination checks apply to both literal addresses and DNS results:

| Destination | Proxy behavior |
| --- | --- |
| RFC 1918 or IPv6 ULA | Requires the `<private>` opt-in, even when a literal host or CIDR rule matches. |
| IPv4 or IPv6 link-local, including cloud metadata addresses | Always blocked. The AWS IPv6 metadata endpoint `fd00:ec2::254` is also blocked. |
| Loopback | Not part of `<private>`. A matching IP, CIDR, or requested hostname can permit it; an allowed hostname may resolve to loopback. |

For example, a hostname on a private network requires both grants:

```jsonc
{ "net": ["packages.corp.example", "<private>"] }
```

The private token also admits direct connections to private addresses. It is not scoped only to the accompanying hostname. Host filtering is not a blanket prohibition on local services: allowed hostnames can resolve to loopback, and permitted endpoints can relay traffic.

| OS | Host-filtered networking | Limits |
| --- | --- | --- |
| Linux | A seccomp notification supervisor redirects TCP connections through the policy proxy, including loopback destinations other than the proxy's own listener. | No client proxy configuration is required. DNS uses the configured resolver. Local services require an explicit matching hostname, IP or CIDR grant. General UDP is denied. Host rules are not an all-channel data-loss boundary. |
| macOS | Seatbelt allows the proxy's loopback port; the proxy checks destinations. | Clients need HTTP CONNECT or SOCKS proxy support. Bypassing the proxy does not grant direct external access. |
| Windows | A same-AppContainer helper provides the proxy; the command has no direct Internet capability. | Clients need proxy support. The helper rejects TLS-inspection and credential-broker policies. No administrator loopback exemption is installed. |

Coarse `net: true` and `net: false` policies do not start a host-filtering proxy. The build jail uses coarse catalog network permissions rather than enforcing its recorded observed-host lists. On Windows, the coarse allow grants public outbound networking, not unrestricted host/LAN/loopback access.

Linux host-filtered sessions replay connected IP sends from bounded snapshots. Batch sends (`sendmmsg`) and non-IP sends return `ENOSYS`; addressed or ancillary-data IP sends return `EPERM`. Zero-copy sends return `EOPNOTSUPP`. Ordinary connected TCP and resolver-bound UDP sends remain available, with a 16 MiB payload limit. Replay suppresses host `SIGPIPE` rather than delivering that signal to the child. Cancellation is checked between bounded waits but cannot be atomic with the final send. These restrictions do not apply to coarse network policies, which do not use this supervisor.

### Environment rules

The supplied ambient map is the source for environment policy. Array entries select optional keys; object entries require exact keys unless `?` or `optional: true` marks them optional.

```jsonc
{
  "net": ["registry.example.com"],
  "vars": {
    "PORT": "port",
    "MODE": "enum:development|production",
    "CACHE_DIR": "$(cache-location)"
  },
  "secrets": {
    "REGISTRY_TOKEN": {
      "format": "/[A-Za-z0-9_-]+/",
      "brokerTo": ["registry.example.com"]
    }
  }
}
```

This example requires `PORT`, `MODE`, and `REGISTRY_TOKEN` in the supplied ambient map. `cache-location` runs once only in an approved source; a dependency source cannot use a dynamic environment value or `brokerTo`. A brokered secret must be a required exact name and name an allowed literal DNS host in a fine-grained network policy. Type strings are `string`, `integer`, `number`, `port`, `/regex/`, and `enum:a|b`. `vars: true` and `vars: "*"` select all supplied variables, while secrets always name keys explicitly. The object form's options are `format`, `optional`, and `brokerTo`.

The environment example inherits named values from the supplied snapshot. A trailing `?` makes a missing value optional. Ordinary secret grants supply sensitive data to the child; an allowed child can use it. A brokered secret instead stays out of the child environment and is injected by the proxy for its approved host. Unlisted environment values are not implicitly inherited by this explicit policy.

Filtering the environment does not hide files granted through `fs`. Linux excludes procfs by default. The [explicit self-metadata permissions](#explicit-linux-process-metadata) grant only the requesting process's selected files; other processes' metadata and environment files remain excluded.

## Resource and command ownership

Compile once, acquire resources, prepare commands and release the session:

```rust,ignore
let policy = nub_sandbox::compile(&permissions, &context)?;
let sandbox = nub_sandbox::Sandbox::acquire(&policy)?;
let command = nub_sandbox::CommandSpec::new(program).args(arguments).cwd(project);
let prepared = sandbox.prepare(command)?;
// Surface prepared.degradation before treating this as confined execution.
let output = prepared.output()?;
let next = sandbox.prepare(next_command)?.spawn()?;
drop(next); // Stops and reaps this command tree.
sandbox.close();
nub_sandbox::cleanup()?;
```

### Explicit Windows native compatibility

Windows embedders can select the native adapter when creating a session. Ordinary acquisition remains unchanged:

```rust,ignore
let sandbox = Sandbox::with_windows_native_compat(&policy)?;
let command = sandbox.prepare(
    CommandSpec::new("cargo").args(["build", "--offline"]).cwd(project),
)?;
// Run and collect the prepared command through the usual execution API.
sandbox.close();
```

The adapter preserves the AppContainer identity and filesystem/network enforcement. It supplies a parent-opened null-device handle, resolves permitted file handles through captured drive aliases, and places supported native pipes and MSYS/Cygwin coordination objects in the package's private namespace. MSYS and Cygwin provide Unix-like process and file interfaces on Windows. When those runtimes replace their own process/default-object ACLs, the adapter preserves their entries and adds the current package identity. Interpreter installations, projects and tool state still need explicit grants.

- The launcher embeds x64 and ARM64 compatibility DLLs (dynamic-link libraries) and injects the matching DLL before resuming each owned command. Microsoft Detours redirects the required Windows API calls inside that process. Ordinary `CreateProcessW` descendants receive the adapter too. Unsupported executable architectures and injection failures return errors; they do not launch an unconfined replacement.
- Adapter DLLs live under the protected Windows resource registry. The command can read/execute its own DLLs but cannot replace them or read the registry. Equivalent policies with identical adapter bytes share the identity and retained assets. Raw sessions and different adapter versions do not share that identity.
- Synchronous native volume queries can translate a captured local device name to a drive letter. The adapter does not open the Mount Manager device or expose volume enumeration, volume-GUID paths or network volumes. This bounded query interface supports ordinary close, not handle duplication; closing the process releases any remaining query objects.
- Existing directory opens can fall back to listing, traversal and attribute reads when the runtime also requests unavailable ACL or extended-attribute reads. The resulting handle does not acquire those extra rights. The filesystem grants remain unchanged, and unrelated file reads and writes remain denied.
- Closing a session releases its lease. Bounded idle retention and explicit cleanup own the DLL directory, profile and recorded ACL entries together. No compiler, elevation or installer runs when a user creates a sandbox; building Nub itself requires both Microsoft Visual C++ (MSVC) toolsets.
- Python's protected-directory adapter remains separate. The [compatibility matrix](COMPATIBILITY.md) distinguishes raw runs from explicit adaptation and records actual operation sequences.

### Python private directories on Windows

Python versions affected by [CPython #134587](https://github.com/python/cpython/issues/134587) create private directories with an ACL that excludes their own AppContainer. Broader ancestor grants cannot fix the non-inheriting child ACL.

The engine supplies an explicit startup adapter:

```rust,ignore
// Add to an embedder-owned sitecustomize.py on the child's PYTHONPATH.
std::fs::write(startup.join("sitecustomize.py"), nub_sandbox::windows_python_compat_source())?;
```

The adapter retains Python's protected owner/admin/system permissions and adds only the current package SID. It changes `os.mkdir(..., 0o700)` inside AppContainer; other modes and ordinary Python processes are unchanged.

It also repairs CPython's non-strict current-directory `realpath` fallback when AppContainer returns a path ending in `\\.`. Empty and equivalent dot paths become canonical directory paths; drive roots keep their separator. Other input paths and strict calls retain CPython's behavior. This prevents GYP from adding an extra parent component to native dependency include paths.

- The embedder owns the startup directory, grants it read access and composes its contents with any existing startup hooks.
- Python's isolated mode, `-S`, or a replaced `PYTHONPATH` can prevent this hook from loading.
- The adapter grants no additional filesystem paths. It cannot repair native subprocesses' `NUL` device or named-pipe access.
- The sandbox's OS enforcement remains in force whether or not the adapter loads.

The Windows build jail supplies this startup file when it resolves Python for a dependency build. It removes inherited `PYTHONPATH` values and grants read access to a content-addressed file in the shared package-manager cache. That file follows the shared cache's lifetime, not an individual sandbox session's cleanup.

### Explicit Windows Node compatibility

Raw execution does not detect runtimes or inject compatibility code. Windows embedders can opt into the Node stdio and realpath adapters before acquiring a session:

```rust,ignore
let mut policy = nub_sandbox::compile(&permissions, &context)?;
#[cfg(windows)]
policy.env.constructed.insert(
    "NODE_OPTIONS".into(),
    nub_sandbox::windows_node_compat_options(&[
        project.clone(),
        node_installation.clone(),
        package_manager_installation.clone(),
        tool_cache.clone(),
    ]),
);
let sandbox = nub_sandbox::Sandbox::acquire(&policy)?;
```

The paths must already be granted; the helper adds no permissions. It replaces, rather than appends to, an ambient `NODE_OPTIONS` value. It requires Node 18.18+ or 19+ and includes no package-specific network gate. OS network restrictions still apply.

The adapters change runtime behavior: asynchronous subprocess streams use AppContainer-local pipes, synchronous captured output is file-backed, and advanced IPC serialization and handle passing are rejected. Realpath traversal tolerates inaccessible ancestors of the supplied roots; dependency symlinks still resolve. The main entry uses `--preserve-symlinks-main`, so supply its resolved path. These adapters do not repair native programs' device opens or protected directory ACLs. The [compatibility matrix](COMPATIBILITY.md#explicit-windows-node-adapters) records the tested sequences.

Deleting and recreating a cache root requires write access to its parent. For a cache at `$cache/agent-tools/yarn`, grant a dedicated parent explicitly:

```json
{
  "fs": {"./":"rw", "$tooldirs":"rw", "$cache/agent-tools":"rw", "$tmp":"rw"},
  "vars": {"YARN_CACHE_FOLDER":true},
  "net": false
}
```

The embedder supplies `YARN_CACHE_FOLDER` pointing to that cache. Neither an environment relocation nor `$tooldirs` silently grants the relocated path's parent.

### Lifetime and cleanup

Each prepared/running command retains its resource lease. Closing the session releases that caller's handle; it does not invalidate commands already prepared through it. A running command owns its streams, cancellation and exit status. There is no detach or reconnect operation.

| OS | Enforcement | Persistent changes |
| --- | --- | --- |
| Linux | Landlock filesystem restrictions and seccomp syscall filters; the general network path uses a notification supervisor. | No host ACL/firewall changes. Session temp, pipes and supervisor resources have owners. |
| macOS | Seatbelt profiles applied through `sandbox-exec`. | No host ACL/firewall changes. Session temp and proxy resources have owners. |
| Windows | AppContainer restricted identities, filesystem ACL grants and a kill-on-close Job for each command tree. | Profile storage, owned ACEs and a persistent recovery journal. |

Landlock is a Linux Security Module. Seccomp means secure computing; its BPF (Berkeley Packet Filter) programs filter system calls. An ACL is an Access Control List; an ACE is one entry in it. Windows identifies an AppContainer with a SID, or Security Identifier. A Job Object controls process lifetime independently of file permissions.

Windows automatically reuses equivalent resolved resource policies, including runtime grants and backend version. Different external paths produce different identities; managed temp is an identity-owned slot rather than a fresh hash input. Active leases are never evicted. The idle cache is bounded by 64 entries, 24 hours and 1 GiB of owned private data; caller project outputs and shared tool caches are not deletion targets. Explicit cleanup reports failures and retains their ownership records for recovery.

The Windows ownership journal upgrades older supported records without deleting their leases or grants. After the first upgraded write, older binaries refuse the journal rather than discard recovery information they do not understand. Use the newer binary for subsequent sandbox commands and cleanup; do not delete the journal to bypass a version error.

Matching Windows policies share a security identity and private storage. Separate session handles are not isolation boundaries between mutually distrusting callers. Command environment values do not create a separate identity unless they change resolved resources. Environment filtering controls what each command inherits; it does not promise confidentiality from other commands sharing that identity. Per-command Jobs control lifetime, not isolation between commands with the same identity.

On Unix, managed temp storage lives in a private per-user directory under the OS temp root. A file-lock lease distinguishes a live session from an abandoned one. Acquisition and explicit cleanup recover abandoned owned directories; normal close removes them immediately. Cleanup checks the recorded directory identity and does not follow payload symlinks. Legacy temporary directories without ownership records are not deletion targets.

The CLI runs the same recovery operation without loading project configuration. A nonzero exit reports incomplete cleanup; its ownership records remain available for retry:

```sh
nub sandbox cleanup
```

Cleanup does not delete a directory whose recorded object identity is missing or no longer matches. This can happen after an external replacement or a crash between directory creation and its identity being journaled. The entry remains tracked, counts toward the resource bound, and requires the reported path to be inspected rather than repeatedly retrying an unsafe deletion. A later successful cleanup removes the retained record.

The Windows build jail also publishes read access to Nub-owned public package caches. Those cache permissions are intentional shared storage metadata, not a particular session's grants; sandbox cleanup does not revoke them.

The API requires no elevation or setup command. Windows' full-disk build-jail compatibility path is deliberately unconfined and reports that loss; it still uses an owned Job. On Unix, owner-loss cleanup uses a private guardian process group. Linux additionally blocks group/session escape syscalls. macOS does not have a verified equivalent restriction: a process that deliberately leaves the group can survive owner loss, although it remains confined. Do not treat ordinary descendant tests as proof against deliberate detachment.
