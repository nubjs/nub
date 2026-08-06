#!/usr/bin/env bash
set -euo pipefail

BWRAP_VERSION=0.11.2
BWRAP_COMMIT=1b80120ef26a28e065e67f89bfef873f13bdd317
BWRAP_SHA256=69abc30005d2186baf7737feacd8da35633b93cf5af38838ecff17c5f8e924f6
LIBCAP_VERSION=2.78
LIBCAP_SHA256=0d621e562fd932ccf67b9660fb018e468a683d7b827541df27813228c996bb11
MUSL_VERSION=1.2.5
MUSL_APK_VERSION=1.2.5-r12
MUSL_SHA256=a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4
MUSL_COPYRIGHT_SHA256=f9bc4423732350eb0b3f7ed7e91d530298476f8fec0c6c427a1c04ade22655af
SOURCE_DATE_EPOCH=1776932328
ALPINE_IMAGE=alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce
ALPINE_REPOSITORY=https://dl-cdn.alpinelinux.org/alpine/v3.22/main
# Pin the direct toolchain inputs by exact package version. Before installation,
# the complete recursive APK closure must also byte-match the architecture-
# specific SHA-256 lock, pinning every transitive package by filename and content.
APK_BUILD_PACKAGES="bash=5.2.37-r0 build-base=0.5-r3 curl=8.14.1-r3 file=5.46-r2 linux-headers=6.14.2-r0 meson=1.8.1-r0 pkgconf=2.4.3-r0 xz=5.8.3-r0"

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
    # ⛔ RETRY IS SAFE HERE *BECAUSE* OF THE CHECKSUM BELOW, AND ONLY BECAUSE OF IT. A retried or
    # partially-written body cannot slip through: the sha256 gate rejects anything that is not the
    # exact expected bytes, so the worst a retry can do is waste time. Without that gate, retrying a
    # download is how a truncated artifact becomes a mysterious build failure three steps later.
    #
    # MEASURED 2026-08-06, and this is why the flags exist: three consecutive `sandbox-conformance`
    # runs died here with `curl: (28) Failed to connect to musl.libc.org after 135310 ms`. The host
    # was UP but degraded — a plain GET of its homepage took 18.2s from a dev machine at the same
    # time. A single attempt with no connect-timeout, no cap and no retry turned a slow upstream
    # into a hard red, and because a failed step SKIPS the rest of the job, every conformance test
    # after it silently stopped running for hours. That is the expensive part: not the flake itself
    # but that it masks everything downstream while looking like a normal failure.
    # ⛔ THE NUMBERS ARE A BUDGET, NOT A VIBE. `--max-time` applies PER ATTEMPT, so the worst case is
    # retries × max-time and it is easy to write a CI step that hangs for half an hour by accident.
    # 3 × 180s + 2 × 10s delay caps this at ~9.3 minutes, against an observed slow-but-working fetch
    # of ~135s — generous enough to absorb that, short enough that a genuinely dead upstream fails
    # while someone is still watching. `--retry-all-errors` is needed because a connect timeout is
    # not in curl's default retry set (needs curl >= 7.71; the Alpine build pins 8.14.1).
    curl --fail --silent --show-error --location \
        --connect-timeout 20 --max-time 180 \
        --retry 3 --retry-delay 10 --retry-all-errors \
        --output "$dest" "$url"
    local actual
    actual=$(sha256_file "$dest")
    [[ "$actual" == "$expected" ]] || {
        echo "checksum mismatch for $url: expected $expected, got $actual" >&2
        exit 1
    }
}

