# Darwin link stubs for the Linux cross-build

`cargo-zigbuild` gives us a macOS **libc** (zig ships `libSystem.tbd`), but nub's darwin
dependency tree also links Apple *frameworks* and a couple of system dylibs:

```
-lcompression -framework Security -framework SystemConfiguration -framework CoreFoundation -liconv
```

Those come from `rustls-native-certs` → `reqwest` (`security-framework`, `core-foundation`) and
from `lzma-sys`. None of them ship with zig, so a Linux cross-build fails at LINK time with
`library not found for -lcompression` — compiling is never the problem.

These `.tbd` files are **stub text-based dylibs**: a TBD v4 header plus the list of symbols nub
actually references. They contain no Apple code and no Apple SDK content — the symbol names were
harvested from nub's own undefined-symbol list — so nothing here depends on Apple's SDK licence,
which is what keeps the whole cross-build route licence-clean.

`scripts/remote-build.ts` installs these to `$HOME/.darwin-stubs` on the builder and points the
linker at them with `-C link-arg=-L… -C link-arg=-F…`.

## When a link fails with an undefined symbol

The symbol lists are a snapshot of what nub referenced when they were generated. If nub starts
calling a new Apple API, the link fails with an undefined `_SecFoo`/`_CFBar`. Regenerate:
capture the undefined symbols from the failing link into `/tmp/undef.txt`, then run
`regenerate.sh`. Widening a regex in that script is usually all that is needed.
