#!/bin/bash

set -euo pipefail

PACKAGE_NAME="${PACKAGE_NAME:-aube}"
CHROOTS="${CHROOTS:-fedora-rawhide-aarch64 fedora-rawhide-x86_64 fedora-44-aarch64 fedora-44-x86_64 fedora-43-aarch64 fedora-43-x86_64 fedora-42-aarch64 fedora-42-x86_64 epel-10-aarch64 epel-10-x86_64}"
BUILD_PROFILE="${BUILD_PROFILE:-release}"
MAINTAINER_NAME="${MAINTAINER_NAME:-aube Release Bot}"
MAINTAINER_EMAIL="${MAINTAINER_EMAIL:-noreply@aube.jdx.dev}"
COPR_OWNER="${COPR_OWNER:-jdxcode}"
COPR_PROJECT="${COPR_PROJECT:-aube}"
DRY_RUN="${DRY_RUN:-false}"

REPO_ROOT="$(pwd)"

usage() {
	echo "Usage: $0 [options]"
	echo ""
	echo "Options:"
	echo "  -v, --version VERSION        Package version (required)"
	echo "  -p, --profile PROFILE        Build profile (default: release)"
	echo "  -c, --chroots CHROOTS        COPR chroots (default: fedora-rawhide-aarch64 fedora-rawhide-x86_64 fedora-44-aarch64 fedora-44-x86_64 fedora-43-aarch64 fedora-43-x86_64 fedora-42-aarch64 fedora-42-x86_64 epel-10-aarch64 epel-10-x86_64)"
	echo "  -o, --owner OWNER            COPR owner (default: jdxcode)"
	echo "  -j, --project PROJECT        COPR project (default: aube)"
	echo "  -n, --name NAME              Package name (default: aube)"
	echo "  -m, --maintainer-name NAME   Maintainer name (default: aube Release Bot)"
	echo "  -e, --maintainer-email EMAIL Maintainer email (default: noreply@aube.jdx.dev)"
	echo "  -d, --dry-run                Build SRPM only, don't submit to COPR"
	echo "  -h, --help                   Show this help"
	echo ""
	echo "Environment variables:"
	echo "  COPR_API_LOGIN               COPR API login (required for submission)"
	echo "  COPR_API_TOKEN               COPR API token (required for submission)"
	echo ""
	echo "Example:"
	echo "  $0 -v 1.0.0-beta.1 -d"
	exit 0
}

while [[ $# -gt 0 ]]; do
	case $1 in
	-v | --version)
		VERSION="$2"
		shift 2
		;;
	-p | --profile)
		BUILD_PROFILE="$2"
		shift 2
		;;
	-c | --chroots)
		CHROOTS="$2"
		shift 2
		;;
	-o | --owner)
		COPR_OWNER="$2"
		shift 2
		;;
	-j | --project)
		COPR_PROJECT="$2"
		shift 2
		;;
	-n | --name)
		PACKAGE_NAME="$2"
		shift 2
		;;
	-m | --maintainer-name)
		MAINTAINER_NAME="$2"
		shift 2
		;;
	-e | --maintainer-email)
		MAINTAINER_EMAIL="$2"
		shift 2
		;;
	-d | --dry-run)
		DRY_RUN="true"
		shift
		;;
	-h | --help)
		usage
		;;
	*)
		echo "Unknown option: $1"
		usage
		;;
	esac
done

if [ -z "${VERSION:-}" ]; then
	echo "Error: VERSION is required"
	echo "Use --version to specify the version or set VERSION environment variable"
	exit 1
fi

# RPM Version field disallows `-`. Convert SemVer prerelease separators
# to `~` so `1.0.0-beta.1` becomes `1.0.0~beta.1`, which rpm collates
# as older than `1.0.0` (correct pre-release ordering).
# Backslash-escape the tilde: bash performs tilde expansion on the
# replacement string in ${var//pat/rep}, even inside "...", so an
# unescaped `~` turns into $HOME and corrupts the Version field (e.g.
# `1.0.0-beta.4` -> `1.0.0/github/homebeta.4` on GitHub Actions).
RPM_VERSION="${VERSION//-/\~}"
# Tarball name keeps the original SemVer string so GitHub's /archive URL
# matches Source0 exactly.
TARBALL_VERSION="${VERSION}"

