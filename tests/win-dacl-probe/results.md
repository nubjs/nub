# Results

## Run 1 — 2026-09-03, six `windows-latest` shards

```
static  failures=0/40   x 6 shards
dynamic failures=0/40   x 6 shards
```

**480 iterations, zero failures.** Every path component was reported stable on every shard, including
the runner-created `D:\`, `D:\a` and `D:\a\_temp`.

### What this establishes

- **The CRT-linkage hypothesis is not supported.** 240 static iterations behaved identically to 240
  dynamic ones against a verbatim copy of the shipped predicate. This does not fully clear the static
  CRT — the probe exercises the security check, not the whole of nub's startup — but it removes the
  only mechanism that had been proposed.
- **The runner's own directories are not the problem.** They passed 480/480 across six VMs, so the
  refusal seen in CI is on a component nub itself creates, not on an ancestor the image supplied.

### What this does NOT establish

Per the rule in the README: both linkages clean everywhere means **the reproduction does not capture
the failure**. Nothing further may be read into it. In particular this is not evidence that the CI
failure was a fluke, only that this staging does not provoke it.

### What the reproduction is missing, in likely order

1. **Defender.** The failing CI step explicitly enables real-time monitoring
   (`Set-MpPreference -DisableRealtimeMonitoring $false`) before the round-trip. The probe inherits the
   image default and does not touch it.
2. **Concurrency.** The real step runs a binary that extracts a payload and spawns a child; the probe
   walks a path in a single thread with nothing else touching those directories. A race in which
   `create_private_directory` loses to another creator and returns `AlreadyExists` — leaving a
   directory with inherited ACEs that the walk then judges — is not staged here at all.
3. **Volume of filesystem activity.** The real run writes a whole extracted runtime under the leaf.

A next iteration should stage 2 directly: several threads racing to create the same chain while the
walk validates it. That is a sharper experiment than adding more single-threaded iterations, which run
1 has already shown saturate at zero.

## Run 2 — the bare-`create_dir_all` arm, six shards

```
5 shards: static 0/40, dynamic 0/40
1 shard:  static 40/40, dynamic 40/40
```

The failing shard's UNSTABLE components were the **ancestors**, and its ACL dump names the cause:

```
PATH D:\a\_temp OWNER=BUILTIN\Administrators
   ACE NT AUTHORITY\Authenticated Users  Modify, Synchronize  inherited=True  flags=None/None
```

`Modify` contains `DELETE`, which is in the non-leaf dangerous mask, and `Authenticated Users` is not
trusted. The ACE is EFFECTIVE, so the inherit-only skip that rescued `C:\` does not apply. nub refuses
the ancestor **correctly** and relocates; ancestors are deliberately unrepairable, because nub must not
re-permission a directory it does not own.

**The CRT-linkage hypothesis is dead.** The failing shard failed 40/40 in BOTH linkages and the clean
shards passed 40/40 in both, so linkage is irrelevant. About 1 `windows-latest` image in 6 carries the
ACE, which matches the 2-of-8 rate seen on the pull request.

## Run 3 — candidate bases, six shards: INVALID

All four bases reported usable on every shard, INCLUDING the `RUNNER_TEMP` control. Per the README rule
a passing control invalidates the run: it drew six good images, so nothing may be concluded from the
candidate rows. Recorded rather than deleted, because "we ran it and everything looked fine" is exactly
the result that gets mistaken for evidence later.

## Run 4 — candidate bases, twelve shards: VALID

Widened to twelve because a third of six-shard runs draw no bad image at all.

| base | usable |
| --- | --- |
| `RUNNER_TEMP` (D:, the control) | **false on 2 of 24** (1 shard x both linkages) |
| `USERPROFILE` (C:) | true, 24/24 |
| `LOCALAPPDATA` (C:) | true, 24/24 |
| `TEMP` (C:) | true, 24/24 |

The control failed, so the run is valid and the candidate rows mean something: every system-volume base
was usable on the very shard where `RUNNER_TEMP` was refused.

**Applied:** `ci.yml`'s Windows embedded-runtime round-trip now puts its cold cache under
`USERPROFILE`. `TEMP` would also work but is an 8.3 short path on these runners, which is a poor thing
to compare paths against.
