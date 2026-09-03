# Windows DACL probe

Decides why a Windows CI runner sometimes refuses nub's runtime-cache root with
`runtime cache path has an unsafe owner or DACL`.

[#830](https://github.com/nubjs/nub/pull/830) hit it on 2 of 8 CI runs; main never did in the same
window. The only difference in the binary under test is that the PR builds it with
`-C target-feature=+crt-static`, so two explanations were live:

- **CRT linkage changes the verdict.** The probe is built BOTH ways and run on the same runner, so
  linkage is the only variable between the two rows.
- **A runner-created ancestor's ACL varies between VMs.** The matrix runs several shards — separate
  VMs, the only way to sample this, since ACLs cannot change within one job — and every component's
  verdict is printed rather than only the final answer.

`src/windows_security.rs` is a **verbatim copy** of `crates/nub-core/src/windows_security.rs`, so the
verdict is the product's rather than a paraphrase. Only the traversal loop is reproduced locally, from
`runtime_cache::walk_windows_base`, so each component can be reported.

## Reading the output

`RESULT linkage=<static|dynamic> failures=<n>/<iterations>` per binary, plus an `UNSTABLE` line naming
any component that was refused, with its `leaf` and `volume_root` flags.

- static > 0 and dynamic == 0, repeatedly → the CRT linkage is implicated.
- both > 0 on some shards and both == 0 on others → the runner image varies, and linkage is a red
  herring.
- both == 0 everywhere → this reproduction does not capture the failure; do not read anything else
  into it.

The job never fails on a clean shard. A shard that does not reproduce is a real result, and a red X on
it would train readers to ignore the probe.
