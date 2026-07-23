# Embedding aube

aube can run inside another application instead of being invoked as a child
process. Embedding avoids CLI startup and output parsing, gives the host
structured progress events and stable error codes, and allows cooperative
cancellation.

The native Rust API is the foundation for every embedding integration; the
Node-API and C ABI distributions are thin adapters over it.

## Supported operations

The stable `aube::embed` facade supports:

- installing a project's declared dependencies
- adding packages and installing the resulting dependency graph
- per-install structured progress and output events
- cooperative cancellation
- concurrent operations in unrelated projects
- stable `ERR_AUBE_*` diagnostic codes

Operations that target the same workspace are serialized. The project lock
spans manifest changes, lockfile updates, and linking, so concurrent callers do
not observe partial state.

## Choosing an integration

- [Rust](./rust.md) — the native `aube::embed` facade for Rust hosts.
- [Node-API](./node.md) — `@jdxcode/aube-node` for Node.js, Bun, Electron, and
  compiled Bun executables. Prefer this for any JavaScript host that supports
  Node-API.
- [C ABI](./ffi.md) — `@jdxcode/aube-ffi` for hosts that need a
  dynamic-library boundary: Deno FFI, Python `ctypes`, and native
  applications.

## Compatibility

The `aube::embed` module is the supported in-process interface. The
`aube::commands` modules remain public for CLI wrappers, but their command
argument types follow the CLI and are not the preferred API for native hosts.

Every error and warning emitted by aube carries a stable identifier. See the
[error code reference](../error-codes.md) for the compatibility policy.

For questions and integration discussion, use
[GitHub Discussions](https://github.com/jdx/aube/discussions).
