# Full-disk READ with WRITE confined, on Linux

Whether nub's Linux build jail can grant a lifecycle script broad filesystem READ while keeping WRITE confined — the `read:"disk"` rung of the capability catalog. **It can.** Landlock separates read from write at the ABI level, the separation was verified against a real kernel with both arms and an unconfined control, and the rung now works end-to-end. Two defects stood between the design and that outcome; both are fixed and both are covered by a test that fails without the fix.

All measurements are from `nub-linux` (GCE, Ubuntu 24.04, e2-standard-4), kernel **6.17.0-1021-gcp**, **Landlock ABI v7**, running unprivileged as an ordinary user.

## The ABI answer: read and write are separate rights

Landlock's filesystem access mask is a bitfield of independent rights, so "read everywhere, write nowhere" is directly expressible: one rule on `/` carrying `LANDLOCK_ACCESS_FS_READ_FILE | READ_DIR | EXECUTE` (`0xd`), plus narrower rules carrying the write bits (`WRITE_FILE`, `MAKE_*`, `REMOVE_*`, `TRUNCATE`, `REFER`).

A standalone C probe against the raw syscalls — no nub code, no libraries beyond libc — confirmed it. Same binary, two arms, the only difference being whether `landlock_restrict_self` was called:

| probe | unconfined control | `/` read rule + one narrow read-write rule |
| --- | --- | --- |
| read a file outside the write set | OK | **OK** |
| `readdir` outside the write set | OK | **OK** |
| read `/etc/passwd` | OK | **OK** |
| write / create / mkdir outside the write set | OK | **`EACCES`** |
| read and write inside the narrow rule | OK | OK |

Write confinement holds against every ordinary mutation route, not just `open(O_WRONLY)`. Thirteen vectors were tried against a path outside the writable rule; all were refused under enforcement and all succeeded in the control:

`open(O_WRONLY)` · `open(O_RDWR)` · `open(O_RDONLY|O_TRUNC)` · `truncate` · `unlink` · `mkdir` · `rename` · `link` (refused as `EXDEV`) · write through a symlink created inside the writable rule · `open` via `../` traversal out of the writable rule · `openat` through an `O_PATH` directory descriptor · `chdir` followed by a relative create.

Two results in that battery are worth stating explicitly rather than leaving implicit:

- **Creating a symlink inside the writable rule is allowed**, and correctly so — the link is a write to a directory the policy grants, and its target is arbitrary text. Writing *through* it is denied.
- **`chmod` and `utimes` on an outside file are allowed even under enforcement.** Landlock mediates no metadata hook at any ABI; this matches the existing note in `crates/nub-sandbox/src/backend/linux_landlock.rs` citing `landlock-lsm/linux#11`. Metadata rewriting outside the write set is bounded by DAC and by the seccomp `deny_metadata` filter, not by Landlock.

The first control run of this battery was void and was discarded: an early `unlink` succeeded, so every later vector ran against a file that no longer existed and reported `ENOENT`, which reads as a denial. The probe now re-creates the target between destructive vectors, which is what makes the table above a genuine positive control.

## One rule on `/` works, and it cannot be clawed back

A single `/` rule is sufficient for the read half, and it is dramatically cheaper than enumerating the disk. It is also unusable on its own, because it re-exposes every secret.

A decoy planted at `~/.ssh/probe_decoy` was **read successfully** under the one-`/`-rule policy. Adding a second rule on a nested directory granting only `EXECUTE` did **not** reduce that: the read still succeeded. Landlock rules union and there is no deny primitive at any ABI, so a nested rule can only ever add rights. **There is no way to grant read on `/` and withhold `~/.ssh`.**

That is the whole justification for `defaults::disk_minus_secrets_read_allows` walking the disk and naming the non-secret parts positively. The enumeration is not redundant work that a single `/` rule could replace — it is the only form the exclusion can take.

A related question settles the shape of that walk: **a rule on a deep path works with no rule on any ancestor.** In a probe granting only `<root>/a/b/c/deep`, that file read fine while the parent directory, `/etc/passwd`, and everything else were denied. Landlock evaluates the resolved path against the rule set and needs no traversal right on intermediate directories. The ancestor entries the walk emits are therefore needed only so those directories can be *listed*, not so their children can be reached.

## Rule-count ceiling: not Landlock's problem

Landlock imposes no practical ceiling. Rules were added in bulk against distinct inodes and every size succeeded with zero failures — no `ENOMEM`, no `E2BIG`:

