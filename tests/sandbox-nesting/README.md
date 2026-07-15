# sandbox-nesting — Linux direct-nesting gate proof

End-to-end proof of the Linux direct-nesting candidate gate (epic item D5): the dedicated helper is selected at level 1, its integrity is verified, a bounded nested launch confirms the host can nest, and every broken-host case fails closed with a precise diagnostic.

A `cargo test` cannot change host ownership, permissions, or the loaded AppArmor profile, so the fail-closed cases live here. `prove.sh` drives the whole matrix with `sudo` and asserts each scenario's diagnostic, then restores the host on exit. The happy path — a set-up host admits the helper and a nested launch succeeds — is also a committed cargo test (`crates/nub-sandbox/tests/linux_nesting.rs`) that skips with a printed reason off a set-up host.

## Running it

The harness needs an AppArmor-restricted Ubuntu-family host with passwordless `sudo`, `cargo`, and a packaged Bubblewrap 0.11.2. It skips with a printed reason otherwise — it never silently passes.

```sh
# provide a prebuilt packaged Bubblewrap, or let the harness build one
NUB_BWRAP=/path/to/nub-resources/bwrap \
CARGO_TARGET_DIR=/tmp/nub-sandbox-target \
  tests/sandbox-nesting/prove.sh
```

## What it proves

Each row runs the `linux_nesting` gate against a deliberately configured host and checks the diagnostic.

| Scenario | Host state | Expected outcome |
| --- | --- | --- |
| Admit + nested launch | correct setup | the level-1 sandbox launches and a nested launch succeeds |
| Mis-owned helper | helper `chown`ed off root | rejected — integrity check failed |
| Group-writable helper | helper `chmod 0770` | rejected — integrity check failed |
| Profile unloaded | `apparmor_parser -R` | fails closed — the helper's AppArmor profile is not loaded |
| Missing group access | caller not in the group | denied — not in the nub-sandbox group |

The harness restores ownership, permissions, and the loaded profile on exit.

## Proven on

Ubuntu 24.04.4, kernel 6.8.0-117, `apparmor_restrict_unprivileged_userns=1`, with the reproducibly-built static Bubblewrap 0.11.2 (`scripts/build-bwrap-resource.sh`) installed as the helper. All five scenarios pass and the host is left unchanged.