echo "=== COPR Build Configuration ==="
echo "Package Name: $PACKAGE_NAME"
echo "Version: $VERSION (rpm: $RPM_VERSION)"
echo "Build Profile: $BUILD_PROFILE"
echo "Chroots: $CHROOTS"
echo "COPR Owner: $COPR_OWNER"
echo "COPR Project: $COPR_PROJECT"
echo "Maintainer: $MAINTAINER_NAME <$MAINTAINER_EMAIL>"
echo "Dry Run: $DRY_RUN"
echo ""

git config --global user.name "$MAINTAINER_NAME"
git config --global user.email "$MAINTAINER_EMAIL"
git config --global --add safe.directory "$REPO_ROOT"

if [ "$DRY_RUN" != "true" ]; then
	if [ -z "${COPR_API_LOGIN:-}" ] || [ -z "${COPR_API_TOKEN:-}" ]; then
		echo "Error: COPR_API_LOGIN and COPR_API_TOKEN environment variables are required for submission"
		exit 1
	fi

	mkdir -p ~/.config
	cat >~/.config/copr <<EOF
[copr-cli]
login = $COPR_API_LOGIN
username = $COPR_OWNER
token = $COPR_API_TOKEN
copr_url = https://copr.fedorainfracloud.org
EOF
fi

BUILD_DIR="/tmp/rpm-build"
mkdir -p "$BUILD_DIR"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}

echo "%_topdir $BUILD_DIR" >~/.rpmmacros
echo "%_tmppath %{_topdir}/tmp" >>~/.rpmmacros

cd "$BUILD_DIR"

echo "=== Creating Source Tarball ==="
git -C "$REPO_ROOT" archive --format=tar --prefix="${PACKAGE_NAME}-${TARBALL_VERSION}/" HEAD >"SOURCES/${PACKAGE_NAME}-${TARBALL_VERSION}.tar"
gzip "SOURCES/${PACKAGE_NAME}-${TARBALL_VERSION}.tar"

echo "=== Vendoring Rust Dependencies ==="
cd SOURCES
tar -xzf "${PACKAGE_NAME}-${TARBALL_VERSION}.tar.gz"
cd "${PACKAGE_NAME}-${TARBALL_VERSION}"

mkdir -p .cargo
cat >.cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF

cargo vendor vendor/
tar -czf "../${PACKAGE_NAME}-vendor-${TARBALL_VERSION}.tar.gz" vendor/ .cargo/

cd ../..
rm -rf "SOURCES/${PACKAGE_NAME}-${TARBALL_VERSION}"

echo "=== Creating RPM Spec File ==="
CHANGELOG_DATE=$(date +'%a %b %d %Y')

# Cargo's profile→directory mapping isn't identity: the built-in
# `dev` profile (and anything inheriting from it) lands in
# `target/debug/`, while `release`, `bench`, and any custom profile
# land in `target/<name>/`.
case "${BUILD_PROFILE}" in
dev | test) TARGET_DIR="target/debug" ;;
*) TARGET_DIR="target/${BUILD_PROFILE}" ;;
esac

# Direct interpolation via unquoted heredoc. RPM macros use `%{...}`
# which doesn't collide with shell `${...}` expansion, so we can emit
# the spec without running it through sed — which would corrupt on `&`
# or `/` in any of the maintainer-supplied values.
cat >"SPECS/${PACKAGE_NAME}.spec" <<EOF
%global debug_package %{nil}
%global _missing_build_ids_terminate_build 0
%global tarball_version ${TARBALL_VERSION}

Name:           ${PACKAGE_NAME}
Version:        ${RPM_VERSION}
Release:        1%{?dist}
Summary:        A fast Node.js package manager

License:        MIT
URL:            https://aube.jdx.dev
Source0:        https://github.com/jdx/aube/archive/v%{tarball_version}/%{name}-%{tarball_version}.tar.gz
Source1:        %{name}-vendor-%{tarball_version}.tar.gz

