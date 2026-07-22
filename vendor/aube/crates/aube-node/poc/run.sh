#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
poc_dir="$repo_root/crates/aube-node/poc"
profile_dir="$repo_root/target/napi"
compiled="$profile_dir/aube-node-poc"

cleanup() {
	rm -f "$poc_dir/aube.node"
}
trap cleanup EXIT

cd "$repo_root"
cargo build --profile napi -p aube-node

case "$(uname -s)" in
Darwin) addon="$profile_dir/libaube_node.dylib" ;;
Linux) addon="$profile_dir/libaube_node.so" ;;
MINGW* | MSYS* | CYGWIN*)
	addon="$profile_dir/aube_node.dll"
	compiled="$compiled.exe"
	;;
*)
	echo "unsupported POC host: $(uname -s)" >&2
	exit 1
	;;
esac

cp "$addon" "$poc_dir/aube.node"

mise x bun@1.3.11 -- bun run "$poc_dir/index.ts"
mise x bun@1.3.11 -- bun build --compile "$poc_dir/index.ts" --outfile "$compiled"
rm "$poc_dir/aube.node"
"$compiled"
