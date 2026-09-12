#!/usr/bin/env bash
set -euo pipefail

readonly upstream_repo="pnpm/plugin-trusted-deps"
readonly list_path="crates/aube/assets/trusted-dependencies.json"
readonly source_path="crates/aube/assets/trusted-dependencies.source.json"

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

commit=$(gh api "repos/${upstream_repo}/commits/main" --jq .sha)
gh api \
	-H "Accept: application/vnd.github.raw+json" \
	"repos/${upstream_repo}/contents/allow.json?ref=${commit}" \
	>"${tmp_dir}/trusted-dependencies.json"

jq -e '
  type == "array"
  and all(.[]; type == "string" and length > 0)
  and length == (unique | length)
  and . == sort
' "${tmp_dir}/trusted-dependencies.json" >/dev/null

jq -n \
	--arg repository "https://github.com/${upstream_repo}" \
	--arg commit "$commit" \
	'{repository: $repository, commit: $commit, path: "allow.json", license: "MIT"}' \
	>"${tmp_dir}/trusted-dependencies.source.json"

install -m 0644 "${tmp_dir}/trusted-dependencies.json" "$list_path"
install -m 0644 "${tmp_dir}/trusted-dependencies.source.json" "$source_path"

echo "Updated trusted dependencies from ${upstream_repo}@${commit}"
