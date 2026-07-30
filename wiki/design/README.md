# System design docs

Long-form design records for subsystems where the mechanism is non-obvious and the discarded alternatives are worth keeping. Code comments link here instead of restating the reasoning.

The first three describe Nub's **build jail** — the confinement applied to dependency lifecycle scripts during `nub install`. It has to be totally unprivileged with no setup command on every platform, and each OS reaches that with a different primitive, so there is one document per OS:

| document | mechanism |
| --- | --- |
| [Linux](build-jail-linux.md) | Landlock plus a seccomp socket-family filter |
| [macOS](build-jail-macos.md) | Seatbelt, via `sandbox-exec` |
| [Windows](build-jail-windows.md) | an AppContainer (LowBox) token |

Each is a ledger with one heading per approach tried: a status (ADOPTED, DEAD, OPEN, REJECTED), what the approach would have bought, the measurement that settled it, and what would have to change for it to become viable again. The dead ends are recorded deliberately — most were expensive to reach, and several were re-proposed after already being refuted.

A fourth document sits above those three. [Architecture](build-jail-architecture.md) asks whether the shape they share — a per-package allowlist pre-granted from a catalog — is the right one at all, judged against how BuildXL, Bazel, Chromium, Nix, Portage and the package managers themselves confine a build. It uses the same ledger form, one heading per candidate architecture.
