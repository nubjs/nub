# System design docs

Long-form design records for subsystems where the mechanism is non-obvious and the discarded alternatives are worth keeping. Code comments link here instead of restating the reasoning.

## The build jail

Nub's **build jail** is the confinement applied to dependency lifecycle scripts during `nub install`. It has to be totally unprivileged with no setup command on every platform, and each OS reaches that with a different primitive.

Start with the first two. One says what the jail is and what it grants; the other says how the machinery that enforces it is put together.

| document | what it owns |
| --- | --- |
| [The build jail](build-jail.md) | Canonical for what the jail is, what it grants, how a grant is decided, and what happens when confinement is unavailable |
| [The sandbox engine](sandbox-engine.md) | The engine behind it: the compile/apply seam, the policy IR, how a backend is selected, and the fail-closed contract |

The per-OS mechanics then get one document each, because the primitives have nothing in common:

| document | mechanism |
| --- | --- |
| [Linux](build-jail-linux.md) | Landlock plus a seccomp socket-family filter |
| [macOS](build-jail-macos.md) | Seatbelt, via `sandbox-exec` |
| [Windows](build-jail-windows.md) | an AppContainer (LowBox) token |

Each is a ledger with one heading per approach tried: a status (ADOPTED, DEAD, OPEN, REJECTED), what the approach would have bought, the measurement that settled it, and what would have to change for it to become viable again. The dead ends are recorded deliberately — most were expensive to reach, and several were re-proposed after already being refuted.

Two documents sit above the ledgers, and two beneath them:

| document | question it answers |
| --- | --- |
| [Architecture](build-jail-architecture.md) | Is the shape those three share — a per-package allowlist pre-granted from a catalog — the right one at all? Judged against how BuildXL, Bazel, Chromium, Nix, Portage and the package managers themselves confine a build, in the same ledger form, one heading per candidate architecture |
| [Catalog generation](build-jail-catalog-generation.md) | How do we learn what a package actually needs? The measurement harness, and why the first answer was wrong in a way that took a long time to see |
| [Catalog criteria](build-jail-catalog-criteria.md) | How to judge whether a grant is right, whether a record is trustworthy, and what is worth chasing |

## Other subsystems

| document | what it covers |
| --- | --- |
| [Compiled executables](compiled-executables.md) | How `nub compile` assembles a single executable that runs with no Node and no `node_modules` on the target machine |
