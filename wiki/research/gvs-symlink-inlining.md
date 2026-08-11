# GVS symlink inlining — can every store symlink fit inside its inode?

**Question (2026-08-11).** Under the global virtual store every `node_modules/<name>` entry is a symlink into `~/.cache/nub/pm/store`. On ext4 a symlink whose target fits in the inode costs zero data blocks; one byte longer allocates a full 4 KB block. Today's targets sit just over that line, so most deps pay 4 KB apiece. Can the store layout be changed so every target stays inline, and what does it cost?

**Verdict: reachable, at the price of a full break from the pnpm store layout.** A layout that hashes nothing and keeps `name@version` readable puts 99.5% of a real 411-package tree under the boundary, and the residual is bounded by existing code rather than left as a tail. The change surface is three items. The prize is small — roughly 4 KB per dep that flips, so single-digit MB on a large project — so this is worth landing alongside other linker work rather than on its own. Not implemented; this record is the design and its evidence.

> [!NOTE]
> Every measurement below was taken on a synthetic ext4 harness, not against a real `nub install`. The layout is verified to resolve correctly under Node, including scoped transitive dependencies, but no benchmark of an actual install exists yet.

## 1. The boundary is exactly 59 bytes

The ext4 inode carries a 60-byte `i_block` array. A symlink target that fits there with its terminator is stored inline; anything longer gets a data block. Measured directly:

```
target=58 chars  blocks=0  INLINE
target=59 chars  blocks=0  INLINE
target=60 chars  blocks=8  4KB BLOCK
target=61 chars  blocks=8  4KB BLOCK
```

The cliff is ext4-specific. Btrfs stores short targets as inline extents, APFS keeps them in the inode record or an xattr, and NTFS reparse points always allocate — so this design saves nothing on those filesystems and costs nothing either.

For contrast, a hardlink is only a directory entry: 1000 hardlinks to one file grew the directory from 4,096 to 28,672 bytes and left the file's block count untouched, or about 25 bytes per link. Store materialization is already effectively free; symlink targets are the one place a per-dep block cost appears.

## 2. Why today's targets overflow

A target decomposes into a climb to the store, the store prefix, the dep-path directory, the literal `node_modules` segment, and the package name:

```
../../../.cache/nub/pm/store/react@18.3.1/node_modules/react     60 chars → block
../../../.cache/nub/pm/store/debug@4.3.4/node_modules/debug      59 chars → inline
```

The fixed overhead is 34 characters before anything is named — 20 for the store prefix and 14 for `/node_modules/` — leaving 25 characters for the dep path plus the name once a typical three-level climb is counted. Median `name@version` in a real tree is 28, so most entries miss.

Two structural facts bound what can be recovered. The `node_modules` segment cannot be shortened, because Node's resolver looks for that literal directory name. The climb cannot be controlled, because it depends on how deep the user's project sits below their home directory.

> [!IMPORTANT]
> Whether GVS targets are absolute or relative today is unresolved. [`gvs-in-ci.md`](gvs-in-ci.md) describes them as absolute symlinks into the machine-global store; the `pathdiff::diff_paths` calls in [`vendor/aube/crates/aube-linker/src/link.rs`](../../vendor/aube/crates/aube-linker/src/link.rs) produce relative ones, but were not confirmed to be on the GVS branch. This design assumes relative. If they are absolute the case for changing is stronger, not weaker — an absolute `/home/<user>/.cache/nub/pm/store/` prefix leaves roughly 14 characters and essentially nothing stays inline.

## 3. The proposed layout

Three departures from pnpm: the package directory is named `p`, its dependencies live inside it rather than as siblings, and a `.s` hop symlink stands in for the climb at each level.

```
<store>/<entry>/p/                        # package files, hardlinked from the CAS
<store>/<entry>/p/node_modules/.s         → ../../..              # hop to the store root
<store>/<entry>/p/node_modules/<dep>      → .s/<entry2>/p
<store>/<entry>/p/node_modules/@sc/<dep>  → ../.s/<entry2>/p

<project>/node_modules/.s                 → <store>               # one long link per project
<project>/node_modules/<name>             → .s/<entry>/p
```

Here `<entry>` is `name@version` with the peer context dropped, and neither `.s` nor `p` can collide with a package because npm names cannot begin with a dot and `p` sits at a level no package name occupies.

The load-bearing property is that a symlink target's basename need not match the package name. Node finds `node_modules/lodash`, follows it, and then walks up from the **realpath** to locate that package's own dependencies — which lands on `p/node_modules/`. Verified end to end, resolving a plain and a scoped transitive dependency:

```
  15 chars blocks=0  .s/dep@1.0.0/p
  42 chars blocks=0  ../.s/@radix-ui+react-scroll-area@1.2.10/p
  19 chars blocks=0  .s/lodash@4.17.21/p

require('lodash') → lodash:dep:@radix-ui/react-scroll-area
require.resolve   → /store/lodash@4.17.21/p/index.js
```

