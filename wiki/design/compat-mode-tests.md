---
lat:
  require-code-mention: true
---

# Compat mode

The behaviors `--node` and `NODE_COMPAT` guarantee, each one mapped to the test that proves it. `lat check` fails if a spec below loses its test, so this file is a coverage map rather than a description. The mechanism itself is in [[architecture]].

Compat mode is the escape hatch that makes augmentation safe to ship: a user who hits a divergence can turn the whole layer off and get their installed Node back. That makes these four properties load-bearing, and each is verified by a differential run rather than by inspection.

## Flag form drops the augmentation layer

A top-level `--node` run performs zero augmentation, proven differentially: the default run eager-loads `.env`, and the `--node` run does not, because vanilla Node does not read `.env`.

Version provisioning deliberately stays on in compat mode and is not asserted here, because it is network-gated.

## Environment form applies tree-wide, including the node hijack

A truthy `NODE_COMPAT` is the persistent form of `--node`, so it must force vanilla Node on a direct `nub <file>` run and through the `node` PATH hijack alike, while an unset variable leaves the default augmented.

The hijack half is reached through an `argv0=node` symlink, so the test is Unix-only. `.env` eager-loading is again the discriminator.

## Compat mode never puts the loader in front of Node

Compat mode does not merely skip the hooks: it must not place the loader in front of Node at all, and must never invoke it. A stub loader that records each invocation stays unrecorded, and the variable it would export is absent.

## Compat mode reports node as the process title and argv0

A compat-mode process identifies itself as `node` in both its process title and `argv[0]`, matching the augmented path on Unix and plain Node on Windows.
