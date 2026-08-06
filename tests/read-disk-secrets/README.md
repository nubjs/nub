# What `read:"disk"` withholds — and the one class it does not

`./run.sh [path-to-nub]` — builds an override-enabled nub if no binary is given, then runs two arms against the real jail. Exit 0 means every arm produced a trustworthy verdict; exit 1 means an arm was VOID and its probes must not be read.

## The result, as of 2026-08-06

| probe | no catalog entry | under `read:"disk"` |
| --- | --- | --- |
| `.env` at an absolute path outside the project | DENIED | ⛔ **READ_OK** |
| plain file, same directory | DENIED | READ_OK |
| a `$HOME`-anchored subtree secret (Keychains / `.ssh` / `.aws` / `.gnupg`) | DENIED | **DENIED** |

⇒ `$HOME`-anchored **subtree** secrets stay withheld under `read:"disk"`. The **`.env*` basename class does not**, exactly as [`../../crates/nub-sandbox/src/compiler/preset.rs`](../../crates/nub-sandbox/src/compiler/preset.rs) states at the relaxation: a depth-independent basename has no finite allow-complement, and `enforce_pure_allowlist` drops every deny, so nothing catches it. That comment also names the fix — a per-backend rendering step — and **no backend has one**.

**Not reachable in what ships today.** The v1 catalog has no read-only-disk tier (`full_disk` is read AND write), so no compiled-in entry can express `read:"disk"`. It becomes reachable if a v2-derived catalog ships, where 22 corpus records sit on that rung (21 darwin, 1 linux, 0 win32).

## Why each probe is here

- **The `.env` probe is the finding.**
- **The plain file is the positive control.** Without it, `.env` being readable is equally consistent with the jail never having applied.
- ⛔ **The subtree secret is what BOUNDS the finding, and it is the probe most likely to be dropped as redundant.** Without it, "`.env` is readable" cannot be distinguished from "`read:"disk"` hands over the whole filesystem" — a far larger claim. Keep it.

## Why this is a shell harness and not a `tests/*.rs` case

Reaching the rung requires a catalog grant, and the only route is `NUB_BUILD_JAIL_CATALOG`, behind the dev-only `nub-cli/build-jail-catalog-override` feature — which CI does not build. A Rust test would either not compile the seam or not run. This harness builds the feature itself and works from a clean checkout with no CI change.

## Two confounds it exists to avoid

Both have already produced wrong readings in this project:

1. **A fixture under `/tmp` cannot test a denial at all.** The jail redirects `TMPDIR` into a private per-package directory, so a "secret" placed there is inside the grant by construction. Fixtures here live under `$HOME`.
2. **A reused package name replays a prior run's script.** `~/.cache/nub/jail-home/` accumulates an entry per name, and a repeat can surface an unrelated script's output. A first attempt at this differential hit exactly that: had it only checked for the ABSENCE of a leak marker, both arms would have read "denied" and the conclusion would have been inverted. Every arm now uses a unique name **and** prints a marker proving the intended script ran; a missing marker is a hard failure, never a quiet pass.

## Reading a failure

- `HARNESS ERROR: the postinstall never ran` — not a verdict. Something upstream broke; the probes below it mean nothing.
- `HARNESS ERROR: the override did not engage` — the arm measured the SHIPPED policy, not the requested grant. A malformed override warns and falls back silently, which is why `OVERRIDDEN`/`REJECTED` are asserted rather than assumed.
- `SUBTREE READ_OK` — the allow-complement broke. Bigger than the `.env` band; investigate before anything else.
- `ENVFILE DENIED` — the per-backend rendering step landed. Good news: update this harness and the `wiki/design/build-jail-*.md` claims that currently record the band as open.