That last line is why the entry keeps `name@version` rather than a hash: stack traces, `__dirname`, and profiler frames all still name the package and its version.

## 4. The budget, and what each variant buys

The tightest position is a scoped dependency inside the store — `../.s/` plus the entry plus `/p`, so 8 characters of overhead. Against the 59-byte ceiling that gives **entry ≤ 51**.

Four variants were measured against `site/pnpm-lock.yaml`, a Next.js and fumadocs tree with 411 unique entries, 53 of them carrying peer context. Median entry length is 28, p95 is 42, longest is 55.

| Variant | Budget | Entries keeping full `name@version` |
| --- | --- | --- |
| A — pnpm-style siblings, `p` leaf | 35 | 79.6% |
| B — A plus the store-level hop | 38 | 87.8% |
| C — dependencies inside the package directory | 45 | 97.3% |
| **D — C plus the hop (proposed)** | **51** | **99.5%** (2 of 411 over) |

The overflow tail is bunched immediately above each threshold rather than spread out, which is why the three characters variant B recovers move coverage by eight points.

## 5. The residual needs no new code

The function that encodes a dep path as a directory name already escapes forbidden characters, flattens peer parentheses to `_`, and truncates with a hash suffix when the result exceeds a cap — a bit-for-bit port of pnpm's own encoding, in [`vendor/aube/crates/aube-lockfile/src/dep_path_filename.rs`](../../vendor/aube/crates/aube-lockfile/src/dep_path_filename.rs). The entire change is its cap:

```rust
pub const DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH: usize = 120;  // → 51
```

At 51 the 33-byte hash tail leaves 18 characters of readable prefix, so an overflowing entry still opens with the package name. Uniqueness holds because the hash covers the full dep path including peer context.

## 6. Change surface

| # | Change | Where |
| --- | --- | --- |
| 1 | Package at `<entry>/p`, dependencies at `<entry>/p/node_modules/` | [`aube-linker/src/link.rs`](../../vendor/aube/crates/aube-linker/src/link.rs) |
| 2 | Hop symlinks: one per project `node_modules`, one per store entry | [`aube-linker/src/link.rs`](../../vendor/aube/crates/aube-linker/src/link.rs) |
| 3 | Virtual-store name cap 120 → 51 | [`aube-lockfile/src/dep_path_filename.rs`](../../vendor/aube/crates/aube-lockfile/src/dep_path_filename.rs) |

## 7. Open risks

- **Bundled dependencies.** Moving dependencies inside the package directory is the one real hazard, and it is why pnpm uses siblings: a tarball shipping its own `node_modules` now shares a directory with generated symlinks. The merge is semantically correct — bundled entries win for the names they cover, links fill the rest, which is what bundling means — but it needs a fixture before this ships.
- **Store verification.** A package directory is no longer a pure reflection of its tarball, so any integrity check assuming that has to account for the generated `node_modules`.
- **Migration.** A layout break means `reset_on_mode_change` wipes and relinks every existing tree on the first install after upgrade.
- **Path-derived package identity.** Tools that infer a package's name from its directory path — some bundlers, license scanners, patch tooling — see `p` instead of the package name. The `name@version` entry one level up is the mitigation, but the breakage class is real.
- **Absolute versus relative targets.** Unresolved, per §2. The design assumes relative and should be re-derived if that is wrong.

## 8. Rejected alternatives

- **Shortening the store prefix alone.** Recovers 13 characters at most and breaks the XDG cache convention; leaves the tail untouched.
- **Dropping the `node_modules` segment.** Not available — Node resolves against that literal name.
- **Hashing the entry outright.** Fits trivially but removes the package name and version from every path, which §3 exists to preserve.
- **Making the tree relocatable via the hop.** Not a benefit this design delivers. [`gvs-in-ci.md`](gvs-in-ci.md) §5(e) already established that relativizing cannot survive a multi-stage `COPY --from`, because the target sits outside the copied subtree. Concentrating the external pointer into one symlink leaves one dangling link instead of many.

## 9. Reproducing the measurements

The boundary probe creates symlinks of increasing target length and reads `stat -c '%b'`; the hardlink cost creates 1000 links to one file and diffs the directory size; the layout verification builds the §3 tree under a temporary directory and runs `node -e 'require("lodash")'` through it. Coverage comes from the `packages:` and `snapshots:` keys of `site/pnpm-lock.yaml`, with the peer-context parenthetical stripped and `/` escaped to `+` to match the encoder, counting entries at or under each budget.

## Changelog

- 2026-08-11 — Initial write-up. Established the 59-byte boundary empirically, verified that a target basename need not match the package name, measured four layout variants against a 411-entry tree, and identified the cap change that bounds the residual. Design only; nothing implemented.
