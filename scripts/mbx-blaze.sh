#!/usr/bin/env sh
# Reserve MBX scheduler capacity while Blaze v2 builds outside Cargo.
set -eu

mbx=${MBX_BIN:-mbx}
blaze=${BLAZE_BIN:-blaze}
cpus=${MBX_BLAZE_CPUS:-2}

exec "$mbx" reserve --cpus "$cpus" -- "$blaze" "$@"