| rules | ruleset build | `landlock_restrict_self` | per allowed `open` |
| --- | --- | --- | --- |
| 0 (unconfined) | — | — | 4.31 µs |
| 1 | 0.0 ms | 0.008 ms | 6.23 µs |
| 1,001 | 10.9 ms | 0.39 ms | 6.74 µs |
| 10,001 | 66.4 ms | 4.41 ms | 6.35 µs |
| 50,001 | 327.8 ms | 23.4 ms | 6.48 µs |
| 100,001 | 680.1 ms | 47.9 ms | 6.50 µs |
| 200,001 | 1,388 ms | 95.1 ms | 6.46 µs |

Build cost is linear at roughly 6.9 µs per rule, which is three syscalls (`open(O_PATH)`, `fstat`, `landlock_add_rule`). **Enforcement cost per open does not grow with rule count at all** — it is flat from 1 rule to 200,001. The ~2 µs overhead comes from having a ruleset, not from its size. Every arm in that table opens a path the ruleset allows, so the numbers compare like with like; an earlier version measured denied opens and was not comparable.

The rule counts the walk actually produces on Linux are far below anything that matters:

| project shape | rules |
| --- | --- |
| project directly under `$HOME` | 163 |
| project three levels down | 163 |
| `$HOME` under a tempdir, `/tmp` holding ~380 entries | 390 |
| `$HOME` under a tempdir, `/tmp` freshly emptied | 46 |

The count is proportional to the sibling count of every ancestor of `$HOME`, which is why the last two rows differ by a factor of eight on the same machine — the only variable was how much `/tmp` held. So the walk is unbounded in principle, and a macOS box measured 36,579 entries in `/var/folders/*/*/T`, which is where that platform's ~35,000-rule figure comes from. On an ordinary Linux layout `$HOME` is two levels deep and the walk stays in the low hundreds.

**The real ceiling was nub's own mount planner, not the kernel.** See below.

## What was wrong in nub, and what changed

`read:"disk"` was previously inert on Linux: the relaxation emitted a bare `**`, `compile_mount_plan` classified it as a whole-root read, dropped it, and emitted no grant. The move to concrete disk-minus-secrets paths removed that silent drop and exposed two hard failures underneath.

### The walk named paths the mount planner refuses

`descend_allowing_all_but` grants every child of `/` that does not lead to a secret, which includes the kernel-virtual trees. `linux_grants::is_reserved_tree` refuses `/proc`, `/sys` and `/dev` outright, and the Linux build jail is Landlock-or-nothing and fail-closed — so the rung did not merely under-grant, it stopped the script running at all. Measured with a jailed lifecycle script before the fix, in nub's own words:

```
the dependency build jail could not COMPILE its policy on this host — this is a nub bug,
not a missing kernel feature:
PolicyNotExpressible("filesystem allow under reserved kernel tree /dev is not permitted")
```

A second instance of the same class turned up alongside it: a directory whose **name carries a glob metacharacter**. The walk emits an unescaped literal and the planner demands a bounded one, so one real directory called `weird[1]name` anywhere the walk enumerates produced `cannot be represented by a bounded literal mount plan` and took the whole rung down with it.

The fix is one guard in the walk, `is_unrepresentable_grant`, which skips both. Skipping costs one directory's readability; emitting costs the entire grant, and that asymmetry is why it fails toward omission. Omitting the kernel trees is also correct on security grounds independent of the mechanism: reading `/proc` wholesale exposes every same-uid process's `environ` and `cmdline`, which is exactly what the eight specific files in `linux_landlock::PROC_READ_PATHS` exist to avoid granting. Nothing is lost, because the device nodes and `/proc` files a toolchain needs are granted as explicit leaf rules by the backend. The reserved-tree list now lives in one place, `defaults::RESERVED_KERNEL_TREES`, read by both the walk that omits it and the planner that refuses it.

### The mount planner was quadratic

`compile_mount_plan` ran a deny-shadow scan for every rule, and that scan iterates the whole rule set — so the function was O(n²) in glob matches, on top of compiling n globs to build the matcher. At the low-hundreds counts of an ordinary Linux layout this is invisible. At the 36,579-rule shape it is a hang, not a slowdown: the test that exercises it ran for over a minute without finishing.

A rule set carrying no `Deny` cannot shadow anything, so the scan and the matcher construction are both skippable outright rather than approximated — and the build jail is exactly that rule set by construction, because `enforce_pure_allowlist` strips every deny as the last step of its compile. Guarding on `has_denies` is exact, not heuristic. The same 36,579-rule case now compiles in 3.16 s.

### Test coverage

