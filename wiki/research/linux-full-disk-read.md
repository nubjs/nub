# Full-disk READ with WRITE confined, on Linux

Whether nub's Linux build jail can grant a lifecycle script broad filesystem READ while keeping WRITE confined — the `read:"disk"` rung of the capability catalog. **It can.** Landlock separates read from write at the ABI level, the separation was verified against a real kernel with both arms and an unconfined control, and the rung now works end-to-end. Two defects stood between the design and that outcome; both are fixed and both are covered by a test that fails without the fix.

The follow-on question — is walking the disk and naming its non-secret parts the RIGHT way to express this, or are we fighting the API? — is settled the same way. **Enumerating the complement is the intended shape.** Landlock has no deny primitive at any ABI, stacking rulesets does not supply one, upstream's own written guidance is to name leaves rather than broad ancestors, and the one peer project with the same problem either grants the whole disk with no exclusion at all or leaves Landlock for a mount namespace. Details below.

Measurements come from two boxes, both GCE, both unprivileged as an ordinary user:

| box | kernel | Landlock ABI | what it measured |
| --- | --- | --- | --- |
| `nub-linux` | 6.17.0-1021-gcp (Ubuntu 24.04) | v7 | read/write separation, rule-count ceiling, the end-to-end jailed install |
| `nub-ll` | 7.0.0-1008-gcp (Ubuntu 26.04) | v8 | ruleset stacking, the empty rule, the walk's residuals |

The stacking and residual batteries were run on both boxes and agree row for row, so nothing below turns on a single kernel.

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

## Stacking a second ruleset does not subtract a path

