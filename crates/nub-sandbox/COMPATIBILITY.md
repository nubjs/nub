# Tool compatibility

The filesystem convenience set does not make every runtime compatible with every OS sandbox. This matrix records complete tested operation sequences, not a guarantee for every command a tool supports.

## Test conditions

The fixtures compare unconfined, explicit-path and tool-directory policies. A tool-directory policy starts with:

```json
{"fs":{"./":"rw","$tooldirs":"rw","$tmp":"rw"}}
```

The fixtures also grant the tested interpreter's installation files and supply its environment and network requirements. Cache roots exist before acquisition. Neither the entire home directory nor the entire filesystem is granted. The exact fixtures are [JavaScript package managers](tests/tool_functionality.rs), [Python](tests/python_tool_functionality.rs), [native toolchains](tests/native_tool_functionality.rs) and [Git](tests/git_tool_functionality.rs).

## Versioned results

“Pass” means the documented operation sequence and its unconfined control passed. “Adapter” requires the explicit runtime setup described below; raw execution remains unchanged. Writable cache parents are required where a tool removes and recreates its cache root.

The [initial version matrix](https://github.com/nubjs/nub/actions/runs/34348165396), [Linux/macOS tool-bundle run](https://github.com/nubjs/nub/actions/runs/34521317570), [Windows Node adapters](https://github.com/nubjs/nub/actions/runs/34400945240), [Windows Python adapter](https://github.com/nubjs/nub/actions/runs/34523672914) and [Windows 11 native tools](https://github.com/nubjs/nub/actions/runs/34405367987) provide the results below. An individual passing sequence does not make every job in its source run green.

| Tool version | Linux x86-64 | macOS arm64 | Server 2022 x86-64 | Windows 11 arm64 | Tested sequence or limit |
| --- | --- | --- | --- | --- | --- |
| npm 11.6.2 | Pass | Pass | Pass | Pass | Install, reinstall, installed bin, user-global operations and cache maintenance. |
| pnpm 9.15.9 / 10.18.3 / 11.26.0 | Pass | Pass | Adapter | Adapter | Full retained sequence with Node stdio/path helpers. Raw commands fail on subprocess pipes or path canonicalization. |
| Yarn 1.22.22 | Pass | Pass | Adapter | Adapter | Install, bin, global install, cache cleanup and reinstall. Linux uses own-process metadata; Windows uses the Node helpers. |
| Yarn 2.4.2 / 3.8.7 / 4.17.0 | Pass | Pass | Pass | Pass | Install, reinstall and execution through the configured store. |
| Bun 1.3.2 | Partial | Partial | Partial | Partial | Unix retained install/bin/global/reinstall passes with bundle directory listings; pruning populated host `bunx` caches requires additional writes. Windows archive/global-bin/cache-cleanup/reinstall passes with the native adapter; local-folder global installs require unsupported symlink creation. |
| Bun 1.4.0 | Pass | Pass | Partial | Partial | Install, reinstall and installed-bin execution pass. Windows global archive install/cache-cleanup/reinstall passes; local-folder global installs require unsupported symlink creation. This version honors private `TMPDIR`. |
| pip 26.2.1 | Pass | Pass | Adapter | Adapter | Local-wheel install, reinstall, import, user install and cache cleanup; Python 3.13.15 startup adapter preserves the package SID on private directories. |
| uv 0.12.11 | Pass | Pass | Adapter | Adapter | Install, import, package and installed tool execution with native and Python private-directory adapters. Raw native subprocess/trampoline operations fail. |
| Cargo 1.91.1 | Pass | Pass | Adapter | Pass | Build and clean with project-local target. Server requires native compatibility. |
| rustup 1.29.0 / 1.29.1 | Pass | Pass | Pass | Pass | Installed-toolchain and home queries, not new toolchain installation. |
| Go 1.25.1 | Pass | Pass | Adapter | Pass | User config, build, install and cache cleanup. Server requires native compatibility. |
| Gradle 8.14 | Pass | Pass | Qualified | Qualified | Offline task twice and daemon cleanup pass after acknowledging the specific full-networking limitation. |
| Maven 3.9.11 | Pass | Pass | Pass | Pass | Offline validation and clean; both execute the user startup file. |
| .NET SDK 10.0.100 / NuGet | Pass | Pass | Pass | Pass | Restore, build, cache cleanup and restore. Unix bundle includes process metadata/shared coordination; cache replacement uses a stable writable parent. |
| Composer 2.8.12 | Pass | Pass | Adapter | Pass | Cold/warm install without plugins or scripts, then cache cleanup. Server requires native compatibility. |

The JavaScript fixtures use Node 22.18.0. Windows 11 runs Cargo, Go and the JVM through x64 emulation; .NET uses ARM64. Bun 1.3.2 also uses x64 emulation there. These are recorded versions, not minimum supported versions. Distinct pnpm and Yarn versions exercise their storage layouts; adding a directory member does not establish compatibility with untested runtimes.

## Explicit Windows native adapter

The [embedded-adapter run](https://github.com/nubjs/nub/actions/runs/34551785167), at `10c9e74c2c`, passes the complete Cargo, Go, Composer, uv, pip and Git sequences on Windows Server 2022 x64 and Windows 11 ARM64. Each has an unconfined control, a raw sandbox control, and a withheld-file canary. The Python sequences also use the Python private-directory helper. Cargo, Go, Composer and Python use x64 toolchains; Git and descendants may have another supported architecture.

Select this adapter through [`Sandbox::with_windows_native_compat`](README.md#explicit-windows-native-compatibility), not through another filesystem sentinel. Granting more cache paths does not repair native device opens, pipe namespaces or drive-alias queries. The same run checks nested execution, anonymous pipe byte transfer, runtime-owned process permissions, DLL write denial and protected-registry read denial. Git LFS still fails its pre-push hook, so this is not an all-tools pass.

Bun 1.3.2 archive installs and global executable launches also pass with the native adapter on both Windows hosts. The [volume-query run](https://github.com/nubjs/nub/actions/runs/34647381373), at `6dfe89c1ee`, verifies cache cleanup and reinstall, real native volume queries, directory enumeration and balanced handle counts after repeated opens and closes. Raw volume queries remain denied. The adapter answers a bounded local-drive query without granting access to the Mount Manager device; file and process canaries remain denied. Local-folder symbolic links and MSYS fork failures still prevent the full tool matrix from passing.

## Explicit Windows Node adapters

The [explicit-adapter run](https://github.com/nubjs/nub/actions/runs/34400945240), at `585cb0ee26`, tests `windows_node_compat_options` with Node 22.18.0 on Server 2022 x86-64 and Windows 11 arm64. Both hosts pass 14 cases, including four unconfined controls and two intentional root-only cache-cleanup denials.

| Tool | Explicit-grant and tool-directory results |
| --- | --- |
| pnpm 9.15.9 / 10.18.3 / 11.26.0 | Local install, retained reinstall, installed-bin execution, global install, cache prune and another reinstall pass in one session. Denied-file canaries and explicit cleanup pass. |
| Yarn 1.22.22 | Install, reinstall, bin execution and global install pass. Root-only grants correctly refuse cache-root recreation. With an explicit dedicated writable cache parent, cleanup and another reinstall pass in the same session. Denied-file canaries and explicit cleanup pass. |

The [fixture](tests/tool_functionality.rs) retains both cache-grant variants. This is opt-in Node adaptation, not a filesystem permission expansion or a change to raw execution. The helper includes no build-jail package-network policy. See the [setup and behavioral limits](README.md#explicit-windows-node-compatibility).

## Git

The Unix sequence covers status, add, commit, clone, fetch, push, linked worktrees and Git LFS. It passes on Linux and macOS with explicit grants for the repository/common-directory locations and a dedicated writable global-config directory. macOS also passes the conventional home-level global-config update. The earlier native-adapter run passes these ordinary Git operations on both Windows hosts, but does not establish general Git Bash support. Git LFS 3.7.1 fails during the pre-push transfer.

The [installed-MSYS argument and path probe](https://github.com/nubjs/nub/actions/runs/34692270797), at `87abecc9f0`, fails on both Windows hosts with a `dofork` error: `0xC0000142` on Server 2022 x64 and `0xC0000005` on Windows 11 ARM64. The adapter does not currently support every process sequence in the installed Git Bash runtime. Passing isolated commands or modified disposable copies does not qualify that installation.

Raw Git still fails with [read access to its whole installation](https://github.com/nubjs/nub/actions/runs/34408888398). Its MSYS runtime also needs package-local coordination objects and pipes. MSYS replaces its process and default-object ACLs with user-only entries; those omit the AppContainer identity. The adapter preserves the existing entries and adds the current package identity, allowing Git to reopen and wait for its child processes. These changes affect runtime-owned objects, not filesystem policy.

Git creates an adjacent lock file and renames it when updating global configuration. A grant on an existing file cannot substitute for parent-directory write access on Linux and Windows. For example, with `GIT_CONFIG_GLOBAL=/work/git-config/config` supplied by the embedder:

```json
{
  "fs": {"./":"rw", "$tooldirs":"rw", "/work/git-config":"rw", "$tmp":"rw"},
  "vars": {"PATH":true, "GIT_CONFIG_GLOBAL":true}
}
```

The writable directory admits the lock/rename protocol without granting all of home. The same principle applies to deleting a cache or build-output root. Configuration-file-only relocations still require explicit paths.

## Cache-root replacement

Relocated NuGet caches can share one writable tool directory. This permits cache clearing and recreation without granting the parent of every arbitrary relocation:

| Environment variable supplied by the embedder | Example value |
| --- | --- |
| `NUGET_PACKAGES` | `/work/nuget/packages` |
| `NUGET_HTTP_CACHE_PATH` | `/work/nuget/http-cache` |
| `NUGET_SCRATCH` | `/work/nuget/scratch` |
| `NUGET_PLUGINS_CACHE_PATH` | `/work/nuget/plugins-cache` |

```json
{
  "fs": {"./": "rw", "$tooldirs": "rw", "/work/nuget": "rw", "$tmp": "rw"},
  "vars": {
    "NUGET_PACKAGES": true,
    "NUGET_HTTP_CACHE_PATH": true,
    "NUGET_SCRATCH": true,
    "NUGET_PLUGINS_CACHE_PATH": true
  }
}
```

The directory `/work/nuget` must exist at acquisition. Cache subdirectories may then be created, cleared and recreated during the session. Using the conventional `~/.nuget` parent instead is already covered by `$tooldirs`. The `true` entries retain the supplied environment values; bare strings in `vars` are type declarations, not literal-value assignments.

## Backend restrictions

### Windows Bun package links

The [Bun 1.4 link controls](https://github.com/nubjs/nub/actions/runs/34532980739) distinguish directory permissions from link creation on Server 2022 and Windows 11 arm64. Copying, hard linking and directory junctions work. File and directory symbolic-link creation returns `EPERM`, including with the entire fixture directory writable. A separate withheld file remains inaccessible.

Bun uses symbolic links for a local-folder global install. Its diagnostic mentions copying files, but adding directories does not fix this operation. Installing a local package archive instead, then clearing its cache and reinstalling it, passes with the ordinary tool bundle and a dedicated writable cache parent. The [retained-command test](https://github.com/nubjs/nub/actions/runs/34530694556) also passes project install, reinstall and installed-bin execution on both Windows hosts.

These commands exercise different installation modes:

```sh
bun install --global ./package      # local-folder links: EPERM in AppContainer
bun install --global ./package.tgz  # archived package: tested successfully
```

### Windows Python compatibility

The [explicit startup adapter](README.md#python-private-directories-on-windows) passes pip 26.2.1's local-wheel install, reinstall, import, user install and cache cleanup with Python 3.13.15 on Server 2022 and Windows 11 arm64. The [native run](https://github.com/nubjs/nub/actions/runs/34523672914) also checks the resulting protected private-directory ACL, nested writes and denied-file canaries.

The Python helper alone does not repair uv's native launcher. The [combined native/Python adapter run](https://github.com/nubjs/nub/actions/runs/34551785167) passes uv's interpreter query and installed native entrypoint on both hosts. The two helpers address different requirements: protected directory creation and native device/path operations.

### Windows Gradle network qualification

The strict matrix refuses Windows' `net-full` degradation before launching Gradle. A [separate execution probe](https://github.com/nubjs/nub/actions/runs/34403539949) explicitly acknowledges that limitation and passes on Server 2022 and Windows 11 arm64: Gradle 8.14 with Temurin 21.0.8 runs an offline task twice and performs daemon cleanup through one retained session. Its plain controls also pass. The [diagnostic test](tests/native_tool_functionality.rs) asserts the exact degradation rather than ignoring all unsupported permissions.

This establishes that the tested Gradle workflow works with the narrower capability. It does not establish full host networking, access to arbitrary host-loopback services, or an unqualified strict-policy pass. No network grants or backend behavior were changed for this probe.

### OS restrictions

- **Linux procfs:** ordinary grants under `/proc` are rejected. Read-only [self-metadata permissions](README.md#explicit-linux-process-metadata) provide the requesting process's maps, statistics, command line and thread metadata without granting another process's files. The tool-directory bundle includes these permissions; exact-path policies can select them individually.
- **Windows private ACLs:** applications can create protected directory ACLs that omit the AppContainer identity. Broader grants on an ancestor do not repair that behavior. Python's private-directory behavior exists in maintained older versions too; selecting an old minor release is not a general workaround.
- **Windows devices and IPC:** filesystem paths do not grant access to every named pipe, the `NUL` device or additional networking capabilities. Server and Windows 11 results differ. The engine does not install administrator device permissions or loopback exemptions.
- **Unix shared temp:** private `TMPDIR` does not relocate paths hardcoded by a runtime. The tool-directory bundle grants `/tmp/.dotnet` for .NET's shared coordination state. This directory is not private session data and survives session cleanup.

### Unix Bun cache locations

The [directory-listing run](https://github.com/nubjs/nub/actions/runs/34529167168) passes the retained Bun 1.3.2 and 1.4.0 sequences on Linux and macOS. The bundle supplies read-only directory listings for project ancestors and conventional temp; independent tests deny sibling file reads, writes, creation and deletion.

Bun 1.3.2 hardcodes `/tmp` or `/private/tmp` for `bunx` downloads and pruning. Listing permits pruning an empty shared cache, but not creating or deleting its entries. Bun 1.4.0 instead checks `TMPDIR`, `TMP` and `TEMP`, allowing private session storage. The versioned fixtures preserve a populated host-cache canary rather than treating an empty-cache pass as general cleanup support.

### Windows path resolution

The [native path probe](https://github.com/nubjs/nub/actions/runs/34526954315) opens the granted file on both Windows hosts. Its NT path query succeeds, but its drive-letter query returns access denied; both plain queries succeed. The withheld-file canary remains denied.

This is distinct from filesystem read permission. The [Microsoft report](https://github.com/microsoft/mxc/issues/694) identifies object-directory and mount-manager access needed by drive-letter translation. The engine does not modify those machine-wide permissions. The Node helper adapts Node's realpath behavior; the native adapter resolves permitted file handles through drive aliases captured by the launcher.

### Linux failure isolation

The [selective syscall-injection run](https://github.com/nubjs/nub/actions/runs/34404715809) tests otherwise unconfined tools, with both plain and tracing-only controls. All controls pass. A separate file-read probe verifies that injection denies only the selected procfs path while ordinary file reads still work.

- Denying only `/proc/self/maps` reproduces Bun 1.4.0's JSON nesting/stack-overflow error during a local install.
- The same isolated denial reproduces CoreCLR initialization error `0x8007000E` during restore. This run selected SDK 10.0.400 and runtime 10.0.12, as recorded by the loaded paths in its trace; the provisioning version did not pin the workload. Denying FIFO creation alone does **not** prevent that restore.
- Denying only `/proc/self/stat` reproduces Node 22.18.0's `uv_resident_set_memory` error from `process.memoryUsage()`, the call made by Yarn Classic's unconditional memory reporter.

These denials are sufficient to reproduce the reported failures; fixing one does not prove no further restriction will be encountered. The [diagnostic harness](../../scripts/sandbox-procfs-diagnostics.py) changes no sandbox policy. Default policies still exclude procfs.

### Windows subprocess controls

The [focused subprocess run](https://github.com/nubjs/nub/actions/runs/34369854867) separates executable access from stream setup. It tests Rust 1.98.1, Python 3.12.10 and the diagnostic executable with seven descriptor configurations, each confined and unconfined. All unconfined cases pass. The confined cases retain a denied-file canary.

On Server 2022, the same executables launch with inherited streams, piped stdin or regular-file streams. Opening `NUL` for stdin or stdout fails with OS error 5. Default Rust `Command::output()` fails, while changing only its stdin to an existing empty file succeeds. All seven configurations pass on Windows 11 arm64.

The parent sandbox launcher can supply an existing handle; it cannot make an unmodified descendant's later `NUL` open succeed. The [diagnostic fixture](tests/windows_subprocess_diagnostics.rs) preserves the individual results rather than treating unsupported configurations as working.

### Windows 11 native tool sequences

The [Windows 11 run](https://github.com/nubjs/nub/actions/runs/34405367987), at `04566d64c9`, exercises native toolchains separately from the Server results above. Cargo 1.91.1, Go 1.25.1 and Composer 2.8.12 pass their complete exact-grant and tool-directory sequences, as do their plain controls. Maven 3.9.11 and rustup 1.29.1 also pass. Gradle passes with the explicitly acknowledged network limitation; its two strict-policy cases refuse preparation.

The runner is Windows 11 arm64. Cargo, Go and the JVM use x64 emulation with x64 MSVC libraries; .NET uses the ARM64 host. These results are not Server results or an all-ARM64 toolchain test. The .NET workload's `global.json` pins SDK 10.0.100, while the original provisioning metadata lists the runner's other installed SDKs too. Installing an SDK alone does not select it for a project.

The [SDK-selection run](https://github.com/nubjs/nub/actions/runs/34407256061), at `f5d73291ad`, asserts `dotnet --version` is exactly `10.0.100` inside each plain, explicit-grant and tool-directory fixture. All three restore/build/cache-cleanup sequences pass.

## Nub build-jail coverage

The build-jail frontend supplies a provisioned Node runtime and its own stdio support; it is not the same configuration as these raw engine tests. The [paired full-application run](https://github.com/nubjs/nub/actions/runs/34699045236) passes 16 framework fixtures per OS on Linux, macOS and Windows, at candidate `dc77487919` and the original reusable-sandbox baseline `b03f68bed9`. Each fixture includes an unconfined control, denied read/write/environment canaries and a frozen reinstall. The [package comparison](https://github.com/nubjs/build-jail-corpus/actions/runs/34701531552) also passes all 42 candidate package arms, including Windows native source builds. Those results do not turn the raw-runtime failures above into passes.
