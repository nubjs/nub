#!/usr/bin/env bash
# Fetch an OCI image's layers straight from a registry and unpack them into a plain directory.
# No Docker daemon, no root: the point is that the "fresh disk" is just a tarball on disk.
# Registries are tried in order because Docker Hub anonymous pulls are IP-rate-limited on CI.
set -euo pipefail

IMAGE="${1:?image, e.g. library/node}"
TAG="${2:?tag, e.g. 22-bullseye}"
DEST="${3:?destination directory}"
ARCH="${4:-amd64}"

ACC=(
  -H "Accept: application/vnd.oci.image.index.v1+json"
  -H "Accept: application/vnd.docker.distribution.manifest.list.v2+json"
  -H "Accept: application/vnd.oci.image.manifest.v1+json"
  -H "Accept: application/vnd.docker.distribution.manifest.v2+json"
)

try_registry() {
  local host="$1" repo="$2" tokurl="$3"
  local tok=""
  if [ -n "$tokurl" ]; then
    tok=$(curl -fsSL "$tokurl" | jq -r '.token // .access_token // empty') || return 1
  fi
  local auth=()
  [ -n "$tok" ] && auth=(-H "Authorization: Bearer $tok")

  local man
  man=$(curl -fsSL "${ACC[@]}" "${auth[@]}" "https://$host/v2/$repo/manifests/$TAG") || return 1

  if [ "$(jq -r '.manifests // empty | length' <<<"$man")" != "" ] && [ "$(jq -r '.manifests // empty | length' <<<"$man")" != "0" ]; then
    local d
    d=$(jq -r --arg a "$ARCH" '.manifests[] | select(.platform.architecture==$a and .platform.os=="linux") | .digest' <<<"$man" | head -1)
    [ -n "$d" ] || return 1
    man=$(curl -fsSL "${ACC[@]}" "${auth[@]}" "https://$host/v2/$repo/manifests/$d") || return 1
  fi

  local layers
  layers=$(jq -r '.layers[].digest' <<<"$man") || return 1
  [ -n "$layers" ] || return 1

  echo "REGISTRY=$host REPO=$repo LAYERS=$(wc -l <<<"$layers")" >&2
  mkdir -p "$DEST"
  local n=0 tmp
  tmp=$(mktemp -d)
  while read -r dg; do
    n=$((n + 1))
    echo "  layer $n $dg" >&2
    # Land the blob on disk rather than piping: GNU tar only auto-detects the compression of a
    # SEEKABLE input, and OCI layers are gzip today but zstd for some publishers.
    curl -fsSL "${auth[@]}" -o "$tmp/layer" "https://$host/v2/$repo/blobs/$dg" || { rm -rf "$tmp"; return 1; }
    tar -x --no-same-owner -f "$tmp/layer" -C "$DEST" || { rm -rf "$tmp"; return 1; }
  done <<<"$layers"
  rm -rf "$tmp"

  # OCI whiteouts: a layer deletes a path by adding .wh.<name>. A base image rarely uses them, but
  # ignoring them silently resurrects deleted files, so handle them rather than assume.
  find "$DEST" -name '.wh.*' -print0 2>/dev/null | while IFS= read -r -d '' w; do
    b=$(basename "$w"); d=$(dirname "$w")
    rm -rf "$d/${b#.wh.}" "$w"
  done
  return 0
}

if try_registry mirror.gcr.io "$IMAGE" ""; then exit 0; fi
echo "pull-rootfs: mirror.gcr.io failed, trying public.ecr.aws" >&2
ECR_REPO="docker/${IMAGE}"
if try_registry public.ecr.aws "$ECR_REPO" "https://public.ecr.aws/token/?service=public.ecr.aws&scope=repository:${ECR_REPO}:pull"; then exit 0; fi
echo "pull-rootfs: public.ecr.aws failed, trying registry-1.docker.io" >&2
if try_registry registry-1.docker.io "$IMAGE" "https://auth.docker.io/token?service=registry.docker.io&scope=repository:${IMAGE}:pull"; then exit 0; fi
echo "pull-rootfs: ALL REGISTRIES FAILED" >&2
exit 1
