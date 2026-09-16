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

The standard cache root is `XDG_CACHE_HOME` or `~/.cache`. The cache convenience does not include every tool's storage location. A dot-directory outside that root needs its own grant unless it is a member of the tool-directory set:

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

The array form `{"fs":["./","$tooldirs","$tmp"]}` means read-write. The object form makes access explicit. Private temp accepts `"rw"` or `true`; `false` adds no temporary-storage grant. Neither mode subtracts from explicit filesystem grants: a read grant for `/` still includes host temp. Neither mode grants the entire host temporary directory. The temp convenience rejects read-only access and suffixes such as `$tmp/work`. The tool-directory set also takes no suffix.

Broad home reads can be combined with writes limited to the project and private temporary storage:

```json
{"fs":{"$home":"r","./":"rw","$tmp":"rw"}}
```

A home grant is literal: it includes SSH keys, package-manager credentials and other readable home files. Changing `$home` to `"rw"` also permits writes throughout home. Neither home grants nor tool-directory grants promise secret-free contents.

### Directory listing

The tool-directory bundle permits listing the project's non-root ancestors and the conventional temporary directory. Bun 1.3 needs these listings for installed-bin execution and cache cleanup. These are directory-node grants, not sibling file grants: they add no file-content access, creation or deletion rights. The listing right also permits listing descendant directories.

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

This option requires Landlock ABI 3 or newer, seccomp user notifications and atomic file-descriptor injection. Linux 6.2 introduced the required Landlock ABI; the running kernel must also enable these facilities. Unsupported hosts refuse acquisition. Policies without these grants do not add read-open notifications; opt-in policies route read opens through the supervisor before ordinary paths continue under Landlock.

