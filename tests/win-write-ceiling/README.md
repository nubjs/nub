# win-write-ceiling — how much disk can a LowBox token be granted, unelevated?

The shipping full-disk grant tier renders on Windows by **declining the LowBox token**, because
there is no ACE that means "everything". Declining the token also declines the AppContainer
capability set, and coarse egress denial *is* a withheld capability (`internetClient`) — so a
full-disk package on Windows reaches the network whether or not the catalog admits it to. This
probe measures whether a **maximum-grantable** tier exists that keeps the token, and therefore
keeps the filesystem and network axes separable.

## Run it

Branch-scoped, no PR: push to `probe/win-lowbox-write-ceiling` and
`.github/workflows/win-write-ceiling-probe.yml` fires on `windows-latest` and `windows-11-arm`.
CI is the only venue — a LowBox token cannot attach a window station in services session 0, so
every launch over SSH returns `0xC0000142` and the standing `nub-win` VM cannot answer this
(MECHANISM-FACTS §5e/§5h). No Rust build: PowerShell plus a C# P/Invoke launcher, a few minutes
a run instead of 35.

## What it measures, and why each part is not optional

| section | question |
| --- | --- |
| 1 | Where can an unelevated user write a DACL at all? An ACE nub cannot write is a tier it cannot build. |
| 2 | Does an inheritable ACE reach files that already exist without a propagating tree walk? This is the cost of a broad grant. |
| 3 | Five real launches, one variable apart: `plain` (no token — what full-disk does today), `ac-bare`, `ac-leaf` (today's tier), `ac-max` (the ceiling), `ac-max-net`. |
| 4/5 | The per-target-class table, and the controls that make it readable. |

**The runner is elevated and a user is not.** Every DACL-write row is taken twice — once as the
runner, once under an impersonated de-elevated restricted token (`Administrators` deny-only, all
privileges bar `SeChangeNotify` dropped). The elevated row exists only as the control that makes
the de-elevated refusal attributable to privilege rather than to a bad path. `ac-max` is then
built from the **measured** de-elevated reach, so it is the most a real user could grant by
construction rather than by guess.

**A uniformly-denied matrix is usually a broken harness**, which has cost this effort real runs.
So: `plain` must succeed at everything a user can do; every confined arm must prove it is confined
(`C:\` listing refused); every arm must reach user code (`child:done`); and each arm's ACEs are
**read back off a pre-existing deep file's own DACL**, because a propagation slip and a kernel
denial are otherwise indistinguishable. `ac-max-net` is the separability control: the same token
plus `internetClient`, so `ac-max`'s egress denial is attributable to the withheld capability
rather than to confinement in general.

**The child must reach user code.** An unflagged confined `node` dies in `resolveMainPath`'s
realpath before running a line (§5h), which renders every op a denial.
`--preserve-symlinks-main` repairs the entry point, and `child.js` imports only `node:`-prefixed
builtins, which never enter `_findPath` — so no realpath shim is needed and the tree-wide
`--preserve-symlinks` (rejected: it silently binds a different package version under the
`Isolated` linker) is not used.

`child.js` also contains no `child_process`: a piped `spawnSync` never returns under an
AppContainer and swallows every op after it, presenting as a timed-out arm rather than a failed
op.
