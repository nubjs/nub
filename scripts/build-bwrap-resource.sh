#!/usr/bin/env bash
set -euo pipefail

BWRAP_VERSION=0.11.2
BWRAP_COMMIT=1b80120ef26a28e065e67f89bfef873f13bdd317
BWRAP_SHA256=69abc30005d2186baf7737feacd8da35633b93cf5af38838ecff17c5f8e924f6
LIBCAP_VERSION=2.78
LIBCAP_SHA256=0d621e562fd932ccf67b9660fb018e468a683d7b827541df27813228c996bb11
SOURCE_DATE_EPOCH=1776932328
ALPINE_IMAGE=alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce

usage() {
    echo "usage: $0 [--container-runtime docker|nerdctl] OUTPUT_DIR" >&2
    exit 2
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

download_checked() {
    local url=$1 dest=$2 expected=$3
    curl --fail --silent --show-error --location --output "$dest" "$url"
    local actual
    actual=$(sha256_file "$dest")
    [[ "$actual" == "$expected" ]] || {
        echo "checksum mismatch for $url: expected $expected, got $actual" >&2
        exit 1
    }
}

native_build() {
    local out=$1
    export SOURCE_DATE_EPOCH LC_ALL=C TZ=UTC
    umask 022
    mkdir -p "$out" /work/prefix
    cd /work

    download_checked \
        "https://github.com/containers/bubblewrap/releases/download/v${BWRAP_VERSION}/bubblewrap-${BWRAP_VERSION}.tar.xz" \
        "bubblewrap-${BWRAP_VERSION}.tar.xz" "$BWRAP_SHA256"
    download_checked \
        "https://mirrors.edge.kernel.org/pub/linux/libs/security/linux-privs/libcap2/libcap-${LIBCAP_VERSION}.tar.xz" \
        "libcap-${LIBCAP_VERSION}.tar.xz" "$LIBCAP_SHA256"

    tar -xf "bubblewrap-${BWRAP_VERSION}.tar.xz"
    tar -xf "libcap-${LIBCAP_VERSION}.tar.xz"
    local cflags="-O2 -ffile-prefix-map=/work=. -fdebug-prefix-map=/work=. -Wdate-time"
    make -C "libcap-${LIBCAP_VERSION}/libcap" \
        SHARED=no PTHREADS=no USE_GPERF=no prefix=/work/prefix lib=lib \
        CFLAGS="$cflags" install-static

    PKG_CONFIG_PATH=/work/prefix/lib/pkgconfig \
    CFLAGS="$cflags" \
    LDFLAGS="-static -Wl,--build-id=none" \
        meson setup "bubblewrap-${BWRAP_VERSION}/build" \
            "bubblewrap-${BWRAP_VERSION}" \
            --buildtype=release \
            -Dprefer_static=true \
            -Dselinux=disabled \
            -Dman=disabled \
            -Dtests=false \
            -Dbash_completion=disabled \
            -Dzsh_completion=disabled
    meson compile -C "bubblewrap-${BWRAP_VERSION}/build"
    strip --strip-all "bubblewrap-${BWRAP_VERSION}/build/bwrap"
    install -m 0755 "bubblewrap-${BWRAP_VERSION}/build/bwrap" "$out/bwrap"

    file "$out/bwrap" | grep -q 'statically linked'
    ! readelf -l "$out/bwrap" | grep -q 'INTERP'
    [[ $("$out/bwrap" --version) == "bubblewrap ${BWRAP_VERSION}" ]]

    install -m 0644 "bubblewrap-${BWRAP_VERSION}.tar.xz" "$out/"
    install -m 0644 "libcap-${LIBCAP_VERSION}.tar.xz" "$out/"
    install -m 0644 "bubblewrap-${BWRAP_VERSION}/COPYING" "$out/COPYING.bubblewrap"
    install -m 0644 "libcap-${LIBCAP_VERSION}/License" "$out/LICENSE.libcap"
    install -m 0755 /build-bwrap-resource.sh "$out/BUILD.sh"
    printf '%s  %s\n' "$BWRAP_SHA256" "bubblewrap-${BWRAP_VERSION}.tar.xz" > "$out/bubblewrap-${BWRAP_VERSION}.tar.xz.sha256"
    printf '%s  %s\n' "$LIBCAP_SHA256" "libcap-${LIBCAP_VERSION}.tar.xz" > "$out/libcap-${LIBCAP_VERSION}.tar.xz.sha256"
    local binary_sha architecture
    binary_sha=$(sha256_file "$out/bwrap")
    architecture=$(uname -m)
    printf '%s  bwrap\n' "$binary_sha" > "$out/bwrap.sha256"
    cat > "$out/BOM.txt" <<EOF
Bubblewrap version: ${BWRAP_VERSION}
Bubblewrap commit: ${BWRAP_COMMIT}
Bubblewrap source SHA-256: ${BWRAP_SHA256}
libcap version: ${LIBCAP_VERSION}
libcap source SHA-256: ${LIBCAP_SHA256}
Build image: ${ALPINE_IMAGE}
Architecture: ${architecture}
Compiler: $(cc -dumpfullversion -dumpversion)
Meson: $(meson --version)
Binutils: $(ld --version | head -1)
Binary SHA-256: ${binary_sha}
EOF
    apk list --installed 2>/dev/null | LC_ALL=C sort > "$out/build-packages.txt"
}

if [[ ${1:-} == --native ]]; then
    [[ $# == 2 ]] || usage
    native_build "$2"
    exit 0
fi

runtime=
if [[ ${1:-} == --container-runtime ]]; then
    [[ $# -eq 3 ]] || usage
    runtime=$2
    shift 2
fi
[[ $# == 1 ]] || usage
output=$1
if [[ -z "$runtime" ]]; then
    if command -v docker >/dev/null 2>&1; then runtime=docker
    elif command -v nerdctl >/dev/null 2>&1; then runtime=nerdctl
    else echo "docker or nerdctl is required" >&2; exit 1
    fi
fi

script_path=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")
work=$(mktemp -d)
cleanup() {
    "$runtime" run --rm -v "$work:/cleanup" "$ALPINE_IMAGE" \
        sh -c 'rm -rf /cleanup/a /cleanup/b' >/dev/null 2>&1 || true
    rmdir "$work" 2>/dev/null || true
}
trap cleanup EXIT
mkdir -p "$work/a" "$work/b"
for build in a b; do
    "$runtime" run --rm \
        -v "$script_path:/build-bwrap-resource.sh:ro" \
        -v "$work/$build:/work" \
        "$ALPINE_IMAGE" sh -lc \
        'apk add --no-cache bash build-base curl file linux-headers meson pkgconf xz >/dev/null && /build-bwrap-resource.sh --native /work/result'
done
diff -qr "$work/a/result" "$work/b/result" >/dev/null || {
    echo "Bubblewrap resource builds were not reproducible" >&2
    diff -qr "$work/a/result" "$work/b/result" >&2 || true
    exit 1
}
rm -rf "$output"
mkdir -p "$(dirname "$output")"
cp -a "$work/a/result" "$output"
echo "built reproducible Bubblewrap resource: $output"