native_build() {
    local out=$1
    local recipe_dir architecture toolchain_lock toolchain_lock_sha spec package version
    recipe_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    architecture=$(uname -m)
    case "$architecture" in
        aarch64) toolchain_lock=bwrap-toolchain-linux-arm64.sha256 ;;
        x86_64) toolchain_lock=bwrap-toolchain-linux-x64.sha256 ;;
        *) echo "unsupported build architecture: $architecture" >&2; exit 1 ;;
    esac
    toolchain_lock_sha=$(sha256_file "$recipe_dir/$toolchain_lock")
    for spec in $APK_BUILD_PACKAGES; do
        package=${spec%%=*}
        version=${spec#*=}
        grep -q "  ${package}-${version}\\.apk$" "$recipe_dir/$toolchain_lock"
    done
    grep -q "  musl-dev-${MUSL_APK_VERSION}\\.apk$" "$recipe_dir/$toolchain_lock"
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
    download_checked \
        "https://musl.libc.org/releases/musl-${MUSL_VERSION}.tar.gz" \
        "musl-${MUSL_VERSION}.tar.gz" "$MUSL_SHA256"

    tar -xf "bubblewrap-${BWRAP_VERSION}.tar.xz"
    tar -xf "libcap-${LIBCAP_VERSION}.tar.xz"
    tar -xf "musl-${MUSL_VERSION}.tar.gz"
    [[ $(sha256_file "musl-${MUSL_VERSION}/COPYRIGHT") == "$MUSL_COPYRIGHT_SHA256" ]]
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
    install -m 0644 "musl-${MUSL_VERSION}/COPYRIGHT" "$out/COPYRIGHT.musl"
    install -m 0755 "${BASH_SOURCE[0]}" "$out/BUILD.sh"
    install -m 0644 "$recipe_dir"/bwrap-toolchain-linux-*.sha256 "$out/"
    printf '%s  %s\n' "$BWRAP_SHA256" "bubblewrap-${BWRAP_VERSION}.tar.xz" > "$out/bubblewrap-${BWRAP_VERSION}.tar.xz.sha256"
    printf '%s  %s\n' "$LIBCAP_SHA256" "libcap-${LIBCAP_VERSION}.tar.xz" > "$out/libcap-${LIBCAP_VERSION}.tar.xz.sha256"
    local binary_sha
    binary_sha=$(sha256_file "$out/bwrap")
    printf '%s  bwrap\n' "$binary_sha" > "$out/bwrap.sha256"
    cat > "$out/BOM.txt" <<EOF
Bubblewrap version: ${BWRAP_VERSION}
Bubblewrap commit: ${BWRAP_COMMIT}
Bubblewrap source SHA-256: ${BWRAP_SHA256}
libcap version: ${LIBCAP_VERSION}
libcap source SHA-256: ${LIBCAP_SHA256}
musl static package: musl-dev-${MUSL_APK_VERSION}.apk
musl upstream notice version: ${MUSL_VERSION}
musl upstream source SHA-256: ${MUSL_SHA256}
musl COPYRIGHT SHA-256: ${MUSL_COPYRIGHT_SHA256}
Build image: ${ALPINE_IMAGE}
APK repository: ${ALPINE_REPOSITORY}
Toolchain lock: ${toolchain_lock}
Toolchain lock SHA-256: ${toolchain_lock_sha}
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

recipe_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script_name=$(basename "${BASH_SOURCE[0]}")
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
        -v "$recipe_dir:/recipe:ro" \
        -v "$work/$build:/work" \
        "$ALPINE_IMAGE" sh -lc \
        'set -eu
         case "$(uname -m)" in
           aarch64) architecture=aarch64; lock=bwrap-toolchain-linux-arm64.sha256 ;;
           x86_64) architecture=x86_64; lock=bwrap-toolchain-linux-x64.sha256 ;;
           *) echo "unsupported build architecture: $(uname -m)" >&2; exit 1 ;;
         esac
         mkdir /work/apks
         # Download the complete locked closure by exact repository path. This
         # deliberately does not consult a mutable APK index or dependency solver.
         while read -r _expected package; do
           for attempt in 1 2 3 4 5; do
             wget -q '"$ALPINE_REPOSITORY"'/$architecture/$package -O /work/apks/$package && break
             rm -f /work/apks/$package
             test "$attempt" -lt 5 || exit 1
           done
         done < /recipe/$lock
         # Reject changed bytes, missing packages, and unexpected packages before
         # apk sees any build input. apk still verifies package signatures below.
         (cd /work/apks && sha256sum *.apk | sort -k2) > /work/toolchain.actual
         cmp /recipe/$lock /work/toolchain.actual
         apk add --no-network /work/apks/*.apk >/dev/null
         exec bash /recipe/'"$script_name"' --native /work/result'
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
