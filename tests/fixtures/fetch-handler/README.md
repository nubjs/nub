# `fetch`-handler fixtures

Entries for the default-export `fetch` handler (`crates/nub-cli/tests/fetch_handler.rs`). Each one is written the way a user would write it, so a fixture doubles as the smallest example of the shape it covers.

Every served fixture is launched with `PORT=0` by the test, which makes the kernel pick a free port; the test reads the chosen one off the `Listening on …` line. So nothing here hardcodes a port, and the suite is safe to run in parallel with itself.
