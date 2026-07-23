#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
profile_dir="$repo_root/target/ffi"

cd "$repo_root"
cargo build --profile ffi -p aube-ffi

case "$(uname -s)" in
Darwin) library="$profile_dir/libaube_ffi.dylib" ;;
Linux) library="$profile_dir/libaube_ffi.so" ;;
MINGW* | MSYS* | CYGWIN*) library="$profile_dir/aube_ffi.dll" ;;
*)
	echo "unsupported FFI smoke host: $(uname -s)" >&2
	exit 1
	;;
esac

AUBE_FFI_LIBRARY="$library" mise x bun@1.3.11 -- bun run crates/aube-ffi/poc/bun.ts
AUBE_FFI_LIBRARY="$library" mise x deno@2.9.2 -- deno run \
	--allow-env=AUBE_FFI_LIBRARY --allow-ffi --allow-read --allow-write \
	crates/aube-ffi/poc/deno.ts
