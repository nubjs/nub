#!/usr/bin/env bash
set -euo pipefail

linux_queue="${BUILDKITE_LINUX_QUEUE:-linux}"
linux_image="${BUILDKITE_LINUX_IMAGE:-aube-linux-rust-stable}"
macos_queue="${BUILDKITE_MACOS_QUEUE:-macos}"

cat <<YAML
env:
  CARGO_TERM_COLOR: always
  CARGO_HOME: "/tmp/aube-cache/cargo"
  CARGO_INCREMENTAL: "0"
  CARGO_TARGET_DIR: "/tmp/aube-cache/cargo-target"
  MSRV_TOOLCHAIN: "1.93.0"
  MISE_CACHE_DIR: "/tmp/aube-cache/mise-cache"
  MISE_DATA_DIR: "/tmp/aube-cache/mise"
  MISE_EXPERIMENTAL: "true"
  PARALLEL_HOME: "/tmp/aube-cache/parallel"
  RUSTUP_TOOLCHAIN: "stable"

steps:
  - group: ":linux: linux"
    key: "linux"
    steps:
      - label: "linux / build"
        key: "linux-build"
        agents:
          queue: "${linux_queue}"
          image: "${linux_image}"
        timeout_in_minutes: 10
        cache:
          name: "cargo-linux-build"
          size: "40g"
          paths:
            - "/tmp/aube-cache/cargo"
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
            - "/tmp/aube-cache/cargo-target"
        commands:
          - "source .buildkite/bootstrap.sh"
          - "mise run build"
        artifact_paths:
          - "target/debug/aube"

      - label: "linux / test"
        key: "linux-test"
        agents:
          queue: "${linux_queue}"
          image: "${linux_image}"
        timeout_in_minutes: 30
        cache:
          name: "cargo-linux-test"
          size: "40g"
          paths:
            - "/tmp/aube-cache/cargo"
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
            - "/tmp/aube-cache/cargo-target"
        commands:
          - "export AUBE_BOOTSTRAP_MSRV=1; source .buildkite/bootstrap.sh"
          - "mise run test"
          # Pipe through cat to break the PTY Buildkite allocates — without it
          # hk thinks stderr is interactive and emits ~10k spinner-frame lines
          # that Buildkite captures as log rows after stripping the cursor
          # escapes. Kept at the pipeline layer (not in mise.toml) so local
          # runs of mise run lint keep their animated UI. set -o pipefail is
          # scoped to this command because Buildkite runs each commands:
          # list entry in its own shell — the pipefail from bootstrap.sh
          # does not carry over, and without it cat's zero exit would mask
          # a failing hk check.
          - "set -o pipefail; mise run lint 2>&1 | cat"
          - "mise run render"
          - "mise run docs:build"

      - label: "linux / bats 1/4"
        key: "linux-bats-0"
        depends_on: "linux-build"
        agents:
          queue: "${linux_queue}"
          image: "${linux_image}"
        timeout_in_minutes: 20
        retry:
          automatic:
            limit: 1
        cache:
          name: "mise-linux-bats"
          size: "40g"
          paths:
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
        commands:
          - "export AUBE_BOOTSTRAP_RUST=0 AUBE_BOOTSTRAP_PARALLEL=1; source .buildkite/bootstrap.sh"
          - 'buildkite-agent artifact download --step "linux-build" "target/debug/aube" .'
          - "chmod +x target/debug/aube"
          - "AUBE_CI_OS=linux AUBE_BATS_SHARD_INDEX=0 AUBE_BATS_SHARD_COUNT=4 mise run ci:bats:nonserial"

      - label: "linux / bats 2/4"
        key: "linux-bats-1"
        depends_on: "linux-build"
        agents:
          queue: "${linux_queue}"
          image: "${linux_image}"
        timeout_in_minutes: 20
        retry:
          automatic:
            limit: 1
        cache:
          name: "mise-linux-bats"
          size: "40g"
          paths:
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
        commands:
          - "export AUBE_BOOTSTRAP_RUST=0 AUBE_BOOTSTRAP_PARALLEL=1; source .buildkite/bootstrap.sh"
          - 'buildkite-agent artifact download --step "linux-build" "target/debug/aube" .'
          - "chmod +x target/debug/aube"
          - "AUBE_CI_OS=linux AUBE_BATS_SHARD_INDEX=1 AUBE_BATS_SHARD_COUNT=4 mise run ci:bats:nonserial"

      - label: "linux / bats 3/4"
        key: "linux-bats-2"
        depends_on: "linux-build"
        agents:
          queue: "${linux_queue}"
          image: "${linux_image}"
        timeout_in_minutes: 20
        retry:
          automatic:
            limit: 1
        cache:
          name: "mise-linux-bats"
          size: "40g"
          paths:
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
        commands:
          - "export AUBE_BOOTSTRAP_RUST=0 AUBE_BOOTSTRAP_PARALLEL=1; source .buildkite/bootstrap.sh"
          - 'buildkite-agent artifact download --step "linux-build" "target/debug/aube" .'
          - "chmod +x target/debug/aube"
          - "AUBE_CI_OS=linux AUBE_BATS_SHARD_INDEX=2 AUBE_BATS_SHARD_COUNT=4 mise run ci:bats:nonserial"

      - label: "linux / bats 4/4"
        key: "linux-bats-3"
        depends_on: "linux-build"
        agents:
          queue: "${linux_queue}"
          image: "${linux_image}"
        timeout_in_minutes: 20
        retry:
          automatic:
            limit: 1
        cache:
          name: "mise-linux-bats"
          size: "40g"
          paths:
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
        commands:
          - "export AUBE_BOOTSTRAP_RUST=0 AUBE_BOOTSTRAP_PARALLEL=1; source .buildkite/bootstrap.sh"
          - 'buildkite-agent artifact download --step "linux-build" "target/debug/aube" .'
          - "chmod +x target/debug/aube"
          - "AUBE_CI_OS=linux AUBE_BATS_SHARD_INDEX=3 AUBE_BATS_SHARD_COUNT=4 mise run ci:bats:nonserial"

      - label: "linux / bats serial"
        key: "linux-bats-serial"
        depends_on: "linux-build"
        agents:
          queue: "${linux_queue}"
          image: "${linux_image}"
        timeout_in_minutes: 20
        retry:
          automatic:
            limit: 1
        cache:
          name: "mise-linux-bats"
          size: "40g"
          paths:
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
        commands:
          - "export AUBE_BOOTSTRAP_RUST=0; source .buildkite/bootstrap.sh"
          - 'buildkite-agent artifact download --step "linux-build" "target/debug/aube" .'
          - "chmod +x target/debug/aube"
          - "AUBE_CI_OS=linux mise run ci:bats:serial"

  - group: ":mac: macos"
    key: "macos"
    steps:
      - label: "macos / test"
        key: "macos-test"
        agents:
          queue: "${macos_queue}"
        env:
          RUSTUP_HOME: "/tmp/aube-cache/rustup"
        timeout_in_minutes: 20
        cache:
          name: "cargo-macos-test"
          size: "40g"
          paths:
            - "/tmp/aube-cache/cargo"
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
            - "/tmp/aube-cache/rustup"
            - "/tmp/aube-cache/cargo-target"
        commands:
          - "source .buildkite/bootstrap.sh"
          - "mise run build"
          - "mise run test"
        artifact_paths:
          - "target/debug/aube"

      - label: "macos / bats 1/2"
        key: "macos-bats-0"
        depends_on: "macos-test"
        agents:
          queue: "${macos_queue}"
        timeout_in_minutes: 20
        retry:
          automatic:
            limit: 1
        cache:
          name: "mise-macos-bats"
          size: "40g"
          paths:
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
        commands:
          - "export AUBE_BOOTSTRAP_RUST=0 AUBE_BOOTSTRAP_PARALLEL=1; source .buildkite/bootstrap.sh"
          - 'buildkite-agent artifact download --step "macos-test" "target/debug/aube" .'
          - "chmod +x target/debug/aube"
          - "AUBE_CI_OS=macos AUBE_BATS_SHARD_INDEX=0 AUBE_BATS_SHARD_COUNT=2 mise run ci:bats:nonserial"

      - label: "macos / bats 2/2"
        key: "macos-bats-1"
        depends_on: "macos-test"
        agents:
          queue: "${macos_queue}"
        timeout_in_minutes: 20
        retry:
          automatic:
            limit: 1
        cache:
          name: "mise-macos-bats"
          size: "40g"
          paths:
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
        commands:
          - "export AUBE_BOOTSTRAP_RUST=0 AUBE_BOOTSTRAP_PARALLEL=1; source .buildkite/bootstrap.sh"
          - 'buildkite-agent artifact download --step "macos-test" "target/debug/aube" .'
          - "chmod +x target/debug/aube"
          - "AUBE_CI_OS=macos AUBE_BATS_SHARD_INDEX=1 AUBE_BATS_SHARD_COUNT=2 mise run ci:bats:nonserial"

      - label: "macos / bats serial"
        key: "macos-bats-serial"
        depends_on: "macos-test"
        agents:
          queue: "${macos_queue}"
        timeout_in_minutes: 20
        retry:
          automatic:
            limit: 1
        cache:
          name: "mise-macos-bats"
          size: "40g"
          paths:
            - "/tmp/aube-cache/mise"
            - "/tmp/aube-cache/mise-cache"
            - "/tmp/aube-cache/parallel"
        commands:
          - "export AUBE_BOOTSTRAP_RUST=0; source .buildkite/bootstrap.sh"
          - 'buildkite-agent artifact download --step "macos-test" "target/debug/aube" .'
          - "chmod +x target/debug/aube"
          - "AUBE_CI_OS=macos mise run ci:bats:serial"

YAML
