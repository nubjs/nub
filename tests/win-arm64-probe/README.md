# win32-arm64 probe

The last of nub's eight published triples with no coverage. It cannot be reached from a Mac:
cross-building a launcher dies in `ring`'s build script needing `aarch64-w64-mingw32-clang`, and
the `msvc` target needs MSVC. So it needs a real Windows-on-ARM runner.

This answers two questions in one run, in order, because the second is worthless if the first fails:

1. **Does `windows-11-arm` resolve for this repo at all?** If the label is unavailable the job sits
   unclaimed rather than failing, so the first step prints the architecture it actually landed on.
   A job that reports `X64` there is a mislabelled runner, not a passing probe.
2. **Does a compiled artifact work?** Build `nub` and the launcher natively, compile a trivial
   program, delete nothing else, and run it. The artifact must print exactly `ok:true`.

Branch-scoped: it runs on pushes to `compile-spike`, with no pull request. Delete the workflow once
the answer is folded into the native-islands matrix.