The gap that hid both defects was structural rather than an oversight in either file. The compiler's own tests assert on the emitted allow-set and never build a mount plan; the planner's tests use hand-written fixtures and never consume the compiler's whole-disk output. Neither side could see the seam between them.

`linux_grants::tests::the_read_disk_allow_set_compiles_to_a_mount_plan` closes it by running one into the other. It carries a positive control — an ordinary non-secret sibling must still be granted — so an emitter that returned nothing cannot pass it, and it fails on an unfixed tree for both of the reasons above.

## End-to-end proof

A real dependency with a `postinstall` script, jailed by a real `nub install` on the Landlock backend. Four arms, same binary and same fixture, differing only in the catalog — except arm D, which is the same probe run directly by node with no jail at all. The probe body is inlined via `node -e`: a probe in a separate file reports `EACCES` on *itself*, because the base jail correctly refuses to read a script outside its read set, and every arm then reads as "no marker".

The secret decoy is planted in the home nub was told about, at a path hardcoded into the probe. `os.homedir()` inside the jail returns a throwaway home and would look somewhere else entirely.

| arm | read outside the write set | write outside it | read the `~/.ssh` decoy | write artifact on disk |
| --- | --- | --- | --- | --- |
| A — no catalog entry (base jail) | `EACCES` | `EACCES` | `EACCES` | absent |
| **B — `read:"disk"`** | **OK** | **`EACCES`** | **`EACCES`** | **absent** |
| C — `write:"disk"` | OK | OK | OK | present |
| D — unjailed control | OK | OK | OK | present |

Arm B is the result: broad read, confined write, secrets withheld. Arm D proves the probe can perform all three operations when nothing stops it, so arm B's two denials are enforcement rather than a broken fixture. Arm C shows what the rung replaces — `write:"disk"` reads the decoy and lands the write, because it applies no filesystem confinement at all. Every write claim is checked against the artifact on disk, not the exit code.

Before the fix, arm B produced no marker at all: the policy failed to compile and the script never ran.

## Scope: this rung is override-gated today

`read:"disk"` reaches only the v2 catalog path, which is behind the `build-jail-catalog-override` cargo feature. A shipped binary compiled without that feature uses the v1 curated table, which has no read rung — only `full_disk`. So this work makes the rung correct in the measurement harness, which is what the grant search needs in order to find a narrower answer than `write:"disk"`, and it will carry over unchanged when v2 ships.

## Re-measuring the eight Linux `write:"disk"` packages

Every Linux `write:"disk"` grant in the catalog was reached by escalation: the package needed broad read, the read rung did nothing, and the ladder climbed past it to the only rung that worked. None was shown to need whole-disk write. With the rung live they can be re-measured.

Status: **in progress at the time of writing, and not yet complete.** The eight are being re-run through the project's own scorer, `tests/build-jail-search/search.mjs`, with the fixed binary and `--force`, rather than through a hand-rolled verdict — the harness runs the control twice and compares the stable intersection of produced paths, which a single-arm reimplementation would get wrong. The packages are `@nuxt/components@2.1.0`, `@opencode-ai/cli@0.0.0-next-16573`, `@tensorflow/tfjs-backend-wasm@1.4.0-alpha2`, `codeceptjs@1.1.3`, `dotnet-2.0.0@1.4.4`, `iedriver@4.0.0`, `postman-code-generators@2.1.1` and `react-native-purchases@1.5.4`.

This section is to be completed with the per-package verdicts and the count that narrowed.

## Reproducing

The probes are standalone C and shell, and none of them needs nub to be built:

- `llprobe.c` — read/write separation, the nested-reduce question, and the deep-rule question. Modes `off`, `read-slash`, `nested-reduce`, `deep-only`.
- `llescape.c` — the thirteen-vector mutation battery, modes `off` and `on`.
- `llcount.c` — rule-count ceiling and per-open cost, given a file of paths.
- `readdisk-probe.sh` — the four-arm end-to-end proof; takes a path to a nub built with `--features nub-cli/build-jail-catalog-override`.

## Changelog

- 2026-08-05 — Initial write-up. Landlock read/write separation confirmed on ABI v7 with both arms and an unconfined control; one `/` read rule shown to work and shown to be unusable alone because it re-exposes secrets irrecoverably; no Landlock rule-count ceiling found to 200,001 rules with flat per-open cost; two nub defects found and fixed (the walk naming reserved kernel trees and glob-metacharacter names, and the quadratic mount planner); `read:"disk"` proved end-to-end on Linux. The eight-package re-measure is outstanding.
