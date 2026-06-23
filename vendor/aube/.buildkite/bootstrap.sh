#!/usr/bin/env bash
set -euo pipefail

if ! command -v mise >/dev/null; then
	curl https://mise.run | sh
	export PATH="$HOME/.local/bin:$PATH"
fi

# Keep Cargo's download/cache state in stable cache-mounted paths so Cargo
# fingerprints do not change when Buildkite checks the repo out elsewhere.
export CARGO_HOME="${CARGO_HOME:-$PWD/.cache/cargo}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-.cache/cargo-target}"
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}"
export MSRV_TOOLCHAIN="${MSRV_TOOLCHAIN:-1.93.0}"
export AUBE_BOOTSTRAP_RUST="${AUBE_BOOTSTRAP_RUST:-1}"
export AUBE_BOOTSTRAP_MSRV="${AUBE_BOOTSTRAP_MSRV:-0}"
export AUBE_BOOTSTRAP_PARALLEL="${AUBE_BOOTSTRAP_PARALLEL:-0}"
export MISE_CACHE_DIR="${MISE_CACHE_DIR:-$PWD/.cache/mise-cache}"
export MISE_DATA_DIR="${MISE_DATA_DIR:-$PWD/.cache/mise}"
export PARALLEL_HOME="${PARALLEL_HOME:-$PWD/.cache/parallel}"
case "$CARGO_HOME" in
/*) ;;
*) CARGO_HOME="$PWD/$CARGO_HOME" ;;
esac
if [[ -n "${RUSTUP_HOME:-}" ]]; then
	case "$RUSTUP_HOME" in
	/*) ;;
	*) RUSTUP_HOME="$PWD/$RUSTUP_HOME" ;;
	esac
	export RUSTUP_HOME
fi
case "$MISE_CACHE_DIR" in
/*) ;;
*) MISE_CACHE_DIR="$PWD/$MISE_CACHE_DIR" ;;
esac
case "$MISE_DATA_DIR" in
/*) ;;
*) MISE_DATA_DIR="$PWD/$MISE_DATA_DIR" ;;
esac
case "$PARALLEL_HOME" in
/*) ;;
*) PARALLEL_HOME="$PWD/$PARALLEL_HOME" ;;
esac
export CARGO_HOME CARGO_TARGET_DIR MISE_CACHE_DIR MISE_DATA_DIR PARALLEL_HOME
export PATH="$CARGO_HOME/bin:$PATH"
mkdir -p "$CARGO_HOME/bin" "$CARGO_HOME/registry/cache" "$CARGO_HOME/registry/index" "$CARGO_HOME/git" "$CARGO_TARGET_DIR" "$MISE_CACHE_DIR" "$MISE_DATA_DIR" "$PARALLEL_HOME"
if [[ -n "${RUSTUP_HOME:-}" ]]; then
	mkdir -p "$RUSTUP_HOME"
fi
touch "$PARALLEL_HOME/will-cite"
if [[ -n "${BUILDKITE:-}" ]]; then
	if [[ -L target && ! -e target ]]; then
		rm target
	fi
	if [[ ! -e target ]]; then
		ln -s "$CARGO_TARGET_DIR" target
	fi
fi
echo "cache dirs: CARGO_HOME=$CARGO_HOME CARGO_TARGET_DIR=$CARGO_TARGET_DIR RUSTUP_HOME=${RUSTUP_HOME:-<default>} MISE_DATA_DIR=$MISE_DATA_DIR MISE_CACHE_DIR=$MISE_CACHE_DIR PARALLEL_HOME=$PARALLEL_HOME"
du_paths=("$CARGO_HOME" "$CARGO_TARGET_DIR" "$MISE_DATA_DIR" "$MISE_CACHE_DIR" "$PARALLEL_HOME")
if [[ -n "${RUSTUP_HOME:-}" ]]; then
	du_paths+=("$RUSTUP_HOME")
fi
du -sh "${du_paths[@]}" 2>/dev/null || true

if command -v apt-get >/dev/null; then
	missing_packages=()
	if ! command -v cc >/dev/null; then
		missing_packages+=(build-essential)
	fi
	if ! command -v pkg-config >/dev/null; then
		missing_packages+=(pkg-config libssl-dev)
	fi
	if [[ "$AUBE_BOOTSTRAP_PARALLEL" == "1" ]] && ! command -v parallel >/dev/null; then
		missing_packages+=(parallel)
	fi
	if ((${#missing_packages[@]})); then
		apt-get update
		apt-get install -y --no-install-recommends "${missing_packages[@]}"
		rm -rf /var/lib/apt/lists/*
	fi
fi

if [[ "$AUBE_BOOTSTRAP_PARALLEL" == "1" ]] && command -v brew >/dev/null && ! command -v parallel >/dev/null; then
	brew install parallel
fi

mise install --locked
eval "$(mise activate bash --shims)"

if [[ "$AUBE_BOOTSTRAP_RUST" != "0" ]]; then
	if ! command -v rustup >/dev/null; then
		curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
			sh -s -- -y --profile default --default-toolchain "$RUSTUP_TOOLCHAIN"
	fi

	if ! rustup toolchain list | grep -Eq "^${RUSTUP_TOOLCHAIN}(-|$)"; then
		rustup toolchain install "$RUSTUP_TOOLCHAIN" --profile default
	fi
	if [[ "$AUBE_BOOTSTRAP_MSRV" == "1" ]] && ! rustup toolchain list | grep -Eq "^${MSRV_TOOLCHAIN}(-|$)"; then
		rustup toolchain install "$MSRV_TOOLCHAIN" --profile default
	fi
	rustup default "$RUSTUP_TOOLCHAIN"
	rustup component add rustfmt clippy --toolchain "$RUSTUP_TOOLCHAIN"
fi
hash -r
