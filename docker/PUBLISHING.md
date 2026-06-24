# Publishing the official nub Docker images

Maintainer runbook for `.github/workflows/docker.yml`. The workflow builds and
smoke-tests both variants (slim + alpine) on every PR and push to `main`, and
publishes a multi-arch image (`linux/amd64` + `linux/arm64`) **only on a release
tag** and **only once a registry is wired up**. The registry and image name are a
maintainer choice — nothing is hardcoded to publish until you set the variable
below. Pick **one** of the two options.

## Tag scheme

| Variant | Tags pushed |
|---|---|
| slim (default) | `latest`, `<version>`, `slim`, `<version>-slim` |
| alpine | `alpine`, `<version>-alpine` |

`<version>` is the release tag with the leading `v` stripped (e.g. `0.1.14`).

---

## Option A — Docker Hub (`nubjs/nub`)

1. **Create the org + repo.** On Docker Hub, create the `nubjs` organization, then a
   repository named `nub` (so the image is `nubjs/nub`). Set it public.
2. **Create an access token.** Docker Hub → Account Settings → Personal access
   tokens (or an org service account) → New token, **Read & Write** scope. Copy it.
3. **Add the GitHub Actions secret + variables** (repo Settings → Secrets and
   variables → Actions):
   - Secret `DOCKERHUB_TOKEN` = the token from step 2.
   - Variable `DOCKERHUB_USERNAME` = the Docker Hub account/org that owns the token.
   - Variable `DOCKER_IMAGE` = `nubjs/nub`.
   - Leave `DOCKER_REGISTRY` **unset** (defaults to Docker Hub).
4. **Publish.** Cut a release the normal way (`make version V=<ver>` → commit → tag
   `v<ver>` → push tag). The `release: published` event triggers `docker.yml`, which
   logs in with `DOCKERHUB_USERNAME` + `DOCKERHUB_TOKEN` and pushes the tags above.
5. **Verify.**
   ```sh
   docker run --rm nubjs/nub nub --version
   docker run --rm nubjs/nub:alpine nub --version
   docker buildx imagetools inspect nubjs/nub:latest   # confirm amd64 + arm64 in the manifest
   ```

---

## Option B — GHCR (`ghcr.io/nubjs/nub`)

No extra secret — the built-in `GITHUB_TOKEN` plus `permissions: packages: write`
(already set in the workflow) is sufficient.

1. **Add the variables** (repo Settings → Secrets and variables → Actions → Variables):
   - Variable `DOCKER_IMAGE` = `ghcr.io/nubjs/nub`.
   - Variable `DOCKER_REGISTRY` = `ghcr.io`.
   - (No `DOCKERHUB_*`; the workflow falls through to `github.actor` + `GITHUB_TOKEN`.)
2. **Publish.** Cut a release as above. The tag-triggered run logs in to `ghcr.io`
   with the built-in token and pushes.
3. **Make the package public** (first publish only): GHCR → the `nub` package →
   Package settings → Change visibility → Public. (Optional but expected for an
   official image; without it, pulls require auth.)
4. **Verify.**
   ```sh
   docker run --rm ghcr.io/nubjs/nub nub --version
   docker buildx imagetools inspect ghcr.io/nubjs/nub:latest
   ```

---

## Publishing to BOTH registries

The workflow targets a single `DOCKER_IMAGE`. To publish to both Docker Hub and
GHCR, the cleanest change is to extend the matrix (or add a second push step) with
a second image name + login. Not wired by default — decide if you actually want two
registries before adding the complexity.

## Notes

- **PRs never push.** The push steps are gated on `github.event_name == 'release'`,
  so a fork PR or a `main` push only builds + smokes. The `DOCKER_IMAGE` guard is a
  second backstop: with it unset, even a release no-ops the push.
- **Base image digests** are pinned in `docker/Dockerfile.{slim,alpine}`. Refresh
  them when you want a newer `node:26-*` base (`docker buildx imagetools inspect
  node:26-slim` → copy the index `Digest`).
- **Provenance + SBOM** attestations are attached on push (`provenance: true`,
  `sbom: true`). They add a manifest entry per image; harmless for consumers.
