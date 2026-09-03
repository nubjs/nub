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
