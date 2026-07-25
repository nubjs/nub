#!/bin/sh
# rustc-qos — machine-global cargo rustc-wrapper: run every rustc at utility QoS
# on darwin so Rust builds always yield to interactive work, no matter which
# entry point invoked cargo. The make/rust-build.sh clamps (6ba6d2f8a0,
# 0ae792acf5) cover only their own entry points; direct `cargo build/test`
# bypassed them and stacked full-priority builds across the agent fleet
# (2026-07-24: 35 of 63 rustc processes at default priority). Installed by
# `make qos-global` into ~/.cargo (config.toml rustc-wrapper -> a stable copy at
# ~/.cargo/rustc-qos.sh). It is deliberately NOT a tracked in-repo
# .cargo/config.toml: a sh wrapper there would break Windows (CI legs +
# contributors), and machine-global also covers stale worktrees and file://
# clones that predate any commit. An uncontended build still gets all cores —
# utility QoS only yields under pressure. NUB_BUILD_FG=1 opts out. Toggling the
# wrapper does not invalidate cargo fingerprints (verified 2026-07-24), so mixed
# wrapped/unwrapped builds share a target dir without rebuild churn.
if [ "${NUB_BUILD_FG:-}" != "1" ] && [ "$(uname)" = "Darwin" ] \
  && command -v taskpolicy >/dev/null 2>&1; then
  exec taskpolicy -c utility "$@"
fi
exec "$@"
