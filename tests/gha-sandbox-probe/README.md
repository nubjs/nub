# GHA hosted-runner sandbox feasibility probe

Branch-scoped probe (no PR) answering: does nub's Linux sandbox work on
GitHub-**hosted** Actions runners — single-level and nested — and what is the
**simplest** setup that makes nested `bwrap` work on a hosted image?

Runs on push to `probe/gha-sandbox-feasibility` via
`.github/workflows/gha-sandbox-probe.yml`. Harvest results by grepping job logs
for `RESULT:<key>=PASS|FAIL|SKIP` and `DETAIL:<key>=...`.

## Tiers

- `diagnostics.sh` — kernel/os, userns+apparmor sysctls, `aa-status`, lockdown,
  then: `userns-smoke`, `custom-apparmor-load` (trivial abi/3.0),
  `nub-apparmor-load` (the real abi/4.0 `nub-bwrap-userns.apparmor`),
  `single-level` (stock bwrap), `nested-baseline` (stock bwrap nested, no fix).
- `probe-nesting.sh <mode>` — applies ONE candidate workaround on a fresh runner,
  then tests a NESTED bwrap launch:
  - `baseline` — control, no workaround
  - `sysctl-disable` (A) — `sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`
  - `complain-knob` (B) — `...userns_complain=1`
  - `aa-complain` (C) — neutralize the stock bwrap AppArmor profile
  - `setuid` (D) — setuid-root bwrap, global restriction left ON
  - `d5-helper` — raw replication of the D5 dedicated-helper + path-bound profile
- container jobs (E) — nested bwrap inside `container: ubuntu:24.04`, plain /
  `--cap-add=SYS_ADMIN` / `--privileged`.
- `nub-single-level-tests` — best-effort: nub's real `linux_enforcement` test with
  `NUB_SANDBOX_REQUIRE_BWRAP=1`.

Read-only w.r.t. production code — adds only this probe + the workflow.
