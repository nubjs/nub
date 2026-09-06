# Offline registry tests

The registry integration tests use a local Verdaccio instance with committed package fixtures. Tests receive its address through the internal `NUB_TEST_REGISTRY` variable.

The registry has no public uplink. Seeding preserves publication timestamps and rewrites tarball URLs to the local server. Socket checks reject malware findings; a known-CVE allowance applies only to its named fixture and alert types.

The CI registry variable is scoped to the converted integration tests. Other tests retain their own environment, including the checks that reject leaked internal variables. Package-extension drift forces matching locked packages through fresh resolution so edits apply on warm installs.
