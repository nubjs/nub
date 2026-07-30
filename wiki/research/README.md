# Sandbox research

Investigations behind Nub's build jail — the confinement applied to dependency lifecycle scripts during an install. The per-OS design ledgers live in [`../design/`](../design); this directory holds the research that fed them.

Each document records what was asked, how it was measured, and what the answer was, with a changelog at the bottom. A verdict that later moved is corrected in place and dated rather than rewritten.

## Prior art and mechanism

| Document | What it establishes |
| --- | --- |
| [sandbox-prior-art](sandbox-prior-art.md) | Whether a standard cross-platform sandboxing library exists, and why not |
| [sandbox-os-enforceability](sandbox-os-enforceability.md) | The capability-by-platform matrix: what each OS can actually enforce unprivileged |
| [sandbox-linux-deny-mechanisms](sandbox-linux-deny-mechanisms.md) | Linux in-place filesystem denial, mechanism by mechanism, and which suits which case |
| [sandbox-linux-userns-backend](sandbox-linux-userns-backend.md) | An optional namespace backend, and what it would close that Landlock cannot |
| [sandbox-linux-confinement-audit](sandbox-linux-confinement-audit.md) | Landlock and seccomp as actually applied |
| [sandbox-policy-provenance-patterns](sandbox-policy-provenance-patterns.md) | How other systems stop a confined process rewriting its own policy |
| [sandbox-carve-grant-set](sandbox-carve-grant-set.md) | Resolving allow and deny globs to a minimal grant set, and the crate survey behind the hand-roll |

## Network

| Document | What it establishes |
| --- | --- |
| [sandbox-private-range-egress](sandbox-private-range-egress.md) | What other sandboxes do about private-range egress by default |
| [sandbox-net-config-surfaces](sandbox-net-config-surfaces.md) | What a user can actually write to configure network policy |
| [sandbox-windows-net-parity](sandbox-windows-net-parity.md) | Why there is no unprivileged path to per-host egress on Windows |
| [sandbox-u5-mitm-security-audit](sandbox-u5-mitm-security-audit.md) | Independent security re-audit of the credential-brokering tier |

## Filesystem and environment

| Document | What it establishes |
| --- | --- |
| [sandbox-macos-version-matrix](sandbox-macos-version-matrix.md) | Whether the environment-read closure holds across macOS versions |
| [sandbox-move-rename-bypass](sandbox-move-rename-bypass.md) | The per-OS verdict on relocating a secret out of a denied path |
| [sandbox-glob-deny-fidelity](sandbox-glob-deny-fidelity.md) | Fidelity of the glob-to-matcher translation on the deny path |
| [sandbox-os-essentials-env](sandbox-os-essentials-env.md) | Which environment variables the OS itself needs on a strip-all floor |

## Executable axis

| Document | What it establishes |
| --- | --- |
| [sandbox-exec-allowlist](sandbox-exec-allowlist.md) | Whether an executable allowlist is a viable confinement axis |
| [sandbox-exec-disk-needs](sandbox-exec-disk-needs.md) | Which common tools fail when only their binary and libraries are readable |

## Grammar, structure and validation

| Document | What it establishes |
| --- | --- |
| [sandbox-cross-platform-grammar-audit](sandbox-cross-platform-grammar-audit.md) | Whether one policy grammar can mean the same thing on three operating systems |
| [sandbox-crate-structure](sandbox-crate-structure.md) | Structuring the engine so it is reusable outside Nub |
| [sandbox-pentest-macos](sandbox-pentest-macos.md) | An adversarial agent given free rein inside the macOS jail, and what it could not do |
| [build-jail-virgin-world](build-jail-virgin-world.md) | Running an install in a pristine OS and copying the result back |
