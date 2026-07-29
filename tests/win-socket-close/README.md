# win-socket-close

A throwaway differential probe for the `crates/nub-cli/tests/init_cmd.rs` registry-fixture flake, where `default_install_uses_a_mature_types_release_under_the_strict_age_floor` failed on `Test (windows-latest, node 24)` with a transport-level error fetching the tarball:

```
failed to fetch @nubjs/types@0.4.13: HTTP error: error sending request for url (http://127.0.0.1:<port>/tarballs/_nubjs_types-0.4.13.tgz)
```

Resolution was correct — the failure was the transfer, and the tarball is the largest body the fixture serves. The fixture's connection handler read one 4096-byte chunk of the request, wrote the response, and let the thread drop the socket.

## What it measures

Two servers that differ in exactly one thing, driven by an identical client:

| Mode | Close discipline |
| --- | --- |
| `Abortive` | one bounded read, write the response, drop the socket |
| `Graceful` | read the request to its end, write, `shutdown(Write)`, drain to EOF |

The client sends its request in two segments with a gap, so the `Abortive` server always closes with unread bytes pending. Each mode serves a 4 MiB body 15 times; a run is a success only when the client receives all 4 MiB.

## Results

CI run [30473314496](https://github.com/nubjs/nub/actions/runs/30473314496), 15 requests per mode:

| Runner | Abortive | Graceful | How the abortive loss surfaces |
| --- | --- | --- | --- |
| `windows-latest` x86_64 | 0/15 | 15/15 | `ConnectionReset` — os error 10054, "forcibly closed by the remote host" |
| `ubuntu-latest` x86_64 | 0/15 | 15/15 | `ConnectionReset` — os error 104 |
| `macos-latest` aarch64 | 0/15 | 15/15 | no RST, no FIN — the transfer stalls until the read times out |

The abortive close loses every response; the graceful close loses none. Windows produces exactly the reset that matches the reported transport error. Do not key on the error kind, though — macOS reproduces the same loss with no reset at all, and one instrumented local run simply stopped at 4,015,071 of 4,194,304 bytes.

The mechanism is therefore not Windows-specific. What is inferred rather than proven is why the real fixture only tripped on Windows: a small loopback GET normally arrives in one segment, leaving nothing unread, and Windows presumably split it often enough to matter. `fetch-retries=0` in that test removes any retry cushion.

## Reproduce

```sh
cd tests/win-socket-close && cargo run --release
```

Exit code is non-zero only if the `Graceful` mode loses a response. `Abortive` failing is the expected observation, not an error.

CI runs it on Linux, macOS, and Windows via `.github/workflows/win-socket-close.yml`, which is branch-scoped — no pull request required.
