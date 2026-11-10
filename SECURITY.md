# Security policy

## Reporting a vulnerability

Email [nub@colinhacks.com](mailto:nub@colinhacks.com). Don't open a public issue or pull request for a security problem.

Include:

- The output of `nub --version` and your OS
- How you installed Nub
- The smallest project or command that reproduces the problem
- What an attacker could do with it

If you can, check that the problem still happens on the latest release. [Upgrading](https://nubjs.com/docs#upgrading) shows how for each install channel.

## What this policy covers

- The `nub` CLI and its `nubx` and `nubr` commands, including the package manager in `vendor/aube`
- The `@nubjs/*` npm packages in `npm/`
- The install scripts `install.sh` and `install.ps1` that nubjs.com serves
- The GitHub Actions in `setup-node/`, `install/` and `npm-ci/`
- The `ghcr.io/nubjs/nub` Docker image, built from `docker/`

`vendor/aube/SECURITY.md` is written for the aube project and doesn't apply here.
