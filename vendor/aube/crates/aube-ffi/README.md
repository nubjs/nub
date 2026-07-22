# aube C ABI

`@jdxcode/aube-ffi` distributes aube's C ABI for Bun FFI, Deno FFI,
Node.js FFI loaders, Python `ctypes`, and native applications. The root npm
package selects one of eight platform packages and exports `libraryPath`,
`headerPath`, and `target`.

```sh
npm install @jdxcode/aube-ffi
```

The same libraries and `aube.h` are attached to GitHub releases for hosts that
do not consume npm packages.

The ABI uses asynchronous operation handles. `aube_install` and `aube_add`
return immediately; `aube_cancel` requests cooperative cancellation and
`aube_wait` blocks until the result is available. Event callbacks run on
aube-managed threads and must be thread-safe. Strings returned by the library
must be released with `aube_string_free`.
A callback-driven host should call `aube_wait` after its terminal event to
release the completed operation handle without blocking.

See the [C ABI embedding guide](https://aube.jdx.dev/embedding/ffi) for schemas,
ownership rules, and examples. For support, use
[GitHub Discussions](https://github.com/jdx/aube/discussions).