BuildRequires:  rust >= 1.93
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  cmake
BuildRequires:  perl-interpreter
BuildRequires:  git
BuildRequires:  openssl-devel
BuildRequires:  pkgconf-pkg-config

%description
Aube is a fast Node.js package manager written in Rust. It mirrors
pnpm's CLI surface and isolated symlink layout so users can swap it
in, with its own aube-owned on-disk state (global store under
\$XDG_DATA_HOME/aube/store/, per-project virtual store in
node_modules/.aube/).
Aube reads and writes pnpm-lock.yaml, package-lock.json,
npm-shrinkwrap.json, yarn.lock, and bun.lock in addition to its
native aube-lock.yaml.

%prep
%autosetup -n %{name}-%{tarball_version}
%setup -q -T -D -n %{name}-%{tarball_version} -a 1

%build
mkdir -p .cargo
cat > .cargo/config.toml << 'CARGO_EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
CARGO_EOF

cargo build --profile ${BUILD_PROFILE} --frozen --bin aube --bin aubr --bin aubx

%install
mkdir -p %{buildroot}%{_bindir}
cp ${TARGET_DIR}/aube %{buildroot}%{_bindir}/aube
# aubr/aubx are multicall shims that dispatch on argv[0]; ship them
# as symlinks to keep the package small.
ln -s aube %{buildroot}%{_bindir}/aubr
ln -s aube %{buildroot}%{_bindir}/aubx

# Disable self-update for package-manager installs.
mkdir -p %{buildroot}%{_libdir}/aube
cat > %{buildroot}%{_libdir}/aube/aube-self-update-instructions.toml <<'TOML'
message = "To update aube from COPR, run:\n\n  sudo dnf upgrade aube\n"
TOML

%files
%license LICENSE
%doc README.md
%{_bindir}/aube
%{_bindir}/aubr
%{_bindir}/aubx
%{_libdir}/aube/aube-self-update-instructions.toml

%changelog
* ${CHANGELOG_DATE} ${MAINTAINER_NAME} <${MAINTAINER_EMAIL}> - %{version}-1
- New upstream release %{version}
EOF

echo "=== Building Source RPM ==="
rpmbuild -bs "SPECS/${PACKAGE_NAME}.spec"

SRPM_FILE=$(find SRPMS -name "*.src.rpm" -type f | head -1)
if [ -n "$SRPM_FILE" ]; then
	cp "$SRPM_FILE" "$REPO_ROOT/"
	echo "SRPM created: $REPO_ROOT/$(basename "$SRPM_FILE")"
else
	echo "Error: No SRPM file found"
	exit 1
fi

if [ "$DRY_RUN" != "true" ]; then
	echo "=== Submitting to COPR ==="
	echo "Submitting $(basename "$SRPM_FILE") to COPR project $COPR_OWNER/$COPR_PROJECT"

	# Build the copr-cli invocation as an array so chroot names and
	# paths that ever grow spaces or shell metacharacters don't get
	# re-split by `eval`.
	copr_cmd=(copr-cli build)
	IFS=' ' read -ra chroot_array <<<"$CHROOTS"
	for chroot in "${chroot_array[@]}"; do
		copr_cmd+=(--chroot "$chroot")
	done
	copr_cmd+=("$COPR_OWNER/$COPR_PROJECT" "$SRPM_FILE")

	"${copr_cmd[@]}"

	echo "Build submitted successfully!"
else
	echo "=== Dry Run Complete ==="
	echo "SRPM built successfully but not submitted to COPR (dry-run mode)"
fi

mkdir -p "$REPO_ROOT/artifacts"
cp "$SRPM_FILE" "$REPO_ROOT/artifacts/" 2>/dev/null || true
cp "SPECS/${PACKAGE_NAME}.spec" "$REPO_ROOT/artifacts/" 2>/dev/null || true

echo "=== Build Complete ==="
echo "Artifacts available in $REPO_ROOT/artifacts/"