The notification path has a measurable cost. In the [Linux release control](https://github.com/nubjs/nub/actions/runs/34522013136), 2,000 small-file opens took a median 62.2 ms with the bundle versus 13.9 ms on the preceding engine's directory-only bundle. An empty command took 2.58 ms versus 2.28 ms. Exact-path policies without metadata notifications remained near their preceding-engine timings. These are microbenchmarks, not package-install timings.

## Platform support

The sandbox runs on Linux only. Acquisition on macOS or Windows fails with a typed error naming Linux. The crate still compiles on those hosts, so an embedder can build and test from any machine.

The engine probes the facilities a policy requires at acquisition. Filesystem confinement uses [Landlock](https://docs.kernel.org/userspace-api/landlock.html) ABI 3 or newer, introduced in Linux 6.2; the runtime ABI probe is the gate, not a version string. ABI 3 is what enforces truncation alongside ordinary file writes. Per-host networking additionally uses seccomp user notifications, pidfd access and `SECCOMP_IOCTL_NOTIF_ADDFD`. Self-process metadata uses atomic `SECCOMP_ADDFD_FLAG_SEND` injection. A host that lacks what the policy needs refuses acquisition rather than running with incomplete enforcement.

Ubuntu 24.04 x86-64, kernel `6.17.0-1022-azure`, runs the filesystem, network and package-install contracts. That is runtime coverage, not a distribution, architecture or kernel-version floor. It also does not set the loader or runtime-library floor for a distributed Nub binary; that floor requires inspecting and running the exact artifact on each claimed baseline.

## Tool directories

The set includes package caches, stores, global installations and user-level tool state. Its members cover Nub, npm, pnpm, Yarn, Bun, pip, uv, Cargo/rustup, Go, Gradle, Maven, NuGet, Composer and Git.

| Additional tool requirement | Scope |
| --- | --- |
| Linux process introspection | The eight read-only [self-metadata capabilities](#explicit-linux-process-metadata), for Node, Bun and .NET. No other-process or environment-file access. |
| Unix .NET coordination | `/tmp/.dotnet`, used by named mutexes even with private temp storage. A writable bundle creates this directory at acquisition; a read-only bundle does not. This is shared tool state, not session-owned data, and session cleanup leaves it in place. |

The [member table and environment mapping](src/compiler/builtin_sets.rs) are the implementation reference. The bundle is opt-in; a policy with only explicit paths gains neither capability automatically.

Examples of conventional roots:

| Tool | Linux |
| --- | --- |
| Nub | `~/.cache/nub`, `~/.config/nub`, `~/.local/share/nub/store` |
| npm | `~/.npm` |
| pnpm | `~/.local/share/pnpm`, `~/.cache/pnpm`, `~/.config/pnpm`, `~/.local/state/pnpm` |
| Bun | `~/.bun/install` |
| Yarn | `~/.cache/yarn`, `~/.config/yarn`, `~/.yarn` |
| pip | `~/.cache/pip`, `~/.config/pip`, `~/.pip`, `~/.local/lib` |
| uv | `~/.cache/uv`, `~/.local/share/uv`, `~/.config/uv`, `~/.local/bin` |
| Cargo/rustup | `~/.cargo`, `~/.rustup` |
| Go | `~/go`, `$cache/go-build`, `~/.config/go` |
| Gradle/Maven | `~/.gradle`, `~/.m2`; file `~/.mavenrc` |
| .NET/NuGet | `~/.dotnet`, `~/.nuget`, `~/.local/share/NuGet` |
| Composer | `~/.cache/composer`, `~/.composer`, `~/.config/composer` |
| Git | `~/.config/git`, `~/.cache/git`, `~/.git-credential-cache`; files `~/.gitconfig`, `~/.gitconfig.lock`, `~/.git-credentials` |

Documented environment locations are expanded from the compilation snapshot, including `NPM_CONFIG_CACHE`, `PNPM_HOME`, `YARN_CACHE_FOLDER`, `BUN_INSTALL`, `UV_CACHE_DIR`, `CARGO_HOME`, `GOPATH`, `GRADLE_USER_HOME`, `NUGET_PACKAGES` and `COMPOSER_HOME`. Resolved policy environment values override that snapshot; an approved `vars` command substitution runs once, and its result determines both the child's value and its tool-directory grant. Granting a directory does not automatically inherit its environment variable.

The set also includes tool-specific children of the supplied XDG roots. Expansion itself does not execute tools, inspect PATH, parse their configuration files or scan the disk.

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

Empty values are ignored. Go's `GOPATH` is split into separate roots using the OS path-list separator. Standard XDG variables add the tool-specific children in the member table, not the entire root. For Nub these are `XDG_CACHE_HOME/nub`, `XDG_DATA_HOME/nub/store` and `XDG_CONFIG_HOME/nub`. An override that resolves to a filesystem root is rejected.

For example, a supplied `UV_CACHE_DIR=/work/uv-cache` adds that location to `$tooldirs`. A relocation found only in a tool's configuration file needs an explicit grant instead:

```json
{"fs":{"./":"rw","$tooldirs":"rw","/work/custom-store":"rw","$tmp":"rw"}}
```

The uv defaults use XDG locations. Its executable fallback follows `XDG_DATA_HOME/../bin`; `UV_INSTALL_DIR` and `UV_PROJECT_ENVIRONMENT` add their configured locations too. [uv storage reference](https://docs.astral.sh/uv/reference/storage/).

Operational limits:

- Landlock grants need existing objects. Missing speculative set members are skipped, not created. Initialize the cache root before acquiring a sandbox, or place it under an already writable directory.
- Sessions retain handles to the granted filesystem objects. Renaming a granted directory preserves access to that directory; replacing its old pathname does not grant the replacement. A speculative path absent at acquisition stays ungranted until a new session is acquired. Files created beneath a retained writable directory remain accessible.
- An exact writable file is not a writable parent directory. Git's default global-config update creates an adjacent `.gitconfig.lock` and renames it; granting the existing `.gitconfig` alone is insufficient, because the grant is on the inode. A dedicated, writable Git config directory supports that protocol without granting all of home.
- Removing or replacing a grant root may require write access to its parent. This affects commands such as `cargo clean` and cache deletion. Put disposable output below a writable directory, or explicitly grant its parent; the engine does not synthesize sibling exceptions.
- Linked Git worktrees and relocated common directories may sit outside the project. Supply `GIT_DIR`/`GIT_COMMON_DIR` or explicit grants for those locations.
- Filesystem access does not provide network access or an interpreter's installation files. Embedders supply those capabilities separately.

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

Network entries accept host patterns and CIDRs. Unlike filesystem grants, network rules retain ordered allow/deny matching. A host grant is not an HTTP-method restriction and does not prevent uploads to that host. The boolean `false` denies egress; `true` disables Nub's network filtering.

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

A seccomp notification supervisor redirects TCP connections through the policy proxy, including loopback destinations other than the proxy's own listener. No client proxy configuration is required, and DNS uses the configured resolver. Local services require an explicit matching hostname, IP or CIDR grant. General UDP is denied. Host rules are not an all-channel data-loss boundary.

Coarse `net: true` and `net: false` policies do not start a host-filtering proxy.

Host-filtered sessions replay connected IP sends from bounded snapshots. Batch sends (`sendmmsg`) and non-IP sends return `ENOSYS`; addressed or ancillary-data IP sends return `EPERM`. Zero-copy sends return `EOPNOTSUPP`. Ordinary connected TCP and resolver-bound UDP sends remain available, with a 16 MiB payload limit. Replay suppresses host `SIGPIPE` rather than delivering that signal to the child. Cancellation is checked between bounded waits but cannot be atomic with the final send. These restrictions do not apply to coarse network policies, which do not use this supervisor.

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

### Cache roots and their parents

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

Enforcement is Landlock filesystem restrictions plus seccomp syscall filters, and the general network path adds a notification supervisor. Nothing on the host changes: no ACL or firewall edits, and session temp, pipes and supervisor resources all have owners.

Landlock is a Linux Security Module. Seccomp means secure computing; its BPF (Berkeley Packet Filter) programs filter system calls.

Managed temp storage lives in a private per-user directory under the OS temp root. A file-lock lease distinguishes a live session from an abandoned one. Acquisition and explicit cleanup recover abandoned owned directories; normal close removes them immediately. Cleanup checks the recorded directory identity and does not follow payload symlinks. Legacy temporary directories without ownership records are not deletion targets.

The CLI runs the same recovery operation without loading project configuration. A nonzero exit reports incomplete cleanup; its ownership records remain available for retry:

```sh
nub sandbox cleanup
```

Cleanup does not delete a directory whose recorded object identity is missing or no longer matches. This can happen after an external replacement or a crash between directory creation and its identity being journaled. The entry remains tracked, counts toward the resource bound, and requires the reported path to be inspected rather than repeatedly retrying an unsafe deletion. A later successful cleanup removes the retained record.

The API requires no elevation and no setup command. Owner-loss cleanup uses a private guardian process group, and the engine blocks the group and session escape syscalls that would let a descendant leave it.
