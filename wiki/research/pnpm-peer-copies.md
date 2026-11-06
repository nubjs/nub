# How many copies of a package pnpm installs

A package with peers gets one directory per distinct set of resolved peers, so the count decides how many module objects a project loads. This records what keeps pnpm's count low, and how Nub's compatibility database stays out of it.

Read from pnpm's source, in `pnpm11/installing/deps-resolver/src` and `pnpm/crates/resolving-deps-resolver/src` at `github.com/pnpm/pnpm`, and checked with lockfile-only fixtures on pnpm 10.27, 11.28 and 12.17.

## Binding from the path

A peer binds to the nearest provider above the package: an ancestor, the importer, then the workspace root.

Which provider a package sees depends on how it was reached, so one package can bind differently on different paths. Each distinct result is a separate entry in the lockfile, written `name@version(peer@version)`. `resolvePeersFromParents.ts` does the binding, and an out-of-range provider still binds, with a warning.

## Hoisting a missing peer

A peer no parent provides is hoisted to the importer from the versions the graph already holds, and required peers and peers some package bound from a parent go before optional ones.

`hoistPeersToImporters` in `resolveRootDependencies.ts` repeats these steps until a round hoists nothing, since a hoisted provider's own peers can then be missing too:

1. Required peers, until none is left, each at the highest version that satisfies every range.
2. `addResolvedPeerProviders`: the provider of a peer that a package bound from a parent becomes a provider for the rest of the importer. When the optional step below hoists another version of the same name, this one stays.
3. One round of `getHoistableOptionalPeers` from `hoistPeers.ts`, again the highest version that satisfies every range. A candidate is passed over when the importer provides one of its own peers at a version outside the range (`peersAcceptProvidedVersions`).

The whole sequence runs when either `auto-install-peers` or `dedupe-peer-dependents` is on. Step 2 needs `auto-install-peers`, and so does the graph-wide search for missing required peers behind step 1, which otherwise sees only the peers reported up from a package's own path.

The order decides the version. `styled-jsx` declares an optional `@babel/core` that accepts anything, and `@angular/compiler-cli` brings 8.0.1. pnpm 11.28 and 12.17 agree on all three of these:

| Beside `styled-jsx` and `@angular/compiler-cli` | `styled-jsx` binds |
| --- | --- |
| Nothing else | `@babel/core@8.0.1` |
| `babel-plugin-styled-components`, whose `@babel/plugin-syntax-jsx` requires `^7` with nothing providing it | `@babel/core@7.29.13` |
| `@vitejs/plugin-react`, which holds 7 and has plugins binding it | `@babel/core@7.29.13` |

This is what removes most copies, because a package no longer differs by whether its path happened to include the provider:

| Project | pnpm 10 | pnpm 11 and 12 |
| --- | --- | --- |
| `my-bad` and `vitest`, where only `vitest` brings `vite` | `my-bad@0.2.11(vite@7.3.7)` | `my-bad@0.2.11(vite@7.3.7)` |
| `debug` and `chalk`, where only `chalk` brings `supports-color` | `debug@4.4.1` | `debug@4.4.1(supports-color@7.2.0)` |

`debug` names `supports-color` only in `peerDependenciesMeta`, which is where the majors differ.

## Collapsing a subset

With `dedupe-peer-dependents`, a variant whose dependencies and resolved peers all appear in a larger variant of the same package collapses into it.

`deduplicateDepPaths.ts` takes the variants from largest to smallest and `depPathCompatibility.ts` decides whether one fits in another. A dependency that differs must be a variant of the same package that fits in turn, so a child bound to another version of a peer keeps two variants apart. Ties go to the greater key, so a variant that fits two incompatible larger ones lands on the same choice on every machine. This catches what the hoist refuses: with `my-bad` in member `a` and `my-bad` plus `vite@5.4.0` in member `b`, `vite` is outside `my-bad`'s `>=6` range, so nothing is hoisted for `a`, and all three pnpm majors still write the single `my-bad@0.2.11(vite@5.4.0)`.

## What still splits

A peer the package itself declares splits it across consumers on different versions, and this is correct: each consumer needs its own version beside the package.

Two members on `react` 18 and 19 get two copies of `@tanstack/react-query`, and two members on `vite` 6 and 7 get two copies of `my-bad`, under pnpm and under Nub alike.

A peer's own peer splits a package the same way. Two members on `terser` 5.30 and 5.31 beside `vite` get two copies of `@vitejs/plugin-basic-ssl`, one bound to each `vite`, since a single copy would hand one member's plugin the other member's `vite`.

## A repair pnpm does not know about

An optional peer from Nub's compatibility database repairs an undeclared import, and pnpm has no such entry, so it must not add a copy pnpm would not install.

Applied as a peer it would: two members on `typescript` 5.8.3 and 5.9.3 would split `@typescript-eslint/types` in two, and neither hoisting nor collapsing helps, because the versions genuinely differ. So [[vendor/aube/crates/aube-resolver/src/peer_context.rs#database_peer_targets]] takes these entries out of the peer set. It runs the path binding once to see which targets each context of the package holds, and the highest of them becomes an optional edge. The package keeps one identity, and the target is a sibling exactly where a peer would have made it one.

The edge is added after the peer pass. In the pass it would be a path pnpm does not have: `nx` holds `supports-color` 7 and has an entry for `@nx/js`, so `debug` under `@nx/js` would bind 7 while every other `debug` binds the 8 the hoist chose.

Binding from the path rather than from anywhere in the graph is deliberate. An edge to a target that only sits on a cousin's path stops the package's phantom eject, which takes the target's `@types` package out of reach: `gradient-string` importing `tinycolor2` goes from resolved to TS7016. It also revives packages platform filtering would drop, such as `@img/sharp-wasm32` under `next`. Removing the database's type-only entries does not reach copies split by a package's own optional peers, and it grows the set of ejected packages.

A lockfile cannot say whether an optional edge is this repair or a dependency the package declares, since both are written the same way. When an entry names the edge's target, the edge stays while the target stays in the graph. Otherwise the package's manifest decides: an undeclared edge is the repair going stale, and it is bound again from the path.

An edge like this moves without changing the package's key, so a store directory an earlier install left is not current just because it exists. The linker brings its dependency links in line with the graph when it reuses the directory.

## Changelog

Revision history.

- 2026-11-05 — Initial publication.
