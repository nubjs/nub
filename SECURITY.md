# Security policy

## Reporting a vulnerability

Report it privately through [GitHub's vulnerability reporting form](https://github.com/nubjs/nub/security/advisories/new). Don't open a public issue or pull request for a security problem.

Include:

- The output of `nub --version` and your OS
- How you installed Nub
- The smallest project or command that reproduces the problem
- What an attacker could do with it

Fixes ship in a new release, and older releases aren't patched, so check that the problem still happens on the latest one. [Upgrading](https://nubjs.com/docs#upgrading) shows how for each install channel.

## What this policy covers

- The `nub` CLI and its `nubx` and `nubr` commands, including the package manager in `vendor/aube`
- The `@nubjs/*` npm packages in `npm/`
- The install scripts `install.sh` and `install.ps1` that nubjs.com serves
- The GitHub Actions in `setup-node/`, `install/` and `npm-ci/`
- The `ghcr.io/nubjs/nub` Docker image, built from `docker/`
