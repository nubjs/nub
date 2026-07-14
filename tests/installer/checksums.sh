#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
fixture=$(mktemp -d)
original_path=$PATH
trap 'rm -rf "$fixture"' EXIT

case "$(uname -ms)" in
    'Darwin arm64') target=darwin-arm64 ;;
    'Darwin x86_64') target=darwin-x64 ;;
    'Linux aarch64' | 'Linux arm64') target=linux-arm64 ;;
    'Linux x86_64') target=linux-x64 ;;
    *) echo "unsupported test platform" >&2; exit 1 ;;
esac
if [[ "$target" == linux-* ]] && { [[ -f /etc/alpine-release ]] || ldd --version 2>&1 | grep -qi musl; }; then
    target="${target}-musl"
fi

archive_name="nub-${target}.tar.gz"
assets="$fixture/assets"
build="$fixture/build"
mock_bin="$fixture/mock-bin"
fallback_bin="$fixture/fallback-bin"
nohash_bin="$fixture/nohash-bin"
mkdir -p "$assets" "$build/bin" "$build/runtime" "$mock_bin" "$fallback_bin" "$nohash_bin"
printf 'NEW-NUB\n' > "$build/bin/nub"
printf 'FROM-ARCHIVE\n' > "$build/runtime/from-archive"
tar -czf "$assets/$archive_name" -C "$build" bin runtime

if command -v sha256sum >/dev/null 2>&1; then
    digest=$(sha256sum "$assets/$archive_name" | awk '{print $1}')
else
    digest=$(shasum -a 256 "$assets/$archive_name" | awk '{print $1}')
fi

cat > "$mock_bin/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
output=
url=
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output) output=$2; shift 2 ;;
        *) url=$1; shift ;;
    esac
done
[[ -n "$output" && -n "$url" ]]
cp "$NUB_TEST_ASSET_DIR/${url##*/}" "$output"
MOCK
chmod +x "$mock_bin/curl"

cat > "$fallback_bin/sha256sum" <<'MOCK'
#!/usr/bin/env bash
exit 1
MOCK
chmod +x "$fallback_bin/sha256sum"

for tool in sha256sum shasum; do
    cat > "$nohash_bin/$tool" <<'MOCK'
#!/usr/bin/env bash
exit 1
MOCK
    chmod +x "$nohash_bin/$tool"
done

case_index=0
run_case() {
    local name=$1
    local sidecar=$2
    local expected_message=$3
    local expect_success=$4
    local hash_mode=${5:-default}
    local case_root install_dir output status test_path profile_before

    case_index=$((case_index + 1))
    case_root="$fixture/case-$case_index"
    install_dir="$case_root/home/.nub"
    mkdir -p "$install_dir/bin" "$install_dir/runtime" "$case_root/tmp"
    printf 'OLD-NUB\n' > "$install_dir/bin/nub"
    printf 'EXISTING-RUNTIME\n' > "$install_dir/runtime/existing"
    printf 'EXISTING-PROFILE\n' > "$case_root/home/.zshrc"
    profile_before=$(cat "$case_root/home/.zshrc")

    rm -f "$assets/$archive_name.sha256"
    if [[ "$sidecar" != MISSING ]]; then
        printf '%b' "$sidecar" > "$assets/$archive_name.sha256"
    fi

    test_path="$mock_bin:$original_path"
    case "$hash_mode" in
        fallback) test_path="$fallback_bin:$test_path" ;;
        none) test_path="$nohash_bin:$test_path" ;;
    esac

    set +e
    if [[ "$expect_success" == 1 ]]; then
        output=$(HOME="$case_root/home" NUB_INSTALL_DIR="$install_dir" NUB_NO_MODIFY_PATH=1 \
            NUB_TEST_ASSET_DIR="$assets" PATH="$test_path" TMPDIR="$case_root/tmp" SHELL=/bin/zsh \
            "$repo_root/install.sh" 9.9.9 2>&1)
    else
        output=$(HOME="$case_root/home" NUB_INSTALL_DIR="$install_dir" \
            NUB_TEST_ASSET_DIR="$assets" PATH="$test_path" TMPDIR="$case_root/tmp" SHELL=/bin/zsh \
            "$repo_root/install.sh" 9.9.9 2>&1)
    fi
    status=$?
    set -e

    if [[ "$expect_success" == 1 ]]; then
        [[ $status -eq 0 ]] || { echo "$name: expected success, got $status: $output" >&2; return 1; }
        [[ $(cat "$install_dir/bin/nub") == NEW-NUB ]] || { echo "$name: new binary was not installed" >&2; return 1; }
        [[ -f "$install_dir/runtime/from-archive" ]] || { echo "$name: verified archive was not extracted" >&2; return 1; }
    else
        [[ $status -ne 0 ]] || { echo "$name: expected failure" >&2; return 1; }
        [[ "$output" == *"$expected_message"* ]] || { echo "$name: missing error '$expected_message': $output" >&2; return 1; }
        [[ $(cat "$install_dir/bin/nub") == OLD-NUB ]] || { echo "$name: existing binary changed before verification" >&2; return 1; }
        [[ $(cat "$install_dir/runtime/existing") == EXISTING-RUNTIME ]] || { echo "$name: existing runtime changed before verification" >&2; return 1; }
        [[ ! -e "$install_dir/runtime/from-archive" ]] || { echo "$name: archive was extracted before verification" >&2; return 1; }
        [[ ! -e "$install_dir/.nub-receipt" ]] || { echo "$name: receipt was written after verification failure" >&2; return 1; }
        [[ $(cat "$case_root/home/.zshrc") == "$profile_before" ]] || { echo "$name: shell profile changed before verification" >&2; return 1; }
        [[ -z $(ls -A "$case_root/tmp") ]] || { echo "$name: temporary download files were not cleaned up" >&2; return 1; }
    fi
    printf 'ok - %s\n' "$name"
}

valid="${digest}  ${archive_name}\\n"
run_case "matching checksum" "$valid" "" 1
run_case "mismatched checksum" "$(printf '0%.0s' {1..64})  ${archive_name}\\n" "Checksum mismatch" 0
run_case "missing sidecar" MISSING "Failed to download checksum" 0
run_case "unavailable hash tools" "$valid" "No usable SHA-256 tool found" 0 none

malformed_cases=(
    "wrong basename|${digest}  wrong.tar.gz\\n"
    "wrong basename case|${digest}  $(printf '%s' "$archive_name" | tr '[:lower:]' '[:upper:]')\\n"
    "extra record|${valid}${valid}"
    "extra field|${digest}  ${archive_name} extra\\n"
    "invalid digest|g${digest:1}  ${archive_name}\\n"
    "short digest|${digest:0:63}  ${archive_name}\\n"
    "CRLF ending|${digest}  ${archive_name}\\r\\n"
    "missing LF|${digest}  ${archive_name}"
    "excess trailing newline|${digest}  ${archive_name}\\n\\n"
)
for malformed in "${malformed_cases[@]}"; do
    run_case "${malformed%%|*}" "${malformed#*|}" "Malformed checksum" 0
done

if command -v shasum >/dev/null 2>&1; then
    run_case "shasum fallback" "$valid" "" 1 fallback
else
    echo "skip - shasum fallback (shasum unavailable)"
fi