Rulesets stack, and stacking is intersection: "a sandboxed thread can only access a file path if all its enforced policy layers grant the access" ([`landlock.rst`, "Layers of file path access rights"](https://docs.kernel.org/userspace-api/landlock.html)). That reads like a subtraction primitive, so it was the most promising untested idea. It is not one — an intersection of allow-unions is still an allow-union.

Measured on ABI v8 with `llstack.c`. Every arm starts from the same layer 1 (one `/` read rule plus one narrow read-write rule) and differs only in what layer 2 does. `off` is the unconfined control:

| mode | layer 2 | read secret | read a normal file | list `~/.ssh` | rules |
| --- | --- | --- | --- | --- | --- |
| `off` | — (unconfined) | OK | OK | OK | 0 |
| `L1` | — (one layer) | **OK** | OK | OK | 2 |
| `L2-slash` | repeat the `/` read grant | **OK** | OK | OK | 3 |
| `L2-zero` | name the secret with an empty access set | **OK** | OK | OK | 3 |
| `L2-exec` | name the secret with `EXECUTE` only | **OK** | OK | OK | 4 |
| `L2-dironly` | grant only `READ_DIR` on `/` | `EACCES` | **`EACCES`** | OK | 3 |
| `L2-noread` | handle `READ_FILE`, grant it nowhere | `EACCES` | **`EACCES`** | OK | 2 |
| `L2-enum` | enumerate the complement | `EACCES` | OK | OK | 33 |

Every row reproduced identically on the ABI v7 box, so none of this is a one-kernel artefact. The only figure that moves is the rule count of the enumerating arm, which tracks how many entries the host's `$HOME` and its ancestors hold — 33 against the sparse fixture home above, 174 against a populated one.

Three results carry the whole answer:

- **A second layer cannot name a path to take it away.** `L2-exec` puts a rule on the secret directory carrying fewer rights than the layer's own `/` rule, and the secret stays readable. Rules union within a layer, and a more specific rule does not win — the same result the ABI v7 session got from a nested rule inside one ruleset, now confirmed across a layer boundary.
- **An empty rule is not a deny.** Adding a rule with `allowed_access = 0` fails with **`ENOMSG`** (errno 42). Upstream's UAPI header accepts an empty `allowed_access` in exactly one place — alongside `LANDLOCK_ADD_RULE_QUIET` (ABI v10) — and that flag is log suppression, not access control: "a sandboxed program cannot use this flag to 'hide' access denials, without denying itself the access in the first place" ([`include/uapi/linux/landlock.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/landlock.h)).
- **What a layer CAN subtract is a right, globally.** `L2-dironly` and `L2-noread` do remove `READ_FILE` — from the entire domain, `/etc/passwd` included. Layering is path-blind: it narrows the right, never the path.

`L2-enum` is the only arm that produces the wanted outcome, and it does so by running the same complement walk one layer later. **Stacking moves the enumeration; it does not remove it.** A single-layer walk (`nub-shape`, 32 rules) and the two-layer version (`L2-enum`, 33 rules) give identical results, so the extra layer is pure cost.

## No deny primitive, and no ABI version that adds one

This is settled upstream, not merely absent so far. Landlock's stated model is deny-by-default plus allow rules, and its author's own description of the API calls those rules exceptions *to* restrictions, never subtractions *from* grants:

> Landlock is a deny-by-default access control, but with a fixed set of access rights for compatibility reasons. […] Each ruleset handles a set of restrictions, and additional rules can add exceptions to these restrictions.
> — Mickaël Salaün, [*Landlock: From a security mechanism idea to a widely available implementation*](https://landlock.io/talks/2024-06-06_landlock-article.pdf), §6.2

Two design principles in the same paper foreclose the obvious workarounds. A Landlock policy "cannot define the error codes returned by system calls […] nor change the kernel interface semantic" (§5.5), and it "is not programmable" and cannot "communicate with user space" (§5.5) — so there is no callback, no eBPF hook, and no in-kernel predicate that could evaluate an exclusion. The kernel's own filesystem guidance points the same way, and it is a direct endorsement of the enumeration:

> It is recommended to set access rights to file hierarchy leaves as much as possible. For instance, it is better to be able to have `~/doc/` as a read-only hierarchy and `~/tmp/` as a read-write hierarchy, compared to `~/` as a read-only hierarchy and `~/tmp/` as a read-write hierarchy.
> — [`Documentation/userspace-api/landlock.rst`](https://docs.kernel.org/userspace-api/landlock.html), "Good practices"

Every ABI version through the newest documented in mainline adds rights or logging, never an exception mechanism:

| ABI | added | relevant? |
| --- | --- | --- |
| v1 | filesystem rights, `path_beneath` rules | the baseline |
| v2 | `LANDLOCK_ACCESS_FS_REFER` | no |
| v3 | `LANDLOCK_ACCESS_FS_TRUNCATE` | no |
| v4 | `LANDLOCK_ACCESS_NET_{BIND,CONNECT}_TCP` | no |
| v5 | `LANDLOCK_ACCESS_FS_IOCTL_DEV` | no |
| v6 | `LANDLOCK_SCOPE_*` (signals, abstract UNIX sockets) | no — and scoping "does not support exceptions via `landlock_add_rule(2)`" |
| v7 | audit-logging flags | no |
| v8 | `LANDLOCK_RESTRICT_SELF_TSYNC` (multithreaded enforcement) | no |
| v9 | `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` (pathname UNIX sockets) | no |
| v10 | UDP network rights, `LANDLOCK_ADD_RULE_QUIET` | no — log suppression only |

The newest ABI reachable on a probe box is v8 — `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` returns 8 on kernel 7.0.0-1008-gcp, so the stacking results above were run against everything up to and including that version. The per-version contents of the table are read from mainline's `landlock.rst` and UAPI header rather than exercised one by one, and v9 and v10 were not run at all. The full filesystem right set in mainline today tops out at bit 16, and every bit in it is a capability to grant.

**Verdict: enumeration is not a workaround for a missing feature. It is the shape the API is designed around, and nothing on the ABI is going to change that.**

## What everyone else does

The projects with nub's exact problem — sandbox an untrusted process that still needs to read most of the machine — split two ways, and neither way is a cleverer Landlock policy.

**OpenAI's Codex CLI does not attempt the exclusion at all.** Its Landlock backend builds one rule for the whole disk and hard-refuses any policy that is not full-disk read:

```rust
// codex-rs/linux-sandbox/src/landlock.rs
if !file_system_sandbox_policy.has_full_disk_read_access() {
    return Err(CodexErr::UnsupportedOperation(
        "Restricted read-only access is not supported by the legacy Linux Landlock filesystem backend."
            .to_string(),
    ));
}
// …
.add_rules(landlock::path_beneath_rules(&["/"], access_ro))?
```

That is arm `L1` above, secrets and all. Codex has since demoted the whole path — the module's own header now reads "Filesystem restrictions are enforced by bubblewrap in `linux_run_main`. Landlock helpers remain available here as legacy/backup utilities", and the flag that selects it is `--use-legacy-landlock`.

**The tools that do express "everything except X" express it with a mount namespace, not an LSM.** Codex's bubblewrap backend documents the fork directly — "use `--ro-bind / /`; other restricted-read policies start from `--tmpfs /` and layer scoped `--ro-bind` mounts" — and masks a subtree by overmounting a `tmpfs` on it. Bubblewrap and Flatpak build a sandbox from an empty tmpfs root and bind in what is needed; systemd's `ProtectHome=`, `InaccessiblePaths=` and `ReadOnlyPaths=` are the same primitive under service-manager names. Overmounting is the real subtraction operator on Linux, and it is a namespace feature.

That option is closed here. The build jail is Landlock-or-nothing by an explicit decision recorded in [`crates/nub-sandbox/src/backend/linux.rs`](../../crates/nub-sandbox/src/backend/linux.rs) — bubblewrap needs a user namespace, which is not universally available unprivileged. Codex works around that by relying on setuid bubblewrap deployments, which is a dependency on a host binary nub does not want. So the namespace option is not an oversight on our side; it is a different availability bet.

**No Landlock userspace library offers an exclusion helper.** Neither the official [Rust](https://github.com/landlock-lsm/rust-landlock) nor [Go](https://github.com/landlock-lsm/go-landlock) binding has an "all but" or "exclude" construct; go-landlock's configurable example calls its *allow* rules "exceptions", matching the kernel's vocabulary. Their onboarding guidance is entirely about naming the narrowest set of paths a program needs.

## What the enumeration costs

Two residuals, both measured, neither fatal, one of them worth a decision.

**A secret directory stays listable.** The walk grants each ancestor of a secret a node-only read, which `linux_grants` compiles to `MountAccess::ListOnly` and `linux_landlock` renders as `LANDLOCK_ACCESS_FS_READ_DIR`. Landlock rights are inherited by everything beneath the path they are attached to and cannot be attached to a directory without its subtree, so a `READ_DIR` grant on `$HOME` is also a `READ_DIR` grant on `~/.ssh`. Filenames leak; contents do not. Measured against a populated `$HOME`, mirroring nub's shape — which is also why the counts land near the 163 recorded below rather than the sparse-fixture figures in the stacking table:

| walk shape | read `~/.ssh/id_rsa` | list `~/.ssh` | list `$HOME` | read a granted file | rules |
| --- | --- | --- | --- | --- | --- |
| `nub-shape` — ancestors granted `READ_DIR` | `EACCES` | **OK** | OK | OK | 173 |
| `nub-noancestor` — ancestor rules dropped | `EACCES` | **`EACCES`** | `EACCES` | OK | 169 |

This is the exact residual already accepted on Windows, where `TRAVERSE_MASK` includes `FILE_LIST_DIRECTORY`: recon, not exfiltration. It is also deliberate rather than an oversight — `LandlockAccess::ListDir`'s own doc records the reasoning. The second row is the available trade: dropping the ancestor rules closes the leak and costs the ability to `readdir` `$HOME` and every other ancestor of a secret, while every granted file stays readable (a deep rule needs no ancestor rule). Whether a build ever lists `$HOME` is the open question, and it is a product call rather than a mechanism one.

**The compile-time snapshot races, and it races in the safe direction for new paths.** Rules are attached to inodes, so a rule on a directory covers whatever appears beneath it later — measured with `llrace.c`, where a file created after `landlock_restrict_self` inside a granted directory reads back fine in both the confined and unconfined arms. The two halves of the race are therefore:

- **A path the walk never named stays denied**, however it comes to exist. The battery carries a directory deliberately withheld from the walk, standing in for one created after the policy compiled: reading a file inside it is `EACCES` in every confined arm and `OK` in the unconfined control. A new top-level entry in `$HOME` is not readable until the next compile. That is an under-grant, and it fails closed.
- **A secret that appears inside an already-granted subtree is readable.** A `.env` written into a directory the walk cleared is covered by that directory's rule. This is the unsafe half, and no enumeration strategy avoids it — it is the same exposure a `--ro-bind` would have.

**Rule count is not a residual.** Nothing cheaper exists: the complement of a subtree, written as a union of subtrees, is exactly the siblings along the secret's ancestor chain. Any rule broad enough to cover two of those siblings would have to sit on a common ancestor, and every common ancestor of two siblings is also an ancestor of the secret. So the walk's output is minimum-cardinality for the reachability half, and the ancestor `READ_DIR` entries are the one band above the minimum — which is what the second row of the table above removes.

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

The rule counts the walk actually produces on Linux are far below anything that matters. These were measured with a script mirroring the walk, then validated against the real Rust on a shared fixture — 2,255 from `disk_minus_secrets_read_allows` against 2,256 from the mirror, the difference being exactly the one reserved-tree entry the fix now skips:

| project shape | rules |
| --- | --- |
| project directly under `$HOME` | 163 |
| project three levels down | 163 |
| `$HOME` under a tempdir, `/tmp` holding ~380 entries | 390 |
| `$HOME` under a tempdir, `/tmp` freshly emptied | 46 |

Those figures predate the fix and each includes three reserved-tree grants (`/proc`, `/sys`, `/dev`), so the post-fix counts are three lower.

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

The working hypothesis was that every Linux `write:"disk"` grant existed only because the package needed broad read, the read rung did nothing, and the ladder climbed past it to the only rung that worked — so none had been shown to need whole-disk write. With the rung live, all eight were re-measured through the project's own scorer, `tests/build-jail-search/search.mjs`, with the fixed binary and `--force`. The harness runs its control twice and compares the stable intersection of produced paths, which is why it is used here rather than a single-arm reimplementation.

**The hypothesis is refuted. None of them narrowed.**

| package | verdict | minimum | what it needs to write |
| --- | --- | --- | --- |
| `@nuxt/components@2.1.0` | MINIMUM | `write.disk` (20) | `~/.config/yarn/link/` |
| `@tensorflow/tfjs-backend-wasm@1.4.0-alpha2` | MINIMUM | `write.disk + network` (23) | `~/.cache/yarn/v6/…` |
| `dotnet-2.0.0@1.4.4` | MINIMUM | `write.disk + network` (23) | `~/.cache/yarn/v1/…`, `~/.net` |
| `postman-code-generators@2.1.1` | MINIMUM | `write.disk + network` (23) | `~/.cache/yarn/v6/…` |
| `iedriver@4.0.0` | MINIMUM | `write.disk + network` (23) | `~/.cache/nub/pm/trust-policy-v1/…`, `$proj/node_modules/.nub-engine` |
| `react-native-purchases@1.5.4` | MINIMUM | `write.disk + network` (23) | inside its own store path under `$proj/node_modules/.store/…` |
| `codeceptjs@1.1.3` | BROKEN-WITHOUT-JAIL-TOO | — | fails unjailed as well; environmental, not a grant question |
| `@opencode-ai/cli@0.0.0-next-16573` | no measurement | — | exceeded the 1800 s per-package cap (`rc=124`); needs a longer cap to answer |

The result is trustworthy in the direction that matters most: in every one of the six real measurements the `read.disk` cell **failed with `overrideEngaged: true`**. The rung was live and being exercised — it was simply not enough. This is not the old inert-rung failure wearing a new face.

**The actual cause of the escalation is a write need, and it is mostly one shared cause: these packages shell out to `yarn`, which writes its own global cache.** Four of the six are blocked on `~/.cache/yarn` or `~/.config/yarn`. No read grant of any breadth can satisfy that.

That leaves a genuine open question, and it is a different defect from the one this document fixes: **why does the ladder escalate past `write.userHome` (cost 7) all the way to `write.disk` (20)?** A write to `~/.cache/yarn` is by definition a write to the user home. The `write.userHome` cell failed too, and the records carry a `pathsLandingInThrowawayHome` field, which points at the answer — the jail hands the script a throwaway `HOME`, so a `write.userHome` grant covers that throwaway directory rather than the real path a nested `yarn` resolves and writes. Closing that would move this whole family from cost 20 to cost 7 and is worth more than anything remaining on the read axis.

## Reproducing

The probes are standalone C and shell, and none of them needs nub to be built:

- `llprobe.c` — read/write separation, the nested-reduce question, and the deep-rule question. Modes `off`, `read-slash`, `nested-reduce`, `deep-only`.
- `llescape.c` — the thirteen-vector mutation battery, modes `off` and `on`.
- `llcount.c` — rule-count ceiling and per-open cost, given a file of paths.
- `llstack.c` + `llrun.sh` — the stacking battery, the empty rule, and the two walk shapes. Modes `off`, `L1`, `L2-slash`, `L2-zero`, `L2-exec`, `L2-dironly`, `L2-noread`, `L2-enum`, `nub-shape`, `nub-noancestor`, plus `abi`.
- `llrace.c` — whether a directory rule covers files created after enforcement. Modes `off` and `on`.
- `readdisk-probe.sh` — the four-arm end-to-end proof; takes a path to a nub built with `--features nub-cli/build-jail-catalog-override`.

## Changelog

- 2026-08-05 (later) — Settled the IDIOM question. **Enumerate-the-complement is correct and there is no better mechanism.** Stacked rulesets do not subtract a path (measured on ABI v8: a second layer naming the secret with fewer rights leaves it readable; an empty `allowed_access` is `ENOMSG`; a layer can only subtract a right globally). No deny primitive at any ABI v1–v10, with the design rationale cited to Salaün's 2024 article and the kernel's own "Good practices" endorsing leaf-level grants. Prior art: Codex's Landlock backend refuses any non-full-disk-read policy and has been demoted to legacy behind bubblewrap; the tools that do express "all but X" do it by overmounting in a mount namespace, which nub's Landlock-or-nothing decision rules out. Two measured residuals recorded — a secret directory stays listable through the ancestor `READ_DIR` grants (closable by dropping them, at the cost of `readdir($HOME)`), and the compile-time snapshot under-grants rather than over-grants for paths created afterwards.
- 2026-08-05 — Initial write-up. Landlock read/write separation confirmed on ABI v7 with both arms and an unconfined control; one `/` read rule shown to work and shown to be unusable alone because it re-exposes secrets irrecoverably; no Landlock rule-count ceiling found to 200,001 rules with flat per-open cost; two nub defects found and fixed (the walk naming reserved kernel trees and glob-metacharacter names, and the quadratic mount planner); `read:"disk"` proved end-to-end on Linux. The eight-package re-measure **refuted** the premise that those grants were artefacts of the inert read rung — none narrowed, six have a real write need (mostly a nested `yarn` writing its own global cache), one is broken unjailed, one exceeded the time cap. The follow-on question is why `write.userHome` does not cover those writes.
