# Enforcement probe

Proves, on the same binary a corpus sweep runs, that the build jail is actually enforcing — in
both directions, inside one run.

```sh
export NUB_BIN=/path/to/nub
./run-probe.sh PROD
./run-probe.sh A0
```

A local `file:` package with no catalog entry, so it inherits the jail's default policy rather
than a grant. Six checks:

| check | what it establishes |
| --- | --- |
| `write_own` | **the positive control.** The package's own directory, writable in both arms |
| `write_outerhome` | the run's `HOME`, which the jail redirects away from |
| `write_realhome` | the machine's real home — a different escape from the redirected one |
| `read_secret` | a planted `TOPSECRET` file under the run's `HOME` |
| `write_project` | the fixture project root |
| `net_connect` | a raw TCP connect, no DNS dependency |

The expected result is `write_own=OK` in **both** arms, everything else denied under `PROD` and
`OK` under `A0`. The script then reports what is actually on disk afterwards, read from outside
the jail, because a script's own report of a successful write is the thing under test.

## Why `write_own` is not optional

An all-denied result is indistinguishable from a script that never ran. Without a check that must
succeed in the confined arm, a binary that failed to start, a package that was never approved, and
a perfectly enforcing jail all produce the same clean-looking wall of `DENIED`.

## Why every path is a baked-in literal

**The jail scrubs the environment.** Passing the probe its target paths through env vars does not
work, and — this is the part worth remembering — it does not fail honestly:

```
write_outerhome=DENIED(ERR_INVALID_ARG_TYPE)   read_secret=DENIED(ERR_INVALID_ARG_TYPE)
```

`path.join(undefined, …)` throws, the catch block records a denial, and four checks report a clean
refusal from a jail that was never consulted. The first version of this probe did exactly that and
produced a plausible, entirely worthless enforcement proof. `run-probe.sh` substitutes absolute
paths into `probe.js.tmpl` before the script is written, and aborts if any placeholder survives.

The errno is load-bearing for the same reason: `EACCES`/`EPERM` is the jail, `ENOENT` is a broken
fixture, and a `TypeError` is a broken probe. The catch block records `e.code` so those stay
distinguishable instead of collapsing into "denied".

## Measured

`77e3b74afe`, Linux 6.17.0-1021-gcp, Landlock ABI 7:

```
PROD  write_own=OK(verified) write_outerhome=DENIED(EACCES) write_realhome=DENIED(EACCES)
      read_secret=DENIED(EACCES) write_project=DENIED(EACCES) net_connect=DENIED(EPERM)
      on_disk: realhome=absent outerhome=absent project=absent
A0    write_own=OK(verified) write_outerhome=OK(wrote) write_realhome=OK(wrote)
      read_secret=OK(read) write_project=OK(wrote) net_connect=OK(connected)
      on_disk: realhome=PRESENT outerhome=PRESENT project=PRESENT
```
