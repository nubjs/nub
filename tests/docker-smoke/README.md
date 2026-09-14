# Docker install smoke — glibc + musl

This harness verifies the built Linux nub binary in a clean, dependency-free container — the honest first-run environment that the dev box (macOS) and `cargo test` cannot replicate.

## What this covers

| Check | Why it matters |
| --- | --- |
| Binary starts, `--version` returns semver | The binary is not a stub and links correctly against glibc/musl |
| `nub <file.ts>` transpiles + runs TypeScript | Augmentation layer is wired; the nub-native addon loaded |
| `nub run <script>` executes a package.json script | `compute_augmentation_env` NODE_OPTIONS path works end-to-end |
| `nub install` + module loads | PM engine boots and materializes a real package from registry |
| No `aube`/`jdx.dev` identity in output | Brand boundary holds in the final binary |

## The loop

1. `make` (or `cargo build --release`) on macOS produces a macOS binary. Run the Docker harness to get the Linux result instead.
2. `tests/docker-smoke/docker-smoke.sh` builds + runs both variants:
   - **glibc** — `Dockerfile.glibc`: `rust:1-bookworm` builder → `node:22-slim` runner
   - **musl** — `Dockerfile.musl`: `rust:alpine` builder → `node:22-alpine` runner
3. Each Dockerfile compiles nub inside the container (so the binary is the correct arch/libc for the runner stage), then `smoke.sh` runs five checks against it.

Run a single variant:
```sh
docker build -f tests/docker-smoke/Dockerfile.glibc -t nub-smoke:glibc .
docker run --rm nub-smoke:glibc
```

## Why two libc variants

nub links against the system C library. The musl binary (Alpine) is a different compile from the glibc binary (Debian), and they fail differently — a glibc binary silently crashes on Alpine due to symbol version mismatches. Both must be validated independently.

## CI gating

The `docker-smoke` CI job (`.github/workflows/docker-smoke.yml`) runs on every push that touches `crates/`, `runtime/`, or the harness itself. It is an ubuntu-latest job that calls both Dockerfiles in sequence, so it doubles as the Linux cross-platform gate for the binary.

The job is **not** in `ci.yml` because the builds are slow (Rust from scratch inside Docker) — keeping it in a separate workflow lets GitHub schedule it independently and lets the fast `ci.yml` jobs finish first. The path filter ensures it only fires when the binary or runtime could have changed.
